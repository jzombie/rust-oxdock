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

## Replacing `build.rs` logic

No. OxDock cannot replace `build.rs` entirely, because Cargo enforces a
hard protocol: build-time logic must execute via a Rust binary defined in
`build.rs`.

OxDock can, however, replace the contents of `build.rs`. Instead of
writing imperative Rust scripts that pull in heavy `[build-dependencies]`,
`build.rs` delegates asset generation, file transformation, and code
generation to an OxDock script:

```rust
// build.rs delegates to an OxDock script file. The spec below is the
// whole delegation shape: pass it to `prepare_assets` from `build.rs`.
let spec = oxdock_build::PrepareSpec::new(oxdock_build::DslSource::File("build.oxdock"));
let _ = spec;
```

The DSL replaces the script logic, not the build entry point itself.

### Why replacing `build.rs` logic with OxDock works

1. Eliminates `[build-dependencies]` compilation bloat. Complex `build.rs`
   files frequently pull in heavy Rust crates (`serde_json`, `glob`,
   `walkdir`, `ureq`, `zip`) under `[build-dependencies]`. This forces
   Cargo to compile large dependency trees twice, once for the host system
   (to run `build.rs`) and once for the target architecture. OxDock bundles
   process piping, JSON/TOML parsing, globbing, and file manipulation into
   its core engine, avoiding double compilation of external utilities.
2. Automatic dependency invalidation (`cargo:rerun-if-*`). Writing
   `build.rs` in Rust requires manually printing
   `cargo:rerun-if-changed=<path>` and `cargo:rerun-if-env-changed=<var>`
   to stdout. Forgetting a single path results in stale build caches. The
   `oxdock-build` integration automatically tracks every file touched
   (`READ`, `GLOB`, `EXPAND`) and environment variable accessed
   (`INHERIT_ENV`, `env:KEY`) during AST lowering, emitting accurate Cargo
   invalidation directives without manual tracking.
3. Sandboxed workdir and destructive mutation guards. Rust `build.rs`
   scripts execute with full process privileges over the workspace
   filesystem. A bug in a path concatenation function can overwrite root
   source files. OxDock enforces a `GuardedPath` sandbox root, isolating
   intermediate file generation to `$OUT_DIR` or temporary build
   directories.

### Trade-off matrix

| Structural vector | Imperative Rust (`build.rs`) | OxDock DSL (`build.oxdock`) |
| --- | --- | --- |
| Cargo entry point | Native (`build.rs`) | Requires a stub in `build.rs` |
| Dependency compile overhead | High (compiles `[build-dependencies]`) | Zero extra crate compilations |
| Cache invalidation | Manual (`cargo:rerun-if-changed`) | Automatic AST reference tracking |
| Filesystem safety | Unrestricted host FS access | Sandboxed `GuardedPath` enforcement |
| Complex logic and algorithms | Native Rust code | Requires host function registration |

### Recommendation

Use OxDock to replace `build.rs` logic for asset pipeline orchestration,
process execution, code generation template expansion, and file syncing.

Keep raw Rust in `build.rs` only when invoking native C compiler
abstractions (such as the `cc` or `bindgen` crates) that require direct
FFI integration with C build systems.

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
