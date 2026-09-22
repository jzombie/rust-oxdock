// Persistent project cache identity and SYSTEM path helpers.
//
// `WORKSPACE CACHE` is a persistent, per-project directory shared across
// runs. It is resolved through `cache-manager` with the `os-cache-dir`
// feature and never evicted by default: this module only ever calls
// creation-only APIs (`ensure_group` with no policy), never the
// `*_with_policy` or `eviction_report` APIs.
//
// `WORKSPACE SYSTEM` grants full filesystem access. Because Windows has
// multiple root namespaces (drive letters, UNC shares), SYSTEM resolution
// never confines paths under a single root. Instead it normalizes candidates
// purely lexically and anchors each result at its own filesystem anchor
// (`/` on Unix, `C:\` or `\\server\share\` on Windows).

use anyhow::Result;

use super::GuardedPath;
use crate::env::{CACHE_APP, CACHE_DIR, CARGO_PKG_NAME, FALLBACK_APP_NAME};

/// Group name under the project cache root owned by workspace resolution.
// Keep this stable: cache contents must survive upgrades.
pub(crate) const CACHE_GROUP: &str = "workspace";

/// Project-local cache parent: `WORKSPACE CACHE --local` roots the cache
/// at `<project>/.cache`, with the shared group segment underneath.
pub(crate) const LOCAL_CACHE_DIR_NAME: &str = ".cache";

/// ProjectDirs identity for the OS-native cache root. Keep stable: the
/// resolved directory persists across upgrades. Unused under Miri, where
/// the synthetic `/miri/cache` path replaces OS-native resolution.
#[cfg_attr(miri, allow(dead_code))]
pub(crate) const CACHE_QUALIFIER: &str = "com";
/// ProjectDirs identity for the OS-native cache root. Keep stable: the
/// resolved directory persists across upgrades. Unused under Miri, where
/// the synthetic `/miri/cache` path replaces OS-native resolution.
#[cfg_attr(miri, allow(dead_code))]
pub(crate) const CACHE_ORGANIZATION: &str = "oxdock";

