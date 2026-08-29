Proc-macro processes do **not** receive `CARGO_FEATURE_*` or `CARGO_CFG_*` by default.
Build scripts do. These helpers re-emit those values as `cargo:rustc-env=...` so proc-macros
can read them and pass them into the OxDock environment (via `BuiltinEnv`).
