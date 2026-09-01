**OxDock** is a Rust-based domain-specific language (DSL) and execution engine designed for hermetic workspace scripting, build orchestration, and file assertions.

---

**Crate Architecture**

* **`oxdock-parser`**: Uses PEG grammar rules to parse script text into AST nodes, command arguments, expressions, and control-flow statements.
* **`oxdock-core-macros`**: A dedicated proc-macro crate hosting `#[oxdock_registry]`. Parses command structs at compile time to build registry dispatch tables without creating circular crate dependencies.
* **`oxdock-core`**: The central runtime. Implements `CommandSpec` for built-in steps (`WORKDIR`, `RUN`, `COPY`, `WRITE`, etc.) and manages process state.
* **`oxdock-macros`**: Provides compile-time macros (`oxdock!`, `oxdock_embed!`) that parse and execute workspace assets during Rust build compilation.
* **`oxdock-process`**: Handles stream piping, process spawning, and variable template expansion (`StreamingExpand`).
* **`docs-gen`**: Generates CLI documentation (`README.md` and reference docs) directly from command metadata.

---

**What You Are Currently Building**

**1. Command Registry Extraction (`oxdock-core-macros`)**
Replacing the legacy declarative `define_pipeline!` macro in `oxdock-core` with a proc-macro attribute (`#[oxdock_registry]`). The macro inspects structs inside `pub mod commands` and auto-generates static dispatchers for:

* `lower_command`: Maps AST arguments to concrete `StepKind` enums.
* `execute_command`: Routes steps to their respective execution logic.
* `all_metadata()`: Collects schema metadata across all commands for `docs-gen`.

**2. Strict Zero-Silent-Omissions Variable Resolution (`oxdock-process`)**
Refactoring `crates/sys/oxdock-process/src/expand.rs` to eliminate silent empty-string fallbacks (`Ok("")`). Resolution across scripts and templates (`EXPAND`) must fail hard (`Err`) under the following conditions:

* Missing script variables (`{{ $undefined }}`) or host environment variables (`{{ env:MISSING }}`).
* Invalid object property lookups (`$data.missing_key`).
* Out-of-bounds array indices (`$list.999`).
* Missing explicit step overrides (`{{ OMITTED }}`).

**3. Test & Documentation Alignment**
Updating `CommandSpec` metadata definitions in `oxdock-core` to ensure runnable ````oxdock` example code blocks render with valid fence metadata (`roots:unified`, `expect_error`) and pass the `docs_conformance` integration test suite.

---

Winging a multi-crate proc-macro refactor risks reintroducing circular dependencies and inconsistent runtime error contracts.

Below is the formal technical specification for implementing `oxdock-core-macros` and enforcing strict variable resolution.

---

## 1. System Architecture & Crate Boundaries

The refactor establishes a strict one-way dependency chain to keep proc-macro expansion separated from runtime execution logic.

| Crate | Role | Allowed Dependencies |
| --- | --- | --- |
| `oxdock-parser` | Defines AST types (`Arg`, `Value`, `CommandMeta`) | None (Leaf Crate) |
| `oxdock-core-macros` | Attribute macro `#[oxdock_registry]` | `syn`, `quote`, `proc-macro2` |
| `oxdock-core` | Implements `CommandSpec` structs & executes steps | `oxdock-parser`, `oxdock-core-macros` |
| `oxdock-macros` | Build-time procedural macros (`oxdock!`) | `oxdock-core`, `oxdock-parser` |
| `oxdock-process` | Process execution, stream piping, string expansion | `oxdock-parser` |

---

## 2. Proc-Macro Contract (`oxdock-core-macros`)

### Input Requirement

The macro `#[oxdock_registry]` must be applied exclusively to a inline module (`pub mod commands { ... }`) containing struct implementations of `CommandSpec`.

### Emitted Token Stream

The proc-macro generates an inner module named `registry` containing static dynamic-dispatch tables:

```rust
pub mod registry {
    use super::*;
    use anyhow::{Result, bail};
    use oxdock_parser::{Arg, CommandMeta};

    // Static array containing trait objects for every struct defined in mod commands
    static COMMANDS: &[&dyn CommandSpec] = &[
        &WorkdirCmd,
        &RunCmd,
        // ... all discovered structs
    ];

    pub fn all_metadata() -> Vec<CommandMeta> {
        COMMANDS.iter().map(|cmd| cmd.metadata()).collect()
    }

    pub fn lower_command(keyword: &str, args: &[Arg]) -> Result<StepKind> {
        for cmd in COMMANDS {
            if cmd.keyword().eq_ignore_ascii_case(keyword) {
                return cmd.lower(args);
            }
        }
        bail!("unknown command keyword: '{keyword}'");
    }

    pub fn execute_command(step: &StepKind, ctx: &mut Context) -> Result<ExecOutcome> {
        for cmd in COMMANDS {
            if cmd.matches_step(step) {
                return cmd.execute(step, ctx);
            }
        }
        bail!("unhandled step variant: '{:?}'", step);
    }
}

```

---

## 3. Command Trait Specification (`CommandSpec`)

Every command struct inside `pub mod commands` must implement the following trait interface in `oxdock-core`:

```rust
pub trait CommandSpec: Send + Sync {
    /// Exact uppercase keyword matching script syntax (e.g., "WORKDIR")
    fn keyword(&self) -> &'static str;

    /// Complete schema metadata consumed by docs-gen
    fn metadata(&self) -> CommandMeta;

    /// Returns true if the StepKind enum variant matches this command
    fn matches_step(&self, step: &StepKind) -> bool;

    /// Lowers parsed AST arguments into a concrete StepKind
    fn lower(&self, args: &[Arg]) -> Result<StepKind>;

    /// Executes runtime I/O logic against process context
    fn execute(&self, step: &StepKind, ctx: &mut Context) -> Result<ExecOutcome>;
}

```

---

## 4. Strict Variable Expansion Contract (`oxdock-process`)

Template evaluation during `EXPAND` steps or string interpolation must follow a zero-silent-omission contract. `StreamingExpand` functions must return `anyhow::Result<String>` and fail immediately on unresolved keys.

```
Placeholder Syntax       Resolution Path                           Failure Condition
──────────────────────────────────────────────────────────────────────────────────────────────────────────
{{ $var }}               Lookup key "var" in `vars` map             Err if "var" is missing
{{ $var.field }}         Traverse key-path on "var" in `vars`       Err if "var" missing, key absent, or index OOB
{{ env:KEY }}            Lookup "KEY" in overrides, then `env`      Err if "KEY" missing from both maps
{{ KEY }}                Lookup "KEY" strictly in `overrides`       Err if "KEY" missing from step overrides

```

### Error Message Standards

* **Missing Script Variable:** `undefined script variable: '$var'`
* **Missing Environment Variable:** `undefined environment variable: 'KEY'`
* **Missing Map Property:** `property 'field' not found on object '$var'`
* **Out of Bounds Index:** `index N out of bounds for list '$var' (len: M)`
* **Primitive Property Traversal:** `cannot access property 'field' on primitive value of '$var'`
* **Missing Step Override:** `missing required step override argument: 'KEY'`
