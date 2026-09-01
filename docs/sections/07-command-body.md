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

```oxdock
WRITE input.txt "streamed-through-stdin"
WITH_IO [stdout=pipe:data] READ input.txt
WITH_IO [stdin=pipe:data] READ
ASSERT_STDOUT "streamed-through-stdin"
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

### EXPAND

Reads a template file (or stdin), expands `{{ env:KEY }}` placeholders using the script environment, and outputs the result to standard output. Explicit `KEY=val` arguments override environment variables.

Like `READ`, `EXPAND` outputs to stdout — use `WITH_IO` to pipe the expanded result to `WRITE` or `APPEND`.

Templates support the same `{{ env:KEY }}` syntax as all other commands. Unprefixed `{{ KEY }}` expands to empty. Keys without `env:` prefix only check explicit overrides, not the script environment.

**File mode** — expand an existing template file:

```oxdock
// Create a template file with literal {{ env:KEY }} tags (\{{ escapes expansion)
WRITE template.md "Hello, \{{ env:NAME }}!"

// Expand the template with an explicit override
EXPAND template.md NAME="Alice"
ASSERT_STDOUT "Hello, Alice!"
```

**Stdin mode** — omit the path to read from stdin instead of a file:

```oxdock
WRITE tmpl.txt "Hello, \{{ env:NAME }}!"
WITH_IO [stdout=pipe:raw_tmpl] READ tmpl.txt
WITH_IO [stdin=pipe:raw_tmpl] EXPAND NAME="Bob"
ASSERT_STDOUT "Hello, Bob!"
```

### WRITE

Writes contents to a file, **replacing** any existing contents (it does not append). Creates parent directories on demand. Without a contents argument it consumes the script's stdin instead (combine with `WITH_IO [stdin...]`):

```oxdock
WRITE input.txt "captured body"
WITH_IO [stdout=pipe:data] READ input.txt
WITH_IO [stdin=pipe:data] WRITE captured.txt
ASSERT_FILE captured.txt "captured body"
```

### APPEND

Appends contents to a file, creating parent directories if needed. Unlike `WRITE` (which overwrites), `APPEND` preserves existing file contents — ideal for log files, GitHub Actions environment files, and step summaries:

```oxdock
APPEND dist/log.txt "build started"
APPEND dist/log.txt "build finished"
ASSERT_FILE dist/log.txt "build startedbuild finished"
```

Without a contents argument it consumes stdin (combine with `WITH_IO [stdin...]`):

```oxdock
WRITE input.txt "line-from-stdin"
WITH_IO [stdout=pipe:data] READ input.txt
WITH_IO [stdin=pipe:data] APPEND dist/stdin-log.txt
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
