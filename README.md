<div align="center">
  <img src="assets/OxDock-logo.svg" alt="OxDock logo" width="360"/>
</div>

<div align="center">
  <a href="https://www.rust-lang.org/">
    <img src="https://img.shields.io/badge/Made%20with-Rust-black?&logo=Rust" alt="Made with Rust" />
  </a>
  <a href="https://github.com/jzombie/rust-oxdock/blob/main/LICENSE">
    <img src="https://img.shields.io/badge/License-Apache%202.0-blue.svg" alt="Apache 2.0" />
  </a>
  <!-- <a href="https://docs.rs/oxdock">
    <img src="https://img.shields.io/docsrs/oxdock" alt="docs.rs" />
  </a> -->
  <a href="https://github.com/jzombie/rust-oxdock/actions/workflows/rust-tests.yml?query=branch%3Amain+event%3Apush">
    <img src="https://img.shields.io/github/actions/workflow/status/jzombie/rust-oxdock/rust-tests.yml?branch=main&label=Miri&logo=github" alt="Miri status" />
  </a>
  <!-- <a href="https://deepwiki.com/jzombie/rust-oxdock">
    <img src="https://deepwiki.com/badge.svg" alt="DeepWiki" />
    </a> -->
  <a href="https://coveralls.io/github/jzombie/rust-oxdock?branch=main">
    <img src="https://coveralls.io/repos/github/jzombie/rust-oxdock/badge.svg?branch=main" alt="Coverage Status" />
  </a>
  <a href="#miri-coverage">
    <img src="https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fjzombie%2Frust-oxdock%2Fbadges%2Fmiri-coverage.json" alt="Miri Coverage" />
  </a>
</div>

**Dockerfile inspired build DSL for Rust**

OxDock is a Dockerfile inspired build DSL for Rust. Embed scripts at compile time with macros, or run the same scripts as standalone CLI pipelines. Native. No containers. No daemon. No VM. All commands run identically on every OS, except RUN.

Supports platform gating, async tasks, and piped workflows for custom pipelines.

