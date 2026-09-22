# OxDock

**Dockerfile inspired build DSL for Rust**

OxDock is a Dockerfile inspired build DSL for Rust. Embed scripts at compile time with macros, or run the same scripts as standalone CLI pipelines. Native. No containers. No daemon. No VM. All commands run identically on every OS, except RUN.

Supports platform gating, async tasks, and piped workflows for custom pipelines.

[Documentation](https://docs.rs/oxdock/0.17.0-alpha/oxdock/)

Add it to your Rust build with `cargo add oxdock@0.17.0-alpha`, or install the standalone runner with `cargo install oxdock@0.17.0-alpha`.

Run a script:

```sh
oxdock <PATH>
```

## Embed at compile time

Scripts run during `rustc`, and their artifacts ship inside the binary with zero heap allocation, `no_std` included:

```rust
use oxdock_macros::oxdock_embed;

oxdock_embed! {
    // Embedded resources are mapped to `SiteAssets::get(resource)`
    name: SiteAssets,
    script: {
        // Scripts run in an ephemeral snapshot workspace: every command
        // sees an isolated temp dir, so the local checkout stays untouched
        // unless the script opts in with WORKSPACE LOCAL. Finished assets
        // are staged to out_dir below, where rustc scoops them up with
        // include_bytes!.
        ENV PROJECT=OxDock
        MKDIR dist
        // Provenance comes from the shell: only the matching gate runs,
        // so this stays green on every OS in CI.
        [unix] LET $os: STRING = RUN uname -srm
        [windows] LET $os: STRING = RUN ver
        LET $toolchain: STRING = RUN cargo --version
        WRITE dist/os.txt "{{ $os }}"
        WRITE dist/toolchain.txt "{{ $toolchain }}"
        WRITE dist/manifest.txt "os toolchain"
    },
    // Generated assets land under target/, keeping the source tree clean
    out_dir: "target/prebuilt",
}

fn main() {
    // Verify we can read the resources we just created
    let manifest = SiteAssets::get("dist/manifest.txt").expect("manifest must be embedded");
    assert_eq!(manifest.data.as_ref(), b"os toolchain");
    let toolchain = SiteAssets::get("dist/toolchain.txt").expect("toolchain must be embedded");
    assert!(toolchain.data.starts_with(b"cargo "));
    let os = SiteAssets::get("dist/os.txt").expect("os must be embedded");
    assert!(!os.data.is_empty());
}
```

For each artifact the macro emits a constant backed by `include_bytes!`, which bakes the file bytes into read-only binary data during compilation. At runtime `get()` scans a static table and returns a borrowed slice, so there are no file reads and no heap allocation. The support types only need `alloc::borrow::Cow` and core iterators, which is why it works in `no_std`.

Asset scripts resolve `STD` (via `IMPORT [STD]`) and `SCRIPT` functions only. There is no `modules:` prefix here, and that is structural, not missing: opaque modules defer membership to runtime, but asset scripts execute at compile time with no `Engine` to resolve against. Scripts needing host functions belong in `build.rs` through the `Engine` facade instead.

### Run scripts inline

The `oxdock!` macro builds the same DSL into a `Vec<Step>` at compile time, so tests and tools can run scripts without a file. Pass the steps to a `run_steps_*` runner with a guarded root. The root types live in `oxdock-fs`, so add both crates: `cargo add oxdock oxdock-fs`. Only portable commands are used below, so the script behaves identically on every OS.

```rust
use oxdock::{oxdock, oxdock_parser, run_steps_with_context};
use oxdock_fs::{GuardedPath, PathResolver};

// A version stamping pipeline: variables, a function, a loop over a list,
// a conditional call, templates, and native assertions. The version comes
// from Cargo at compile time, never a literal.
let crate_version = env!("CARGO_PKG_VERSION");
let steps: Vec<oxdock_parser::Step> = oxdock! {
    ENV PROJECT=OxDock
    LET $version: STRING = #crate_version
    MKDIR dist
    FUNC STAMP($name: STRING) {
        WRITE dist/{{ $name }}.txt {{ $name }} {{ env:PROJECT }} {{ $version }}
        RETURN $name
    }
    FOR $name: STRING IN ["alpha", "beta"] {
        STAMP($name)
    }
    FUNC PICK($flag: BOOL) {
        IF $flag {
            RETURN "alpha"
        }
        RETURN "beta"
    }
    LET $picked: STRING = PICK(true)
    WRITE dist/picked.txt {{ $picked }}
    LET $a: STRING = READ dist/alpha.txt
    LET $b: STRING = READ dist/beta.txt
    LET $p: STRING = READ dist/picked.txt
    ASSERT_EQ $a "alpha OxDock 0.17.0-alpha"
    ASSERT_EQ $b "beta OxDock 0.17.0-alpha"
    ASSERT_EQ $p "alpha"
};

let temp = GuardedPath::tempdir().expect("tempdir");
let root = temp.as_guarded_path().clone();
run_steps_with_context(&root, &root, &steps).expect("run script");

let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
let out = root.join("dist/alpha.txt").expect("out path");
assert_eq!(
    resolver.read_to_string(&out).expect("read out"),
    "alpha OxDock 0.17.0-alpha"
);
```

Use `#var` to inject Rust values into the script (any value that implements `Display`). DSL variables keep their `$var` form and are unaffected. Guards accept injected values too, so Rust flags can gate steps. The script below wires a pipe between steps, reads one line back into a variable, and expands every generated file.

```rust
use oxdock::{oxdock, oxdock_parser, run_steps_with_context};
use oxdock_fs::{GuardedPath, PathResolver};

let project = "OxDock";
let verbose = true;
let steps: Vec<oxdock_parser::Step> = oxdock! {
    ENV PROJECT=#project
    MKDIR dist
    [bool:#verbose] WRITE dist/verbose.log "verbose on"
    LET $log: PIPE
    WITH_IO [stdout=$log] ECHO "built {{ env:PROJECT }}"
    WITH_IO [stdin=$log] READ_LINE $line
    WRITE dist/build.txt "{{ $line }}"
    IMPORT [STD]
    FOR $f: STRING IN GLOB("dist/*.txt") {
        EXPAND $f
    }
    ASSERT_CONTAINS stdout "built OxDock"
    LET $build: STRING = READ dist/build.txt
    LET $verbose: STRING = READ dist/verbose.log
    ASSERT_EQ $build "built OxDock"
    ASSERT_EQ $verbose "verbose on"
};

let temp = GuardedPath::tempdir().expect("tempdir");
let root = temp.as_guarded_path().clone();
run_steps_with_context(&root, &root, &steps).expect("run script");

let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
let out = root.join("dist/build.txt").expect("out path");
assert_eq!(
    resolver.read_to_string(&out).expect("read out"),
    "built OxDock"
);
```

The top level runners need the default `cli` feature. With `--no-default-features`, run the same steps through `oxdock::oxdock_core::run_steps_*` instead.

Scripts that call host functions declare their modules up front: the macro parses at compile time with only `STD` known, so `modules: [DEMO],` as the first line makes `IMPORT [DEMO]` and `DEMO::...` calls resolve (membership is checked at runtime). Scripts using only `STD` and `SCRIPT` functions omit it. See [Extending OxDock from Rust](#extending-oxdock-from-rust) for the complete example.

## Extend the language

New types and functions take two attributes, a type registration, and a function module. Small
`Copy` scalars can ride inline on the stack with zero allocation
instead of heap boxing; both forms, with stateful functions, live under
[Extending OxDock from Rust](#extending-oxdock-from-rust) below.

```rust
use oxdock::{Engine, HostModule, OxDockFn, OxDockType, Value, oxdock_func, oxdock_type};
use std::fmt;

/// Word count summary: computed in Rust, carried as one script value.
#[oxdock_type(name = "STATS")]
#[derive(Debug, Clone, PartialEq)]
struct Stats {
    words: usize,
    chars: usize,
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} words, {} chars", self.words, self.chars)
    }
}

/// Summarize a string: `STATS("hello brave world")` carries its counts.
#[oxdock_func(pure, name = "STATS")]
fn summarize(text: String) -> anyhow::Result<Value> {
    let words = text.split_whitespace().count();
    let chars = text.chars().count();
    Ok(Value::mint_heap(
        Stats::descriptor(),
        Stats { words, chars },
    ))
}

/// Read the word count back out: `WORD_COUNT($s)` is an `INT`.
#[oxdock_func(pure, returns = "INT")]
fn word_count(summary: Value) -> anyhow::Result<Value> {
    let Some(stats) = summary.read_heap::<Stats>(Stats::descriptor()) else {
        anyhow::bail!("WORD_COUNT() expects a STATS value");
    };
    Ok(Value::int(stats.words as i64))
}

fn main() -> anyhow::Result<()> {
    let mut engine = Engine::new();
    engine.register_type::<Stats>();
    engine.register_module(HostModule {
        name: "DEMO".to_string(),
        funcs: vec![Summarize::registration(), WordCount::registration()],
        types: vec![],
    });
    let temp = oxdock_fs::GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let steps: Vec<oxdock::oxdock_parser::Step> = oxdock::oxdock! {
        modules: [DEMO],
        IMPORT [DEMO]
        LET $s: STATS = STATS("hello brave world")
        LET $n: INT = WORD_COUNT($s)
        ASSERT_EQ $n 3
    };
    let run = engine.run_steps(&root, &steps)?;
    assert_eq!(format!("{}", run.bindings["s"]), "3 words, 17 chars");
    Ok(())
}
```

## Runtime architecture

Four mechanisms keep script execution predictable: how values are stored, how bytes move between commands, how state stays isolated, and how the host stays sandboxed. Each one is shown running below.

### Values: words, not objects

Every script value is a fixed 128 bit word that lives on the stack: a static type descriptor pointer plus a 64 bit payload. Only payload contents may live on the heap, never the word itself.

Scalars that fit (`INT`, `FLOAT`, `BOOL`, handles) ride inline, copied into the payload byte for byte with zero allocation, so arithmetic and loop counters run at register speed and never touch the allocator. Strings and host types ride behind a thin pointer to a single owned box holding the concrete Rust type, while containers (`LIST`, `MAP`) ride behind a thin pointer to a shared reference counted buffer. The pointer stays thin because every payload type is sized, so there are no fat pointers and no trait objects anywhere in the path.

A naive tagged enum needs 32 bytes per value (a 24 byte payload plus tag and padding), so the word form holds four values per 64 byte cache line instead of two.

Type checks compare one descriptor address, and operations (`clone`, `drop`, equality, formatting) call the descriptor directly, with no registry lookup and no lock. Each type owns one compile time descriptor singleton, so identity is pointer equality that fails closed. Host types extend the same path: `#[oxdock_type]` derives a static descriptor for the payload struct, and `inline` selects the zero allocation form for small `Copy` scalars.

There is no garbage collector because values form trees, not graphs. Each exclusive heap word owns its box exactly once: cloning allocates a fresh box with a deep copy, dropping frees it. Each container word co-owns its buffer instead: cloning a `LIST` or `MAP` bumps a reference count in constant time with no allocation, dropping releases one count. A `LIST` owns its items and a `MAP` owns its entries.

Nothing is mutably borrowed from two places, so cycles cannot form and plain deterministic cleanup suffices. Pointer casts always round trip through the same concrete box or buffer type, and inline words never enter the pointer domain, which keeps provenance intact. The lifecycle is checked under Miri.

No pauses, no write barriers, no background collector. Python, JavaScript, and Lua permit aliasing and cycles and need tracing collectors to reclaim them; here there is nothing to trace.

| Design | Stack word | Scalar allocs | Cache density | Host extensibility |
| --- | --- | --- | --- | --- |
| Tagged Rust enum | 32 bytes | 0 | 2 values per line | Closed |
| Boxed trait objects | 16 byte fat pointer | 1 per scalar | Medium | Open |
| 16 byte word (this design) | 16 bytes | 0 | 4 values per line | Open, static descriptors |
| NaN boxing (LuaJIT, V8) | 8 bytes | 0 | 8 values per line | Constrained |

Two trade offs come with the form. Cloning a string still deep copies it, since only containers share buffers. And 16 bytes is roomier than 8 byte NaN boxing, which buys clean 64 bit integer and float storage plus safe abstraction boundaries without pointer masking.

### Transport: pipes

A command's standard streams can be rerouted through pipes declared with `LET $p: PIPE`, so producers and consumers connect without touching the terminal or temp files. Buffers stay in memory and spill to a guarded temp file past 8 MiB, and background single command tasks can promote a pipe to a zero copy OS kernel pair instead.

```oxdock
LET $log: PIPE
WITH_IO [stdout=$log] ECHO hello
WITH_IO [stdin=$log] READ_LINE $line
ASSERT_EQ $line "hello"
```

### Scope isolation

State mutations stay where the script puts them. Entering a braced block or a function call snapshots variables and settings, and exiting restores all of them, so nothing leaks outward. Background tasks fork the same way, so concurrent workers cannot observe each other's half finished mutations. Only pipes and filesystem effects cross these boundaries, by design.

```oxdock
FUNC SHADOW($v: STRING) {
    LET $inner: STRING = "inner"
    RETURN $v
}
LET $out: STRING = SHADOW("param")
ASSERT_EQ $out "param"
```

### Sandboxing

Every path resolves inside a guarded workspace root, and escapes are rejected before any filesystem call. Scripts start with an empty process environment and opt into host variables explicitly.

```oxdock expect_error:"escapes allowed root"
WRITE ../escape.txt "nope"
```

### Prepare during the build

`oxdock_embed!` ships artifacts inside the binary. `oxdock_prepare!` runs the same script but emits no runtime module. Use it when assets only need to exist during the build, for codegen or `include!` workflows.

```rust
use oxdock_macros::oxdock_prepare;

oxdock_prepare! {
    name: PreparedAssets,
    script: {
        MKDIR gen
        WRITE gen/out.txt generated
        LET $o: STRING = READ gen/out.txt
        ASSERT_EQ $o "generated"
    },
    out_dir: "target/prebuilt_prepare",
}

fn main() {}
```

### Stream bytes between steps

`WITH_IO` routes stdout into script pipes declared with `LET $p: PIPE` and back into stdin, so steps form custom pipelines. Pipes hold bytes in memory and spill to a temp file above 8 MiB. Wrapping a single RUN in ASYNC promotes the pipe to a zero copy OS kernel pipe instead; the consumer must then run while the producer is alive.

```oxdock
LET $msg: PIPE
WITH_IO [stdout=$msg] ECHO piped-bytes
WITH_IO [stdin=$msg] WRITE piped.txt
READ piped.txt
ASSERT_CONTAINS stdout "piped-bytes"
```

### Workspaces start ephemeral

Scripts start in an ephemeral snapshot workspace, an isolated temp dir that leaves the source tree untouched. Pull inputs with `COPY` or `COPY_GIT`. Switch to the local directory with `WORKSPACE LOCAL` when the script should mutate in place, to the persistent per-project cache with `WORKSPACE CACHE` for artifacts that must survive restarts, or to `WORKSPACE SYSTEM` for full filesystem access.

```oxdock
WRITE snap.txt from-snapshot
LET $s: STRING = READ snap.txt
ASSERT_EQ $s "from-snapshot"
WORKSPACE LOCAL
WRITE local.txt from-local
LET $l: STRING = READ local.txt
ASSERT_EQ $l "from-local"
WORKSPACE CACHE
WRITE cached.txt from-cache
LET $c: STRING = READ cached.txt
ASSERT_EQ $c "from-cache"
WORKSPACE SYSTEM
WRITE sys.txt from-system
LET $y: STRING = READ sys.txt
ASSERT_EQ $y "from-system"
```

OxDock scripts automate build-time work: creating files, snapshotting
workspaces, verifying artifacts with native assertions. You run them
two ways off the same core: embedded in your build with macros, or as
standalone processes with the CLI.

This crate is the front door. It re-exports the CLI runner (enabled by
default) and the build macros (always available), so most users only
ever depend on `oxdock`.

One language for the whole build: farm steps out to npm, bundlers, or code generators and pull their artifacts back under cargo's control. Pipe bytes between steps without buffering whole outputs, fan work out with `ASYNC`, or skip embedding entirely and run the same scripts as standalone CLI processes.

# DSL Reference

> **Prototype status**: OxDock is still being prototyped. DSL syntax and Rust APIs may change without deprecation warnings until the first stable release.

Scripts are sequences of instructions, one per line. Instructions may be prefixed with **guards** (`[...]`) that decide whether they run, and grouped into **scoped blocks** (`{ ... }`). The authoritative grammar is [`crates/oxdock-parser/src/dsl.pest`](https://github.com/jzombie/rust-oxdock/blob/main/crates/oxdock-parser/src/dsl.pest), which is also embedded in the parser crate as the `LANGUAGE_SPEC` constant for tooling.

## Lexical structure

- **Commands are uppercase** and case-sensitive: `WORKDIR`, not `workdir`. Lowercase or mixed-case spellings are parse errors with an uppercase hint.
- One instruction per line; a semicolon (`;`) splits multiple instructions on a single line.
- Paths and arguments use forward slashes (`/`) for portability (see [Path Separators](#path-separators)).
- Scripts do **not** inherit your shell environment unless `INHERIT_ENV` opts specific keys in (see [Selective environment inheritance](#selective-environment-inheritance)).

### Declared variable types

Every variable binding declares its type at the binding site. `LET $name: TYPE = ...` creates the binding, `$name = ...` mutates it, and bodies use the bare `$name` reference. The leading `$` keeps mutation distinct from `KEY=value` command assignments. Repeating `LET` for the same name in the same scope is a redeclaration error. Loop variables are declared the same way: `FOR $item: STRING IN ...`. Valid types: `STRING`, `INT`, `FLOAT`, `BOOL`, `PIPE`, `LIST`, `MAP`, `HANDLE`, `DURATION`, `PATH`.

```oxdock
LET $count: INT = 1
$count = 2
LET $msg: STRING = hello
FOR $item: STRING IN ["a", "b"] {
    ECHO "{{ $item }}"
}
WRITE count.txt "{{ $count }}"
LET $c: STRING = READ count.txt
ASSERT_EQ $c "2"
```

The `env:KEY` expression reads the script environment into a plain value. A `$var` reference never reads the environment, even when the names match:

```oxdock
ENV FOO="bar"
LET $e: STRING = env:FOO
WRITE env.txt "{{ $e }}"
LET $v: STRING = READ env.txt
ASSERT_EQ $v "bar"
```

### Functions and commands

Parentheses mark the boundary between computing a value and running a pipeline step. Builtin functions (`INSPECT`, `LOAD_TOML`, `LOAD_JSON`, `GLOB`, `INT`, `FLOAT`, `PATH_TYPE`) evaluate to an in-memory value and never write to standard output. They compute or query (`Value::Map`, `Value::String`, `Value::Int`, `Value::Float`, `Value::List`) with zero stream side effects, so they appear only where values are expected: on the right-hand side of `LET`, inside `IF` conditions, or nested in other calls. Commands (`READ`, `ECHO`, `RUN`, `WRITE`, `ASSERT_EQ`) are line-starting statements with space-separated arguments. They drive the I/O pipeline, streaming bytes to stdout or mutating state, which makes their output available to pipes and `LET` capture. When a function evaluates, process stdout stays completely untouched. When a command runs, streaming bytes is the payload:

```oxdock
// Functions compute values; stdout stays untouched.
IMPORT [STD]
LET $t: STRING = PATH_TYPE("missing.txt")
LET $n: INT = INT("41") + 1
ASSERT_EQ $t "absent"
ASSERT_EQ $n 42
```

### Statements and semicolons

```oxdock
// One line, two instructions: the semicolon splits them.
ECHO one; ECHO two
ASSERT_CONTAINS stdout "one"
ASSERT_CONTAINS stdout "two"
```

### RUN shell and exec forms

Shell form (`RUN <command...>`) joins its arguments and runs the string in the system shell. Exec form (`RUN ["exe", "arg", ...]`) spawns the executable directly with no shell. Exec form has no shell expansion, globbing, redirection, or pipes. Quoted `{{ ... }}` templates still interpolate per element, and guards and wrappers (`ASYNC`, `TIMEOUT`, `WITH_IO`) apply to both forms.

```oxdock
RUN ["cargo", "--version"]
ASSERT_CONTAINS stdout "cargo"
```

### Comments

Three comment styles are supported: `//` line comments, nestable `/* ... */` block comments, and `#` comments. A `#` comment occupies a whole line (optionally indented) and may also trail values inside multi-line `()`, `[]`, and `{}` brackets; inside a command payload a `#` is ordinary text. Similarly, `//` ends a `RUN` argument list but survives inside quoted strings:

```oxdock
// slash comment at end of line
# hash comment occupies the whole line

/* block comments
   /* nest */
   like this */
ECHO visible-after-comments
ASSERT_CONTAINS stdout "visible-after-comments"
```

```oxdock
ECHO hash-mid-line # stays-in-payload
RUN echo run-args-stop-at-slashes // removed-as-comment
ASSERT_CONTAINS stdout "hash-mid-line # stays-in-payload"
ASSERT_CONTAINS stdout "run-args-stop-at-slashes"
```

Comment markers inside quoted strings are always preserved.

### Quoting and escaping

Arguments accept single- or double-quoted strings; the escape sequences `\"` and `\'` embed a quote, and any other backslash escape keeps the escaped character while dropping the backslash. Quoted fragments containing whitespace, `;`, newlines, `//`, or `/*` retain their quotes when `RUN` reconstructs the command string:

```oxdock
// Single and double quotes behave identically.
ECHO 'single quotes'
ECHO "double quotes"

// \" embeds a quote; the backslash itself is consumed.
ECHO "escaped \" quote"
ASSERT_CONTAINS stdout "single quotes"
ASSERT_CONTAINS stdout "double quotes"
ASSERT_CONTAINS stdout 'escaped " quote'
```

## Templates

`{{ env:KEY }}` interpolates script environment values into arguments at execution time. Values come from the script environment (`ENV`, inherited keys) — there is no fallback to host variables in command context, and unknown keys expand to an empty string. The unprefixed form `{{ KEY }}` is not a valid template and also expands to empty, so always use the `env:`-prefixed spelling:

```oxdock
ENV GREETING=hello-world

// env:-prefixed form: interpolates from the SCRIPT environment.
ECHO <{{ env:GREETING }}>

// Bare braces are not a template: they expand to empty.
ECHO <{{ GREETING }}>
ASSERT_CONTAINS stdout "<hello-world>"
ASSERT_CONTAINS stdout "<>"
```

## Guards and scoped blocks

A guard is a bracketed expression that gates the instruction or block that follows it. Inside the brackets:

- `env:KEY` passes when variable `KEY` exists and is non-empty; `eq(env:KEY, value)` and `ne(env:KEY, value)` compare values.
- Bare platform tags pass based on the host: `linux`, `macos` (alias `mac`), `windows`, `unix`. Tags are case-insensitive.
- A comma-separated list means **AND**: `[env:A, linux]`.
- Disjunction is expressed as a call — `any(expr, expr, ...)` with at least two branches — not an infix operator.
- Conjunction is expressed as a call — `all(expr, expr, ...)` — or implicitly via comma separation.
- Any predicate may be negated with `not(...)`: `[not(env:SKIP)]`.
- Parentheses group expressions: `[any(env:A, linux), mac]`.

Guards attach to the next instruction. Several guard lines in a row chain onto the same target, and a guard immediately followed by `{` opens a guarded block whose guard applies to every enclosed instruction.

Guard evaluation checks the script environment first and falls back to the process environment, so guards interact naturally with `INHERIT_ENV` and `ENV`.

### Environment guards

```oxdock env:DEPLOY_TARGET=staging
// Copy the key from the host environment (the runner injects it).
INHERIT_ENV [DEPLOY_TARGET]

// Passes when the variable exists with any non-empty value.
[env:DEPLOY_TARGET] ECHO deploy-target-visible

// Equality against the inherited value.
[eq(env:DEPLOY_TARGET, staging)] ECHO deploying-to-staging

// Inequality: skipped below, because DEPLOY_TARGET IS staging.
[ne(env:DEPLOY_TARGET, staging)] ECHO deploying-elsewhere

ASSERT_CONTAINS stdout "deploy-target-visible"
ASSERT_CONTAINS stdout "deploying-to-staging"
```

### Platform guards

```oxdock
// Exactly one block runs depending on the host OS; every command
// inside a guarded block inherits the block's guard.
[windows] {
  WRITE os-report.txt windows
  ECHO windows-detected
  LET $rep: STRING = READ os-report.txt
  ASSERT_EQ $rep "windows"
  ASSERT_CONTAINS stdout "windows-detected"
}
[unix] {
  WRITE os-report.txt unix-family
  ECHO unix-detected
  LET $rep: STRING = READ os-report.txt
  ASSERT_EQ $rep "unix-family"
  ASSERT_CONTAINS stdout "unix-detected"
}
```

### Negation, disjunction, and composition

```oxdock env:OXDOCK_DOC_FEATURE_A=enabled
// Bring the runner-injected value into the script environment.
INHERIT_ENV [OXDOCK_DOC_FEATURE_A]

// not(...) inverts the predicate: passes because the variable does NOT exist.
[not(env:OXDOCK_DOC_UNDEFINED_VAR)] ECHO negation-passes-for-undefined

// any(...) passes when ANY branch holds; A exists, so this runs.
[any(env:OXDOCK_DOC_FEATURE_A, env:OXDOCK_DOC_FEATURE_B)] ECHO or-matched-a-branch

// Comma composes with AND: (A or linux) AND A — true here on every OS.
[any(env:OXDOCK_DOC_FEATURE_A, linux), env:OXDOCK_DOC_FEATURE_A] ECHO composed-and-or-guard

ASSERT_CONTAINS stdout "negation-passes-for-undefined"
ASSERT_CONTAINS stdout "or-matched-a-branch"
ASSERT_CONTAINS stdout "composed-and-or-guard"
```

### Multi-line guards

Bracket expressions may span lines. Chained guard lines apply conjunctively to the next instruction; here neither variable is defined, so the gated instruction is skipped:

```oxdock
// Brackets may span lines; chained lines AND together and gate
// the next command.
[
  env:OXDOCK_DOC_CHAIN_ONE,
  env:OXDOCK_DOC_CHAIN_TWO
]

// Neither variable exists, so this WRITE is skipped entirely.
WRITE chained.txt applied

// The artifact was never created.
IMPORT [STD]
LET $t: STRING = PATH_TYPE("chained.txt")
ASSERT_EQ $t "absent"
```

### Scoped blocks

Braced blocks scope everything: `LET` variables, `ENV` values, `WORKDIR`, and `WORKSPACE` all revert when the block exits. Files created inside a block persist on disk, and pipes registered with `WITH_IO` stay open — those are the only things that cross a scope boundary. (A bare `{ ... }` needs an always-true guard: `[bool:true]`. Single commands, including single `WITH_IO` lines like `READ_LINE`, never open a scope.)

```oxdock
LET $a: STRING = "some_value"
ENV MODE="production"
MKDIR scoped_area
WORKDIR scoped_area

// Guarded block: LET, ENV, and WORKDIR below are scoped and revert
// when the block closes.
[bool:true] {
    LET $a: STRING = "inner_value"
    ENV MODE="staging"
    WRITE inner.txt "{{ $a }}-{{ env:MODE }}"
}

// $a is back to "some_value", MODE is back to "production",
// and cwd is back at scoped_area — but files persist.
LET $in_body: STRING = READ inner.txt
ASSERT_EQ $in_body "inner_value-staging"
WRITE outer.txt "{{ $a }}-{{ env:MODE }}"
LET $out_body: STRING = READ outer.txt
ASSERT_EQ $out_body "some_value-production"
```

`IF`/`ELSE` branches, `FOR` loop bodies, `TIMEOUT` bodies, `ASYNC` bodies, and `WITH_IO [..] { ... }` blocks are all scopes under the same rule: only files and pipes leak out.

### EXIT in nested blocks

`EXIT <code>` stops the pipeline immediately with an `EXIT requested with code <code>` error — steps after it never run, at any nesting depth. Unwinding still happens on the way out: every enclosing block reverts its `LET`/`ENV`/`WORKDIR`/`WORKSPACE` state before the error propagates, anonymous background tasks are killed synchronously, and files written before the `EXIT` persist. An `EXIT` inside `TIMEOUT` passes through unwrapped (never relabeled as a deadline error); an `EXIT` inside an `ASYNC` task ends that task with an error, which the parent sees at `AWAIT` or end-of-pipeline reaping.

```oxdock expect_error:"EXIT requested with code 3"
WRITE before.txt "persisted"
LET $b: STRING = READ before.txt
ASSERT_EQ $b "persisted"
[bool:true] {
    EXIT 3
    WRITE unreachable.txt "never"
}
```

## Deadlines with TIMEOUT

`TIMEOUT <duration> <command>` bounds a single step, `TIMEOUT <duration> { ... }` bounds a block, and `TIMEOUT <duration> AWAIT $task` bounds a task join. Durations accept `ms`, `s`, `m`, and `h` suffixes (a bare number means seconds, e.g. `TIMEOUT 30 ...`). A step that overruns its deadline is cancelled — a blocking foreground process is killed — and the pipeline fails with a `TIMEOUT after <duration>` error. `SLEEP <duration>` parks the step without spawning a shell, which makes it ideal for testing deadlines portably (a `SLEEP` inside an expired `TIMEOUT` is interrupted instead of running out the clock).

```oxdock
// Inline form bounds a single command.
TIMEOUT 30s WRITE heartbeat.txt alive
LET $beat: STRING = READ heartbeat.txt
ASSERT_EQ $beat "alive"

// Block form bounds multiple steps.
TIMEOUT 30s {
    WRITE a.txt one
    WRITE b.txt two
}
LET $a: STRING = READ a.txt
LET $b: STRING = READ b.txt
ASSERT_EQ $a "one"
ASSERT_EQ $b "two"

// AWAIT form bounds a task join.
LET $quick: HANDLE = ASYNC {
    ECHO hi
}
TIMEOUT 30s AWAIT $quick
```

`ASYNC` wraps any command or block — including `TIMEOUT`, `CANCEL`, `SLEEP`, and nested `ASYNC` — in either nesting order with the same deadline semantics. `LET $task: HANDLE = ASYNC TIMEOUT 30s RUN "build"` enforces the deadline inside the background thread (a later `AWAIT $task` surfaces the `TIMEOUT` error), while `TIMEOUT 30s AWAIT $task` preempts a hung task from the awaiting side:

```oxdock
// ASYNC wraps TIMEOUT: the deadline fires inside the background thread.
LET $bounded: HANDLE = ASYNC TIMEOUT 30s ECHO "bounded"
AWAIT $bounded
```

The one structural exception is `WITH_IO`, which must wrap `ASYNC` from the outside (`LET $p: PIPE` first, then `WITH_IO [stdout=$p] ASYNC ...`) so pipe endpoints are allocated synchronously on the main thread before the worker spawns. Placing `WITH_IO` directly inside `ASYNC` is rejected at parse time.

## Cancelling tasks with CANCEL

`CANCEL $task` synchronously stops a named background task spawned via `LET $task: HANDLE = ASYNC ...`. It is blocking: when the statement returns, the task thread has been joined and its OS process reaped, so no residual filesystem or stream mutation can follow and the next step runs in a quiet workspace. Only named tasks can be cancelled; a later `AWAIT $task` fails with a cancellation error, and a second `CANCEL $task` fails as already cancelled.

```oxdock
// CANCEL form stops a named background task synchronously.
LET $worker: HANDLE = ASYNC SLEEP 30s
CANCEL $worker
```

## Command reference

The full command reference with runnable examples lives in the
[workspace README](https://github.com/jzombie/rust-oxdock#command-reference).
It is not bundled here because in-page links cannot resolve on all
renderers. Every example there is executed against the implementation,
so the documented behavior is guaranteed to match.

## Selective environment inheritance

Scripts no longer inherit the caller's environment wholesale. Host variables stay private unless you opt in explicitly.

- Add `INHERIT_ENV [FOO, BAR, BAZ]` at the very top of the script to copy those keys from the process environment before any other command runs.
- The directive must be top-level—no guards, no surrounding blocks, and no repeats. Trying to nest or guard it triggers a parser error so scripts stay deterministic.
- Subsequent `ENV` commands can override inherited values, similar to how Docker's `ENV` overrides `--env` flags.
- Test harnesses and embedders can supply values programmatically; the [environment-guards example](#environment-guards) injects `DEPLOY_TARGET` through the docs-conformance runner rather than the real process environment.

Keeping inheritance selective avoids leaking secrets by default while still allowing ergonomics for well-known keys (proxy settings, artifact caches, etc.).

## Path Separators

- **Cross-platform behavior:** Paths in OxDock scripts are treated as filesystem paths and are resolved using Rust's `Path`/`PathBuf` APIs. That means you can use either `/`-separated paths or `./`-prefixed relative paths in scripts and they will be interpreted correctly on Windows, macOS, and Linux.

- **Path separator preference / requirement:** For consistency and portability, OxDock scripts should use the forward slash (`/`) as the path separator in script source. While the runtime resolves paths using platform APIs and will accept platform-specific absolute paths, using `/` in scripts (even on Windows) avoids needing to escape backslashes (`\`) and matches Docker-style examples. If you must reference a native Windows absolute path, prefer the `C:/path/to` form or escape backslashes carefully.

- **Relative paths:** A leading `./` indicates a path relative to the current DSL working directory (the same semantics used by Docker). For example: `COPY ./src ./out` or `SYMLINK ./dir ./dir-link` will work on all platforms.

- **Absolute paths:** Use platform-appropriate absolute paths (e.g., `/usr/bin` on Unix-like systems, `C:\path\to` on Windows). OxDock will use the host OS path semantics when resolving absolute paths.

- **Symlinks and Windows:** Creating symlinks on Windows may require elevated permissions on some older OS versions; where symlinks are not available the CLI falls back to copying directory contents so scripts remain functional across platforms.

- **Globbing & shell expansion:** OxDock does not implicitly perform shell globbing or shell-side expansion for file arguments — when you need shell semantics use `RUN` with the platform shell, or add explicit DSL commands that accept wildcards if you want portable behavior.

## Workspaces & Filesystem

- **How workspaces are created:** OxDock materializes a clean workspace as an isolated temporary directory. It does not implicitly populate that directory from Git; scripts can pull files in via `COPY` (from the build context) or `COPY_GIT` (from a specific revision). Treat this workspace as a scratchpad surface for experimentation: you can run scripts inside it, create or modify files, and prepare assets for publishing without affecting your main source tree or requiring `--allow-dirty` workflows.

- **Typical usage pattern:** the temporary workspace is intended for short lived build and test iterations. Run scripts against it, inspect outputs, and discard when done. Because it is separate from the original repo it is safe to run multiple concurrent experiments without changing the original repo.

- **Four workspace roots:** `WORKSPACE SNAPSHOT` (the default ephemeral temp location), `WORKSPACE LOCAL` (the local directory), `WORKSPACE CACHE` (a persistent per-project cache directory shared across runs), and `WORKSPACE SYSTEM` (full filesystem access, not hermetic). `WORKSPACE` selection reverts at scope exit like `WORKDIR`.

- **Persistent cache:** `WORKSPACE CACHE` resolves through the `cache-manager` crate with OS-native per-user roots (macOS `~/Library/Caches`, Linux `$XDG_CACHE_HOME` or `~/.cache`, Windows `%LOCALAPPDATA%`) namespaced by application identity (explicit builder argument, `OXDOCK_CACHE_APP`, `CARGO_PKG_NAME`, or the running binary name, in that order; `OXDOCK_CACHE_DIR` pins an exact directory). The cache directory is created on first use, survives restarts, and is never evicted by default.

- **Filesystem gating via `oxdock-fs`:** all filesystem operations in the runtime are routed through the crate internal `oxdock-fs` abstraction. That module centralizes path resolution, canonicalization and access checks so reads and writes can be validated against the allowed workspace root and build context. `WORKSPACE SYSTEM` intentionally bypasses these checks; scripts using it are not hermetic.

- **What `oxdock-fs` protects you from:** the guardrails are pragmatic. They prevent common mistakes such as accidentally writing outside the materialized workspace or reading files from arbitrary absolute paths. However, they are not a full sandbox. A determined process or script can still create destructive actions (for example, invoking native `RUN` commands that modify external state). If you require strict isolation, run OxDock inside a container or VM.

- **Performance:** routing via `oxdock-fs` adds negligible overhead for typical workloads. The module focuses on correctness and containment with minimal runtime cost so interactive iteration remains fast.

## Common usage

Install the binary from the registry:

```sh
cargo install oxdock@0.17.0-alpha
```

Run a script file:

```sh
oxdock ./build.oxfile
# same as: oxdock --script ./build.oxfile
```

Print help:

```sh
oxdock --help
```

## Extending OxDock from Rust

Scripts call host functions and custom types that Rust code registers
under a module. Registration is a type plus a module on one facade, and
scripts name the module: `IMPORT [DEMO]` lets the rest call
`MAKE_TAG()` bare, or qualify as `DEMO::MAKE_TAG()`. This example runs as
written: the shape first, the definitions it names right below it.

### Complete example: define a type, define functions, run a script

```rust
use oxdock::{Engine, HostModule, OxDockFn, OxDockType, oxdock_func, oxdock_type};
use std::fmt;

// The script below is the DSL itself, not a string: the `oxdock!` macro
// builds it into steps at compile time.

fn main() -> anyhow::Result<()> {
    let mut engine = Engine::new();
    engine.register_type::<Tag>();
    engine.register_module(HostModule {
        name: "DEMO".to_string(),
        funcs: vec![MakeTag::registration(), ReadTag::registration()],
        types: vec![],
    });

    let temp = oxdock_fs::GuardedPath::tempdir().unwrap();
    let root_path = temp.as_guarded_path().clone();
    let steps: Vec<oxdock::oxdock_parser::Step> = oxdock::oxdock! {
        modules: [DEMO],
        IMPORT [DEMO]
        LET $t: TAG = MAKE_TAG()
        LET $s: STRING = READ_TAG($t)
        ASSERT_EQ $s "demo"
        WRITE tag.txt "{{ $s }}::{{ $t }}"
    };
    let run = engine.run_steps(&root_path, &steps)?;
    assert!(run.bindings.contains_key("s"));

    let reader = oxdock_fs::PathResolver::new(root_path.root(), root_path.root()).unwrap();
    let written = reader
        .read_to_string(&root_path.join("tag.txt").unwrap())
        .expect("script writes tag.txt");
    assert_eq!(written.trim(), "demo::tag:demo");
    Ok(())
}

// The type. `#[oxdock_type]` implements `OxDockType` on the payload struct
// itself. The default is the heap path: the word holds a thin pointer to
// an owned box. Heap payloads require `Clone + PartialEq + Display +
// Debug + Send + Sync`.

/// Opaque label type.
#[oxdock_type(name = "TAG")]
#[derive(Debug, Clone, PartialEq)]
struct Tag(String);

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "tag:{}", self.0)
    }
}

// The functions. `#[oxdock_func(pure)]` implements `OxDockFn` on a
// registration marker named after the function (`make_tag` becomes
// `MakeTag`). The DSL name defaults to the uppercased Rust name.
// `Value::mint_heap` with the payload type's own descriptor mints a word
// of the registered type, and `read_heap` with that descriptor reads it
// back. No name lookup runs anywhere on this path.

/// Mint one opaque label.
#[oxdock_func(pure)]
fn make_tag() -> anyhow::Result<oxdock::Value> {
    Ok(oxdock::Value::mint_heap(Tag::descriptor(), Tag("demo".into())))
}

/// Read the payload back out through a descriptor-checked typed read.
#[oxdock_func(pure, returns = "STRING")]
fn read_tag(val: oxdock::Value) -> anyhow::Result<oxdock::Value> {
    let Some(tag) = val.read_heap::<Tag>(Tag::descriptor()) else {
        anyhow::bail!("READ_TAG() expects a TAG value");
    };
    Ok(oxdock::Value::string(tag.0.clone()))
}
```

The shape is always the same. `LET $t: TAG = MAKE_TAG()` calls a host
function like any native one: arity, depth budget, and the declared `TAG`
coercion apply uniformly. `READ_TAG($t)` reclaims the payload through an
id checked read. `{{ $t }}` renders the custom value through its
`Display`, so interpolation, `WRITE`, and equality treat host values like
native ones. `TYPES()` lists every registered name and
`TYPE_DESCRIBE("TAG")` returns its summary and docs, so scripts introspect
host surface exactly like native surface. Host types stay opaque:
literal syntax, `$var.key` traversal, and `FOR` iteration remain `LIST`
and `MAP` only, so queryable containers expose host accessor functions
(`MATRIX_GET($m, $row, $col)`) instead of new syntax.

### Reference: stateful functions

Without `pure`, the first parameter must be `cx: &mut StepCtx<P>`, which
exposes variables, environment, pipes, and IO. Override the DSL name and
the declared return type explicitly when the defaults do not fit:

```rust
use oxdock::{HostModule, OxDockFn, StepCtx, oxdock_func};
use oxdock::oxdock_core::ProcessManager;

/// Read an environment variable, defaulting to empty.
#[oxdock_func(name = "ENV_OR", returns = "STRING")]
fn env_or<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    key: String,
) -> anyhow::Result<oxdock::Value> {
    Ok(oxdock::Value::string(cx.get_env(&key).unwrap_or_default()))
}

fn main() {
    let mut engine = oxdock::Engine::new();
    engine.register_module(HostModule {
        name: "DEMO".to_string(),
        funcs: vec![EnvOr::registration()],
        types: vec![],
    });
}
```

Parameters accept `Value`, `String`, `i64`, `f64`, and `bool`, in either
form. Stateful markers register exactly like pure ones. Pure functions run
on the compiled math path too; stateful ones stay on the AST path unless
they opt in with `#[oxdock_func(rpn)]` (as `GLOB` does).

### Reference: inline types

`#[oxdock_type(inline)]` selects the zero allocation path for `Copy`
scalars that fit in 64 bits: the word holds the value bytes directly. This
is the same derivation the startup integer, float, boolean, and handle
types use:

```rust
use oxdock::{OxDockType, oxdock_type};
use std::fmt;

/// Entity handle.
#[oxdock_type(name = "ENTITY", inline)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct EntityId(u64);

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "entity#{}", self.0)
    }
}

fn main() {
    let mut engine = oxdock::Engine::new();
    engine.register_type::<EntityId>();
    let val = oxdock::Value::mint_inline(EntityId::descriptor(), EntityId(7));
    assert_eq!(val.inline_bits(), 7);
    assert_eq!(format!("{val}"), "entity#7");
    assert_eq!(val.clone(), val);
}
```

Four limits decide what can ride inline:

1. **Fixed 64 bit slot.** Anything larger panics at mint time (a runtime
   check, not a compile time one).
2. **`Copy` payloads only.** Inline clones copy bits and drops do
   nothing, which resource owning types cannot satisfy.
3. **No variable length data.** `STRING`, `LIST`, and `MAP` can never fit
   a fixed slot and stay behind the pointer.
4. **Widening punishes everything.** A bigger slot means fewer words per
   cache line and costlier moves for the scalars that dominate scripts.

One boundary to keep straight: words stored inside a `LIST` or `MAP`
live in that container's heap buffer, so inline describes the payload
encoding, not a promise that every word sits on the stack.

### Reference: queryable containers

Opaque types can still answer queries: pair the payload with host
accessor functions and scripts read cells, lengths, and keys through
ordinary calls, with no new syntax. The example below mints a grid and
reads one cell through its accessor; from a script the same call spells
`LET $c: INT = MATRIX_GET($m, 0, 1)`. Key paths and `FOR` iteration stay
unavailable by the boundary above.

```rust
use oxdock::{HostModule, OxDockFn, OxDockType, Value, oxdock_func, oxdock_type};
use std::fmt;

/// Integer grid with no literal syntax: scripts query it through functions.
#[oxdock_type(name = "MATRIX")]
#[derive(Debug, Clone, PartialEq)]
struct Matrix(Vec<Vec<i64>>);

impl fmt::Display for Matrix {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "matrix[{}x{}]", self.0.len(), self.0.first().map_or(0, Vec::len))
    }
}

/// Mint a fixed 1x2 grid.
#[oxdock_func(pure)]
fn make_matrix() -> anyhow::Result<Value> {
    Ok(Value::mint_heap(
        Matrix::descriptor(),
        Matrix(vec![vec![1, 2]]),
    ))
}

/// Read one cell by row and column.
#[oxdock_func(pure, returns = "INT")]
fn matrix_get(board: Value, row: i64, col: i64) -> anyhow::Result<Value> {
    let Some(grid) = board.read_heap::<Matrix>(Matrix::descriptor()) else {
        anyhow::bail!("MATRIX_GET() expects a MATRIX value");
    };
    let cell = usize::try_from(row)
        .ok()
        .and_then(|r| grid.0.get(r))
        .and_then(|r| usize::try_from(col).ok().and_then(|c| r.get(c)))
        .copied()
        .ok_or_else(|| anyhow::anyhow!("index out of bounds"))?;
    Ok(Value::int(cell))
}

fn main() -> anyhow::Result<()> {
    let word = Value::mint_heap(Matrix::descriptor(), Matrix(vec![vec![1, 2]]));
    let cell = matrix_get(word, 0, 1).unwrap();
    assert_eq!(cell.as_i64(), Some(2));

    let mut engine = oxdock::Engine::new();
    engine.register_type::<Matrix>();
    engine.register_module(HostModule {
        name: "DEMO".to_string(),
        funcs: vec![
            MakeMatrix::registration(),
            MatrixGet::registration(),
        ],
        types: vec![],
    });
    let temp = oxdock_fs::GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let run = engine.run_script(
        &root,
        "IMPORT [DEMO]\nLET $m: MATRIX = MAKE_MATRIX()\nLET $c: INT = MATRIX_GET($m, 0, 1)\n",
    )?;
    assert_eq!(run.bindings["c"].as_i64(), Some(2));
    Ok(())
}
```

### Reference: evaluation paths and the `rpn` flag

Two evaluators run scripts, and the distinction decides where a host
function may run. The AST evaluator walks the parsed tree one step at a
time. Every step knows its line number, so failures name it (`step 1:
INT() expects 1 argument(s), got 2`). Scopes, pipes, declarations, and
every statement form live here. The RPN evaluator runs arithmetic and
comparison expressions compiled to a flat stack-machine program
(`PushConst`, `LoadVar`, `Call`, `Add`, ...). A stack program carries
values only: no statements, no scopes, no pipes, and no step numbers, so
a failing call inside math reports the bare error (`INT() expects 1
argument(s), got 2`).

The tradeoff is expressiveness against compactness, not speed. The tree
can say anything the language can say, with errors that point at the
script. The stack program can only compute values, which is exactly what
math needs and nothing more. That is why not everything runs on RPN:
statements, declarations, scoping, and IO orchestration have no stack
encoding, and step-numbered errors require the tree.

For host functions the rule follows from that split. Pure functions take
only values and touch nothing, so they run on both paths with no flag.
Stateful functions default to AST-only. Opt in with
`#[oxdock_func(rpn)]` only for read-only queries that stay meaningful
inside math (`GLOB`, `LOAD_TOML`, `LOAD_JSON` do this): the function
still receives full step context, but its failures lose their step
numbers. Side-effecting stateful functions stay out, since stack-order
execution with step-less errors is the worst place for an effect to go
wrong.

### Why embedding Rust here stays simple

Compared against embedding Rust in Python, there are three structural
reasons the host boundary stays small, and none of them are API polish.

#### No foreign runtime to host

Embedding Rust in Python means linking an interpreter, managing the
GIL, and marshaling across two object models with different lifetimes.
OxDock functions are plain Rust functions returning `Result<Value>`:
the VM is just the calling convention, and values are fixed size words
with deterministic lifetimes, so there is nothing to pin, nothing
reference counted, and nothing kept alive across the boundary.

#### No binding layer

The Python path needs module registration plus type conversions
negotiated with dynamic types. `#[oxdock_func]` derives the
registration marker, the arity gate, and the `String`, `i64`, `f64`,
`bool`, and `Value` extraction from the signature, and doc comments
become introspectable metadata for free.

#### No build system dance

No maturin, no ABI tags, and no wheels built per interpreter. The host
crate depends on `oxdock-core` and calls `Engine::register_module`.
The one thing Python still wins is its C ABI as a stable interop target
for other languages. The OxDock boundary is Rust only, which is exactly
what keeps it cheap.

Use the macros (macros-only build, no CLI):

```sh
cargo add oxdock --no-default-features
```

Or pin the version in `Cargo.toml`:

```toml
[dependencies]
oxdock = { version = "0.17.0-alpha", default-features = false }
```

## Glossary

- **AST**: The parsed tree of a script. Statements, declarations, and IO walk this tree one step at a time.
- **Descriptor**: One type's static singleton: its name, docs, and the vtable every value of that type carries.
- **Fat pointer**: A pointer that carries metadata alongside the address. Words avoid them: every payload type is sized, so payload pointers stay thin.
- **Heap**: Memory for data that outgrows the stack. Container contents and non-inline payloads live here, each owned by exactly one word.
- **Inline**: Payload bytes carried inside the word itself, with zero allocation. Available to `Copy` scalars that fit in 64 bits.
- **NaN boxing**: A technique that packs values into 64 bits by reusing NaN float patterns. Denser than 16 byte words at the cost of pointer masking and constrained host values.
- **Payload**: The 64 bit data half of a word: either inline bytes or a pointer to one owned box.
- **Provenance**: The recorded origin of a pointer, which Rust uses to judge whether a memory access is valid. Round tripping through the same box type preserves it.
- **RPN**: Reverse Polish Notation: arithmetic compiled to a flat stack program instead of tree walking.
- **Vtable**: The operations half of a descriptor: function pointers that clone, drop, compare, and render values of that type.
- **Word**: The fixed 128 bit unit of every script value: a descriptor pointer plus a payload.

## License

`OxDock` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
