# oxdock-logic-tests

Workspace-level test harness and fixtures for OxDock

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

Fixtures used by the build-time macro integration tests live under
`fixtures/integration/buildtime_macros/<name>/`. These are exercised by the same fixture
harness via `case.toml` expectations.

Parity fixtures live under `fixtures/parity/<case>/` and compare string DSL to token DSL.

- `dsl.txt` holds the string-based DSL.
- `tokens.rs` holds the braced-token version (the contents of a `script: { ... }` block).
- Errors are defined in `case.toml` under `[expect.error]` (supports `contains` or `equals`).

The parity harness parses both and asserts their ASTs match.

Fixtures define expectations in `case.toml`. This keeps error handling and output
assertions consistent across all harnesses. When present, the harness runs one
test per case file. Without it, the harness runs the fixture default and
expects success (Tier 3 trials default to `cargo run --quiet`; Tier 1/2
trials execute in-process or via a pre-compiled binary — see
`test-execution-tiers.md`).

`case.toml` format:

```
name = "failure"
args = ["run"]

[expect]
status = "failure"

[expect.stderr]
contains = ["failed to parse manifest"]

[expect.error]
contains = "failed to parse manifest"
```

Multiple cases can be defined under `cases/` as either `cases/<case>.toml` or
`cases/<case>/case.toml`. Each case produces its own test invocation.

Non-parity fixtures live under `fixtures/integration/`, for example:

- `fixtures/integration/buildtime_macros/build_from_manifest/`
- `fixtures/integration/buildtime_macros/<name>/`

Workspace-level fixtures and a [libtest-mimic](https://crates.io/crates/libtest-mimic) harness.

- Fixtures live under `fixtures/` as standalone Cargo projects (including nested subdirectories).
- The harness auto-discovers directories with `Cargo.toml` and runs each
  trial at the fastest execution tier preserving its semantics (per-trial
  `cargo` unless allowlisted for an in-process tier; see
  `test-execution-tiers.md`).
- Workspace dependencies are patched to local paths at runtime.

To add a fixture, create a new `fixtures/<name>/` (or nested) folder with a `Cargo.toml` and source files.

## Test execution tiers

Trials run at one of three tiers (fastest first). The harness picks the
fastest tier that preserves the fixture's semantics; no per-trial `cargo`
invocation happens outside Tier 3.

- **Tier 1 — in-process (parallel, allowlisted only).** `ast_commands` cases execute
  via the shared `ast_runner` (`parse_script` +
  `run_steps_with_fs_with_io` against isolated
  `GuardedPath::tempdir` roots, `PathResolver` filesystem fidelity — no
  mocks). Script fixtures listed in `IN_PROCESS_SCRIPT_FIXTURES`
  (`src/lib.rs`) run their `script.oxfile` the same way, through
  `run_steps_with_context_result_with_io`. Engine errors are
  formatted with `format_fixture_stderr` so assertions match the text the
  fixture binaries print on stderr.
- **Tier 2 — pre-compiled binary (parallel).** Fixtures listed in
  `PRECOMPILED_FIXTURES` (`tests/integration_harness.rs`) are instantiated
  and `cargo build`ed **once** before the thread pool starts; each trial
  spawns the binary directly. Only for fixtures whose trials run the built
  binary as-is.
- **Tier 3 — per-trial `cargo` (same-fixture serialized).** Fixtures that
  test Cargo itself (proc-macros, build scripts, `cargo check`/`cargo
  test`, per-case features/env) keep one cargo invocation per trial against
  a **shared** target dir. Same-fixture trials serialize on a per-fixture
  lock, and each trial evicts its package (`cargo clean -p`) first: Cargo's
  fingerprint does not distinguish same-package fixture copies, so without
  eviction a trial could reuse a sibling's artifacts built under different
  env. Different fixtures run in parallel. `CARGO_INCREMENTAL=1` is set
  unless the host already expresses a choice.

Harness `main`s do process-global setup (e.g. `prefer_tmpfs_for_tempdirs`,
which points `TMPDIR` at `/dev/shm` on Linux) strictly before
`libtest_mimic::run` spawns worker threads. Trials never mutate process
environment — per-trial env travels in `ExecIo` (Tier 1) or per-child
`CommandBuilder` env (Tiers 2/3).

## Persistent fixture target dir

The shared target dir is **persistent, not per-run**: resolved as
`$OXDOCK_FIXTURE_TARGET_DIR`, then `$CARGO_TARGET_DIR/oxdock-fixtures`,
then `<workspace>/target/oxdock-fixtures`. Fixture dependency artifacts
therefore survive across harness runs — the first run pays the dependency
compilation once, later runs rebuild only changed fixture crates. `cargo
clean` wipes it with the rest of `target/`; concurrent harness runs share
it safely via Cargo's own target-dir locking.

`cargo test -p oxdock-logic-tests` runs Tier 1/2 plus Tier 3;
`--features slow-integration` additionally runs every `ast_commands` case
in-process (default is the `write`/`with_io` smoke pair plus coverage
validation).

## License

`oxdock-logic-tests` is distributed under the terms of the Apache License (Version 2.0).
