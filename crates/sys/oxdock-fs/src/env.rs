//! Shared environment variable names.
//!
//! Every `OXDOCK_`/`CARGO_` runtime name the workspace reads or writes lives
//! here as a `pub const`, so call sites never repeat a hardcoded string. (The
//! `env!("...")` compile-time macros keep their literals: that macro only
//! accepts literal arguments. Third-party names like `GIT_*`, `RUSTFLAGS`,
//! or `TERM` stay at their call sites.)

/// Exact-directory override for the persistent project cache root.
pub const CACHE_DIR: &str = "OXDOCK_CACHE_DIR";
/// Application identity override for the persistent project cache root.
pub const CACHE_APP: &str = "OXDOCK_CACHE_APP";
/// Fallback application identity when no other stage resolves one.
pub const FALLBACK_APP_NAME: &str = "oxdock";

/// Overrides workspace-root discovery; highest-priority input.
pub const WORKSPACE_ROOT: &str = "OXDOCK_WORKSPACE_ROOT";
/// Debug flag shared by the asset pipeline.
pub const EMBED_DEBUG: &str = "OXDOCK_EMBED_DEBUG";
/// Content-fingerprint salt for the asset pipeline.
pub const EMBED_FINGERPRINT_SALT: &str = "OXDOCK_EMBED_FINGERPRINT_SALT";
/// Cache-bust gate for the asset pipeline.
pub const EMBED_FORCE_REBUILD: &str = "OXDOCK_EMBED_FORCE_REBUILD";
/// Banner relayed to spawned shells.
pub const BANNER: &str = "OXDOCK_BANNER";
/// Forces stdout/stderr inheritance for spawned processes.
pub const INHERIT_STDOUT: &str = "OXDOCK_INHERIT_STDOUT";
/// Shell re-exec marker (Windows).
pub const SHELL_REEXEC: &str = "OXDOCK_SHELL_REEXEC";
/// Target-dir override for fixture cargo invocations.
pub const FIXTURE_TARGET_DIR: &str = "OXDOCK_FIXTURE_TARGET_DIR";
/// Case filter for the `ast_commands` fixture runner.
pub const AST_CASE: &str = "OXDOCK_AST_CASE";
/// Coverage-only mode for the `ast_commands` fixture runner.
pub const AST_ONLY_COVERAGE: &str = "OXDOCK_AST_ONLY_COVERAGE";

/// Test guards: multi-guard pass token.
pub const MULTI_GUARD_TEST_PASS: &str = "OXDOCK_MULTI_GUARD_TEST_PASS";
/// Test guards: multi-guard fail token.
pub const MULTI_GUARD_TEST_FAIL: &str = "OXDOCK_MULTI_GUARD_TEST_FAIL";
/// Test guards: unset-token probe.
pub const GUARD_TEST_TOKEN_UNSET: &str = "OXDOCK_GUARD_TEST_TOKEN_UNSET";

/// Cargo-provided manifest directory of the current package.
pub const CARGO_MANIFEST_DIR: &str = "CARGO_MANIFEST_DIR";
/// Cargo-provided package name of the current process.
pub const CARGO_PKG_NAME: &str = "CARGO_PKG_NAME";
/// Cargo-provided primary-package marker.
pub const CARGO_PRIMARY_PACKAGE: &str = "CARGO_PRIMARY_PACKAGE";
/// Cargo target-directory override.
pub const CARGO_TARGET_DIR: &str = "CARGO_TARGET_DIR";
/// Cargo incremental-compilation toggle.
pub const CARGO_INCREMENTAL: &str = "CARGO_INCREMENTAL";
/// Cargo-provided enabled-features list.
pub const CARGO_CFG_FEATURE: &str = "CARGO_CFG_FEATURE";

/// Build output directory for build scripts.
pub const OUT_DIR: &str = "OUT_DIR";
/// Cargo-provided build target triple.
pub const TARGET: &str = "TARGET";
/// Cargo-provided build profile.
pub const PROFILE: &str = "PROFILE";
/// Toolchain override for rustc probing.
pub const RUSTC: &str = "RUSTC";

/// Unix shell resolution.
pub const SHELL: &str = "SHELL"; // TODO: Rename to UNIX_SHELL?
/// Windows shell resolution.
pub const COMSPEC: &str = "COMSPEC"; // TODO: Rename to WINDOWS_COMSPEC?
/// System temp-dir override honored by fixture runners.
pub const TMPDIR: &str = "TMPDIR";
