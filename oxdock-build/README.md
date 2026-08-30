# oxdock-build

Build script helpers for OxDock.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.
- `emit_feature_envs()`: forward `CARGO_FEATURE_*` and `CARGO_CFG_FEATURE`
- `emit_cfg_envs()`: forward `CARGO_CFG_*` derived from `rustc --print cfg`
- `emit_feature_and_cfg_envs()`: calls both

Build-script helpers for forwarding Cargo feature/cfg information to [OxDock's proc-macro
execution environment](../oxdock-macros).

> **This crate is intended to be called from a `build.rs` build script. It is not
meant to run at application runtime.**

```rust
fn main() {
    oxdock_build::emit_feature_and_cfg_envs()
        .expect("failed to emit feature/cfg envs");
}
```

| Item | Helper Necessary? |
| --- | --- |
| `FOO=1 cargo build` | No |
| CLI args sent to `cargo run -- ...` | TBD |
| Available cargo features / cfgs | Yes |
| All other environment variables | No |

Proc-macro processes do **not** receive `CARGO_FEATURE_*` or `CARGO_CFG_*` by default.
Build scripts do. These helpers re-emit those values as `cargo:rustc-env=...` so proc-macros
can read them and pass them into the OxDock environment (via `BuiltinEnv`).

## License

`oxdock-build` is distributed under the terms of the Apache License (Version 2.0).
