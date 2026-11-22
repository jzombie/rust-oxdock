# Export Consolidation (#2 in this thread)

## Objective
Collapse the three parallel export lists (`func_exports` / `type_exports` /
`const_exports`) behind a single `Exports` owner, then unify the
duplicate-export rule to one base-name test for all kinds. Same pattern as
the AST twin collapse: structural consolidation first (no behavior change),
behavior change second (one rule flip inside the single owner).

## Inventory (sites holding the three lists)

- `oxdock-parser/src/parser.rs`: `ParseCtx` (3 `RefCell<Vec<String>>`, ~2631),
  `ScriptWithExports` (~9901), `ParsedScript` (~10051), `parse_export_at`
  duplicate checks (~9151 func, ~9182 type, ~9211 const), `parse_with_ctx`
  construction (~9893), `validate_exports` call (~9756).
- `oxdock-parser/src/commands.rs`: `validate_exports(ctx, func, type, const,
  top, top_types, top_consts, ...)` (~6119).
- `oxdock-core/src/exec/state.rs`: `ModuleRecord` (3 vecs, ~288;
  `entry_record` constructor).
- `oxdock-core/src/exec/import.rs`: `ClosureFile` (3 vecs, ~1147), harvest
  sites (~431, ~979).
- `oxdock-core/src/exec/mcp.rs`: serving struct + `func_exports()` accessor
  (~44, ~182); `oxdock-mcp/src/server.rs:840` emptiness check.
- `oxdock-core/src/exec/mod.rs:585`: `validate_closure_for_run` collects
  `type_exports` across closure files.
- `oxdock-core/src/exec/native.rs`: `ModuleGraphView::const_exports`,
  `describe_tools(func_exports: &[String])` (~1876).
- Tests asserting on `parsed.func_exports` / `.type_exports`
  (`oxdock-parser/tests/integration/import.rs:342,370,377,870,1183`,
  `oxdock-mcp/tests/session.rs:301`).

## Design (single owner)

- `ExportKind::{Func, Type, Const}` + `Exports { funcs, types, consts }` in
  `oxdock-parser` next to `ParsedScript` (core already depends on parser).
  Struct-of-vecs preserves per-kind source order exactly.
- `Exports::insert(kind, name) -> Result<(), String>` owns the duplicate
  rule; `parse_export_at` calls it per name. `Exports::contains_*` /
  `is_empty` accessors serve harvest, MCP, and server call sites.
- `validate_exports(ctx, &exports, top, top_types, top_consts, ...)` takes
  one struct; the three validation loops stay kind-specific (different
  membership gates), only the threading collapses.
- Error wording byte-identical: `duplicate EXPORT \`{name}\``.

## Sequence

1. **Slice 1: structural (no behavior change).** Introduce `ExportKind` +
   `Exports` with `insert` implementing the *current* asymmetric rule
   (same-kind spelling for func/type, base test const-involved). Migrate all
   sites above. Func/type `parse_export_at` blocks collapse into one
   kind-parameterized call. Suites green.
2. **Slice 2: uniform base rule (behavior change).** Flip `Exports::insert`
   to one base-name test for all kinds ("one base name rides exactly one
   export kind", extending the existing const-block comment). Previously
   valid `EXPORT FOO` + `EXPORT LIB::FOO` (same base, distinct spelling)
   now fails as `duplicate EXPORT`. Add regression tests: same-base
   distinct-spelling func pair rejected, type pair rejected, mixed-kind
   same-base rejected, distinct bases still pass, `TYPE` re-export with
   same-named local *func* (unexported) still passes. Full workspace suite
   measures the blast radius.

## Verification per slice
- `cargo test -p oxdock-parser` (244+296)
- `cargo test -p oxdock-core --lib` (399) + `--test integration` (620)
- `cargo test -p oxdock-logic-tests --test commands_harness --features slow-integration` (113)
- `cargo test -p oxdock-mcp --test session` (26)
- `cargo clippy --workspace --all-targets` (0)
- `cargo run -p oxdock-render` clean
- Slice 2 only: full `cargo test --workspace --all-features --lib --bins --tests --examples -- --include-ignored`

## Status
- [x] Slice 1: Exports struct
- [x] Slice 2: uniform base rule
- [x] Slice 3 (review follow-up): qualified `EXPORT TYPE ALIAS::NAME` rejected at parse (unservable downstream: MCP bare-only, closure validation bare-only, harvests bare-only); bare behavior unchanged
- [x] Slice 4 (piecemeal elimination): `gate_harvested_name` in `import.rs` owns harvest validity (export gate then defined check, exact historical messages) for all 5 call sites — runtime `import_funcs` func/type, runtime own-harvest, static `merge_harvest`, static `collect_site` func/type (order unified to export-first); dead MCP qualified-`TYPE` arm deleted (parse owns it); order-pinning regression test
