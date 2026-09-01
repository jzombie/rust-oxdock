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


> **OxDock is an experimental DSL used for building embeddable artifacts and orchestrating pipelines.**
>
> **It is currently in alpha and is subject to rapid API changes.**

# OxDock

OxDock is a Dockerfile-inspired DSL that runs **natively on your host** — no containers, no daemon, no VM. It comes in two flavors sharing one core: a [Rust build-time macro](./oxdock-macros/) whose scripts run during compilation, embedding resources directly into the binary's data section (no heap allocation when the program starts; the generated asset structs are pure Rust and work in `no_std` targets), and a [standalone CLI](./oxdock-cli/) that orchestrates cross-platform workflows as ordinary local processes.

Unlike Docker, commands execute directly on the host: they can be guarded by platform/env conditions, run inside scoped blocks so changes to `ENV` or `WORKDIR` don’t leak, and interoperate with containers whenever you want them — you can invoke Docker from an OxDock script, or even install Docker, while the DSL itself stays portable.

## Variants

OxDock comes in two variants, each of which are independent of the other, but share the same core:

- [oxdock-macros](./oxdock-macros/): Provides a Rust build-time dependency which runs OxDock scripts during the compilation of a Rust program.
- [oxdock-cli](./oxdock-cli/): Command-line interface for running OxDock scripts from the command line.

## Goals

OxDock has a simple goal to provide a simple DSL that works the same across Mac, Linux, and Windows, including support for background processes, symlinks, and boolean conditionals (such as env and platform-based command filtering), which runs the same whether it's used as a preprocessing step in a build-time Rust macro, or as a CLI program, regardless of platform it is building on.

Every internal command is engineered to run the same way across platforms, except for the `RUN` command, which calls native programs.

**OxDock adds no additional runtime dependencies if used as a macro preprocessor.**

## Quick start

The following script is a complete OxDock script — it builds artifacts **and verifies them** with native assertions. Every fenced `oxdock` example in this README is executed against the implementation by [`crates/oxdock-logic-tests/tests/docs_conformance.rs`](./crates/oxdock-logic-tests/tests/docs_conformance.rs), so what you read here is guaranteed to match what the DSL actually does:

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

Run it with the CLI:

```bash
cargo install --path oxdock-cli
oxdock --script Oxfile
```

Or embed the same script at compile time — the macro runs the script during `rustc` and generates a pure-Rust struct whose assets live in the binary's data section, readable at runtime with zero heap allocation:

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

- `env:KEY` passes when variable `KEY` exists and is non-empty; `env:KEY==value` and `env:KEY!=value` compare values.
- Bare platform tags pass based on the host: `linux`, `macos` (alias `mac`), `windows`, `unix`. Tags are case-insensitive.
- A comma-separated list means **AND**: `[env:A, linux]`.
- Disjunction is expressed as a call — `or(expr, expr, ...)` with at least two branches — not an infix operator.
- Any predicate may be negated with a (repeatable) leading `!`: `[!env:SKIP]`.
- Parentheses group expressions: `[or(env:A, linux), mac]`.

Guards attach to the next instruction. Several guard lines in a row chain onto the same target, and a guard immediately followed by `{` opens a guarded block whose guard applies to every enclosed instruction.

Guard evaluation checks the script environment first and falls back to the process environment, so guards interact naturally with `INHERIT_ENV` and `ENV`.

### Environment guards

```oxdock env:DEPLOY_TARGET=staging
// Copy the key from the host environment (the runner injects it).
INHERIT_ENV [DEPLOY_TARGET]

// Passes when the variable exists with any non-empty value.
[env:DEPLOY_TARGET] ECHO deploy-target-visible

// Equality against the inherited value.
[env:DEPLOY_TARGET==staging] ECHO deploying-to-staging

// Inequality: skipped below, because DEPLOY_TARGET IS staging.
[env:DEPLOY_TARGET!=staging] ECHO deploying-elsewhere

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

// ! inverts the predicate: passes because the variable does NOT exist.
[!env:OXDOCK_DOC_UNDEFINED_VAR] ECHO negation-passes-for-undefined

// or(...) passes when ANY branch holds; A exists, so this runs.
[or(env:OXDOCK_DOC_FEATURE_A, env:OXDOCK_DOC_FEATURE_B)] ECHO or-matched-a-branch

// Comma composes with AND: (A or linux) AND A — true here on every OS.
[or(env:OXDOCK_DOC_FEATURE_A, linux), env:OXDOCK_DOC_FEATURE_A] ECHO composed-and-or-guard

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

Instructions guarded by a `{ ... }` block run inside a scope: changes to `WORKDIR`, `WORKSPACE`, or `ENV` revert once the block exits, so temporary setup cannot leak outward. Blocks nest.

```oxdock
ENV SCOPE_MARKER=armed

