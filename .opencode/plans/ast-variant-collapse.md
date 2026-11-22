# AST Variant Collapse (#1) + ENTRY_UNIT-as-ModuleRecord (#4)

## Objective
Eliminate every remaining LET-vs-CONST twin variant behind a single owner, same as the
`ModuleGraphView` / `ForwardError` seam. No two match arms restate the same branch logic.
Compiler-guided (exhaustive matches fail to compile until updated). Full suites green after
each slice.

## Inventory (twins to collapse)

### A. `MathOp` (`crates/oxdock-parser/src/ast.rs:628`)
- `LoadVar(String)` vs `LoadConst(String)`
- `LoadKeyPath { base, keys }` vs `LoadConstKeyPath { base, keys }`
- `Inspect(String)` vs `InspectConst(String)`
- `LoadConstRef { alias, name }` stays (qualified shape, no twin).
- Consumers: `exec/args.rs` RPN evaluator (~1471, ~1492, ~1536), `Expr::CompiledMath` lowering in `parser.rs`, fuzz parity `fuzz_parity.rs`, `ast_runner.rs`.

### B. `Expr` (`ast.rs:659`)
- `Var(String)` vs `Const(String)`
- `KeyPath { base, keys }` vs `ConstKeyPath { base, keys }`
- `Inspect(String)` vs `InspectConst(String)`
- Consumers: `walk`/`walk_mut` (already paired arms — collapse to one arm with accessor), `exec/args.rs` eval (~472, ~486, ~600), `static_check.rs` typing (~1666, ~2397), `parser.rs` lowering, `macro_input.rs`, `oxdock-macros`, `track.rs`, `ast_runner.rs`.

### C. `StepKind` (`commands.rs:1357 declare_commands! structural`)
- `Assign { var, decl_type, expr }` vs `Const { var, decl_type, expr }`
- `AssignCapture { var, decl_type, cmd }` vs `ConstCapture { var, decl_type, cmd }`
- `AssignAsync { var, decl_type, body }` vs `ConstAsync { var, decl_type, body }`
- `AwaitCapture { out_var, out_type, task_var, task_is_const }` vs `ConstAwaitCapture { ... }` (fields already identical — pure duplicate)
- `Await { var, is_const }` / `Cancel { var, is_const }` are the DONE pattern: single variant + `is_const: bool`.
- Consumers: walkers (`walk_exprs`, `walk_raw_strings`, child-kinds, bodies — all paired arms), `Display` (~5496+), `step_kind_name` (~7725), `parser.rs` lowering, `exec/steps.rs` dispatch (×2: live + dry-run), `exec/static_check.rs` decl typing (~539+), `exec/import.rs` harvest, `exec/handlers.rs`, `pipeline.rs`, `track.rs`, fuzzers, macro corpus.

### D. `ENTRY_UNIT`-as-`ModuleRecord` (#4, separate migration after A–C)
- `exec/import.rs:1656 ENTRY_UNIT: &str = "entry"` + `state.rs:306 ModuleRecord` keyed by display path.
- Entry currently special-cased at ~8 sites (`import.rs:1805`, `static_check.rs:891,944,999,2449,3280,4888`, `handlers.rs:2932`).
- Goal: entry becomes one `ModuleRecord` under key `ENTRY_UNIT`; `unwrap_or(ENTRY_UNIT)` fallbacks deleted; lookups go through one function.
- No compiler net (same types both branches) — hand-verified lookup parity, one call site per slice.

## Non-goals
- No behavior change: `Display` output byte-identical (reparse-stable), error wording unchanged.
- No `ExportItem` struct work here (done/declined separately).
- No blanket func/type base-name dedup (declined: spelling-addressed serving).

## Design (single owner per collapse)
- Follow the existing `is_const: bool` precedent (`Await`, `Cancel`):
  - `StepKind::Assign { var, decl_type, expr, is_const: bool }` absorbs `Const`; same for `AssignCapture`, `AssignAsync`.
  - `AwaitCapture` absorbs `ConstAwaitCapture` (already same fields; keep `task_is_const`, add `is_const` for the out binding — verify field semantics first).
  - `Expr::Var { name: String, is_const: bool }` OR `Expr::Binding { name, is_const }` — name TBD at implementation; same for `KeyPath { base, keys, is_const }`, `Inspect { name, is_const }`.
  - `MathOp::Load { name: String, is_const: bool }`, `LoadKeyPath { base, keys, is_const }`, `Inspect { name, is_const }`.
