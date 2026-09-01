OxDock guard brackets `[...]` provide step-level execution filtering based on host platform properties and sequential process environment state (`ctx.env`). They evaluate sequentially at the exact moment the engine reaches the guarded step in the execution pipeline.

**Supported Guard Syntax**

| Guard Category | Valid Examples | Explanation |
| --- | --- | --- |
| **Platform** | `[unix]`, `[windows]`, `[macos]`, `[linux]` | Literals matching the target operating system. |
| **Env Existence** | `[env:KEY]`, `[#rust_var]` | True if key exists in process `ctx.env` or compile-time macro context. |
| **Env Equality** | `[env:KEY == val]`, `[env:#k == #v]` | String comparison against the runtime process environment map. |
| **Negation** | `[not(unix)]`, `[not(env:KEY)]` | Inverts inner predicate truth value. |
| **Logical AND** | `[unix, env:KEY == val]` | Comma-separated list (all predicates must evaluate to true). |

**Invalid Syntax & Structural Constraints**

| Invalid Syntax | Reason For Failure | Correct Alternative |
| --- | --- | --- |
| `[$x]` / `[env:$x == 1]` | Script variables (`$var`) in `vars` are grammatically forbidden inside guard brackets. | Use `env:KEY` with `ENV` instructions or macro parameters. |
| `[#platform_var]` | Platform predicates require hardcoded OS identifier tokens. | `[unix]` or `[windows]` |
| `[#var == 1]` | Missing mandatory `env:` namespace prefix for environment variables. | `[env:#var == 1]` |
| `[READ path]` | Guards cannot perform I/O operations, function calls, or subcommands. | Execute sequential DSL steps. |

---

**Guard Execution Lifecycle**

* **Sequential Runtime Evaluation:** As the runner iterates through steps, step guards evaluate against the current state of `ctx.env`. Preceding steps that mutate process environment state (e.g., `ENV STAGE=prod`) immediately update `ctx.env`, enabling subsequent `[env:STAGE == "prod"]` guards to match.
* **Grammatical Namespace Segregation:** Guard brackets evaluate strictly against platform properties and the `env` table. They cannot query the script variable table (`vars`). Syntax containing `$var` is rejected at parse time by `oxdock-parser` PEG grammar rules to prevent declarative guards from morphing into imperative control-flow logic.

---

**Rule of Thumb**

* Use **`#rust_var`** inside guards when passing host-side Rust variables into the `oxdock!` macro at compile time.
* Use **`env:KEY`** inside guards to check process environment variables (reflecting both ambient host environment and prior `ENV` step mutations).
* Never use **`$var`** inside guards; script variable lookups are grammatically restricted to step payload arguments, template expansions (`EXPAND`), and explicit expression functions (`LOAD_TOML`).


----

`!windows` is a syntax leak—it introduces prefix operator syntax (`!`) into an otherwise function-based predicate model (`not(...)`, `or(...)`, `eq(...)`).

In a functional guard system, prefix operators create unnecessary parser branching and visual inconsistency.

**Inconsistent vs. Uniform Functional Syntax**

| Inconsistent Syntax | Uniform Functional Syntax | AST Mapping |
| --- | --- | --- |
| `!windows` | `not(windows)` | `Guard::Not(Box<Guard::Platform("windows")>)` |
| `or(env:A, env:B)` | `any(env:A, env:B)` | `Guard::Any(Vec<Guard>)` |
| `and(env:A, env:B)` | `all(env:A, env:B)` | `Guard::All(Vec<Guard>)` |
| `eq(env:FOO, bar)` | `eq(env:FOO, "bar")` | `Guard::Eq(String, String)` |

Purging `!` in favor of `not(...)` removes operator prefix handling entirely from `dsl.pest`. Every guard node becomes either a atom identifier (`linux`, `env:FOO`) or a function call (`func(...)`).
