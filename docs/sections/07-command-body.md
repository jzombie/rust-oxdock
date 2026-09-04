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

### WORKDIR

Change the working directory.

**Syntax:** `WORKDIR <path>`

Sets the current working directory.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `string` | yes | Directory to change to |

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
| `target` | `SNAPSHOT|LOCAL` | yes | Target root |

**Examples:**

**Example: switch roots**

```oxdock
WORKSPACE LOCAL
```


### ENV

Set an environment variable.

**Syntax:** `ENV KEY=value`

Inserts or updates an env var.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `assignment` | `KEY=value` | yes | KEY=value pair |

**Examples:**

**Example: set env**

```oxdock
ENV APP_MODE=production
```


### INHERIT_ENV

Inherit env vars from host.

**Syntax:** `INHERIT_ENV <key>...`

Imports host env vars.


### ECHO

Print to stdout.

**Syntax:** `ECHO <message>`

Outputs message to stdout.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `message` | `string` | yes | Text |

**Output:** Stdout

**Examples:**

**Example: echo**

```oxdock
ECHO build-complete
```


### RUN

Execute shell command.

**Syntax:** `RUN <command...>`

Runs command in cwd.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `command` | `string...` | yes | Command |

**Examples:**

**Example: run**

```oxdock
RUN echo hello
```


### RUN_BG

Run in background.

**Syntax:** `RUN_BG <command...>`

Like RUN but background.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `command` | `string...` | yes | Command |

**Examples:**

**Example: bg**

```oxdock
RUN_BG sleep 1
```


### COPY

Copy file into workspace.

**Syntax:** `COPY [--from-current-workspace] <from> <to>`

Copies from host.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `from` | `path` | yes | Source |
| `to` | `path` | yes | Dest |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--from-current-workspace` | Flag | From workspace root |

**Examples:**

**Example: copy**

```oxdock roots:unified
WRITE src.txt content
COPY src.txt dst.txt
ASSERT_FILE dst.txt content
```


### COPY_GIT

Copy from git revision.

**Syntax:** `COPY_GIT [--include-dirty] <rev> <src> <dst>`

Checkout and copy.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `rev` | `string` | yes | Rev |
| `src` | `path` | yes | Src |
| `dst` | `path` | yes | Dst |

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
| `from` | `path` | yes | Target |
| `to` | `path` | yes | Link |

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
| `path` | `path` | yes | Dir path |

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
| `path` | `path` | no | Dir |

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
| `path` | `path` | no | File |

**Output:** Stdout

**Examples:**

**Example: read**

```oxdock
WRITE note.txt "hello"
READ note.txt
```


### WRITE

Write to file.

**Syntax:** `WRITE <path> [<contents>]`

Writes contents.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `path` | yes | File |
| `contents` | `string` | no | Content |

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
| `path` | `path` | yes | File |
| `contents` | `string` | no | Content |

**Examples:**

**Example: append**

```oxdock
WRITE log.txt line1
APPEND log.txt line2
ASSERT_FILE log.txt line1line2
```


### EXPAND

Expand templates.

**Syntax:** `EXPAND [<path>] [<KEY=val> ...]`

Expands placeholders.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `path` | no | Template |

**Output:** Stdout

**Examples:**

**Example: expand**

```oxdock
ENV NAME="Alice"
WRITE template.md "Hello {{ env:NAME }}!"
EXPAND template.md
ASSERT_STDOUT "Hello Alice!"
```


### ASSERT_FILE

Assert file exists.

**Syntax:** `ASSERT_FILE [--hash <sha256>] <path> [<expected>]`

Verifies file.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `path` | yes | File |
| `expected` | `string` | no | Expected |

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


### ASSERT_DIR

Assert dir exists.

**Syntax:** `ASSERT_DIR <path>`

Verifies dir.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `path` | yes | Dir |

**Examples:**

**Example: assert dir**

```oxdock
MKDIR dist/assets
ASSERT_DIR dist/assets
```


### ASSERT_ABSENT

Assert path absent.

**Syntax:** `ASSERT_ABSENT <path>`

Verifies absence.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `path` | yes | Path |

**Examples:**

**Example: assert absent**

```oxdock
ASSERT_ABSENT missing.txt
```


### ASSERT_STDOUT

Assert stdout contains.

**Syntax:** `ASSERT_STDOUT <substring>`

Verifies stdout.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `substring` | `string` | yes | Substring |

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
| `path` | `path` | yes | File |

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

Terminates.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `code` | `int` | yes | Code |

**Examples:**

**Example: exit**

```oxdock expect_error:"EXIT requested with code 0"
EXIT 0
```