/// Sanitize a raw application identity into a filesystem-safe path segment.
// Only ASCII alphanumerics plus `.`, `_`, `-` survive; everything else
// becomes `_`. Leading and trailing dots are stripped so the segment can
// never be empty, hidden, or `.`/`..`. Falls back to `FALLBACK_APP_NAME`
// when nothing usable remains.
pub(crate) fn sanitize_app_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim().trim_matches('.');
    if trimmed.is_empty() {
        FALLBACK_APP_NAME.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Resolve the cache application identity without reading any manifest off
// disk and without compile-time `env!` inside `oxdock-fs` (which would
// freeze to `oxdock-fs`). Precedence:
// 1. Explicit constructor/builder argument.
// 2. `OXDOCK_CACHE_APP` environment override.
// 3. `CARGO_PKG_NAME` runtime environment (present when launched by Cargo,
//    and visible to proc-macro/build-script hosts through the compiler
//    driver environment).
// 4. Running binary file stem (`std::env::current_exe`), which identifies
//    installed or prebuilt binaries outside Cargo.
// 5. `FALLBACK_APP_NAME`.
//
// Nested (not let-chained) throughout: this crate carries no MSRV pin, so
// it must compile on toolchains predating the `let_chains` stabilization.
#[allow(clippy::collapsible_if)]
pub(crate) fn auto_detect_app_name(explicit: Option<&str>) -> String {
    if let Some(name) = explicit.filter(|s| !s.trim().is_empty()) {
        return sanitize_app_name(name);
    }
    if let Ok(name) = std::env::var(CACHE_APP) {
        if !name.trim().is_empty() {
            return sanitize_app_name(&name);
        }
    }
    if let Ok(name) = std::env::var(CARGO_PKG_NAME) {
        if !name.trim().is_empty() {
            return sanitize_app_name(&name);
        }
    }
    // `current_exe` is an unsupported isolation operation under Miri
    // (hard error, not a recoverable `Err`), so this stage is native-only.
    #[cfg(not(miri))]
    if let Ok(exe) = std::env::current_exe() {
        if let Some(stem) = exe.file_stem().and_then(|s| s.to_str()) {
            if !stem.trim().is_empty() {
                return sanitize_app_name(stem);
            }
        }
    }
    FALLBACK_APP_NAME.to_string()
}

/// Resolve the project cache root directory. Pure path computation except
// for reading the `OXDOCK_CACHE_DIR` exact-directory override: no
// directories are created here. Creation happens on first cache-targeted
// resolve through `ensure_cache_dir`. Nested (not let-chained): see
// `auto_detect_app_name`.
#[allow(clippy::collapsible_if)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub(crate) fn resolve_cache_dir(explicit: Option<&str>) -> std::path::PathBuf {
    #[cfg(not(miri))]
    {
        if let Ok(dir) = std::env::var(CACHE_DIR) {
            if !dir.trim().is_empty() {
                return std::path::PathBuf::from(dir);
            }
        }
        let app = auto_detect_app_name(explicit);
        cache_manager::CacheRoot::from_project_dirs(CACHE_QUALIFIER, CACHE_ORGANIZATION, &app)
            .map(|root| root.path().to_path_buf())
            .unwrap_or_else(|_| std::env::temp_dir().join(format!("oxdock-cache-{app}")))
    }
    #[cfg(miri)]
    {
        // Honor the exact override under Miri too (pure env read, no host
        // I/O): the synthetic backend keys state by path, so pinned tests
        // stay hermetic and meaningful without touching the host.
        if let Ok(dir) = std::env::var(CACHE_DIR) {
            if !dir.trim().is_empty() {
                return std::path::PathBuf::from(dir);
            }
        }
        let app = auto_detect_app_name(explicit);
        std::path::PathBuf::from(format!("/miri/cache/{app}"))
    }
}

/// Build the self-rooted cache guard for an explicit or auto-detected app
// identity. No filesystem I/O: the directory is created on first
// cache-targeted resolve.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub(crate) fn cache_guard_for(explicit: Option<&str>) -> GuardedPath {
    let dir = resolve_cache_dir(explicit).join(CACHE_GROUP);
    GuardedPath::from_guarded_parts(dir.clone(), dir)
}

/// Build the self-rooted project-local cache guard under `anchor`
// (`<anchor>/.cache/workspace`, issue #163 follow-up). No filesystem I/O:
// the directory is created on first cache-targeted resolve. Deliberately
// ignores the `OXDOCK_CACHE_DIR` override: `--local` means the project
// tree, unconditionally, so the override stays scoped to the OS-native
// flavor.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub(crate) fn cache_guard_for_local(anchor: &GuardedPath) -> GuardedPath {
    let dir = anchor
        .as_path()
        .join(LOCAL_CACHE_DIR_NAME)
        .join(CACHE_GROUP);
    GuardedPath::from_guarded_parts(dir.clone(), dir)
}

/// Ensure the cache directory exists using only creation-only
// `cache-manager` APIs. Never applies an eviction policy, so cached
// artifacts are never reclaimed by default. No-op under Miri.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub(crate) fn ensure_cache_dir(cache_dir: &std::path::Path) -> Result<()> {
    #[cfg(not(miri))]
    {
        cache_manager::CacheRoot::from_root(cache_dir.to_path_buf())
            .ensure_group(CACHE_GROUP)
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("failed to ensure cache dir {}: {e}", cache_dir.display()))
    }
    #[cfg(miri)]
    {
        let _ = cache_dir;
        Ok(())
    }
}

/// Topmost ancestor of an absolute path: `/` on Unix, the drive (`C:\`) or
// UNC share (`\\server\share\`) on Windows. Pure lexical computation, no
// filesystem access, so it works uniformly on every drive without requiring
// a common root.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub(crate) fn system_anchor(path: &std::path::Path) -> std::path::PathBuf {
    path.ancestors()
        .last()
        .map(|a| a.to_path_buf())
        .unwrap_or_default()
}

