# Environment Variable Contracts

Every environment variable the OxDock workspace reads, writes, or documents —
with its current status after the build-script pipeline migration
(Prototype commits, `d2b732e..HEAD`), exact consuming functions, and
transition rationale.

Statuses:

- **Retained** — read by the same logical component as before the migration.
- **Relocated** — same semantics, different owning crate/function.
- **Purged** — intentionally deleted; a test or doc pins the removal.
- **New** — introduced by the build-script pipeline.

| Variable | Status | Consumers (file : function) | Notes |
|---|---|---|---|
| `OXDOCK_WORKSPACE_ROOT` | Retained | `crates/sys/oxdock-fs/src/lib.rs` : `discover_workspace_root`; propagated to child processes via `oxdock-fs::FixtureInstance::cargo` / CLI | Overrides workspace-root discovery; highest-priority input |
| `OXDOCK_INHERIT_STDOUT` | Retained | `crates/oxdock-core/src/exec/handlers.rs` : `run` (read from script env map) | Value `1`/`true` (case-insensitive) forces stdout+stderr inheritance even when capture streams are wired; pinned by executor tests |
| `OXDOCK_EMBED_DEBUG` | Relocated (shared) | helpers `assets.rs` : `embed_debug_enabled()` → invoked from BOTH `oxdock-buildtime-macros/src/lib.rs` : `build_assets` AND helpers `build_and_materialize` | Truthiness `1` / any-case `true` |
| `RUST_ANALYZER_INTERNALS_DO_NOT_USE` | Relocated (shared) | helpers `assets.rs` : `execution_is_skipped_with` → invoked from BOTH the restored inline macros (`expand_embed_internal`/`expand_prepare_internal`) and helpers asset engine | IDE skip predicate: stub placeholder / silent success instead of execution |
| `RUSTFLAGS` (`--cfg miri` substring) | Relocated (shared) | same as above | Miri-configured builds skip asset execution; unrelated flags (e.g. `-Dwarnings`) must NOT trigger it (negative branch pinned in tests) |
| current-exe contains `rust-analyzer` | Relocated (shared) | same as above | Executable-name fallback predicate |
| `VSCODE_PID` + absence of `TERM` | Relocated (shared) | same as above | VS Code background-task heuristic; interactive terminals do not skip |
| `OXDOCK_EMBED_FORCE_REBUILD` | **Purged** | *(none)* | Old macro cache-bust gate. Invalidation is now owned by Cargo via `cargo:rerun-if-changed` directives emitted by helpers `run_spec`; completeness + ordering pinned by `packaging_invariant::directives_are_complete_and_ordered`. Do not re-introduce. |
| `.git` presence / `CARGO_PRIMARY_PACKAGE` (gate input) | **Purged** (as gate) | *(none in production code)* | The old `should_build = has_git \|\| is_primary` gate is gone; build scripts always execute outside IDE-skip contexts. Registry consumers running build.rs equals the normal ecosystem trust model. |
| `CARGO_PRIMARY_PACKAGE` (test infra) | Retained | `crates/sys/oxdock-process/src/serial_cargo_env.rs` : `manifest_env_guard` | Set/restored around tests that emulate primary vs dependency packages; no production reader remains |
| `CARGO_MANIFEST_DIR` | Retained | `crates/sys/oxdock-fs/src/workspace_fs/mod.rs` : `PathResolver::from_manifest_env`; `oxdock-buildtime-helpers/src/assets.rs` : `run_spec`, `script_text`, `clear_dir` helpers | Anchor for manifest-relative resolution and directive paths |
| `OUT_DIR` | New | `oxdock-buildtime-helpers/src/assets.rs` : `out_dir_root`; `oxdock-buildtime-macros` : `embed!` expansion (`include!(concat!(env!("OUT_DIR"), …))`) | Destination for materialized assets and generated typed modules |
| `RUSTC` | Retained | `oxdock-buildtime-helpers/src/lib.rs` : `emit_cfg_envs` (default `"rustc"`) | Toolchain override for `--print cfg` probing |
| `TARGET` | Retained | `oxdock-buildtime-helpers/src/lib.rs` : `collect_cfg_lines` | Forwarded as `--target` when present |
| `SHELL` | Retained | `crates/sys/oxdock-process/src/shell.rs` : `shell_program` (unix fallback `sh`) | Interactive-shell resolution |
| `COMSPEC` | Retained | `crates/sys/oxdock-process/src/shell.rs` : `shell_program` (windows fallback `cmd`) | Windows shell resolution |
| `CARGO_FEATURE_*` / `CARGO_CFG_*` | Retained | `crates/sys/oxdock-process/src/builtin_env.rs` : `BuiltinEnv::collect`; emitted for consumer crates by helpers `emit_feature_envs` | Natively visible inside build-script processes, so DSL scripts read them without relay (env_injection fixture pins this) |