// Guarded block: the WORKDIR change below is scoped and reverts
// when the block closes.
[env:SCOPE_MARKER==armed] {
  WORKDIR scoped-area
  WRITE inner.txt written-inside-scoped-block
  ASSERT_FILE inner.txt written-inside-scoped-block
}

// cwd is back at the workspace root — but files persist.
ASSERT_FILE scoped-area/inner.txt written-inside-scoped-block
WRITE outer.txt written-after-scope-restored
ASSERT_FILE outer.txt written-after-scope-restored
```

Files created inside a scope persist; only the working directory, workspace root, and environment revert.

## Command Reference

| Command | Syntax |
| --- | --- |
| [`INHERIT_ENV`](#inherit_env) | `INHERIT_ENV [KEY1, KEY2, ...]` |
| [`WORKDIR`](#workdir) | `WORKDIR <path>` |
| [`WORKSPACE`](#workspace) | `WORKSPACE SNAPSHOT\|LOCAL` |
| [`ENV`](#env) | `ENV KEY=value` |
| [`ECHO`](#echo) | `ECHO <message>` |
| [`RUN`](#run) | `RUN <command...>` |
| [`RUN_BG`](#run_bg) | `RUN_BG <command...>` |
| [`COPY`](#copy) | `COPY [--from-current-workspace] <from> <to>` |
| [`WITH_IO`](#with_io) | `WITH_IO [bindings] [command \| { block }]` |
| [`COPY_GIT`](#copy_git) | `COPY_GIT [--include-dirty] <rev> <src> <dst>` |
| [`HASH_SHA256`](#hash_sha256) | `HASH_SHA256 <path>` |
| [`SYMLINK`](#symlink) | `SYMLINK <from> <to>` |
| [`MKDIR`](#mkdir) | `MKDIR <path>` |
| [`LS`](#ls) | `LS [<path>]` |
| [`CWD`](#cwd) | `CWD` |
| [`READ`](#read) | `READ [<path>]` |
| [`WRITE`](#write) | `WRITE <path> [<contents>]` |
| [`APPEND`](#append) | `APPEND <path> [<contents>]` |
| [`EXPAND`](#expand) | `EXPAND [<path>] [<KEY=val> ...]` |
| [`ASSERT_FILE`](#assert_file) | `ASSERT_FILE [--hash <sha256>] <path> [<expected>]` |
| [`ASSERT_DIR`](#assert_dir) | `ASSERT_DIR <path>` |
| [`ASSERT_ABSENT`](#assert_absent) | `ASSERT_ABSENT <path>` |
| [`ASSERT_STDOUT`](#assert_stdout) | `ASSERT_STDOUT <substring>` |
| [`EXIT`](#exit) | `EXIT <code>` |

### WITH_IO

Reroutes the standard streams of the next command or, in block form, of every enclosed command. Bindings map streams (`stdin`, `stdout`, `stderr`) to named pipes (`stdout=pipe:name`). Pipe names registered by the host runtime tee structured output elsewhere; a name bound as output can later feed another command's `stdin`, connecting commands without touching the terminal.

**Inline form:** `WITH_IO [bindings] <command>`

**Block form:**
```oxdock
WITH_IO [stdout=pipe:log] {
  ECHO first
  ECHO second
}
WITH_IO [stdin=pipe:log] WRITE captured.txt
```

Nested blocks stack defaults; inline bindings override inherited ones for their command only; closing a block restores previous wiring.

### INHERIT_ENV

Declares which host environment variables to inherit into the script. Must appear before any other commands and at most once. Without this directive, the script starts with an empty environment.

```oxdock
INHERIT_ENV [PATH, HOME]
```

### FOR

Iterates over a list or map. The loop variable receives each element (lists) or value (maps); with two variables, the first receives the key.

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

### IF / ELSE IF / ELSE

Conditional execution. The condition is evaluated as a boolean expression.

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
```

### LET

Assigns a value to a script-local variable. Variables are usable in templates (`{{ $var }}`), guards, and expressions.

```oxdock
LET $name = "world"
ECHO "hello, {{ $name }}"

LET $items = ["a", "b"]
LET $count = 42
```

### RUN

Execute a shell command.

