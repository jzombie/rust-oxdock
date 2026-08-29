Workspace-level fixtures and a [libtest-mimic](https://crates.io/crates/libtest-mimic) harness.

- Fixtures live under `fixtures/` as standalone Cargo projects (including nested subdirectories).
- The harness auto-discovers directories with `Cargo.toml` and runs `cargo run --quiet` by default.
- Workspace dependencies are patched to local paths at runtime.

To add a fixture, create a new `fixtures/<name>/` (or nested) folder with a `Cargo.toml` and source files.
