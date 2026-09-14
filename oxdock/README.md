# OxDock

**Dockerfile inspired build DSL for Rust**

OxDock is a Dockerfile inspired build DSL for Rust. Embed scripts at compile time with macros, or run the same scripts as standalone CLI pipelines. Native. No containers. No daemon. No VM. All commands run identically on every OS, except RUN.

Supports platform gating, async tasks, and piped workflows for custom pipelines.

[Documentation](https://docs.rs/oxdock/0.13.0-alpha/oxdock/)

Add it to your Rust build with `cargo add oxdock@0.13.0-alpha`, or install the standalone runner with `cargo install oxdock@0.13.0-alpha`.

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
    ASSERT_FILE dist/alpha.txt "alpha OxDock 0.13.0-alpha"
    ASSERT_FILE dist/beta.txt "beta OxDock 0.13.0-alpha"
    ASSERT_FILE dist/picked.txt "alpha"
};

let temp = GuardedPath::tempdir().expect("tempdir");
let root = temp.as_guarded_path().clone();
run_steps_with_context(&root, &root, &steps).expect("run script");

let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
let out = root.join("dist/alpha.txt").expect("out path");
assert_eq!(
    resolver.read_to_string(&out).expect("read out"),
    "alpha OxDock 0.13.0-alpha"
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

## Runtime architecture

Three mechanisms keep script execution predictable: how bytes move between commands, how state stays isolated, and how the host stays sandboxed. Each one is shown running below.

### Transport: pipes

A command's standard streams can be rerouted through named pipes, so producers and consumers connect without touching the terminal or temp files. Buffers stay in memory and spill to a guarded temp file past 8 MiB, and background single command tasks can promote a pipe to a zero copy OS kernel pair instead.

```oxdock
WITH_IO [stdout=pipe:log] ECHO hello
WITH_IO [stdin=pipe:log] READ_LINE $line
WRITE line.txt "{{ $line }}"
ASSERT_FILE line.txt "hello"
```

### Scope isolation

State mutations stay where the script puts them. Entering a braced block or a function call snapshots variables and settings, and exiting restores all of them, so nothing leaks outward. Background tasks fork the same way, so concurrent workers cannot observe each other's half finished mutations. Only pipes and filesystem effects cross these boundaries, by design.

```oxdock
FUNC SHADOW($v: STRING) {
    LET $inner: STRING = "inner"
    RETURN $v
}
LET $out: STRING = CALL SHADOW("param")
WRITE out.txt "{{ $out }}"
ASSERT_FILE out.txt "param"
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
        ASSERT_FILE gen/out.txt generated
    },
    out_dir: "target/prebuilt_prepare",
}

fn main() {}
```

### Stream bytes between steps

`WITH_IO` routes stdout into named script pipes and back into stdin, so steps form custom pipelines. Pipes hold bytes in memory and spill to a temp file above 8 MiB. Wrapping a single RUN in ASYNC promotes the pipe to a zero copy OS kernel pipe instead; the consumer must then run while the producer is alive.

```oxdock
WITH_IO [stdout=pipe:msg] ECHO piped-bytes
WITH_IO [stdin=pipe:msg] WRITE piped.txt
READ piped.txt
ASSERT_STDOUT piped-bytes
```

### Workspaces start ephemeral

Scripts start in an ephemeral snapshot workspace, an isolated temp dir that leaves the source tree untouched. Pull inputs with `COPY` or `COPY_GIT`. Switch to the local directory with `WORKSPACE LOCAL` when the script should mutate in place.

```oxdock
WRITE snap.txt from-snapshot
ASSERT_FILE snap.txt from-snapshot
WORKSPACE LOCAL
WRITE local.txt from-local
ASSERT_FILE local.txt from-local
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

- **Filesystem gating via `oxdock-fs`:** all filesystem operations in the runtime are routed through the crate internal `oxdock-fs` abstraction. That module centralizes path resolution, canonicalization and access checks so reads and writes can be validated against the allowed workspace root and build context.

- **What `oxdock-fs` protects you from:** the guardrails are pragmatic. They prevent common mistakes such as accidentally writing outside the materialized workspace or reading files from arbitrary absolute paths. However, they are not a full sandbox. A determined process or script can still create destructive actions (for example, invoking native `RUN` commands that modify external state). If you require strict isolation, run OxDock inside a container or VM.

- **Performance:** routing via `oxdock-fs` adds negligible overhead for typical workloads. The module focuses on correctness and containment with minimal runtime cost so interactive iteration remains fast.

## Common usage

Install the binary from the registry:

```sh
cargo install oxdock@0.13.0-alpha
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

Use the macros (macros-only build, no CLI):

```sh
cargo add oxdock --no-default-features
```

Or pin the version in `Cargo.toml`:

```toml
[dependencies]
oxdock = { version = "0.13.0-alpha", default-features = false }
```

## License

`OxDock` is distributed under the terms of the Apache License (Version 2.0).