[Documentation](https://docs.rs/oxdock/0.10.0-alpha/oxdock/)

## Quick start

Add it to your Rust build with `cargo add oxdock@0.10.0-alpha`, or install the standalone runner with `cargo install oxdock@0.10.0-alpha`.

Run a script:

```sh
oxdock <PATH>
```

Scripts run during `rustc`, and their artifacts ship inside the binary with zero heap allocation, `no_std` included:

```rust
use oxdock_macros::oxdock_embed;

oxdock_embed! {
    // Embedded resources are mapped to `HelloAssets::get(resource)`
    name: HelloAssets,
    script: {
        ENV PROJECT=OxDock
        MKDIR dist
        WRITE dist/hello.txt Built with {{ env:PROJECT }}
        ASSERT_FILE dist/hello.txt Built with {{ env:PROJECT }}
    },
    // Generated assets land under target/, keeping the source tree clean
    out_dir: "target/prebuilt",
}

fn main() {
    // Verify we can read the resource we just created
    let file = HelloAssets::get("dist/hello.txt").expect("dist/hello.txt must be embedded");
    assert_eq!(file.data.as_ref(), b"Built with OxDock");
}
```

For each artifact the macro emits a constant backed by `include_bytes!`, which bakes the file bytes into read-only binary data during compilation. At runtime `get()` scans a static table and returns a borrowed slice, so there are no file reads and no heap allocation. The support types only need `alloc::borrow::Cow` and core iterators, which is why it works in `no_std`.

The same script also runs standalone through the CLI. It builds artifacts **and verifies them** with native assertions. Every fenced `oxdock` example in this README is executed against the implementation by [`crates/oxdock-logic-tests/tests/docs_conformance.rs`](./crates/oxdock-logic-tests/tests/docs_conformance.rs), so what you read here is guaranteed to match what the DSL actually does:

```oxdock
// Script-local variable: usable by templates and guards below.
ENV PROJECT=OxDock

// Creates the directory and any missing parents.
MKDIR dist

// Interpolate the variable into the file body via a template.
WRITE dist/hello.txt Built with {{ env:PROJECT }}

// Fail the script unless the artifact exists with exactly these bytes.
ASSERT_FILE dist/hello.txt Built with {{ env:PROJECT }}

// LS prints "<dir>:" then the entry names, sorted.
LS dist

// Assert stdout buffer of previous LS command is "hello.txt"
ASSERT_STDOUT hello.txt
```

Save the script above as `./build.oxfile` and run it by path (install once, see above):

```sh
oxdock ./build.oxfile
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

`WITH_IO` routes stdout into named pipes and back into stdin, so steps form custom pipelines without temp files.

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

One language for the whole build: farm steps out to npm, bundlers, or code generators and pull their artifacts back under cargo's control. Pipe bytes between steps without buffering whole outputs, fan work out with `ASYNC`, or skip embedding entirely and run the same scripts as standalone CLI processes.

## Variants

OxDock comes in two variants, each of which are independent of the other, but share the same core:

- [oxdock-macros](./oxdock-macros/): Provides a Rust build-time dependency which runs OxDock scripts during the compilation of a Rust program.
- [oxdock-cli](./oxdock-cli/): Command-line interface for running OxDock scripts from the command line.

## Goals

OxDock has a simple goal to provide a simple DSL that works the same across Mac, Linux, and Windows, including support for background processes, symlinks, and boolean conditionals (such as env and platform-based command filtering), which runs the same whether it's used as a preprocessing step in a build-time Rust macro, or as a CLI program, regardless of platform it is building on.

Every internal command is engineered to run the same way across platforms, except for the `RUN` command, which calls native programs.

**OxDock adds no additional runtime dependencies if used as a macro preprocessor.**

# DSL Reference

Scripts are sequences of instructions, one per line. Instructions may be prefixed with **guards** (`[...]`) that decide whether they run, and grouped into **scoped blocks** (`{ ... }`). The authoritative grammar is [`crates/oxdock-parser/src/dsl.pest`](./crates/oxdock-parser/src/dsl.pest), which is also embedded in the parser crate as the `LANGUAGE_SPEC` constant for tooling.

## Lexical structure

- **Commands are uppercase** and case-sensitive: `WORKDIR`, not `workdir`. Lowercase or mixed-case spellings are parse errors with an uppercase hint.
- One instruction per line; a semicolon (`;`) splits multiple instructions on a single line.
- Paths and arguments use forward slashes (`/`) for portability (see [Path Separators](#path-separators)).
- Scripts do **not** inherit your shell environment unless `INHERIT_ENV` opts specific keys in (see [Selective environment inheritance](#selective-environment-inheritance)).

### Statements and semicolons

```oxdock
// One line, two instructions: the semicolon splits them.
ECHO one; ECHO two
ASSERT_STDOUT one
ASSERT_STDOUT two
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
LET $a = "some_value"
ENV MODE="production"
MKDIR scoped_area
WORKDIR scoped_area

// Guarded block: LET, ENV, and WORKDIR below are scoped and revert
// when the block closes.
[bool:true] {
    LET $a = "inner_value"
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
LET $quick = ASYNC {
    ECHO hi
}
TIMEOUT 30s AWAIT $quick
```

`ASYNC` wraps any command or block — including `TIMEOUT`, `CANCEL`, `SLEEP`, and nested `ASYNC` — in either nesting order with the same deadline semantics. `LET $task = ASYNC TIMEOUT 30s RUN "build"` enforces the deadline inside the background thread (a later `AWAIT $task` surfaces the `TIMEOUT` error), while `TIMEOUT 30s AWAIT $task` preempts a hung task from the awaiting side:

```oxdock
// ASYNC wraps TIMEOUT: the deadline fires inside the background thread.
LET $bounded = ASYNC TIMEOUT 30s ECHO "bounded"
AWAIT $bounded
```

The one structural exception is `WITH_IO`, which must wrap `ASYNC` from the outside (`WITH_IO [stdout=pipe:p] ASYNC ...`) so pipe endpoints are allocated synchronously on the main thread before the worker spawns. Placing `WITH_IO` directly inside `ASYNC` is rejected at parse time.

## Cancelling tasks with CANCEL

`CANCEL $task` synchronously stops a named background task spawned via `LET $task = ASYNC ...`. It is blocking: when the statement returns, the task thread has been joined and its OS process reaped, so no residual filesystem or stream mutation can follow and the next step runs in a quiet workspace. Only named tasks can be cancelled; a later `AWAIT $task` fails with a cancellation error, and a second `CANCEL $task` fails as already cancelled.

```oxdock
// CANCEL form stops a named background task synchronously.
LET $worker = ASYNC SLEEP 30s
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
| [`RUN`](#run) | `RUN <command...>` |
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
| [`WITH_IO`](#with_io) | `WITH_IO [bindings] <command> \| WITH_IO [bindings] { <commands> }` |
| [`FOR`](#for) | `FOR $item IN <expr> { <commands> } \| FOR $key, $value IN <expr> { <commands> }` |
| [`IF`](#if) | `IF <expr> { <commands> } [ELSE IF <expr> { <commands> }] [ELSE { <commands> }]` |
| [`LET`](#let) | `LET $var = <expr> \| LET $var = ASYNC { <commands> } \| LET $var = <command> \| LET $var = AWAIT $task` |
| [`ASYNC`](#async) | `ASYNC <command...> \| ASYNC { <commands> } \| LET $var = ASYNC { <commands> }` |
| [`AWAIT`](#await) | `AWAIT $var \| LET $out = AWAIT $var` |
| [`CANCEL`](#cancel) | `CANCEL $var` |
| [`TIMEOUT`](#timeout) | `TIMEOUT <duration> <command...> \| TIMEOUT <duration> { <commands> } \| TIMEOUT <duration> AWAIT $var` |

### WITH_IO

Reroute standard streams.

**Syntax:** `WITH_IO [bindings] <command> | WITH_IO [bindings] { <commands> }`

Reroutes the standard streams of the next command or, in block form, of every enclosed command. Bindings map streams (`stdin`, `stdout`, `stderr`) to named pipes (`stdout=pipe:name`). Pipe names registered by the host runtime tee structured output elsewhere; a name bound as output can later feed another command's `stdin`, connecting commands without touching the terminal. Nested blocks stack defaults; inline bindings override inherited ones for their command only; closing a block restores previous wiring.

**Examples:**

**Example: with_io block**

```oxdock
WITH_IO [stdout=pipe:log] {
  ECHO first
  ECHO second
}
WITH_IO [stdin=pipe:log] WRITE captured.txt
```


### FOR

Iterate over a list or map.

**Syntax:** `FOR $item IN <expr> { <commands> } | FOR $key, $value IN <expr> { <commands> }`

The loop variable receives each element (lists) or value (maps); with two variables, the first receives the key. Loop variables are scoped to the loop body and do not leak outward. The body may be a braced block or a single-line `{ ... }` command. `GLOB("...")` patterns must be quoted (`*` is not a bare word, so `GLOB(*)` is a parse error); GLOB returns a root-relative sorted list, empty when nothing matches, and rejects `..` escapes.

**Examples:**

**Example: for loop**

```oxdock
LET $items = ["a", "b"]
FOR $item IN $items {
  ECHO $item
}

LET $map = {"x": 1}
FOR $k, $v IN $map {
  ECHO "$k=$v"
}
```

**Example: expand every match**

```oxdock
# single-line body; $x is a template path, WHO an override
WRITE a.txt "hi \{{ env:WHO }}!"
FOR $x IN GLOB("*.txt") { EXPAND $x WHO=World }
ASSERT_STDOUT "hi World!"
```


### IF

Conditional execution.

**Syntax:** `IF <expr> { <commands> } [ELSE IF <expr> { <commands> }] [ELSE { <commands> }]`

The condition is evaluated as a boolean expression. Prefix `!` negates (`IF !false`); only Bool values are accepted as conditions.

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

**Syntax:** `LET $var = <expr> | LET $var = ASYNC { <commands> } | LET $var = <command> | LET $var = AWAIT $task`

Assigns a value to a script-local variable. Variables are usable in templates (`{{ $var }}`), guards, and expressions. With `ASYNC`, spawns a background task and stores its handle (see ASYNC). The `$` sigil on the name is mandatory. The right-hand side is always an expression — literals, lists, maps, comparisons, `GLOB("*.md")` — never a `{{ ... }}` template; interpolation happens in string values, not here. Bare words need no quotes: `LET $d = 30s` binds the same string as `LET $d = "30s"`. When the right-hand side is a synchronous command (`LET $out = ECHO hi`), the command runs to completion and its exact stdout bytes are captured into the variable as a string (no newline stripping; commands with no stdout capture as `""`; non-UTF8 stdout is an error). Combining capture with an explicit `WITH_IO [stdout=pipe:...]` is a parse error. `LET $out = AWAIT $task` captures a background task's stdout the same way; bare `AWAIT $task` forwards it to the parent stdout instead.

**Examples:**

**Example: let**

```oxdock
LET $name = "world"
ECHO "hello, {{ $name }}"

LET $items = ["a", "b"]
LET $count = 42
```

**Example: glob binding**

```oxdock
# the RHS is an expression: GLOB(...) runs and binds a list
WRITE a.txt "x"
LET $files = GLOB("*.txt")
FOR $f IN $files { ECHO $f }
ASSERT_STDOUT "a.txt"
```

**Example: scoped variable reverts**

```oxdock
# LET inside a braced block reverts when the block exits
LET $a = "outer"
[bool:true] {
    LET $a = "inner"
    WRITE inner.txt "{{ $a }}"
}
WRITE outer.txt "{{ $a }}"
ASSERT_FILE inner.txt "inner"
ASSERT_FILE outer.txt "outer"
```

**Example: capture command output**

```oxdock
LET $out = ECHO hi
WRITE captured.txt "{{ $out }}"
ASSERT_FILE captured.txt "hi\n"
```


### ASYNC

Run steps in a background thread.

**Syntax:** `ASYNC <command...> | ASYNC { <commands> } | LET $var = ASYNC { <commands> }`

Runs a command or block of commands in a background thread with subshell isolation. Mutations (ENV, WORKDIR) stay within the block. With `LET`, stores a task handle for `AWAIT`.

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
LET $task = ASYNC {
    ECHO "built"
}
AWAIT $task
```


### AWAIT

Join a background task.

**Syntax:** `AWAIT $var | LET $out = AWAIT $var`

Blocks until the named task completes. Propagates errors if the task failed. Bare `AWAIT $var` forwards the task's stdout to the parent stdout; `LET $out = AWAIT $var` captures it into `$out` instead (same UTF-8 and spilling rules as `LET $var = <command>`).

**Examples:**

**Example: await**

```oxdock
LET $task = ASYNC ECHO "done"
AWAIT $task
```

**Example: await capture**

```oxdock
LET $task = ASYNC ECHO "done"
LET $out = AWAIT $task
WRITE captured.txt "{{ $out }}"
ASSERT_FILE captured.txt "done\n"
```


### CANCEL

Synchronously cancel a background task.

**Syntax:** `CANCEL $var`

Kills the named background task spawned via LET $var = ASYNC .... Blocking: returns only after the task thread has been joined and its OS process reaped, so no residual filesystem or stream mutation follows. A later AWAIT $var reports cancellation. Only named tasks can be cancelled.

**Examples:**

**Example: cancel**

```oxdock
LET $task = ASYNC SLEEP 30s
CANCEL $task
```


### TIMEOUT

Enforce an execution deadline.

**Syntax:** `TIMEOUT <duration> <command...> | TIMEOUT <duration> { <commands> } | TIMEOUT <duration> AWAIT $var`

Aborts the wrapped step or block with a deadline error if it exceeds the duration (e.g. 500ms, 10s, 2m; a bare number means seconds). A blocking foreground process is killed.

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
LET $budget = "30s"
TIMEOUT $budget WRITE heartbeat.txt alive
ASSERT_FILE heartbeat.txt alive
```


### WORKDIR

Change the working directory.

**Syntax:** `WORKDIR <path>`

Sets the current working directory. Relative paths resolve against the current directory; `/` resets to the workspace root. Paths cannot escape the workspace.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`path`](#value-type-path) | yes | Directory to change to |

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

Inserts or updates an env var. The value uses the unified string-value rules shared by every command: `"..."` or `'...'` quotes keep exact bytes (spaces, tabs), a lone `$var` evaluates that variable, `{{ ... }}` placeholders interpolate, unquoted words join with single spaces, and the first `=` splits key from value (`KEY=a=b` stores `a=b`). A `$var` inside larger text stays literal — write `{{ $var }}` to interpolate there.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `assignment` | [`KEY=value`](#value-type-keyvalue) | yes | KEY=value pair |

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
LET $who = "Alice"
ENV GREETING=$who
WRITE out.txt "{{ env:GREETING }}"
ASSERT_FILE out.txt "Alice"
```

**Example: all value forms agree**

```oxdock
# a bare variable, a quoted literal, and a template all
# store plain strings through the same value rules
LET $x = "Ada"
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

Declares which host environment variables to inherit into the script. Must appear before any other commands and at most once. Without this directive, the script starts with an empty environment.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `keys` | [`string...`](#value-type-string) | no | Host variables to inherit |

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
| `message` | [`string...`](#value-type-string) | yes | Text |

**Output:** Stdout

**Examples:**

**Example: echo**

```oxdock
ECHO build-complete
```

**Example: variables**

```oxdock
# a lone $x evaluates; {{ }} interpolates inside text
LET $x = "World"
ECHO {{ $x }}
ECHO $x
ASSERT_STDOUT "World"
```


### RUN

Execute shell command.

**Syntax:** `RUN <command...>`

Runs command in cwd.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `command` | [`string...`](#value-type-string) | yes | Command |

**Examples:**

**Example: run**

```oxdock
RUN echo hello
```


### COPY

Copy file into workspace.

**Syntax:** `COPY [--from-current-workspace] <from> <to>`

Copies from host.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `from` | [`path`](#value-type-path) | yes | Source |
| `to` | [`path`](#value-type-path) | yes | Dest |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--from-current-workspace` | Flag | Copy from workspace instead of build context |

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
| `rev` | [`string`](#value-type-string) | yes | Rev |
| `src` | [`path`](#value-type-path) | yes | Src |
| `dst` | [`path`](#value-type-path) | yes | Dst |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--include-dirty` | Flag | Include dirty |

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
| `from` | [`path`](#value-type-path) | yes | Target |
| `to` | [`path`](#value-type-path) | yes | Link |

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
| `path` | [`path`](#value-type-path) | yes | Dir path |

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
| `path` | [`path`](#value-type-path) | no | Dir |

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
| `path` | [`path`](#value-type-path) | no | File |

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

Reads bytes until newline without waiting for EOF, leaving the pipe open. Trailing newline is stripped (shell-read parity). On premature EOF assigns accumulated bytes and returns.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `var` | [`$var`](#value-type-var) | yes | Variable to store the line |

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
| `path` | [`path`](#value-type-path) | yes | File |
| `contents` | [`string...`](#value-type-string) | no | Content |

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
| `path` | [`path`](#value-type-path) | yes | File |
| `contents` | [`string...`](#value-type-string) | no | Content |

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

A template is any text file — or piped stdin when no path is given — containing `{{ ... }}` placeholders. EXPAND replaces each placeholder and prints the result to stdout. Placeholders: `{{ NAME }}` reads a `KEY=val` override passed on this command; `{{ env:NAME }}` reads an override, falling back to the environment; `{{ $var }}` reads a script variable (dotted paths allowed). A missing key is an error, never a silent empty. Substitution runs in a single pass. EXPAND is not recursive and does not expand nested placeholders: a value that itself contains `{{ ... }}` is inserted verbatim and never expanded again. A bare `$var` argument is a template path; `KEY=val` arguments are overrides whose values follow the unified string-value rules (same as `ENV`: quotes keep exact bytes, a lone `$var` evaluates, `{{ ... }}` interpolates). NOTE: `WRITE` interpolates `{{ ... }}` while writing, so escape it (`\{{ ... }}`) when writing a template file for a later `EXPAND`. With no path, the template arrives on stdin through a pipe. When piping from a shell, single-quote the template (`echo '{{ $x }}'`): double quotes let the shell swallow `$x`, so oxdock receives an empty `{{ }}` placeholder and errors.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`path`](#value-type-path) | no | Template file to expand; omit to expand stdin |
| `overrides` | [`KEY=value...`](#value-type-keyvalue) | no | Template overrides shadowing that key (unified string values) |

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
LET $who = "Bob"
WRITE template.md "Hi \{{ env:WHO }}!"
EXPAND template.md WHO=$who
ASSERT_STDOUT "Hi Bob!"
```

**Example: override forms agree**

```oxdock
# a bare variable and a template-with-tail expand identically
LET $x = "Ada"
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

Checks the path is a file, then optionally compares its bytes (or `--hash` SHA-256 digest) against the expectation. Any mismatch aborts the pipeline with a step-numbered error showing expected vs actual.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`path`](#value-type-path) | yes | File |
| `expected` | [`string...`](#value-type-string) | no | Expected |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--hash` | String | SHA-256 |

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

Checks the path is a directory, aborting the pipeline with a step-numbered error otherwise.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`path`](#value-type-path) | yes | Dir |

**Examples:**

**Example: assert dir**

```oxdock
MKDIR dist/assets
ASSERT_DIR dist/assets
```


### ASSERT_ABSENT

Assert path absent.

**Syntax:** `ASSERT_ABSENT <path>`

Checks nothing exists at the path, aborting the pipeline with a step-numbered error if it does.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`path`](#value-type-path) | yes | Path |

**Examples:**

**Example: assert absent**

```oxdock
ASSERT_ABSENT missing.txt
```


### ASSERT_STDOUT

Assert stdout contains.

**Syntax:** `ASSERT_STDOUT <substring>`

Checks the preceding step's stdout contains the substring, aborting the pipeline with a step-numbered error otherwise.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `substring` | [`string...`](#value-type-string) | yes | Substring |

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
| `path` | [`path`](#value-type-path) | yes | File |

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

Stops the pipeline immediately with an `EXIT requested with code <code>` error; steps after it never run, at any nesting depth. Enclosing blocks still unwind their LET/ENV/WORKDIR/WORKSPACE state, anonymous background tasks are killed synchronously, and files written before the EXIT persist.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `code` | [`int`](#value-type-int) | yes | Code |

**Examples:**

**Example: exit**

```oxdock expect_error:"EXIT requested with code 0"
EXIT 0
```


### SLEEP

Pause execution for a duration.

**Syntax:** `SLEEP <duration>`

Parks the step for the duration (e.g. 500ms, 10s, 2m). Cooperative: checks for cancellation so an enclosing TIMEOUT or task teardown interrupts the sleep. Cross-platform alternative to shell sleep for testing time boundaries.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `duration` | [`duration`](#value-type-duration) | yes | How long to sleep |

**Examples:**

**Example: sleep**

```oxdock
SLEEP 100ms
```

**Example: sleep variable duration**

```oxdock
# durations resolve at runtime, so variables work too —
# quoted or bare, both bind the same string
LET $pause = "100ms"
SLEEP $pause
LET $bare = 100ms
SLEEP $bare
```


## Value types

### Value type: string

Arbitrary text under the unified string-value rules: quotes keep exact bytes, a lone `$var` evaluates, and `{{ ... }}` placeholders interpolate.

### Value type: path

Workspace path, resolved against the current working directory and guarded against escaping the workspace.

### Value type: int

Integer, e.g. an exit code.

### Value type: duration

Positive time span: a number with an `ms`, `s`, `m`, or `h` suffix — a bare number means seconds — e.g. `500ms`, `10s`, `2m`.

### Value type: $var

Script variable reference. The `$` sigil is mandatory.

### Value type: KEY=value

`KEY=value` assignment splitting on the first `=` (`KEY=a=b` stores `a=b`). Values follow the unified string-value rules.

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

## How these examples are tested

Every ```` ```oxdock ```` fence in this document is extracted with [`oxdock_parser::extract_fenced_blocks`](./crates/oxdock-parser/src/markdown.rs) and executed by [`crates/oxdock-logic-tests/tests/docs_conformance.rs`](./crates/oxdock-logic-tests/tests/docs_conformance.rs) against the real parser and interpreter, so the documentation cannot drift from the implementation. Enforcement layers:

- **Parse & execute:** every snippet must parse and run clean (or fail with its declared `expect_error:` message) on Linux, macOS, and Windows CI.
- **Coverage gates:** every parser command must appear in at least one executable example, and key structural features (`any(`, `not(`, `{{ env:`, `[env:`) must be demonstrated.
- **Compile-time parity:** a [build-time fixture](./crates/oxdock-logic-tests/fixtures/integration/buildtime_macros/assert_verification/) runs this README's quick-start script through `oxdock_embed!`, assertions included.
- **Real-binary check:** the quick start is additionally executed through the actual `oxdock` binary exactly as documented (`--script Oxfile`).
- **Doctest execution:** the Rust quick start is wired into [`crates/oxdock-doc-tests`](./crates/oxdock-doc-tests/) and compiled *and* run by `cargo test --doc` on every CI OS.
- **Reference integrity:** every relative Markdown link target and every repo path referenced from a ```` ```bash ```` fence must exist.

Snippets contain nothing but OxDock — copy any of them straight into an `Oxfile` or an `oxdock_embed!` macro. Runner-specific configuration lives in the fence info-string, which Markdown renders as inert metadata:

```text
```oxdock                                    plain snippet, must parse and run clean
```oxdock env:KEY=value                      inject an environment value (visible to INHERIT_ENV/guards)
```oxdock roots:unified                      run with workspace root == build context (COPY/COPY_GIT demos)
```oxdock expect_error:"message substring"   snippet must fail with this text in its error
```

Everything else you see inside the fences — including the `ASSERT_*` commands — is part of the DSL itself and executes identically in your own pipelines.

If you change the DSL, update this reference in the same commit — CI will hold you to it.

## Environment variable contracts

Environment variables understood by the toolchain (workspace roots, caching fingerprints, IDE integrations) are specified in [ENV_CONTRACTS.md](./ENV_CONTRACTS.md).

## GitHub Actions Integration

OxDock scripts can emit GitHub Actions workflow commands using native DSL primitives.
Steps that only make sense on a runner live inside `[env:GITHUB_ACTIONS]` blocks:
guards consult the script environment, so each snippet first bridges the runner
variable in with `INHERIT_ENV`. Where `GITHUB_ACTIONS` is absent the whole block
skips and `docs_conformance` still passes; on a hosted runner it executes.

### Log annotations

`ECHO` writes to stdout, which GitHub Actions intercepts for annotations:

```oxdock
INHERIT_ENV [GITHUB_ACTIONS]

[env:GITHUB_ACTIONS] {
    ECHO "::notice::test notice message"
    ECHO "::warning::test warning message"
    ECHO "::error::test error message"
}
```

### Collapsible log groups

Group markers go through `ECHO` — no shell required:

```oxdock
INHERIT_ENV [GITHUB_ACTIONS]

[env:GITHUB_ACTIONS] {
    ECHO "::group::unit tests"
    ECHO "running tests"
    ECHO "::endgroup::"
}
```

### Job summary, step outputs, and environment variables

`APPEND` writes to append-only runner state files without truncating earlier entries:

```oxdock
INHERIT_ENV [GITHUB_ACTIONS]

[env:GITHUB_ACTIONS] {
    APPEND dist/summary.md "### Build Report\n- Passed: 123\n- Failed: 0\n"
    APPEND dist/outputs.txt "artifact_path=dist/app.tar\n"
    APPEND dist/env.txt "NOTEBOOK_MODE=release\n"
}
```

On GitHub Actions, replace the paths with the runner-provided env vars (`{{ env:GITHUB_STEP_SUMMARY }}`, `{{ env:GITHUB_OUTPUT }}`, `{{ env:GITHUB_ENV }}`):

## Testing & Coverage

### Testing

Testing is performed across Linux, Mac, and Windows environments, and UB (Undefined Behavior) testing is handled by [Miri](https://github.com/rust-lang/miri).

There is strong prioritization in keeping unit and integration tests compatible with Miri, because doing so also encourages clean separation of process and filesystem modeling from direct OS calls, avoiding scattered filesystem and process usage throughout the codebase.

### Coverage reporting

#### LLVM line coverage (cargo-llvm-cov)

The `coverage (cargo-llvm-cov)` GitHub Actions job installs [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) and publishes [`lcov`](https://github.com/linux-test-project/lcov) data to Coveralls. Once the repository is enabled on Coveralls, pushes and pull requests to `main` automatically update the badge above.

To reproduce the report locally (requires the nightly LLVM tools component):

```bash
cargo install cargo-llvm-cov
rustup component add llvm-tools-preview
cargo llvm-cov --workspace --all-features --lcov --output-path lcov.info
```

#### Miri coverage

The CI `miri` job monitors how many workspace unit tests can run under [`cargo miri`](https://github.com/rust-lang/miri). On pushes to `main`, the job publishes a badge description (`badges/miri-coverage.json` on the `badges` branch) that backs the Miri coverage badge above.

To keep the badge grounded in real coverage reporting, the workflow multiplies two signals:

1. **Runnable test ratio:** how many workspace tests are runnable under Miri vs. the total (`cargo miri test -- --list`).
2. **LLVM line coverage baseline:** the percent reported by `cargo llvm-cov --summary-only` (the same value sent to Coveralls).

The badge therefore shows an approximate “effective Miri coverage” (baseline coverage × runnable ratio), which can never exceed the standard coverage percentage but gives a tangible sense of how much of the tested surface area is validated under the runner.

To test the calculation locally without waiting for CI:

```bash
cargo llvm-cov --workspace --all-features --summary-only > coverage-summary.txt
BASE_LINE_COVERAGE=$(awk '/^TOTAL/ {print $10}' coverage-summary.txt | tr -d '%' | head -n1) \
  scripts/.github/miri-badge-report.sh
```

The helper emits the same badge JSON (`badges/miri-coverage.json`) and summary text used by CI, making it easy to confirm the numbers before opening a PR.

If you run new tests under Miri locally, you can sanity-check parity with CI via:

```bash
cargo +nightly miri setup
cargo +nightly miri test --workspace --all-features --lib --tests
```

## License

`OxDock` is distributed under the terms of the Apache License (Version 2.0).
