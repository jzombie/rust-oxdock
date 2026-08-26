<div align="center">
  <img src="assets/OxDock-logo.svg" alt="OxDock logo" width="360"/>
</div>

<div align="center">
  <a href="https://www.rust-lang.org/">
    <img src="https://img.shields.io/badge/Made%20with-Rust-black?&logo=Rust" alt="Made with Rust" />
  </a>
  <!-- <a href="https://docs.rs/oxdock">
    <img src="https://img.shields.io/docsrs/oxdock" alt="docs.rs" />
  </a> -->
  <a href="https://github.com/jzombie/oxdock-rs/actions/workflows/rust-tests.yml?query=branch%3Amain+event%3Apush">
    <img src="https://img.shields.io/github/actions/workflow/status/jzombie/oxdock-rs/rust-tests.yml?branch=main&label=Miri&logo=github" alt="Miri status" />
  </a>
  <a href="https://deepwiki.com/jzombie/rust-oxdock">
    <img src="https://deepwiki.com/badge.svg" alt="DeepWiki" />
  </a>
  <a href="https://coveralls.io/github/jzombie/oxdock-rs?branch=main">
    <img src="https://coveralls.io/repos/github/jzombie/oxdock-rs/badge.svg?branch=main" alt="Coverage Status" />
  </a>
  <a href="#miri-coverage">
    <img src="https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fjzombie%2Foxdock-rs%2Fbadges%2Fmiri-coverage.json" alt="Miri Coverage" />
  </a>
</div>

# OxDock

OxDock is a Docker-inspired language that runs **natively on your host** — no containers, no daemon, no VM. It comes in two flavors sharing one core: a [Rust build-time macro](./oxdock-buildtime-macros/) whose scripts run during compilation, embedding resources directly into the binary's data section (no heap allocation when the program starts; the generated asset structs are pure Rust and work in `no_std` targets), and a [standalone CLI](./oxdock-cli/) that orchestrates cross-platform workflows as ordinary local processes.

Unlike Docker, commands execute directly on the host: they can be guarded by platform/env conditions, run inside scoped blocks so changes to `ENV` or `WORKDIR` don’t leak, and interoperate with containers whenever you want them — you can invoke Docker from an OxDock script, or even install Docker, while the DSL itself stays portable.

## Variants

OxDock comes in two variants, each of which are independent of the other, but share the same core:

- [oxdock-buildtime-macros](./oxdock-buildtime-macros/): Provides a Rust build-time dependency which runs OxDock scripts during the compilation of a Rust program.
- [oxdock-cli](./oxdock-cli/): Command-line interface for running OxDock scripts from the command line.

## Goals

OxDock has a simple goal to provide a simple language that works the same across Mac, Linux, and Windows, including support for background processes, symlinks, and boolean conditionals (such as env and platform-based command filtering), which runs the same whether it's used as a preprocessing step in a build-time Rust macro, or as a CLI program, regardless of platform it is building on.

Every internal command is engineered to run the same way across platforms, except for the `RUN` command, which calls native programs.

<!-- TODO: Mention that OxDock adds no additional runtime dependencies if used as a preprocessor. -->

## Quick start

The following script is a complete OxDock program — it builds artifacts **and verifies them** with native assertions. Every fenced `oxdock` example in this README is executed against the implementation by [`crates/oxdock-logic-tests/tests/docs_conformance.rs`](./crates/oxdock-logic-tests/tests/docs_conformance.rs), so what you read here is guaranteed to match what the language actually does:

```oxdock
// Script-local variable: usable by templates and guards below.
ENV PROJECT=oxdock

// Creates the directory and any missing parents.
MKDIR dist

// Interpolate the variable into the file body via a template.
WRITE dist/hello.txt Built with {{ env:PROJECT }}

// Fail the script unless the artifact exists with exactly these bytes.
ASSERT_FILE dist/hello.txt Built with {{ env:PROJECT }}

// LS prints "<dir>:" then the entry names, sorted.
LS dist
ASSERT_STDOUT hello.txt
```

