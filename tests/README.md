Tests live across the workspace; prefer [oxdock-logic-tests](../crates/oxdock-logic-tests/) for cross-crate, workflow (API instructions, etc.), and regression tests.

## Strategy

- **Unit tests** live beside the code they cover (`src/` and `tests/` inside each crate).
- **Conformance suites** in [oxdock-logic-tests](../crates/oxdock-logic-tests/tests/) prove cross-crate behavior end to end: `docs_conformance` executes every `oxdock` fence in the READMEs, while `setup_action_conformance`, `smoke_script_conformance`, `release_script_conformance`, and their siblings replay the CI workflows locally.
- **Property tests** run in-process under `cargo test` via proptest (no separate fuzzer setup): `tests/fuzz/fuzz_parity.rs` asserts `Display` round-trips and guard/provenance fixpoints, and `tests/fuzz/fuzz_numeric.rs` asserts numeric value lowering (grouped spellings stage what plain ones do; malformed numerics fail closed). Run them with `cargo test -p oxdock-logic-tests --test fuzz_harness`.
- **Directed boundary tables** pin exact edge values (`i64::MIN` staging, `u64::MAX`, overflow rejections) where random generation cannot aim.

Prefer `cargo test --workspace --tests` to cover all crates. Fixtures for the build-time macros live under `crates/oxdock-logic-tests/fixtures/integration/buildtime_macros`.

Full workspace results are visible on the [CI runs](https://github.com/jzombie/rust-oxdock/actions/workflows/rust-tests.yml) page.
