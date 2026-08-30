OxDock guard brackets `[...]` serve a single purpose: **static plan pruning** based on host platform properties or pre-execution environment variables (`ExecIo`). They evaluate before any script steps run.

**Supported Guard Syntax**

| Guard Category | Valid Examples | Explanation |
| --- | --- | --- |
| **Platform** | `[unix]`, `[windows]`, `[macos]`, `[linux]` | Literals matching target OS. |
| **Env Existence** | `[env:KEY]`, `[#rust_var]` | True if key exists in `ExecIo` or macro context. |
| **Env Equality** | `[env:KEY == val]`, `[env:#k == #v]` | String comparison against `ExecIo` environment map. |
| **Negation** | `[not(unix)]`, `[not(env:KEY)]` | Inverts inner predicate truth value. |
| **Logical AND** | `[unix, env:KEY == val]` | Comma-separated list (all must be true). |

**Invalid Syntax & Common Misconceptions**

| Invalid Syntax | Reason For Failure | Correct Alternative |
| --- | --- | --- |
| `[$x]` / `[env:$x == 1]` | Runtime DSL variables (`$var`) created by `LET` do not exist at guard evaluation time. | Use host Rust branching before step construction. |
| `[#platform_var]` | Platform predicates require hardcoded OS identifier tokens. | `[unix]` or `[windows]` |
| `[#var == 1]` | Missing mandatory `env:` namespace prefix. | `[env:#var == 1]` |
| `[READ path]` | Guards cannot perform I/O operations, function calls, or subcommands. | Execute sequential DSL steps. |

**Rule of Thumb**

* Use **`#rust_var`** inside guards when passing host-side Rust variables into the `oxdock!` macro at compile time.
* Use **`env:KEY`** inside guards when reading environment keys loaded into `ExecIo` at application start.
* Never use **`$dsl_var`** inside guards; runtime script state requires step execution, not compile/plan-time guards.

## The Two-Phase Architecture

Phase 1: Static Plan Assembly (Compile & Setup)

Macro Expansion: The oxdock! macro converts DSL tokens directly into a static Vec<Step> array and substitutes #rust_vars at Rust compile time.

Guard Evaluation: Scope guards ([...]) evaluate against the environment map (ExecIo) to prune non-matching steps out of the plan before execution begins.

Phase 2: Sequential Step Execution (Runtime)

Linear Execution: The runner iterates through the pre-pruned Vec<Step> array.

I/O Engine: Steps execute their operations sequentially (spawning processes, reading globs, expanding templates, writing files).

This ahead-of-time design keeps script execution fast and deterministic with zero runtime parsing overhead, but it is precisely why scope guards [...] cannot react to dynamic runtime state (like $dsl_vars) generated mid-execution.
