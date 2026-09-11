**Dockerfile inspired build DSL for Rust**

OxDock is a Dockerfile inspired build DSL for Rust. Embed scripts at compile time with macros, or run the same scripts as standalone CLI pipelines. Native. No containers. No daemon. No VM. All commands run identically on every OS, except RUN.

Supports platform gating, async tasks, and piped workflows for custom pipelines.

[Documentation](https://docs.rs/oxdock/0.12.0-alpha/oxdock/)

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
        ASSERT_FILE dist/os.txt
        ASSERT_FILE dist/toolchain.txt
        ASSERT_FILE dist/manifest.txt "os toolchain"
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
        CALL STAMP($name)
    }
    FUNC PICK($flag: BOOL) {
        IF $flag {
            RETURN "alpha"
        }
        RETURN "beta"
    }
    LET $picked: STRING = CALL PICK(true)
    WRITE dist/picked.txt {{ $picked }}
    ASSERT_FILE dist/alpha.txt "alpha OxDock 0.12.0-alpha"
    ASSERT_FILE dist/beta.txt "beta OxDock 0.12.0-alpha"
    ASSERT_FILE dist/picked.txt "alpha"
};

let temp = GuardedPath::tempdir().expect("tempdir");
let root = temp.as_guarded_path().clone();
run_steps_with_context(&root, &root, &steps).expect("run script");