- One accessor per collapsed enum, defined once next to the enum:
  - `StepKind::binding_kind(&self) -> Option<BindingKind>` or `is_const(&self) -> Option<bool>` (None = not a binding step).
  - `Expr::is_const_binding(&self) -> Option<bool>`, `MathOp::is_const_load(&self) -> Option<bool>`.
  - Callers match on the accessor, never restate `Assign-vs-Const` arms.
- Walkers/macros: `define_expr_walkers!` / `define_raw_string_walkers!` arm lists shrink (paired arms become one); `declare_commands!` structural list shrinks.
- `Display` + `step_kind_name`: branch on `is_const` internally, output strings unchanged.

## Sequence (one slice per commit, suites after each)
1. **MathOp loads** (smallest, flat enum, ~3 sites): `Load{is_const}`, `LoadKeyPath{is_const}`, `Inspect{is_const}`. Files: `ast.rs`, `args.rs` evaluator, `parser.rs` lowering, `ast_runner.rs`, fuzzers.
2. **Expr bindings**: `Var/Const`, `KeyPath/ConstKeyPath`, `Inspect/InspectConst`. Files: `ast.rs` walk fns, `args.rs`, `static_check.rs`, `parser.rs`, `macro_input.rs`, `oxdock-macros`, `track.rs`.
3. **StepKind assigns** (largest): `Assign`, `AssignCapture`, `AssignAsync` + flag; then `AwaitCapture`/`ConstAwaitCapture` merge. Files: `commands.rs` (decl + Display + walkers + step_kind_name), `parser.rs`, `steps.rs` (both dispatch sites), `static_check.rs`, `import.rs`, `handlers.rs`, `pipeline.rs`, `track.rs`, tests.
4. **ENTRY_UNIT-as-ModuleRecord**: introduce `ModuleRecord` for entry, delete fallbacks one site at a time with lookup-parity tests.

## Verification per slice
- `cargo test -p oxdock-core --test integration` (620)
- `cargo test -p oxdock-core --lib` (399)
- `cargo test -p oxdock-parser` (244+296)
- `cargo test -p oxdock-mcp --test session` (26)
- `cargo test -p oxdock-logic-tests --test docs_conformance` (8)
- `cargo clippy -p oxdock-core -p oxdock-parser --all-targets` (0)
- `cargo run -p oxdock-render` clean (Display byte-stability)

## Status
- [x] Slice 1: MathOp loads (collapsed `Load{is_const}`, `LoadKeyPath{is_const}`, `Inspect{is_const}`; deps: ast, commands lowering, args eval, static_check typer, macros emitter, arithmetic tests; suites green)
- [x] Slice 2: Expr bindings (collapsed `Var{is_const}`, `KeyPath{is_const}`, `Inspect{is_const}`; deps: ast walk/Display, commands lowering + 3 var-extract sites, args eval, static_check typing + template/placeholder constructors, command check_arg, macros emitter, track, lib/exec/integration/fuzz tests; suites green)
- [x] Slice 3: StepKind assigns (collapsed `Assign{is_const}`, `AssignCapture{is_const}`, `AssignAsync{is_const}`, `AwaitCapture{out is_const}`; existing `binding_kind` now flips the flag; merged twin dispatch wrappers in handlers/steps/lib/pipeline/mod; Display/metadata_name branch on flag with byte-identical output; fuzz + corpus + exec/integration/macro tests updated; suites green)
- [x] Slice 4: ENTRY_UNIT-as-ModuleRecord (entry is one `ModuleRecord` under `ENTRY_UNIT`; `unit_key` owns the fallback; `unit_top_funcs` owns frame reads; `callee_file_ctx` is one lookup)
  - [x] Amend: `validate_closure_for_run` mutates the entry record in place (keeps live `init` + sealed jails)
  - [x] Amend: entry prelude (`envs`, `inherit_names`, `consts`) consolidated into `ENTRY_UNIT` `ModuleInit` via `entry_record`/`entry_seed` single owners; `entry_*` fields deleted
  - [x] Coverage: `coverage.toml` twins removed (`Const`, `ConstCapture`, `ConstAsync`, `ConstAwaitCapture`); const cases folded into surviving variants (`Assign`, `AssignCapture`, `AssignAsync`, `AwaitCapture`)
