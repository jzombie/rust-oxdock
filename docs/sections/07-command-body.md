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