let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
let out = root.join("dist/alpha.txt").expect("out path");
assert_eq!(
    resolver.read_to_string(&out).expect("read out"),
    "alpha OxDock 0.12.0-alpha"
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
    WITH_IO [stdout=pipe:log] ECHO "built {{ env:PROJECT }}"
    WITH_IO [stdin=pipe:log] READ_LINE $line
    WRITE dist/build.txt "{{ $line }}"
    FOR $f: STRING IN GLOB("dist/*.txt") {
        EXPAND $f
    }
    ASSERT_STDOUT "built OxDock"
    ASSERT_FILE dist/build.txt "built OxDock"
    ASSERT_FILE dist/verbose.log "verbose on"
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

# DSL Reference

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
ASSERT_FILE count.txt "2"
```

The `env:KEY` expression reads the script environment into a plain value. A `$var` reference never reads the environment, even when the names match:

```oxdock
ENV FOO="bar"
LET $e: STRING = env:FOO
WRITE env.txt "{{ $e }}"
ASSERT_FILE env.txt "bar"
```

### Statements and semicolons

```oxdock
// One line, two instructions: the semicolon splits them.
ECHO one; ECHO two
ASSERT_STDOUT one
ASSERT_STDOUT two
```

### RUN shell and exec forms

Shell form (`RUN <command...>`) joins its arguments and runs the string in the system shell. Exec form (`RUN ["exe", "arg", ...]`) spawns the executable directly with no shell. Exec form has no shell expansion, globbing, redirection, or pipes. Quoted `{{ ... }}` templates still interpolate per element, and guards and wrappers (`ASYNC`, `TIMEOUT`, `WITH_IO`) apply to both forms.

```oxdock
RUN ["cargo", "--version"]
ASSERT_STDOUT cargo
```

### Comments

Three comment styles are supported: `//` line comments, nestable `/* ... */` block comments, and `#` comments. A `#` comment is only recognized at the start of a line (optionally indented); inside a command payload a `#` is ordinary text. Similarly, `//` ends a `RUN` argument list but survives inside quoted strings:

```oxdock
// slash comment at end of line
# hash comment occupies the whole line

/* block comments
   /* nest */
   like this */
ECHO visible-after-comments
ASSERT_STDOUT visible-after-comments
```

```oxdock
ECHO hash-mid-line # stays-in-payload
RUN echo run-args-stop-at-slashes // removed-as-comment
ASSERT_STDOUT hash-mid-line # stays-in-payload
ASSERT_STDOUT run-args-stop-at-slashes
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
ASSERT_STDOUT single quotes
ASSERT_STDOUT double quotes
ASSERT_STDOUT escaped " quote
```

## Templates

`{{ env:KEY }}` interpolates script environment values into arguments at execution time. Values come from the script environment (`ENV`, inherited keys) — there is no fallback to host variables in command context, and unknown keys expand to an empty string. The unprefixed form `{{ KEY }}` is not a valid template and also expands to empty, so always use the `env:`-prefixed spelling:

```oxdock
ENV GREETING=hello-world

// env:-prefixed form: interpolates from the SCRIPT environment.
ECHO <{{ env:GREETING }}>

// Bare braces are not a template: they expand to empty.
ECHO <{{ GREETING }}>
ASSERT_STDOUT <hello-world>
ASSERT_STDOUT <>
```

## Guards and scoped blocks

A guard is a bracketed expression that gates the instruction or block that follows it. Inside the brackets:

- `env:KEY` passes when variable `KEY` exists and is non-empty; `eq(env:KEY, value)` and `neq(env:KEY, value)` compare values.
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
[neq(env:DEPLOY_TARGET, staging)] ECHO deploying-elsewhere

ASSERT_STDOUT deploy-target-visible
ASSERT_STDOUT deploying-to-staging
```

### Platform guards

```oxdock
// Exactly one block runs depending on the host OS; every command
// inside a guarded block inherits the block's guard.
[windows] {
  WRITE os-report.txt windows
  ECHO windows-detected
  ASSERT_FILE os-report.txt windows
  ASSERT_STDOUT windows-detected
}
[unix] {
  WRITE os-report.txt unix-family
  ECHO unix-detected
  ASSERT_FILE os-report.txt unix-family
  ASSERT_STDOUT unix-detected
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

ASSERT_STDOUT negation-passes-for-undefined
ASSERT_STDOUT or-matched-a-branch
ASSERT_STDOUT composed-and-or-guard
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
ASSERT_ABSENT chained.txt
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
ASSERT_FILE inner.txt "inner_value-staging"
WRITE outer.txt "{{ $a }}-{{ env:MODE }}"
ASSERT_FILE outer.txt "some_value-production"
```

`IF`/`ELSE` branches, `FOR` loop bodies, `TIMEOUT` bodies, `ASYNC` bodies, and `WITH_IO [..] { ... }` blocks are all scopes under the same rule: only files and pipes leak out.

### EXIT in nested blocks

`EXIT <code>` stops the pipeline immediately with an `EXIT requested with code <code>` error — steps after it never run, at any nesting depth. Unwinding still happens on the way out: every enclosing block reverts its `LET`/`ENV`/`WORKDIR`/`WORKSPACE` state before the error propagates, anonymous background tasks are killed synchronously, and files written before the `EXIT` persist. An `EXIT` inside `TIMEOUT` passes through unwrapped (never relabeled as a deadline error); an `EXIT` inside an `ASYNC` task ends that task with an error, which the parent sees at `AWAIT` or end-of-pipeline reaping.

```oxdock expect_error:"EXIT requested with code 3"
WRITE before.txt "persisted"
ASSERT_FILE before.txt "persisted"
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
ASSERT_FILE heartbeat.txt alive

// Block form bounds multiple steps.
TIMEOUT 30s {
    WRITE a.txt one
    WRITE b.txt two
}
ASSERT_FILE a.txt one
ASSERT_FILE b.txt two

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

The one structural exception is `WITH_IO`, which must wrap `ASYNC` from the outside (`WITH_IO [stdout=pipe:p] ASYNC ...`) so pipe endpoints are allocated synchronously on the main thread before the worker spawns. Placing `WITH_IO` directly inside `ASYNC` is rejected at parse time.

## Cancelling tasks with CANCEL

`CANCEL $task` synchronously stops a named background task spawned via `LET $task: HANDLE = ASYNC ...`. It is blocking: when the statement returns, the task thread has been joined and its OS process reaped, so no residual filesystem or stream mutation can follow and the next step runs in a quiet workspace. Only named tasks can be cancelled; a later `AWAIT $task` fails with a cancellation error, and a second `CANCEL $task` fails as already cancelled.

```oxdock
// CANCEL form stops a named background task synchronously.
LET $worker: HANDLE = ASYNC SLEEP 30s
CANCEL $worker
```

<!-- GENERATED by docs-gen from oxdock-parser metadata. Do not edit by hand. -->
## Command Reference

| Command | Syntax |
| --- | --- |
| [`WORKDIR`](#workdir) | `WORKDIR <path>` |
| [`WORKSPACE`](#workspace) | `WORKSPACE SNAPSHOT\|LOCAL` |
| [`ENV`](#env) | `ENV KEY=value` |
| [`INHERIT_ENV`](#inherit_env) | `INHERIT_ENV <key>...` |
| [`ECHO`](#echo) | `ECHO <message>` |
| [`RUN`](#run) | `RUN <command...> \| RUN ["exe", "arg", ...]` |
| [`COPY`](#copy) | `COPY [--from-current-workspace] <from> <to>` |
| [`COPY_GIT`](#copy_git) | `COPY_GIT [--include-dirty] <rev> <src> <dst>` |
| [`SYMLINK`](#symlink) | `SYMLINK <from> <to>` |
| [`MKDIR`](#mkdir) | `MKDIR <path>` |
| [`LS`](#ls) | `LS [<path>]` |
| [`CWD`](#cwd) | `CWD` |
| [`READ`](#read) | `READ [<path>]` |
| [`READ_LINE`](#read_line) | `READ_LINE $var` |
| [`WRITE`](#write) | `WRITE <path> [<contents>]` |
| [`APPEND`](#append) | `APPEND <path> [<contents>]` |
| [`EXPAND`](#expand) | `EXPAND [<path>] [<KEY=val> ...]` |
| [`ASSERT_FILE`](#assert_file) | `ASSERT_FILE [--hash <sha256>] <path> [<expected>]` |
| [`ASSERT_DIR`](#assert_dir) | `ASSERT_DIR <path>` |
| [`ASSERT_ABSENT`](#assert_absent) | `ASSERT_ABSENT <path>` |
| [`ASSERT_STDOUT`](#assert_stdout) | `ASSERT_STDOUT <substring>` |
| [`HASH_SHA256`](#hash_sha256) | `HASH_SHA256 <path>` |
| [`EXIT`](#exit) | `EXIT <code>` |
| [`SLEEP`](#sleep) | `SLEEP <duration>` |
| [`WITH_IO`](#with_io) | `WITH_IO [<stream>[=pipe:<name>\|=$var], ...] <command> \| WITH_IO [bindings] { <commands> }` |
| [`FOR`](#for) | `FOR $item: TYPE IN <expr> { <commands> } \| FOR $key: STRING, $value: TYPE IN <expr> { <commands> }` |
| [`IF`](#if) | `IF <expr> { <commands> } [ELSE IF <expr> { <commands> }] [ELSE { <commands> }]` |
| [`LET`](#let) | `LET $var: TYPE = <expr> \| LET $var: TYPE = ASYNC { <commands> } \| LET $var: TYPE = <command> \| LET $var: TYPE = AWAIT $task` |
| [`MUTATION`](#mutation) | `$var = <expr>` |
| [`ASYNC`](#async) | `ASYNC <command...> \| ASYNC { <commands> } \| LET $var: HANDLE = ASYNC { <commands> }` |
| [`AWAIT`](#await) | `AWAIT $var \| LET $out: STRING = AWAIT $var` |
| [`CANCEL`](#cancel) | `CANCEL $var` |
| [`TIMEOUT`](#timeout) | `TIMEOUT <duration> <command...> \| TIMEOUT <duration> { <commands> } \| TIMEOUT <duration> AWAIT $var` |
| [`FUNC`](#func) | `FUNC NAME($param: TYPE, ...) { <commands> }` |
| [`CALL`](#call) | `CALL NAME(<expr>, ...) \| LET $var: TYPE = CALL NAME(<expr>, ...)` |
| [`RETURN`](#return) | `RETURN <expr>` |
| [`WHILE`](#while) | `WHILE <bool-expr> { <commands> }` |
| [`BREAK`](#break) | `BREAK` |
| [`CONTINUE`](#continue) | `CONTINUE` |

### WITH_IO

Reroute standard streams.

**Syntax:** `WITH_IO [<stream>[=pipe:<name>|=$var], ...] <command> | WITH_IO [bindings] { <commands> }`

Reroutes the standard streams of the next command or, in block form,
of every enclosed command.

Bindings map streams (`stdin`, `stdout`, `stderr`) to named script
pipes (`stdout=pipe:name`, `stderr=pipe:name`) or to a PIPE-typed
variable (`stdin=$p`, resolved against the live pipe registry when
the step runs). Both stdout and stderr pipes capture output the same way.

Pipes hold bytes in memory and spill to a temp file above 8 MiB, so a
producer can finish before the consumer starts.

If WITH_IO wraps an ASYNC block whose body is a single RUN, guarded or
not, the pipe is a zero copy OS kernel pipe instead: pair it with a
consumer that runs while the producer is alive, since output past the
64 KiB kernel buffer stalls until drained. That promotion never crosses
a CALL boundary: pipes created, bound, or passed by variable inside FUNC
bodies are always script pipes, even when the surrounding task would
otherwise promote.

A second producer or consumer on a live name is an explicit error. A name
bound as output can later feed another command's `stdin`, connecting
commands without touching the terminal. Binding `stdout` and `stderr` to
the same live pipe name fails deterministically. Merge streams in shell
via `2>&1` instead.

Nested blocks stack defaults; inline bindings override inherited ones for
their command only; closing a block restores previous wiring.


**Examples:**

**Example: with_io block**

```oxdock
WITH_IO [stdout=pipe:log] {
  ECHO first
  ECHO second
}
WITH_IO [stdin=pipe:log] WRITE captured.txt
```

**Example: variable pipe binding**

```oxdock
# Declare the pipe first with the explicit handle operator
# (like `env:KEY`): `pipe:log` names a pipe without touching
# a stream. A plain string here would be a TypeMismatch.
# `$p` (not `pipe:$p`) is the variable form; literals stay
# `pipe:name`.
LET $p: PIPE = pipe:log
WITH_IO [stdout=$p] ECHO hello
WITH_IO [stdin=$p] READ_LINE $line
WRITE line.txt "{{ $line }}"
ASSERT_FILE line.txt "hello"
```


### FOR

Iterate over a list or map.

**Syntax:** `FOR $item: TYPE IN <expr> { <commands> } | FOR $key: STRING, $value: TYPE IN <expr> { <commands> }`

The loop variable receives each element (lists) or value (maps); with
two variables, the first receives the key.

Loop variables are declared with explicit types and scoped per iteration
via declare_var; they do not leak outward. The body may be a braced block
or a single-line `{ ... }` command.

`GLOB("...")` patterns must be quoted (`*` is not a bare word, so
`GLOB(*)` is a parse error); GLOB returns a root-relative sorted list,
empty when nothing matches, and rejects `..` escapes.


**Examples:**

**Example: for loop**

```oxdock
LET $items: LIST = ["a", "b"]
FOR $item: STRING IN $items {
  ECHO $item
}

LET $map: MAP = {"x": 1}
FOR $k: STRING, $v: INT IN $map {
  ECHO "$k=$v"
}
```

**Example: expand every match**

```oxdock
# single-line body; $x is a template path, WHO an override
WRITE a.txt "hi \{{ env:WHO }}!"
FOR $x: STRING IN GLOB("*.txt") { EXPAND $x WHO=World }
ASSERT_STDOUT "hi World!"
```


### IF

Conditional execution.

**Syntax:** `IF <expr> { <commands> } [ELSE IF <expr> { <commands> }] [ELSE { <commands> }]`

The condition is evaluated as a boolean expression.

Prefix `!` negates (`IF !false`); only Bool values are accepted as
conditions.


**Examples:**

**Example: if else**

```oxdock
IF true {
  ECHO yes
} ELSE {
  ECHO no
}

IF false {
  ECHO skipped
} ELSE IF true {
  ECHO fallback
}

IF !false {
  ECHO inverted
}
```


### LET

Bind script-local variables.

**Syntax:** `LET $var: TYPE = <expr> | LET $var: TYPE = ASYNC { <commands> } | LET $var: TYPE = <command> | LET $var: TYPE = AWAIT $task`

Declares a script-local variable with an explicit type (STRING, INT,
FLOAT, BOOL, PIPE, LIST, MAP, HANDLE, DURATION, PATH). Duplicate LET
in the same scope frame is a redeclaration error; mutate with
`$var = <expr>`.

Variables are usable in templates (`{{ $var }}`), guards, and
expressions. With `ASYNC`, spawns a background task and stores its
handle (see ASYNC). The `$` sigil on the name is mandatory.

The right-hand side is always an expression — literals, lists, maps,
comparisons, `env:KEY` reads, `pipe:NAME` handles, `INSPECT($var)`
snapshots, `GLOB("*.md")` — never a `{{ ... }}` template;
interpolation happens in string values, not here.

Bare words need no quotes: `LET $d: STRING = 30s` binds the same string
as quoted.

When the right-hand side is a synchronous command
(`LET $out: STRING = ECHO hi`), the command runs to completion and its
exact stdout bytes are captured into the variable as a string (no newline
stripping; commands with no stdout capture as `""`; non-UTF8 stdout is
an error). Combining capture with an explicit
`WITH_IO [stdout=pipe:...]` is a parse error.

`LET $out: STRING = AWAIT $var` captures a background task's stdout the
same way; bare `AWAIT $var` forwards it to the parent stdout instead.

`LET $e: STRING = env:FOO` reads the script environment into a plain
string.


**Examples:**

**Example: let**

```oxdock
LET $name: STRING = "world"
ECHO "hello, {{ $name }}"

LET $items: LIST = ["a", "b"]
LET $count: INT = 42
```

**Example: glob binding**

```oxdock
# the RHS is an expression: GLOB(...) runs and binds a list
WRITE a.txt "x"
LET $files: LIST = GLOB("*.txt")
FOR $f: STRING IN $files { ECHO $f }
ASSERT_STDOUT "a.txt"
```

**Example: scoped variable reverts**

```oxdock
# LET inside a braced block reverts when the block exits
LET $a: STRING = "outer"
[bool:true] {
    LET $a: STRING = "inner"
    WRITE inner.txt "{{ $a }}"
}
WRITE outer.txt "{{ $a }}"
ASSERT_FILE inner.txt "inner"
ASSERT_FILE outer.txt "outer"
```

**Example: capture command output**

```oxdock
LET $out: STRING = ECHO hi
WRITE captured.txt "{{ $out }}"
ASSERT_FILE captured.txt "hi\n"
```

**Example: inspect a variable**

```oxdock
# INSPECT($var) snapshots a variable into a MAP: declared
# type plus live details (pipe backend stats here), so
# scripts can branch on engine state.
LET $p: PIPE = pipe:log
WITH_IO [stdout=$p] ECHO hello
LET $info: MAP = INSPECT($p)
IF $info.is_os_pipe {
    WRITE unexpected.txt "should be a script pipe"
}
WRITE kind.txt "{{ $info.type }}"
ASSERT_FILE kind.txt "PIPE"
```


### MUTATION

Mutate a declared variable.

**Syntax:** `$var = <expr>`

Reassigns an existing variable, validating the new value against the
TypeKind bound at LET time via coerce_value with ExecState context.

The leading `$` distinguishes mutation from `KEY=value` command
assignments. Assigning an undeclared variable or a mismatched type is
an error.


**Examples:**

**Example: mutate**

```oxdock
LET $count: INT = 1
$count = 2
```


### ASYNC

Run steps in a background thread.

**Syntax:** `ASYNC <command...> | ASYNC { <commands> } | LET $var: HANDLE = ASYNC { <commands> }`

Runs a command or block of commands in a background thread with
subshell isolation.

Mutations (ENV, WORKDIR) stay within the block. With `LET`, stores a
task handle for `AWAIT`.


**Examples:**

**Example: async**

```oxdock
ASYNC ECHO "first"

ASYNC {
    ECHO "first"
    ECHO "second"
}
```

**Example: async task handle**

```oxdock
LET $task: HANDLE = ASYNC {
    ECHO "built"
}
AWAIT $task
```


### AWAIT

Join a background task.

**Syntax:** `AWAIT $var | LET $out: STRING = AWAIT $var`

Blocks until the named task completes. Propagates errors if the task failed.

Bare `AWAIT $var` forwards the task's stdout to the parent stdout;
`LET $out: STRING = AWAIT $var` captures it into `$out` instead (same
UTF-8 and spilling rules as `LET $var: STRING = <command>`).


**Examples:**

**Example: await**

```oxdock
LET $task: HANDLE = ASYNC ECHO "done"
AWAIT $task
```

**Example: await capture**

```oxdock
LET $task: HANDLE = ASYNC ECHO "done"
LET $out: STRING = AWAIT $task
WRITE captured.txt "{{ $out }}"
ASSERT_FILE captured.txt "done\n"
```


### CANCEL

Synchronously cancel a background task.

**Syntax:** `CANCEL $var`

Kills the named background task spawned via LET $var: HANDLE = ASYNC ....

Blocking: returns only after the task thread has been joined and its OS
process reaped, so no residual filesystem or stream mutation follows. A
later AWAIT $var reports cancellation. Only named tasks can be cancelled.


**Examples:**

**Example: cancel**

```oxdock
LET $task: HANDLE = ASYNC SLEEP 30s
CANCEL $task
```


### TIMEOUT

Enforce an execution deadline.

**Syntax:** `TIMEOUT <duration> <command...> | TIMEOUT <duration> { <commands> } | TIMEOUT <duration> AWAIT $var`

Aborts the wrapped step or block with a deadline error if it exceeds the
duration (e.g. 500ms, 10s, 2m; a bare number means seconds).

A blocking foreground process is killed.


**Examples:**

**Example: timeout**

```oxdock
TIMEOUT 30s WRITE heartbeat.txt alive
```

**Example: timeout block**

```oxdock
TIMEOUT 30s {
    WRITE a.txt one
    WRITE b.txt two
}
```

**Example: timeout variable duration**

```oxdock
# durations resolve at runtime, so variables work too
LET $budget: DURATION = "30s"
TIMEOUT $budget WRITE heartbeat.txt alive
ASSERT_FILE heartbeat.txt alive
```


### FUNC

Define a user function.

**Syntax:** `FUNC NAME($param: TYPE, ...) { <commands> }`

Defines a user function with UPPERCASE name and explicitly typed
parameters.

Params bind by position with declare_var coercion before the body runs.
Bodies run in a fresh variable scope; LETs inside do not leak. A nested
FUNC definition is scoped to its block and reverts on exit. Names share
one namespace with host-registered functions.


**Examples:**

**Example: func def call**

```oxdock
FUNC GREET($name: STRING) {
  RETURN $name
}
LET $res: STRING = CALL GREET("ada")
WRITE greeting.txt "{{ $res }}"
ASSERT_FILE greeting.txt "ada"
```


### CALL

Invoke a user or host function.

**Syntax:** `CALL NAME(<expr>, ...) | LET $var: TYPE = CALL NAME(<expr>, ...)`

Invokes a FUNC-defined or host-registered function by UPPERCASE name.

Bare CALL discards the return value and keeps stdout side effects.
LET $var: TYPE = CALL captures the RETURN value (fallthrough without
RETURN captures as ""), coerced to the declared type; stdout inside the
callee stays observable via ASSERT_STDOUT and pipes.

Combining LET-capture with WITH_IO [stdout=pipe:...] is a parse error.


**Examples:**

**Example: call**

```oxdock
FUNC SHOUT($name: STRING) {
  ECHO "{{ $name }}"
  RETURN $name
}
CALL SHOUT("ada")
ASSERT_STDOUT "ada"
```

**Example: call with pipes**

```oxdock
# A pipe handle travels into a function as a typed argument
# and is usable as a binding target in both directions.
# `pipe:ch` constructs the handle; `$p` passes it on.
FUNC DRAIN($q: PIPE) {
  WITH_IO [stdin=$q] READ_LINE $line
  RETURN $line
}
LET $p: PIPE = pipe:ch
WITH_IO [stdout=$p] ECHO "payload"
LET $got: STRING = CALL DRAIN($p)
WRITE got.txt "{{ $got }}"
ASSERT_FILE got.txt "payload"
```


### RETURN

Return a value from a function.

**Syntax:** `RETURN <expr>`

Ends the nearest enclosing function call with a value.

Falling off the end without RETURN yields "". RETURN outside a function
(including at top level or across an ASYNC boundary) is an error.


**Examples:**

**Example: return**

```oxdock
FUNC PICK($flag: BOOL) {
  IF $flag {
    RETURN "yes"
  }
  RETURN "no"
}
LET $res: STRING = CALL PICK(true)
WRITE picked.txt "{{ $res }}"
ASSERT_FILE picked.txt "yes"
```


### WHILE

Loop while a condition holds.

**Syntax:** `WHILE <bool-expr> { <commands> }`

Re-evaluates a Bool condition each iteration (same is_truthy rule as IF;
non-Bool is a type error).

Each iteration runs in a fresh scope; mutate outer state with $var = ...
so the next check observes it. BREAK exits the loop; CONTINUE skips to
the next check.


**Examples:**

**Example: while loop**

```oxdock
LET $done: BOOL = false
WHILE !$done {
  WRITE tick.txt "once"
  $done = true
}
ASSERT_FILE tick.txt "once"
```


### BREAK

Exit the innermost loop.

**Syntax:** `BREAK`

Exits the innermost enclosing FOR or WHILE loop.

BREAK outside a loop, or across a FUNC or ASYNC boundary, is an error.


**Examples:**

**Example: break**

```oxdock
FOR $x: STRING IN ["a", "b"] {
  BREAK
}
```


### CONTINUE

Skip to the next loop iteration.

**Syntax:** `CONTINUE`

Skips the rest of the innermost enclosing FOR or WHILE body and starts
the next iteration.

CONTINUE outside a loop, or across a FUNC or ASYNC boundary, is an error.


**Examples:**

**Example: continue**

```oxdock
FOR $x: STRING IN ["a", "b"] {
  CONTINUE
}
```


### WORKDIR

Change the working directory.

**Syntax:** `WORKDIR <path>`

Sets the current working directory.

Relative paths resolve against the current directory; `/` resets to
the workspace root. Paths cannot escape the workspace.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | yes | Directory to change to |

**Examples:**

**Example: change working directory**

```oxdock
WORKDIR project/src
WRITE generated.txt generated-under-workdir
ASSERT_FILE generated.txt generated-under-workdir
```


### WORKSPACE

Switch workspace roots.

**Syntax:** `WORKSPACE SNAPSHOT|LOCAL`

SNAPSHOT or LOCAL root.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `target` | `SNAPSHOT\|LOCAL` | yes | Target root |

**Examples:**

**Example: switch roots**

```oxdock
WORKSPACE LOCAL
```


### ENV

Set an environment variable.

**Syntax:** `ENV KEY=value`

Inserts or updates an env var.

The value uses the unified string-value rules shared by every command:
`"..."` or `'...'` quotes keep exact bytes (spaces, tabs), a lone `$var`
evaluates that variable, `{{ ... }}` placeholders interpolate, unquoted
words join with single spaces, and the first `=` splits key from value
(`KEY=a=b` stores `a=b`).

A `$var` inside larger text stays literal — write `{{ $var }}` to
interpolate there.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `assignment` | `STRING` | yes | KEY=value pair; the value resolves as STRING |

**Examples:**

**Example: set env**

```oxdock
ENV APP_MODE=production
```

**Example: quoted value with spaces**

```oxdock
# quotes keep the space: SET_FORTH stores `outer scope`
ENV SET_FORTH="outer scope"
WRITE out.txt "{{ env:SET_FORTH }}"
ASSERT_FILE out.txt "outer scope"
```

**Example: variable value**

```oxdock
# a lone $var evaluates, like ECHO $var
LET $who: STRING = "Alice"
ENV GREETING=$who
WRITE out.txt "{{ env:GREETING }}"
ASSERT_FILE out.txt "Alice"
```

**Example: all value forms agree**

```oxdock
# a bare variable, a quoted literal, and a template all
# store plain strings through the same value rules
LET $x: STRING = "Ada"
ENV A=$x
ENV B="hello world"
ENV C="{{ $x }} concatenated"
WRITE check.txt "{{ env:A }}|{{ env:B }}|{{ env:C }}"
ASSERT_FILE check.txt "Ada|hello world|Ada concatenated"
```

**Example: scoped env reverts**

```oxdock
# ENV inside a braced block reverts when the block exits
ENV MODE=production
[bool:true] {
    ENV MODE=staging
    WRITE inner.txt "{{ env:MODE }}"
}
WRITE outer.txt "{{ env:MODE }}"
ASSERT_FILE inner.txt "staging"
ASSERT_FILE outer.txt "production"
```


### INHERIT_ENV

Inherit env vars from host.

**Syntax:** `INHERIT_ENV <key>...`

Declares which host environment variables to inherit into the script.

Must appear before any other commands and at most once. Without this
directive, the script starts with an empty environment.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `keys` | [`STRING...`](#value-type-string) | no | Host variables to inherit |

**Examples:**

**Example: inherit env**

```oxdock
INHERIT_ENV [PATH, HOME]
```


### ECHO

Print to stdout.

**Syntax:** `ECHO <message>`

Outputs message to stdout.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `message` | [`STRING...`](#value-type-string) | yes | Text |

**Output:** Stdout

**Examples:**

**Example: echo**

```oxdock
ECHO build-complete
```

**Example: variables**

```oxdock
# a lone $x evaluates; {{ }} interpolates inside text
LET $x: STRING = "World"
ECHO {{ $x }}
ECHO $x
ASSERT_STDOUT "World"
```


### RUN

Execute shell command or direct executable.

**Syntax:** `RUN <command...> | RUN ["exe", "arg", ...]`

Shell form (`RUN <command...>`) runs the joined command string in the
system shell (`$SHELL -c` / `COMSPEC /C`).

Exec form (`RUN ["exe", "arg", ...]`) spawns the executable directly
with no shell, so there is no shell expansion, globbing, redirection,
or pipes; use it for portable commands.

Guards and wrappers (`ASYNC`, `TIMEOUT`, `WITH_IO`, `LET`) apply to
both forms.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `command` | [`STRING...`](#value-type-string) | yes | Command |

**Examples:**

**Example: run**

```oxdock
RUN echo hello
```

**Example: run exec form**

```oxdock
RUN ["cargo", "--version"]
```


### COPY

Copy file into workspace.

**Syntax:** `COPY [--from-current-workspace] <from> <to>`

Copies from host.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `from` | [`PATH`](#value-type-path) | yes | Source |
| `to` | [`PATH`](#value-type-path) | yes | Dest |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--from-current-workspace` | `BOOL` | Copy from workspace instead of build context |

**Examples:**

**Example: copy**

```oxdock roots:unified
WRITE src.txt content
COPY src.txt dst.txt
ASSERT_FILE dst.txt content
```

**Example: copy from workspace**

```oxdock roots:unified
WRITE ws-src.txt ws-content
COPY --from-current-workspace ws-src.txt ws-copy.txt
ASSERT_FILE ws-copy.txt ws-content
```


### COPY_GIT

Copy from git revision.

**Syntax:** `COPY_GIT [--include-dirty] <rev> <src> <dst>`

Checkout and copy.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `rev` | [`STRING`](#value-type-string) | yes | Rev |
| `src` | [`PATH`](#value-type-path) | yes | Src |
| `dst` | [`PATH`](#value-type-path) | yes | Dst |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--include-dirty` | `BOOL` | Include dirty |

**Examples:**

**Example: git copy**

```oxdock expect_error:"COPY source missing"
COPY_GIT HEAD src.txt dst.txt
```


### SYMLINK

Create symlink.

**Syntax:** `SYMLINK <from> <to>`

Creates symlink.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `from` | [`PATH`](#value-type-path) | yes | Target |
| `to` | [`PATH`](#value-type-path) | yes | Link |

**Examples:**

**Example: symlink**

```oxdock roots:unified
WRITE original.txt content
SYMLINK original.txt link.txt
ASSERT_FILE link.txt content
```


### MKDIR

Create directory.

**Syntax:** `MKDIR <path>`

Creates dir with parents.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | yes | Dir path |

**Examples:**

**Example: mkdir**

```oxdock
MKDIR deeply/nested/tree
```


### LS

List directory.

**Syntax:** `LS [<path>]`

Lists entries.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | no | Dir |

**Output:** Stdout

**Examples:**

**Example: ls**

```oxdock
MKDIR inventory
WRITE inventory/a.txt a
LS inventory
```


### CWD

Print working directory.

**Syntax:** `CWD`

Outputs cwd.

**Output:** Stdout

**Examples:**

**Example: cwd**

```oxdock
CWD
```


### READ

Read file to stdout.

**Syntax:** `READ [<path>]`

Outputs file contents.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | no | File |

**Output:** Stdout

**Examples:**

**Example: read**

```oxdock
WRITE note.txt "hello"
READ note.txt
```


### READ_LINE

Read one line from stdin into a variable.

**Syntax:** `READ_LINE $var`

Reads bytes until newline without waiting for EOF, leaving the pipe open.

Trailing newline is stripped (shell-read parity). On premature EOF
assigns accumulated bytes and returns.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `var` | `STRING` | yes | Target variable (`$name`); the line binds as STRING |

**Examples:**

**Example: read line**

```oxdock
WITH_IO [stdout=pipe:lines] ECHO "first"
WITH_IO [stdin=pipe:lines] READ_LINE $reply
```


### WRITE

Write to file.

**Syntax:** `WRITE <path> [<contents>]`

Writes contents.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | yes | File |
| `contents` | [`STRING...`](#value-type-string) | no | Content |

**Examples:**

**Example: write**

```oxdock
WRITE output.txt hello-world
```


### APPEND

Append to file.

**Syntax:** `APPEND <path> [<contents>]`

Appends contents.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | yes | File |
| `contents` | [`STRING...`](#value-type-string) | no | Content |

**Examples:**

**Example: append**

```oxdock
WRITE log.txt line1
APPEND log.txt line2
ASSERT_FILE log.txt line1line2
```


### EXPAND

Expand a template file (or stdin) to stdout.

**Syntax:** `EXPAND [<path>] [<KEY=val> ...]`

A template is any text file — or piped stdin when no path is given —
containing `{{ ... }}` placeholders. EXPAND replaces each placeholder
and prints the result to stdout.

Placeholders: `{{ NAME }}` reads a `KEY=val` override passed on this
command; `{{ env:NAME }}` reads an override, falling back to the
environment; `{{ $var }}` reads a script variable (dotted paths allowed).
A missing key is an error, never a silent empty.

Substitution runs in a single pass. EXPAND is not recursive and does not
expand nested placeholders: a value that itself contains `{{ ... }}` is
inserted verbatim and never expanded again.

A bare `$var` argument is a template path; `KEY=val` arguments are
overrides whose values follow the unified string-value rules (same as
`ENV`: quotes keep exact bytes, a lone `$var` evaluates,
`{{ ... }}` interpolates).

NOTE: `WRITE` interpolates `{{ ... }}` while writing, so escape it
(`\{{ ... }}`) when writing a template file for a later `EXPAND`.

With no path, the template arrives on stdin through a pipe. When piping
from a shell, single-quote the template (`echo '{{ $x }}'`): double
quotes let the shell swallow `$x`, so oxdock receives an empty `{{ }}`
placeholder and errors.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | no | Template file to expand; omit to expand stdin |
| `overrides` | `STRING...` | no | Template overrides shadowing that key (unified string values) |

**Output:** Stdout

**Examples:**

**Example: expand**

```oxdock
ENV NAME="Alice"
WRITE template.md "Hello {{ env:NAME }}!"
EXPAND template.md
ASSERT_STDOUT "Hello Alice!"
```

**Example: override with spaces**

```oxdock
# WRITE would interpolate {{ }} right away, so escape it:
# the file must literally contain {{ env:NAME }} for EXPAND
WRITE template.md "Hello \{{ env:NAME }}!"
EXPAND template.md NAME="Alice Smith"
ASSERT_STDOUT "Hello Alice Smith!"
```

**Example: variable override**

```oxdock
# same escaping: keep the placeholder literal until EXPAND;
# a lone $who evaluates, like ECHO $who
LET $who: STRING = "Bob"
WRITE template.md "Hi \{{ env:WHO }}!"
EXPAND template.md WHO=$who
ASSERT_STDOUT "Hi Bob!"
```

**Example: override forms agree**

```oxdock
# a bare variable and a template-with-tail expand identically
LET $x: STRING = "Ada"
WRITE template.md "Hi \{{ env:NAME }} and \{{ env:NAME2 }}!"
EXPAND template.md NAME=$x NAME2="{{ $x }} concatenated"
ASSERT_STDOUT "Hi Ada and Ada concatenated!"
```

**Example: expand stdin**

```oxdock
# no path: the template arrives on stdin through a pipe
WITH_IO [stdout=pipe:tpl] ECHO "Hello \{{ env:NAME }}!"
WITH_IO [stdin=pipe:tpl] EXPAND NAME=Alice
ASSERT_STDOUT "Hello Alice!"
```

**Example: override does not leak**

```oxdock
# KEY=val overrides shadow env for that EXPAND only —
# they never update the environment itself
ENV NAME="Alice"
WRITE template.md "Hi \{{ env:NAME }}!"
EXPAND template.md NAME="Bob"
ASSERT_STDOUT "Hi Bob!"
EXPAND template.md
ASSERT_STDOUT "Hi Alice!"
```


### ASSERT_FILE

Assert file exists.

**Syntax:** `ASSERT_FILE [--hash <sha256>] <path> [<expected>]`

Checks the path is a file, then optionally compares its bytes (or
`--hash` SHA-256 digest) against the expectation.

Any mismatch aborts the pipeline with a step-numbered error showing
expected vs actual.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | yes | File |
| `expected` | [`STRING...`](#value-type-string) | no | Expected |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--hash` | `STRING` | SHA-256 |

**Examples:**

**Example: assert file**

```oxdock
WRITE payload.bin stable-content
ASSERT_FILE payload.bin stable-content
```

**Example: assert file hash**

```oxdock
# --hash compares the SHA-256 digest instead of raw bytes
WRITE payload.bin stable-content
ASSERT_FILE --hash 08135c1b6349b0e4f894c36221952f0de00e6b4d82f80895abf359755e77103c payload.bin
```


### ASSERT_DIR

Assert dir exists.

**Syntax:** `ASSERT_DIR <path>`

Checks the path is a directory, aborting the pipeline with a
step-numbered error otherwise.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | yes | Dir |

**Examples:**

**Example: assert dir**

```oxdock
MKDIR dist/assets
ASSERT_DIR dist/assets
```


### ASSERT_ABSENT

Assert path absent.

**Syntax:** `ASSERT_ABSENT <path>`

Checks nothing exists at the path, aborting the pipeline with a
step-numbered error if it does.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | yes | Path |

**Examples:**

**Example: assert absent**

```oxdock
ASSERT_ABSENT missing.txt
```


### ASSERT_STDOUT

Assert stdout contains.

**Syntax:** `ASSERT_STDOUT <substring>`

Checks the preceding step's stdout contains the substring, aborting the
pipeline with a step-numbered error otherwise.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `substring` | [`STRING...`](#value-type-string) | yes | Substring |

**Examples:**

**Example: assert stdout**

```oxdock
ECHO build-complete
ASSERT_STDOUT build-complete
```


### HASH_SHA256

Print SHA-256.

**Syntax:** `HASH_SHA256 <path>`

Computes digest.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | yes | File |

**Output:** Stdout

**Examples:**

**Example: hash**

```oxdock
WRITE payload.txt hello
HASH_SHA256 payload.txt
```


### EXIT

Exit pipeline.

**Syntax:** `EXIT <code>`

Stops the pipeline immediately with an `EXIT requested with code <code>`
error; steps after it never run, at any nesting depth.

Enclosing blocks still unwind their LET/ENV/WORKDIR/WORKSPACE state,
anonymous background tasks are killed synchronously, and files written
before the EXIT persist.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `code` | [`INT`](#value-type-int) | yes | Code |

**Examples:**

**Example: exit**

```oxdock expect_error:"EXIT requested with code 0"
EXIT 0
```


### SLEEP

Pause execution for a duration.

**Syntax:** `SLEEP <duration>`

Parks the step for the duration (e.g. 500ms, 10s, 2m).

Cooperative: checks for cancellation so an enclosing TIMEOUT or task
teardown interrupts the sleep. Cross-platform alternative to shell sleep
for testing time boundaries.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `duration` | [`DURATION`](#value-type-duration) | yes | How long to sleep |

**Examples:**

**Example: sleep**

```oxdock
SLEEP 100ms
```

**Example: sleep variable duration**

```oxdock
# durations resolve at runtime, so variables work too —
# quoted or bare, both bind the same string
LET $pause: STRING = "100ms"
SLEEP $pause
LET $bare: STRING = 100ms
SLEEP $bare
```


## Value types

### Value type: STRING

Arbitrary text. Quotes keep exact bytes, lone `$var` evaluates, `{{ ... }}` interpolates.

### Value type: INT

64-bit signed integer, e.g. an exit code.

### Value type: FLOAT

64-bit float, e.g. a ratio.

### Value type: BOOL

Boolean `true` or `false`.

### Value type: PIPE

Named script pipe. Validity is checked against the pipe registry at coercion time.

### Value type: LIST

Ordered list of values.

### Value type: MAP

String-keyed map of values.

### Value type: HANDLE

Background ASYNC task handle for AWAIT/CANCEL.

### Value type: DURATION

Positive time span: `500ms`, `10s`, `2m`, `1h`; bare number means seconds.

### Value type: PATH

Workspace path, resolved against cwd and guarded against escape.