**Syntax:** `RUN <command...>`

Runs the command in the current working directory. Arguments are joined with spaces. Child stdout/stderr stream to the script's configured outputs, and a non-zero exit code fails the script. This is the one intentionally platform-specific command: use platform guards to provide per-OS invocations when needed.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `command` | `string...` | yes | Shell command to execute |

**Examples:**

**Example: run cargo**

```oxdock
RUN cargo build
```


### RUN_BG

Execute a shell command in the background.

**Syntax:** `RUN_BG <command...>`

Like RUN, but spawns the command in the background and continues the script. If any background child finishes early with a non-zero status, the script fails and remaining children are killed. At script end the first child is awaited to completion and the remainder are killed. Background children started before an EXIT are killed before unwinding.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `command` | `string...` | yes | Shell command to run in background |


### ECHO

Print a message to stdout.

**Syntax:** `ECHO <message>`

Outputs the message to stdout. Supports template expansion.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `message` | `string` | yes | Text to print |

**Output:** Stdout


### WORKDIR

Change the working directory for subsequent steps.

**Syntax:** `WORKDIR <path>`

Sets the current working directory. Relative paths resolve against the current root.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `string` | yes | Directory to change to |

**Examples:**

**Example: change dir**

```oxdock
WORKDIR src
```


### WORKSPACE

Switch between snapshot and local workspace roots.

**Syntax:** `WORKSPACE SNAPSHOT|LOCAL`

SNAPSHOT targets the read-only snapshot root; LOCAL targets the mutable build-context root.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `target` | `SNAPSHOT|LOCAL` | yes | SNAPSHOT or LOCAL |


### ENV

Set an environment variable for subsequent steps.

**Syntax:** `ENV KEY=value`

Inserts or updates an environment variable. The value is an expandable string.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `assignment` | `KEY=value` | yes | KEY=value pair |

**Examples:**

**Example: set env**

```oxdock
ENV FOO=bar
```


### COPY

Copy a file or directory into the workspace.

**Syntax:** `COPY [--from-current-workspace] <from> <to>`

Copies a file/directory from the host filesystem into the workspace. Plain COPY <from> <to> resolves <from> against the build context (the tree OxDock was invoked against), regardless of the current working directory. COPY --from-current-workspace <from> <to> resolves <from> against the workspace root instead. Parent directories at the destination are created on demand.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `from` | `path` | yes | Source path (build context or workspace root) |
| `to` | `path` | yes | Destination path in workspace |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--from-current-workspace` | Flag | Copy from the current workspace root instead of build context |


### COPY_GIT

Copy a file or directory from a git revision.

**Syntax:** `COPY_GIT [--include-dirty] <rev> <src> <dst>`

Checks out a specific git revision and copies the specified path into the workspace.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `rev` | `string` | yes | Git revision spec |
| `src` | `path` | yes | Source path in repository |
| `dst` | `path` | yes | Destination path in workspace |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--include-dirty` | Flag | Include uncommitted changes |


### SYMLINK

Create a symbolic link.

**Syntax:** `SYMLINK <from> <to>`

Creates a symlink at 'to' pointing to 'from'.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `from` | `path` | yes | Target of the symlink |
| `to` | `path` | yes | Link path to create |


### MKDIR

Create a directory.

**Syntax:** `MKDIR <path>`

Creates the directory at the given path, including parents.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `path` | yes | Directory path to create |


### LS

List directory contents.

**Syntax:** `LS [<path>]`

Lists entries in the given directory, or the current directory if omitted.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `path` | no | Directory to list (optional) |

**Output:** Stdout


### CWD

Print the current working directory.

**Syntax:** `CWD`

Outputs the current working directory to stdout.

**Output:** Stdout


### READ

Read file contents to stdout.

**Syntax:** `READ [<path>]`

Outputs the file contents. If no path, reads from stdin.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `path` | no | File to read (optional, stdin if omitted) |

**Output:** Stdout


### WRITE

Write contents to a file.

**Syntax:** `WRITE <path> [<contents>]`

Writes contents to a file, replacing any existing contents. Creates parent directories on demand. Without a contents argument it consumes the script's stdin instead (combine with WITH_IO [stdin...]).

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `path` | yes | File path to write |
| `contents` | `string` | no | File contents (optional, stdin if omitted) |


### APPEND

Append contents to a file.

**Syntax:** `APPEND <path> [<contents>]`

Appends the contents to the specified file, creating it if it doesn't exist.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `path` | yes | File path to append to |
| `contents` | `string` | no | Content to append (optional, stdin if omitted) |