Run it with the CLI:

```bash
cargo install --path oxdock-cli
oxdock --script Oxfile
```

Or embed the same script at compile time — the macro runs the script during `rustc` and generates a pure-Rust struct whose assets live in the binary's data section, readable at runtime with zero heap allocation:

```rust
use oxdock_buildtime_macros::embed;

embed! {
    // Embedded resources are mapped to `HelloAssets::get(resource)`
    name: HelloAssets,
    script: {
        ENV PROJECT=oxdock
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
    assert_eq!(file.data.as_ref(), b"Built with oxdock");
}
```

<!-- TODO: Describe Oxfile (which is a script which runs in the OxDock interpreter or is embedded in Rust compile-time macros). Note that guard expressions borrow TOML-like syntax for single-line conditions, gates are loosely inspired by Rust's derive macros, and support multi-line guarded blocks using `{ ... }` braces. -->

# Language Reference

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

## Command reference

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
| [`COPY_GIT`](#copy_git) | `COPY_GIT [--include-dirty] <rev> <src> <dst>` |
| [`WITH_IO`](#with_io) | `WITH_IO [bindings] [command \| { block }]` |
| [`HASH_SHA256`](#hash_sha256) | `HASH_SHA256 <path>` |
| [`SYMLINK`](#symlink) | `SYMLINK <from> <to>` |
| [`MKDIR`](#mkdir) | `MKDIR <path>` |
| [`LS`](#ls) | `LS [<path>]` |
| [`CWD`](#cwd) | `CWD` |
| [`READ`](#read) | `READ [<path>]` |
| [`WRITE`](#write) | `WRITE <path> [<contents>]` |
| [`ASSERT_FILE`](#assert_file) | `ASSERT_FILE [--hash <sha256>] <path> [<expected>]` |
| [`ASSERT_DIR`](#assert_dir) | `ASSERT_DIR <path>` |
| [`ASSERT_ABSENT`](#assert_absent) | `ASSERT_ABSENT <path>` |
| [`ASSERT_STDOUT`](#assert_stdout) | `ASSERT_STDOUT <substring>` |
| [`EXIT`](#exit) | `EXIT <code>` |

### WORKDIR

Sets the current working directory for subsequent commands. Relative paths resolve against the current directory; parent directories used by later writes are created on demand. Assertions are also working-directory relative:

```oxdock
// Relative WORKDIR; later relative paths resolve against it.
WORKDIR project/src

// Parent directories are created on demand by WRITE.
WRITE generated.txt generated-under-workdir
ASSERT_FILE generated.txt generated-under-workdir
```

### WORKSPACE

Switches the filesystem root between the isolated scratch workspace (`SNAPSHOT`, the default surface scripts operate on) and the build context (`LOCAL`, the tree OxDock was invoked against). Switching also resets the working directory to the new root.

```oxdock
// LOCAL: operate directly on the build context tree.
WORKSPACE LOCAL
WRITE context-note.txt written-into-build-context
ASSERT_FILE context-note.txt written-into-build-context

// SNAPSHOT: back to the scratch workspace — the context file is
// not visible here because the roots differ.
WORKSPACE SNAPSHOT
ASSERT_ABSENT context-note.txt

WRITE workspace-note.txt written-into-workspace
ASSERT_FILE workspace-note.txt written-into-workspace
```

### ENV

Defines or overrides a script environment variable. Values support templates and quoting. Later `ENV` commands override earlier ones, mirroring how Docker's `ENV` overrides `--env` flags.

```oxdock
ENV APP_MODE=production

// Guards read script variables set by ENV.
[env:APP_MODE==production] ECHO running-in-production
[env:APP_MODE!=production] ECHO running-in-something-else
ASSERT_STDOUT running-in-production
```

### ECHO

Writes a message (with trailing newline) to standard output. Messages may mix quoted and unquoted fragments and support templates.

```oxdock
ECHO plain-message-with-spaces
ASSERT_STDOUT plain-message-with-spaces
```

### RUN

Executes a command string through the host shell. This is the one intentionally platform-specific command: use platform guards to provide per-OS invocations when needed. Child stdout/stderr stream to the script's configured outputs, and a non-zero exit fails the script. Set `OXDOCK_INHERIT_STDOUT=1` (see [ENV_CONTRACTS.md](./ENV_CONTRACTS.md)) to force streaming straight to the terminal instead of any capture.

```oxdock
// Host shell differs per OS: pick the invocation with guards.
[unix] RUN echo native-unix-shell
[windows] RUN cmd /c echo native-windows-shell

// Child output streams into the script's stdout, so it is assertable.
[unix] ASSERT_STDOUT native-unix-shell
[windows] ASSERT_STDOUT native-windows-shell
```

### RUN_BG

Like `RUN`, but spawns the command in the background and continues the script. Lifecycle rules: if any background child finishes early with a non-zero status, the script fails and remaining children are killed; otherwise at script end the first child is awaited to completion and the remainder are killed. Background children started before an `EXIT` are killed before unwinding.

```oxdock
// Spawn a slow child; the script does NOT wait for it here.
[unix] RUN_BG sleep 1
[windows] RUN_BG ping -n 2 127.0.0.1

// Mainline continues immediately. At script end the FIRST child
// is awaited to completion and any remaining children are killed.
ECHO mainline-continues-immediately
ASSERT_STDOUT mainline-continues-immediately
```

### COPY

Copies a file or directory into the workspace. Destinations always resolve within the contained workspace; parent directories are created on demand. Source resolution differs by mode:

- Plain `COPY <from> <to>` resolves `<from>` against the **build context** (the tree OxDock was invoked against), regardless of the current working directory.
- `COPY --from-current-workspace <from> <to>` resolves `<from>` against the configured **workspace root** instead. In the shipped CLI and macro runtimes the workspace root is pinned to the invocation directory, so the two forms currently coincide there; the flag matters for embedders that install a filesystem layer whose workspace root differs from its build context.

The examples below opt into unified roots via fence metadata (`roots:unified`) so source resolution is observable in self-contained snippets. Note that sources never follow the current working directory — only destinations do:

```oxdock roots:unified
// Seed a file at the (unified) build-context root.
WRITE context-file.txt copied-by-default-resolution
MKDIR app

// Default form: the SOURCE resolves against the build context,
// regardless of the current working directory.
COPY context-file.txt app/local-copy.txt
ASSERT_FILE app/local-copy.txt copied-by-default-resolution
```

```oxdock roots:unified
WRITE root-file.txt resolved-from-workspace-root
WORKDIR app

// Flag form: the source resolves from the workspace ROOT — note it
// still ignores the current working directory ("app").
COPY --from-current-workspace root-file.txt duplicated.txt
ASSERT_FILE duplicated.txt resolved-from-workspace-root
```

### COPY_GIT

Copies a file or tree out of a Git revision without checking anything out. `<rev>` is any revision spec `git` understands, `<src>` is a path relative to the build context, and `<dst>` lands in the workspace. Git plumbing runs against the build context itself, so it must be (or live inside) a Git repository: files are fetched with `git show <rev>:<src>`; trees use `ls-tree` + `show` extraction. With `--include-dirty`, current working-tree contents overlay the copied result. Source paths are containment-checked exactly like `COPY`; the destination must stay inside the allowed workspace. Requires the `git` CLI on the host.

A note on the self-provisioning example below: `git -c key=value` overrides apply to that single command invocation only — no git configuration is ever modified. If you adapt the provisioning steps for a real repository, drop them so commits use your normal identity.

```oxdock roots:unified
// Everything below happens inside a disposable unified-root
// workspace — no repository of yours is touched.
RUN git init -q .

WRITE tracked.txt recovered-from-git-history

// Stage and commit the file. The -c flags override the committer
// identity for THIS command only; git configuration (global,
// system, or repo-level) is never read-modified or written.
RUN git add tracked.txt
RUN git -c user.name=oxdock-docs -c user.email=docs@oxdock.invalid commit -qm init

// Recover the committed blob from history into the workspace.
COPY_GIT HEAD tracked.txt restored.txt
ASSERT_FILE restored.txt recovered-from-git-history
```

### WITH_IO

Reroutes the standard streams of the next command — or, in block form, of every enclosed command. Bindings map streams (`stdin`, `stdout`, `stderr`) either to themselves (`stdin`) or to named pipes (`stdout=pipe:name`). Pipe names registered by the host runtime tee structured output elsewhere; a name bound as output can later feed another command's `stdin`, connecting commands without touching the terminal. Nested blocks stack defaults, inline bindings override inherited ones for their command only, and closing a block restores the previous wiring.

```oxdock
// Route this ECHO's stdout into a named pipe instead of the terminal.
WITH_IO [stdout=pipe:notes] ECHO routed-into-pipe

// Bind the same name as stdin and capture it into a file. The hash
// form is used because ECHO appends a trailing newline.
WITH_IO [stdin=pipe:notes] WRITE notes-captured.txt
ASSERT_FILE --hash 8f0cedf92cb5465ccf6c63d544f6cde9f9356d83c6036463ecdb0acf22f4ccd5 notes-captured.txt

// Without a binding, output goes to the terminal as usual.
ECHO printed-to-terminal
ASSERT_STDOUT printed-to-terminal
```

Bare stream bindings expose the script's own stdin to the inner command:

```oxdock stdin:streamed-through-stdin
// stdin flows through untouched; the runner feeds it via fence metadata.
WITH_IO [stdin] READ
ASSERT_STDOUT streamed-through-stdin
```

Pipes connect commands to each other; relaying into `WRITE` makes the handoff verifiable byte-for-byte:

```oxdock
WITH_IO [stdout=pipe:relay] ECHO relayed-content

// The same pipe name now feeds another command's stdin.
WITH_IO [stdin=pipe:relay] WRITE relayed.txt
ASSERT_FILE --hash 8db60e0348412da251e028cdc7e4f6dd88b95571596225b63bca9a356f5ccf1d relayed.txt
```

Block form hoists bindings over every enclosed command:

```oxdock
// Every enclosed ECHO inherits stdout=pipe:block-log.
WITH_IO [stdout=pipe:block-log] {
  ECHO blocked-one
  ECHO blocked-two
}

// Drain the captured output into a file for verification.
WITH_IO [stdin=pipe:block-log] WRITE block-captured.txt
ASSERT_FILE --hash bbbb7836492c68a5a7da2d8690dc9313f959ecc18b37ff09dd2c1866a88c049a block-captured.txt

// Closing the block restores the previous wiring.
ECHO outside-block-restores-terminal
ASSERT_STDOUT outside-block-restores-terminal
```

Defaults stack across nested blocks, an inline binding overrides the inherited one for its command only, and closing each block restores the previous wiring:

```oxdock
// Outer block routes plain enclosed commands to block_outer.
WITH_IO [stdout=pipe:block_outer] {
  ECHO outer-1

  // Inline bare [stdout] overrides the inherited binding — for this
  // one command only.
  WITH_IO [stdout] ECHO override-to-terminal

  // Nested block stacks on top; block_inner wins inside it.
  WITH_IO [stdout=pipe:block_inner] {
    ECHO inner-2
  }

  // Inner block closed: outer routing resumes for outer-3.
  ECHO outer-3
}

// Drain both pipes; each captured exactly its own commands' output.
WITH_IO [stdin=pipe:block_outer] WRITE outer-captured.txt
WITH_IO [stdin=pipe:block_inner] WRITE inner-captured.txt
ASSERT_FILE --hash 325b4603ad9aeb8be08236146d5b27eaba3858a752a4623c18d008b7343b72a7 outer-captured.txt
ASSERT_FILE --hash 400b5f3a2268115082559537ddb90ac533a82df612a931dec2114a902ee05ae3 inner-captured.txt
ASSERT_STDOUT override-to-terminal

// Outside both blocks: terminal wiring fully restored.
ECHO outside
ASSERT_STDOUT outside
```

### HASH_SHA256

Prints the SHA-256 digest (hexadecimal, with trailing newline) of a file — or of a directory tree, hashed recursively in sorted order with forward-slash relative paths, producing a stable fingerprint:

```oxdock
WRITE payload.txt stable-content

// The digest is deterministic: sha256("stable-content").
HASH_SHA256 payload.txt
ASSERT_STDOUT 08135c1b6349b0e4f894c36221952f0de00e6b4d82f80895abf359755e77103c
```

### SYMLINK

Creates `<to>` as a symlink pointing at `<from>`. Sources resolve like `COPY` sources (against the build context; see [COPY](#copy)), so the example below opts into unified roots. On Windows hosts where symlinks require elevated permissions, the operation transparently falls back to copying so scripts remain functional:

```oxdock roots:unified
WRITE original.txt reachable-through-link

// link.txt references original.txt — or copies it on Windows hosts
// where symlinks need elevated permissions.
SYMLINK original.txt link.txt
READ link.txt
ASSERT_STDOUT reachable-through-link
```

### MKDIR

Creates a directory and any missing parents (`create_dir_all` semantics):

```oxdock
// Creates every missing parent, like create_dir_all.
MKDIR deeply/nested/tree
ASSERT_DIR deeply/nested/tree
```

### LS

Lists a directory's entries (or the current directory when omitted) sorted by name, preceded by a header line naming the directory:

```oxdock
MKDIR inventory
WRITE inventory/alpha.txt first
WRITE inventory/beta.txt second

// Prints "<dir>:" then the entries, sorted by name.
LS inventory
ASSERT_STDOUT alpha.txt
ASSERT_STDOUT beta.txt
```

### CWD

Prints the canonical physical path of the current working directory:

```oxdock
WORKDIR level-one/level-two

// Prints the canonical physical path; assert the stable suffix
// rather than the machine-specific prefix.
CWD
ASSERT_STDOUT level-two
```

### READ

Prints a file's raw bytes to standard output; without an argument, echoes the script's stdin:

```oxdock
WRITE note.txt file-read-back

// Raw bytes in, raw bytes out — no newline is appended.
READ note.txt
ASSERT_STDOUT file-read-back
```

### WRITE

Writes contents to a file, creating parent directories. Without a contents argument it consumes the script's stdin instead (combine with `WITH_IO [stdin...]`):

```oxdock stdin:stdin-captured-body
// No contents argument: the body comes from the script's stdin,
// which the runner feeds via the fence metadata above.
WITH_IO [stdin] WRITE captured.txt
ASSERT_FILE captured.txt stdin-captured-body
```

### ASSERT_FILE

Verifies that a workspace file exists and optionally matches expected content. The two-argument form compares exact bytes against the expanded message; `--hash` instead compares the file's SHA-256 digest (useful for content with trailing newlines or non-text bytes):

```oxdock
WRITE payload.bin stable-content

// Exact-byte comparison against the expanded message.
ASSERT_FILE payload.bin stable-content

// Digest comparison: handy for trailing newlines or binary bytes.
ASSERT_FILE --hash 08135c1b6349b0e4f894c36221952f0de00e6b4d82f80895abf359755e77103c payload.bin
```

A failed assertion stops the script with a `step N: ASSERT_...` error describing exactly what differed.

### ASSERT_DIR

Verifies that a directory exists at the given workspace path:

```oxdock
MKDIR dist/assets

// Fails the script if the directory is missing.
ASSERT_DIR dist/assets
```

### ASSERT_ABSENT

Verifies that no file or directory exists at the given workspace path — useful for asserting cleanup ran or a gated branch never executed.

Guards attach only to the command that immediately follows them. In the script below, the chained multi-line guard gates off just the first `WRITE`; because neither variable exists, that write is skipped and the artifact never comes into existence — the assertion confirms it, it does not remove anything:

```oxdock
// Neither variable exists, so this chained guard skips ONLY the
// next command: the WRITE below never runs.
[
  env:RELEASE_SIGNING_KEY,
  env:ALSO_UNDEFINED
]
WRITE signed-artifact.txt signed-content
ASSERT_ABSENT signed-artifact.txt

// Commands after the gated one execute normally, unguarded.
WRITE fallback-artifact.txt fallback-content
ASSERT_FILE fallback-artifact.txt fallback-content
```

### ASSERT_STDOUT

Verifies that the given substring was emitted to the script's stdout sink — covering interpreter output (`ECHO`, `LS`, `READ`, ...) as well as output streamed from `RUN` children. Matching is substring-based, so absolute paths can be asserted by a stable suffix:

```oxdock
// Interpreter output (ECHO) ...
ECHO build-complete

// ... and output streamed from RUN children both reach the log.
RUN echo artifact-built-ok
ASSERT_STDOUT build-complete
ASSERT_STDOUT artifact-built-ok
```

### EXIT

Stops the script: background children are killed first, then execution fails with `EXIT requested with code N`. The CLI surfaces this as a failed run (the requested code travels in the error, while the process itself exits non-zero); the build-time macro treats it as a compilation failure. The example below declares its expected failure through fence metadata so the documentation harness can execute it:

```oxdock expect_error:"EXIT requested with code 42"
// Teardown order: background children are killed before the error
// surfaces.
WRITE teardown-order.txt background-children-killed-first
ASSERT_FILE teardown-order.txt background-children-killed-first

// Fails the script with "EXIT requested with code 42"; nothing
// after this line executes.
EXIT 42
```

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
- **Compile-time parity:** a [build-time fixture](./crates/oxdock-logic-tests/fixtures/integration/buildtime_macros/assert_verification/) runs this README's quick-start script through `embed!`, assertions included.
- **Real-binary check:** the quick start is additionally executed through the actual `oxdock` binary exactly as documented (`--script Oxfile`).
- **Doctest execution:** the Rust quick start is wired into [`crates/oxdock-doc-tests`](./crates/oxdock-doc-tests/) and compiled *and* run by `cargo test --doc` on every CI OS.
- **Reference integrity:** every relative Markdown link target and every repo path referenced from a ```` ```bash ```` fence must exist.

Snippets contain nothing but OxDock — copy any of them straight into an `Oxfile` or an `embed!` macro. Runner-specific configuration lives in the fence info-string, which Markdown renders as inert metadata:

```text
```oxdock                                    plain snippet, must parse and run clean
```oxdock env:KEY=value                      inject an environment value (visible to INHERIT_ENV/guards)
```oxdock stdin:text                         feed the snippet's stdin
```oxdock roots:unified                      run with workspace root == build context (COPY/COPY_GIT demos)
```oxdock expect_error:"message substring"   snippet must fail with this text in its error
```

Everything else you see inside the fences — including the `ASSERT_*` commands — is part of the language itself and executes identically in your own pipelines.

If you change the language, update this reference in the same commit — CI will hold you to it.

## Environment variable contracts

Environment variables understood by the toolchain (workspace roots, caching fingerprints, IDE integrations) are specified in [ENV_CONTRACTS.md](./ENV_CONTRACTS.md).

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
