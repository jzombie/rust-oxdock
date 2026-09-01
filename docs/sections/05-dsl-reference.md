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

Instructions guarded by a `{ ... }` block run inside a scope: changes to `WORKDIR`, `WORKSPACE`, or `ENV` revert once the block exits, so temporary setup cannot leak outward. Blocks nest.

```oxdock
ENV SCOPE_MARKER=armed

// Guarded block: the WORKDIR change below is scoped and reverts
// when the block closes.
[eq(env:SCOPE_MARKER, armed)] {
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