### EXPAND

Expand template placeholders in a file.

**Syntax:** `EXPAND [<path>] [<KEY=val> ...]`

Reads the file (or stdin), expands {{ env:KEY }} placeholders, and outputs to stdout.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `path` | no | Template file path (optional, stdin if omitted) |

**Output:** Stdout


### ASSERT_FILE

Assert a file exists and optionally matches expected content.

**Syntax:** `ASSERT_FILE [--hash <sha256>] <path> [<expected>]`

Verifies the file exists. With --hash, checks the SHA-256 digest. With an expected argument, checks the contents match.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `path` | yes | File path to verify |
| `expected` | `string` | no | Expected file contents |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--hash` | String | Expected SHA-256 hash of the file contents |


### ASSERT_DIR

Assert a directory exists.

**Syntax:** `ASSERT_DIR <path>`

Verifies the directory exists.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `path` | yes | Directory path to verify |


### ASSERT_ABSENT

Assert a file or directory does not exist.

**Syntax:** `ASSERT_ABSENT <path>`

Verifies the path does not exist.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `path` | yes | Path that must not exist |


### ASSERT_STDOUT

Assert stdout contains a substring.

**Syntax:** `ASSERT_STDOUT <substring>`

Verifies that subsequent command output contains the given substring.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `substring` | `string` | yes | Expected substring in stdout |


### HASH_SHA256

Print the SHA-256 hash of a file.

**Syntax:** `HASH_SHA256 <path>`

Computes and outputs the SHA-256 digest of the file contents.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `path` | yes | File or directory to hash |

**Output:** Stdout


### EXIT

Exit the pipeline with a status code.

**Syntax:** `EXIT <code>`

Terminates the pipeline immediately with the given exit code.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `code` | `int` | yes | Exit status code |



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

- **Typical usage pattern:** the temporary workspace is intended for short-lived build/test iterations — run scripts against it, inspect outputs, and discard when done. Because it is separate from the original repo it is safe to run multiple concurrent experiments without changing the original repo.

- **Filesystem gating via `oxdock-fs`:** all filesystem operations in the runtime are routed through the crate-internal `oxdock-fs` abstraction. That module centralizes path resolution, canonicalization and access checks so reads and writes can be validated against the allowed workspace root and build context.

- **What `oxdock-fs` protects you from:** the guardrails are pragmatic — they prevent common mistakes such as accidentally writing outside the materialized workspace or reading files from arbitrary absolute paths. However, they are not a full sandbox: a determined process or script can still create destructive actions (e.g., invoking native `RUN` commands that modify external state). If you require strict isolation, run OxDock inside a container or VM.

- **Performance:** routing via `oxdock-fs` adds negligible overhead for typical workloads. The module focuses on correctness and containment with minimal runtime cost so interactive iteration remains fast.

## How these examples are tested

Every ```` ```oxdock ```` fence in this document is extracted with [`oxdock_parser::extract_fenced_blocks`](./crates/oxdock-parser/src/markdown.rs) and executed by [`crates/oxdock-logic-tests/tests/docs_conformance.rs`](./crates/oxdock-logic-tests/tests/docs_conformance.rs) against the real parser and interpreter, so the documentation cannot drift from the implementation. Enforcement layers:

- **Parse & execute:** every snippet must parse and run clean (or fail with its declared `expect_error:` message) on Linux, macOS, and Windows CI.
- **Coverage gates:** every parser command must appear in at least one executable example, and key structural features (`or(`, `{{ env:`, `[env:`) must be demonstrated.
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
All examples below use `[env:GITHUB_ACTIONS]` guards so they execute on CI runners
but are skipped during local `docs_conformance` tests.

### Log annotations

`ECHO` writes to stdout, which GitHub Actions intercepts for annotations:

```oxdock
ECHO "::notice::test notice message"
ECHO "::warning::test warning message"
ECHO "::error::test error message"
```

### Collapsible log groups

```oxdock
RUN echo "::group::unit tests"
RUN echo "running tests"
RUN echo "::endgroup::"
```

### Job summary, step outputs, and environment variables

`APPEND` writes to append-only runner state files without truncating earlier entries:

```oxdock
APPEND dist/summary.md "### Build Report\n- Passed: 123\n- Failed: 0\n"
APPEND dist/outputs.txt "artifact_path=dist/app.tar\n"
APPEND dist/env.txt "NOTEBOOK_MODE=release\n"
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
