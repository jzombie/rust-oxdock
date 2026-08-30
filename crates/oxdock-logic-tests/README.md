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
test per case file. Without it, the harness defaults to `cargo run --quiet` and
expects success.

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

- `fixtures/integration/build_from_manifest/`
- `fixtures/integration/buildtime_macros/<name>/`

Workspace-level fixtures and a [libtest-mimic](https://crates.io/crates/libtest-mimic) harness.

- Fixtures live under `fixtures/` as standalone Cargo projects (including nested subdirectories).
- The harness auto-discovers directories with `Cargo.toml` and runs `cargo run --quiet` by default.
- Workspace dependencies are patched to local paths at runtime.

To add a fixture, create a new `fixtures/<name>/` (or nested) folder with a `Cargo.toml` and source files.

## License

`oxdock-logic-tests` is distributed under the terms of the Apache License (Version 2.0).