/// Lexically normalize an absolute path without touching the filesystem
// (no creation, no canonicalization syscalls). `.` segments drop, `..`
// pops the previous segment, clamped at the filesystem anchor so
// normalization can never name a path above the drive or share root.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub(crate) fn normalize_absolute(path: &std::path::Path) -> std::path::PathBuf {
    let anchor = system_anchor(path);
    let mut out = anchor.clone();
    for comp in path.components() {
        match comp {
            std::path::Component::Prefix(_)
            | std::path::Component::RootDir
            | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if out != anchor {
                    let _ = out.pop();
                }
            }
            std::path::Component::Normal(seg) => out.push(seg),
        }
    }
    out
}

/// Wrap an absolute path as a system guard anchored at its own filesystem
// anchor. Each drive or share carries its own anchor, so cross-drive and
// UNC paths never fail a containment check: there is no single root to be
// contained under.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub(crate) fn system_wrap_absolute(path: &std::path::Path) -> GuardedPath {
    let normalized = normalize_absolute(path);
    let anchor = system_anchor(&normalized);
    GuardedPath::from_guarded_parts(anchor, normalized)
}

#[cfg(test)]
pub(crate) static CACHE_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Serialized view of the cache-identity environment (issue #163). Process
// env is global, so tests that mutate it hold the lock and restore prior
// values on drop (mirrors `SerialCargoEnv` in `oxdock-process`). Shared by
// every test module in this crate that pins `OXDOCK_CACHE_*` so parallel
// tests cannot race each other's overrides.
#[cfg(test)]
pub(crate) struct SerialCacheEnv {
    _lock: MutexGuard<'static, ()>,
    prev: Vec<(&'static str, Option<String>)>,
}

#[cfg(test)]
impl SerialCacheEnv {
    pub(crate) fn new(vars: &[(&'static str, Option<&str>)]) -> Self {
        let lock = CACHE_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut prev = Vec::with_capacity(vars.len());
        for (key, value) in vars {
            prev.push((*key, std::env::var(key).ok()));
            // SAFETY: mutations are serialized by `CACHE_ENV_LOCK` and
            // scoped to this guard, which restores prior values on drop.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
        Self { _lock: lock, prev }
    }
}

#[cfg(test)]
impl Drop for SerialCacheEnv {
    fn drop(&mut self) {
        unsafe {
            for (key, value) in &self.prev {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[cfg(test)]
use std::sync::{Mutex, MutexGuard};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::{CACHE_APP, CACHE_DIR, CARGO_PKG_NAME};

    #[test]
    fn sanitize_keeps_safe_segments_and_falls_back() {
        assert_eq!(sanitize_app_name("oxdock"), "oxdock");
        assert_eq!(sanitize_app_name("my-crate_2.0"), "my-crate_2.0");
        assert_eq!(sanitize_app_name("my crate!"), "my_crate_");
        assert_eq!(sanitize_app_name("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_app_name(""), "oxdock");
        assert_eq!(sanitize_app_name("..."), "oxdock");
        assert_eq!(sanitize_app_name("   "), "___");
    }

    #[test]
    fn explicit_identity_wins_over_environment() {
        let _env = SerialCacheEnv::new(&[
            (CACHE_APP, Some("env-app")),
            (CARGO_PKG_NAME, Some("cargo-app")),
        ]);
        assert_eq!(auto_detect_app_name(Some("given")), "given");
        assert_eq!(auto_detect_app_name(Some("  ")), "env-app");
    }

    #[test]
    fn env_override_wins_over_cargo_runtime_env() {
        let _env = SerialCacheEnv::new(&[
            (CACHE_APP, Some("my app!")),
            (CARGO_PKG_NAME, Some("cargo-app")),
        ]);
        assert_eq!(auto_detect_app_name(None), "my_app_");
    }

    #[test]
    fn cargo_runtime_env_is_used_when_no_override() {
        let _env = SerialCacheEnv::new(&[(CACHE_APP, None), (CARGO_PKG_NAME, Some("cargo-app"))]);
        assert_eq!(auto_detect_app_name(None), "cargo-app");
    }

    #[cfg_attr(
        miri,
        ignore = "std::env::current_exe is an unsupported isolation operation under Miri"
    )]
    #[test]
    fn binary_stem_is_used_outside_cargo() {
        let _env = SerialCacheEnv::new(&[(CACHE_APP, None), (CARGO_PKG_NAME, None)]);
        let detected = auto_detect_app_name(None);
        assert!(!detected.is_empty());
        match std::env::current_exe().ok().and_then(|exe| {
            exe.file_stem()
                .and_then(|s| s.to_str())
                .map(sanitize_app_name)
        }) {
            Some(stem) => assert_eq!(detected, stem),
            None => assert_eq!(detected, FALLBACK_APP_NAME),
        }
    }

    #[test]
    fn cache_dir_honors_exact_override() {
        let _env = SerialCacheEnv::new(&[(CACHE_DIR, Some("/tmp/oxdock-test-cache"))]);
        let dir = resolve_cache_dir(Some("ignored"));
        assert!(dir.ends_with("oxdock-test-cache"));
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    #[test]
    fn system_anchor_and_normalization_are_lexical() {
        let anchor = system_anchor(std::path::Path::new("/a/b"));
        assert_eq!(anchor, std::path::PathBuf::from("/"));
        let normalized = normalize_absolute(std::path::Path::new("/a/../b/./c"));
        assert_eq!(normalized, std::path::PathBuf::from("/b/c"));
        let clamped = normalize_absolute(std::path::Path::new("/../x"));
        assert_eq!(clamped, std::path::PathBuf::from("/x"));
        let wrapped = system_wrap_absolute(std::path::Path::new("/a/../b"));
        assert_eq!(wrapped.as_path(), std::path::Path::new("/b"));
    }

    /// Creating the cache dir must never delete pre-existing entries: no
    /// eviction policy is ever applied (issue #163). Takes the project
    /// root: the group segment is appended inside, exactly once.
    #[cfg(not(miri))]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    #[test]
    fn ensure_cache_dir_creates_without_evicting() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let dir = temp.as_guarded_path().as_path().join("cache-root");
        let dir_str = dir.to_string_lossy().into_owned();
        let _env = SerialCacheEnv::new(&[(CACHE_DIR, Some(dir_str.as_str()))]);

        let resolved = resolve_cache_dir(None);
        assert_eq!(resolved, dir);
        ensure_cache_dir(&resolved).expect("ensure");
        let group = resolved.join(CACHE_GROUP);
        assert!(group.is_dir(), "group segment created exactly once");
        assert!(
            !group.join(CACHE_GROUP).exists(),
            "group segment must not double"
        );
        let probe = group.join("keep.txt");
        std::fs::write(&probe, b"keep").expect("seed cache entry");
        ensure_cache_dir(&resolved).expect("re-ensure");
        assert_eq!(std::fs::read(&probe).expect("read probe"), b"keep");
    }

    /// Project-local guards root at `<anchor>/.cache/workspace` with no
    /// filesystem I/O; the exact override still wins when set.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    #[test]
    fn local_guard_roots_under_anchor() {
        let _env = SerialCacheEnv::new(&[(CACHE_DIR, None)]);
        let temp = GuardedPath::tempdir().expect("tempdir");
        let anchor = temp.as_guarded_path().clone();
        let guard = cache_guard_for_local(&anchor);
        let expected = anchor
            .as_path()
            .join(LOCAL_CACHE_DIR_NAME)
            .join(CACHE_GROUP);
        assert_eq!(guard.as_path(), expected.as_path());
        assert_eq!(guard.root(), expected.as_path());
    }
}