## Transition log (Prototype commits)

1. **Purged:** `OXDOCK_EMBED_FORCE_REBUILD` plus the `.git`/`CARGO_PRIMARY_PACKAGE`
   execution gates. Rationale: Cargo's `rerun-if-changed` invalidation replaces
   manual cache busting; registry consumers executing build scripts is the
   normal ecosystem trust model. Guard rails: directive golden-completeness and
   packaging-invariant tests in `oxdock-buildtime-helpers/tests/packaging_invariant.rs`.
2. **Relocated:** `OXDOCK_EMBED_DEBUG` and all four IDE/Miri skip predicates,
   from proc-macro execution time (`oxdock-buildtime-macros`) to build-script
   execution time (`oxdock-buildtime-helpers`). Semantics preserved verbatim;
   full 8-branch decision matrix unit-tested.
3. **Retained:** every other variable above; consumption sites moved only where
   the containing module was split (`exec.rs` → `exec/handlers.rs`,
   process `lib.rs` → submodules).

## Verification

- `cargo test -p oxdock-buildtime-helpers --test packaging_invariant`
  covers skip-predicate branches, directive completeness/ordering, and
  with-git/without-git equivalence.
- Full battery: `cargo fmt --all --check` · `cargo clippy --workspace
  --all-targets --all-features -- -D warnings` · `cargo test --workspace
  --all-features --tests` · Miri on `oxdock-process` + `oxdock-core`
  (`cargo +nightly miri test -p oxdock-process -p oxdock-core --all-features`;
  falls back to the CI Miri job when no local nightly toolchain is installed).

## Dual-mode asset pipeline

1. **Inline (primary contract).** `embed! { name, script: { … } | "…", out_dir }`
   and `prepare! { … }` execute the DSL inside the proc-macro via the guarded
   engine. No build.rs is required. Cache: `<out_dir>/.oxdock_hash`
   content-fingerprint of script text, statically discoverable inputs, and all
   referenced env values (`{{ env:KEY }}` templates + `[env:KEY]` guards);
   matching fingerprint ⇒ zero re-execution. Known limitation: dynamically
   constructed inputs that are invisible to static analysis are not
   fingerprinted — touch the script (or a watched file) to invalidate.
2. **Build-script (alternative).** `oxdock_buildtime_helpers::embed_assets /
   prepare_assets` from build.rs with `embed!(Ident)` consumer macro. Adds
   Cargo-native `rerun-if-changed` invalidation for tracked inputs.

Both paths share skip predicates, debug flag semantics, the guarded executor,
and the staged materializer.

**Forcing rebuilds.** Two levers: bump `OXDOCK_EMBED_FINGERPRINT_SALT`
(precision — one rebuild, then caching resumes) or set
`OXDOCK_EMBED_FORCE_REBUILD=1` (bypasses cache reads while set). On the inline
path, changing an env var does NOT by itself re-invoke rustc — pair it with a
source-file `touch` or `cargo clean -p <pkg>` so the macro actually re-expands.
On the build.rs path, salt edits trigger reruns automatically via the emitted
directive.
