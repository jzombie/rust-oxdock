use super::AccessMode;
use super::PathResolver;
use super::guard_path;
use crate::PathLike;
use anyhow::Result;
use std::borrow::Cow;
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::fs::File;
#[cfg(not(miri))]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::fs::OpenOptions;
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::path::{Path, PathBuf};
#[cfg(not(miri))]
use std::sync::OnceLock;
#[cfg(miri)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(not(miri))]
use std::time::Duration;
#[allow(clippy::disallowed_types)]
use tempfile::{Builder, TempDir};
#[cfg(not(miri))]
use tracing::warn;

#[cfg(miri)]
static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[cfg_attr(miri, allow(dead_code))]
const OXDOCK_TEMP_PREFIX: &str = "oxdock-";
#[cfg_attr(miri, allow(dead_code))]
const OXDOCK_TEMP_MARKER: &str = ".oxdock-tempdir";
#[cfg_attr(miri, allow(dead_code))]
const OXDOCK_TEMP_LOCK: &str = ".oxdock-tempdir.lock";

/// Path guaranteed to stay within a guard root. The root is stored alongside the
/// resolved absolute path so consumers cannot escape without constructing a new
/// guard explicitly.
#[derive(Clone, Eq, PartialEq)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub struct GuardedPath {
    root: PathBuf,
    path: PathBuf,
}

impl GuardedPath {
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub fn new(root: &Path, candidate: &Path) -> Result<Self> {
        let guarded = guard_path(root, candidate, AccessMode::Read)?;
        Ok(Self {
            root: root.to_path_buf(),
            path: guarded,
        })
    }

    /// Build a guard from string paths without requiring callers to reference `std::path` types.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub fn new_from_str(root: &str, candidate: &str) -> Result<Self> {
        Self::new(Path::new(root), Path::new(candidate))
    }

    /// Create a guard where the root is the path itself.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub fn new_root(root: &Path) -> Result<Self> {
        let guarded = guard_path(root, root, AccessMode::Read)?;
        Ok(Self {
            root: guarded.clone(),
            path: guarded,
        })
    }

    /// Build a root guard from a string path without exposing `std::path` types to callers.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub fn new_root_from_str(root: &str) -> Result<Self> {
        Self::new_root(Path::new(root))
    }

    /// Create a guarded temporary directory using `tempfile::Builder`.
    #[cfg(not(miri))]
    pub fn tempdir() -> Result<GuardedTempDir> {
        Self::tempdir_with(|_| {})
    }

    /// Create a guarded temporary directory with a custom `tempfile::Builder`
    /// configuration (e.g., prefixes). The temporary directory is deleted when
    /// the returned `GuardedTempDir` is dropped unless it is persisted.
    #[cfg(not(miri))]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub fn tempdir_with<F>(configure: F) -> Result<GuardedTempDir>
    where
        F: FnOnce(&mut Builder),
    {
        let mut builder = Builder::new();
        builder.prefix(OXDOCK_TEMP_PREFIX);
        configure(&mut builder);
        let tempdir = builder.tempdir()?;
        let guard = GuardedPath::new_root(tempdir.path())?;
        // Write the PID lock BEFORE the marker: cleanup scans on marker
        // existence and reclaims marker-without-lock dirs, so publishing the
        // marker last makes this directory invisible to a concurrent startup
        // sweep until the lock (with our live PID) is durably in place. The
        // accepted trade-off is that a crash inside this two-write window
        // leaves an unmarked dir the GC never reclaims.
        let lock = write_temp_lock(&guard)?;
        write_temp_marker(&guard)?;
        Ok(GuardedTempDir::new(guard, Some(tempdir), Some(lock)))
    }

    /// Create a synthetic temporary directory when running under Miri.
    /// No host filesystem operations are performed to keep isolation intact.
    #[cfg(miri)]
    pub fn tempdir() -> Result<GuardedTempDir> {
        Self::tempdir_with(|_| {})
    }

    /// Miri-safe variant of `tempdir_with` that allocates a synthetic root path
    /// without touching the host filesystem.
    #[cfg(miri)]
    #[allow(clippy::disallowed_types)]
    pub fn tempdir_with<F>(_configure: F) -> Result<GuardedTempDir>
    where
        F: FnOnce(&mut Builder),
    {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!("/miri/tempdir-{id}"));
        let guard = GuardedPath {
            root: path.clone(),
            path,
        };
        Ok(GuardedTempDir::new(guard, None, None))
    }

    /// Remove any stale tempdirs created by OxDock in the system temp dir. This helps
    /// recover space when a process was terminated with SIGKILL and drops did not run.
    #[cfg(not(miri))]
    pub fn cleanup_stale_tempdirs() -> Result<()> {
        cleanup_marked_tempdirs()
    }

    /// No-op placeholder under Miri where no host tempdirs are created.
    #[cfg(miri)]
    pub fn cleanup_stale_tempdirs() -> Result<()> {
        Ok(())
    }

    #[allow(clippy::disallowed_types)]
    /// Audited escape hatch: yields a raw std path reference.
    /// Prefer guarded operations; do not persist the value beyond the
    /// guarded operation that produced it.
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    /// Backend-aware existence check that avoids host `stat` when running under Miri.
    pub fn exists(&self) -> bool {
        PathResolver::new(self.root(), self.root())
            .map(|resolver| resolver.exists(self))
            .unwrap_or(false)
    }

    #[allow(clippy::disallowed_types)]
    /// Audited escape hatch: yields a raw std path reference to the guard root.
    /// Prefer guarded operations; do not persist the value beyond the
    /// guarded operation that produced it.
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[allow(clippy::disallowed_types)]
    /// Audited escape hatch: yields an owned std path outside the guard.
    /// Prefer guarded operations; do not persist the value beyond the
    /// guarded operation that produced it.
    pub fn to_path_buf(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn display(&self) -> String {
        command_path(self).display().to_string()
    }

    pub fn join(&self, rel: &str) -> Result<Self> {
        GuardedPath::new(&self.root, &self.path.join(rel))
    }

    /// Return the parent directory as a guarded path, if it exists within the same root.
    pub fn parent(&self) -> Option<Self> {
        let parent = self.path.parent()?.to_path_buf();
        GuardedPath::new(&self.root, &parent).ok()
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub(crate) fn from_guarded_parts(root: PathBuf, path: PathBuf) -> Self {
        Self { root, path }
    }

    /// Evaluate a glob pattern against the sandbox root.
    /// Normalizes backslashes, escapes the root path, and returns workspace-relative paths.
    #[allow(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::disallowed_macros
    )]
    pub fn glob_paths(&self, pattern: &str) -> std::result::Result<Vec<PathBuf>, std::io::Error> {
        let clean = if cfg!(not(windows)) && pattern.as_bytes().get(1) == Some(&b':') {
            &pattern[2..]
        } else {
            pattern
        };
        let posix = clean.replace('\\', "/");
        let path_pattern = Path::new(&posix);

        let rel_pattern = if path_pattern.is_absolute() {
            path_pattern
                .strip_prefix(&self.root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| posix.trim_start_matches('/').to_string())
        } else {
            posix.trim_start_matches('/').to_string()
        };

        let root_str = self.root.to_string_lossy().replace('\\', "/");
        let escaped_root = glob::Pattern::escape(&root_str);
        let search = format!("{}/{}", escaped_root, rel_pattern);

        let mut results = Vec::new();
        for entry in glob::glob(&search)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?
        {
            let path = entry?;
            if path.starts_with(&self.root) {
                results.push(path);
            }
        }
        Ok(results)
    }
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
impl std::fmt::Display for GuardedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        command_path(self).display().fmt(f)
    }
}

impl std::fmt::Debug for GuardedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let root = GuardedPath::from_guarded_parts(self.root.clone(), self.root.clone());
        f.debug_struct("GuardedPath")
            .field("root", &normalized_path(&root))
            .field("path", &normalized_path(self))
            .finish()
    }
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
impl PathLike for GuardedPath {
    fn as_path(&self) -> &Path {
        self.as_path()
    }

    fn root(&self) -> &Path {
        self.root()
    }

    fn to_path_buf(&self) -> PathBuf {
        self.to_path_buf()
    }

    fn join(&self, rel: &str) -> Result<Self> {
        self.join(rel)
    }

    fn parent(&self) -> Option<Self> {
        self.parent()
    }

    fn new_from_str(root: &str, candidate: &str) -> Result<Self> {
        GuardedPath::new(Path::new(root), Path::new(candidate))
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn new_root(root: &Path) -> Result<Self> {
        GuardedPath::new_root(root)
    }
}

/// Temporary directory that cleans itself up on drop while exposing the guarded path.
#[allow(clippy::disallowed_types)]
pub struct GuardedTempDir {
    guard: Option<GuardedPath>,
    tempdir: Option<TempDir>,
    lock: Option<File>,
}

impl GuardedTempDir {
    #[allow(clippy::disallowed_types)]
    fn new(guard: GuardedPath, tempdir: Option<TempDir>, lock: Option<File>) -> Self {
        Self {
            guard: Some(guard),
            tempdir,
            lock,
        }
    }

    pub fn as_guarded_path(&self) -> &GuardedPath {
        self.guard.as_ref().unwrap()
    }

    #[allow(clippy::disallowed_types)]
    pub fn into_parts(mut self) -> (Option<TempDir>, GuardedPath) {
        let _ = self.lock.take();
        (self.tempdir.take(), self.guard.take().unwrap())
    }
}

impl std::ops::Deref for GuardedTempDir {
    type Target = GuardedPath;

    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap()
    }
}

#[allow(clippy::disallowed_methods)]
impl Drop for GuardedTempDir {
    fn drop(&mut self) {
        // Ensure the lock file is closed before the TempDir is dropped so
        // platform-specific file-handle semantics (Windows) don't prevent
        // directory removal. Taking the `lock` here moves the `File` into a
        // temporary which is dropped at the end of this function (before
        // the fields are automatically dropped), ensuring the handle is
        // released prior to `tempdir`'s removal.
        let _ = self.lock.take();

        // Best-effort explicit cleanup: some platforms may leave the tempdir
        // behind if `tempfile::TempDir` cannot delete while a handle lingers.
        // By taking ownership and dropping the TempDir early we can retry a
        // removal if it somehow survives.
        #[cfg(not(miri))]
        if let Some(tempdir) = self.tempdir.take() {
            let path = tempdir.path().to_path_buf();
            drop(tempdir);
            if path.exists() {
                let _ = std::fs::remove_dir_all(&path);
                // Windows can defer directory teardown if a scanner briefly
                // holds a handle; retry a few times to avoid leaving debris
                // that breaks parent-process assertions.
                if path.exists() {
                    for _ in 0..5 {
                        std::thread::sleep(Duration::from_millis(10));
                        if !path.exists() {
                            break;
                        }
                        let _ = std::fs::remove_dir_all(&path);
                        if !path.exists() {
                            break;
                        }
                    }
                }
            }
        }

        #[cfg(miri)]
        {
            if let Some(guard) = &self.guard {
                super::backend::cleanup_synthetic_root(guard.root());
            }
        }
    }
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
impl std::fmt::Display for GuardedTempDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.guard.as_ref().unwrap().fmt(f)
    }
}

/// Path wrapper that intentionally skips guard checks. Use only for paths that
/// originate outside the guarded workspace (e.g., external file handles).
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub struct UnguardedPath {
    path: PathBuf,
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
impl UnguardedPath {
    /// Constructs an unguarded handle for a path that originates OUTSIDE the
    /// guarded workspace (host tooling, TTY devices, CLI-provided paths).
    /// Every call site asserts, by name, that escaping the guard is
    /// intentional. Audited escape hatch: prefer guarded operations.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub fn external(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[allow(clippy::disallowed_types)]
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    #[allow(clippy::disallowed_types)]
    pub fn to_path_buf(&self) -> PathBuf {
        self.path.clone()
    }
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
impl std::fmt::Display for UnguardedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.path.display().fmt(f)
    }
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
impl PathLike for UnguardedPath {
    fn as_path(&self) -> &Path {
        self.as_path()
    }

    fn root(&self) -> &Path {
        // For UnguardedPath the "root" is the path itself; callers expecting a
        // root should handle this semantic difference.
        self.as_path()
    }

    fn to_path_buf(&self) -> PathBuf {
        self.to_path_buf()
    }

    fn join(&self, rel: &str) -> Result<Self> {
        // Preserve forward-slash style when the original path or the
        // incoming relative fragment uses `/`. On Windows `Path::join`
        // will format with backslashes which breaks tests that expect
        // normalized forward-slash strings. If either side contains a
        // forward slash treat the join as a string join and construct a
        // `PathBuf` from the resulting string so the underlying OsString
        // keeps the `/` characters.
        let base = self.path.to_string_lossy();
        if base.contains('/') || rel.contains('/') {
            let base = base.trim_end_matches('/');
            let rel = rel.trim_start_matches('/');
            let joined = format!("{}/{}", base, rel);
            Ok(UnguardedPath::external(PathBuf::from(joined)))
        } else {
            Ok(UnguardedPath::external(self.path.join(rel)))
        }
    }

    fn parent(&self) -> Option<Self> {
        self.path
            .parent()
            .map(|p| UnguardedPath::external(p.to_path_buf()))
    }

    fn new_from_str(_root: &str, candidate: &str) -> Result<Self> {
        Ok(UnguardedPath::external(Path::new(candidate).to_path_buf()))
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn new_root(root: &Path) -> Result<Self> {
        Ok(UnguardedPath::external(root.to_path_buf()))
    }
}

/// Convert a guarded path into a platform-normalized `Path` suitable for
/// passing to system `Command` APIs. This lives in `oxdock-fs` so that
/// usage of `std::path::Path` stays inside the crate that is allowed to touch
/// host path types and syscalls.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub fn command_path(path: &GuardedPath) -> Cow<'_, std::path::Path> {
    use std::path::{Component, Prefix};

    let mut components = path.as_path().components();
    if let Some(Component::Prefix(prefix)) = components.next() {
        match prefix.kind() {
            Prefix::VerbatimDisk(drive) => {
                let mut buf = PathBuf::from(format!("{}:", char::from(drive)));
                buf.extend(components);
                return Cow::Owned(buf);
            }
            Prefix::VerbatimUNC(server, share) => {
                let mut buf = PathBuf::from(r"\\");
                buf.push(server);
                buf.push(share);
                buf.extend(components);
                return Cow::Owned(buf);
            }
            Prefix::Verbatim(_) => {
                let mut buf = PathBuf::new();
                buf.push(prefix.as_os_str());
                buf.extend(components);
                return Cow::Owned(buf);
            }
            _ => {}
        }
    }

    Cow::Borrowed(path.as_path())
}

/// Normalize a path string to use forward slashes, replacing backslashes.
/// This is useful for consistent path representation (e.g. when embedding files or in mocks).
pub fn to_forward_slashes(s: &str) -> String {
    s.replace('\\', "/")
}

/// Convert a guarded path into a stable, forward-slash string suitable for
/// serialization, embedding, logging, or CLI output. This strips Windows
/// verbatim prefixes and ensures separators are `/`.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub fn normalized_path(path: &GuardedPath) -> String {
    let cmd = command_path(path);
    let s = cmd.to_string_lossy();
    to_forward_slashes(&s)
}

/// Backward-compatible alias for `normalized_path`. Prefer `normalized_path`
/// in new code to make intent clearer.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub fn embed_path(path: &GuardedPath) -> String {
    normalized_path(path)
}

#[cfg_attr(miri, allow(dead_code))]
#[cfg(not(miri))]
fn write_temp_marker(guard: &GuardedPath) -> Result<()> {
    let resolver = PathResolver::new_guarded(guard.clone(), guard.clone())?;
    let marker = guard.join(OXDOCK_TEMP_MARKER)?;
    resolver.write_file(&marker, b"oxdock-tempdir")?;
    Ok(())
}

#[cfg_attr(miri, allow(dead_code))]
#[cfg(miri)]
fn write_temp_marker(_guard: &GuardedPath) -> Result<()> {
    Ok(())
}

#[cfg_attr(miri, allow(dead_code))]
#[cfg(not(miri))]
fn write_temp_lock(guard: &GuardedPath) -> Result<File> {
    #[allow(clippy::disallowed_methods)]
    let pid = std::process::id();
    let lock_path = guard.join(OXDOCK_TEMP_LOCK)?;
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(lock_path.as_path())?;
    use std::io::Write;
    writeln!(file, "{pid}")?;
    #[allow(clippy::disallowed_methods)]
    file.sync_all()?;
    Ok(file)
}

#[cfg_attr(miri, allow(dead_code))]
#[cfg(miri)]
fn write_temp_lock(_guard: &GuardedPath) -> Result<File> {
    Err(anyhow::anyhow!("temp lock unused under miri"))
}

#[cfg(not(miri))]
fn cleanup_marked_tempdirs() -> Result<()> {
    cleanup_marked_tempdirs_in(std::env::temp_dir())
}

#[cfg(not(miri))]
pub(crate) fn run_temp_cleanup_once() {
    static CLEANUP: OnceLock<()> = OnceLock::new();
    CLEANUP.get_or_init(|| {
        if let Err(err) = cleanup_marked_tempdirs() {
            warn!(%err, "failed to cleanup stale oxdock tempdirs");
        }
    });
}

#[cfg(miri)]
pub(crate) fn run_temp_cleanup_once() {}

#[cfg(not(miri))]
#[allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::collapsible_if
)]
fn cleanup_marked_tempdirs_in(base: std::path::PathBuf) -> Result<()> {
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    for entry in std::fs::read_dir(&base)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !name.starts_with(OXDOCK_TEMP_PREFIX) {
            continue;
        }

        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let marker = path.join(OXDOCK_TEMP_MARKER);
        if !marker.exists() {
            continue;
        }

        let lock_path = path.join(OXDOCK_TEMP_LOCK);
        if let Some(pid) = read_lock_pid(&lock_path) {
            if is_pid_alive(pid) {
                continue;
            }
        }

        #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
        match std::fs::remove_dir_all(&path) {
            Ok(_) => {}
            Err(err) => warn!(?path, %err, "failed to remove stale oxdock tempdir"),
        }
    }
    Ok(())
}

#[cfg(miri)]
#[allow(dead_code, clippy::disallowed_types)]
fn cleanup_marked_tempdirs_in(_base: std::path::PathBuf) -> Result<()> {
    Ok(())
}

#[cfg_attr(miri, allow(dead_code))]
#[cfg(not(miri))]
#[allow(clippy::disallowed_types)]
fn read_lock_pid(lock_path: &Path) -> Option<u32> {
    if !lock_path.exists() {
        return None;
    }
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    match std::fs::read_to_string(lock_path) {
        Ok(contents) => contents.trim().parse::<u32>().ok(),
        Err(_) => None,
    }
}

#[cfg_attr(miri, allow(dead_code))]
#[cfg(miri)]
#[allow(clippy::disallowed_types)]
fn read_lock_pid(_lock_path: &Path) -> Option<u32> {
    None
}

#[cfg_attr(miri, allow(dead_code))]
#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[allow(clippy::disallowed_methods)]
    let res = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if res == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error();
    match errno {
        Some(libc::EPERM) => true,
        Some(libc::ESRCH) => false,
        _ => true, // default to conservative keep-alive
    }
}

#[cfg_attr(miri, allow(dead_code))]
#[cfg(windows)]
fn is_pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // If the process cannot be opened, assume it is gone so we can
            // reclaim stale tempdirs rather than leaking them.
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code as *mut u32);
        CloseHandle(handle);
        if ok == 0 {
            true
        } else {
            code == (STILL_ACTIVE as u32)
        }
    }
}

#[cfg_attr(miri, allow(dead_code))]
#[cfg(not(any(unix, windows)))]
fn is_pid_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guarded_path_join_and_parent_round_trip() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let child = root.join("child/grandchild").expect("join");
        let parent = child.parent().expect("parent");
        assert_eq!(parent.as_path(), root.as_path().join("child"));
        assert_eq!(child.root(), root.root());
    }

    #[allow(clippy::disallowed_types)]
    #[test]
    fn unguarded_path_join_and_parent_work() {
        let root = UnguardedPath::external("/tmp/oxdock-fs-test");
        let joined = root.join("child").expect("join");
        assert_eq!(
            joined.as_path().to_string_lossy(),
            "/tmp/oxdock-fs-test/child"
        );
        let parent = joined.parent().expect("parent");
        assert_eq!(parent.as_path().to_string_lossy(), "/tmp/oxdock-fs-test");
    }

    #[test]
    fn normalized_path_normalizes_backslashes() {
        // Avoid using a hard-coded Windows drive prefix as the root when
        // running cross-platform tests. Construct a guarded root from a
        // temporary directory and synthesize a path that contains
        // backslashes so `normalized_path` still exercises the
        // backslash-to-forward-slash normalization.
        #[cfg(not(miri))]
        {
            let tmp = GuardedPath::tempdir().expect("tempdir");
            let root_buf = tmp.as_guarded_path().root().to_path_buf();
            let root_display = root_buf.to_string_lossy();

            #[allow(clippy::disallowed_types)]
            let path_with_backslashes =
                std::path::PathBuf::from(format!("{}\\foo\\bar", root_display));
            let guard = GuardedPath::from_guarded_parts(root_buf, path_with_backslashes);
            let normalized = normalized_path(&guard);
            assert!(!normalized.contains('\\'));
            assert!(normalized.contains("foo"));
            assert!(normalized.contains("bar"));
        }

        // Under Miri we don't touch the host filesystem; exercise the
        // normalization helper directly instead.
        #[cfg(miri)]
        {
            let s = r"C:\sandbox\foo\bar";
            let replaced = to_forward_slashes(s);
            assert!(!replaced.contains('\\'));
            assert!(replaced.contains("sandbox"));
            assert!(replaced.contains("foo"));
            assert!(replaced.contains("bar"));
        }
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn cleanup_stale_tempdirs_removes_marked_dirs() {
        #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
        let base = std::env::temp_dir().join("oxdock-cleanup-test");
        #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
        std::fs::create_dir_all(&base).expect("create temp sandbox");

        // Create a tempdir and intentionally leak it to simulate a killed process.
        let mut builder = Builder::new();
        builder.prefix(OXDOCK_TEMP_PREFIX);
        #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
        let tempdir = builder.tempdir_in(&base).expect("tempdir_in");
        let guard = GuardedPath::new_root(tempdir.path()).expect("guarded root");
        write_temp_marker(&guard).expect("marker");
        let lock = write_temp_lock(&guard).expect("lock");
        let temp = GuardedTempDir::new(guard, Some(tempdir), Some(lock));
        let path = temp.as_guarded_path().to_path_buf();
        std::mem::forget(temp);

        // Ensure marker exists (already written by tempdir())
        let marker = path.join(OXDOCK_TEMP_MARKER);
        assert!(marker.exists());
        assert!(path.exists());

        // Simulate a dead process by removing the lock so cleanup can reclaim it.
        let lock = path.join(OXDOCK_TEMP_LOCK);
        let _ = std::fs::remove_file(&lock);

        cleanup_marked_tempdirs_in(base.clone()).expect("cleanup");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn cleanup_skips_live_pid_marked_dir() {
        let base = std::env::temp_dir().join("oxdock-cleanup-live-test");
        std::fs::create_dir_all(&base).expect("create base");

        let mut builder = Builder::new();
        builder.prefix(OXDOCK_TEMP_PREFIX);
        let tempdir = builder.tempdir_in(&base).expect("tempdir_in");
        let guard = GuardedPath::new_root(tempdir.path()).expect("guard");
        write_temp_marker(&guard).expect("marker");
        let _lock = write_temp_lock(&guard).expect("lock");
        #[allow(deprecated)]
        let leaked = tempdir.into_path();

        cleanup_marked_tempdirs_in(base.clone()).expect("cleanup");
        assert!(leaked.exists(), "live pid should prevent removal");

        let _ = std::fs::remove_dir_all(&leaked);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn plant_marked_dir(
        base: &std::path::Path,
        name: &str,
        lock_bytes: Option<Vec<u8>>,
        with_marker: bool,
    ) -> std::path::PathBuf {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).expect("plant dir");
        if with_marker {
            std::fs::write(dir.join(OXDOCK_TEMP_MARKER), b"oxdock-tempdir").expect("marker");
        }
        if let Some(bytes) = lock_bytes {
            std::fs::write(dir.join(OXDOCK_TEMP_LOCK), bytes).expect("lock");
        }
        dir
    }

    /// Pins the full reclaim/skip decision matrix of the stale-tempdir GC,
    /// including the corrupt-lock trade-off: an unreadable/unparseable lock
    /// falls through to RECLAIM because the marker proves ownership and a
    /// crashed writer must stay recoverable (see plan D2).
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    #[cfg_attr(
        miri,
        ignore = "plants host tempdir fixtures; blocked under Miri isolation"
    )]
    #[test]
    fn cleanup_decision_matrix_pins_reclaim_semantics() {
        let base = std::env::temp_dir().join(format!("oxdock-matrix-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("sandbox");

        let self_pid = std::process::id();
        let removed: Vec<std::path::PathBuf> = vec![
            plant_marked_dir(&base, "oxdock-empty-lock", Some(Vec::new()), true),
            plant_marked_dir(
                &base,
                "oxdock-garbage-lock",
                Some(b"not-a-pid".to_vec()),
                true,
            ),
            plant_marked_dir(
                &base,
                "oxdock-multiline-lock",
                Some(b"123\n456".to_vec()),
                true,
            ),
            plant_marked_dir(&base, "oxdock-negative-lock", Some(b"-5".to_vec()), true),
            plant_marked_dir(&base, "oxdock-non-utf8-lock", Some(vec![0xff, 0xfe]), true),
            plant_marked_dir(&base, "oxdock-missing-lock", None, true),
        ];
        let kept: Vec<std::path::PathBuf> = vec![
            // Whitespace around a valid live PID still parses and keeps the dir.
            plant_marked_dir(
                &base,
                "oxdock-padded-live-lock",
                Some(format!("\n {self_pid}\n ").into_bytes()),
                true,
            ),
            // Lock present but marker absent: invisible to the sweep by design
            // (this is the invariant the lock-before-marker creation order
            // relies on).
            plant_marked_dir(
                &base,
                "oxdock-locked-unmarked",
                Some(self_pid.to_string().into_bytes()),
                false,
            ),
            // Prefixed non-directory entries are ignored.
            {
                let file = base.join("oxdock-stray-file.txt");
                std::fs::write(&file, b"x").expect("stray file");
                file
            },
            // Unmarked prefixed directory: not provably ours... actually it IS
            // skipped because the marker gate runs first.
            plant_marked_dir(&base, "oxdock-plain-unmarked", None, false),
        ];

        cleanup_marked_tempdirs_in(base.clone()).expect("sweep");

        for path in &removed {
            assert!(!path.exists(), "expected reclaim: {}", path.display());
        }
        for path in &kept {
            assert!(path.exists(), "expected keep: {}", path.display());
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The system's raison d'être: a marked tempdir whose lock names a
    /// genuinely dead (spawned + reaped) owner gets reclaimed.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    #[cfg_attr(
        miri,
        ignore = "spawns a probe process and touches host tempdirs; blocked under Miri"
    )]
    #[test]
    fn cleanup_reclaims_tempdir_of_reaped_owner() {
        let mut child = {
            #[cfg(windows)]
            {
                std::process::Command::new("cmd")
                    .args(["/C", "exit 0"])
                    .spawn()
                    .expect("spawn probe")
            }
            #[cfg(not(windows))]
            {
                std::process::Command::new("sh")
                    .args(["-c", "exit 0"])
                    .spawn()
                    .expect("spawn probe")
            }
        };
        child.wait().expect("reap probe");
        let dead_pid = child.id();

        let base = std::env::temp_dir().join(format!("oxdock-dead-owner-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("sandbox");
        let stale = plant_marked_dir(
            &base,
            "oxdock-victim",
            Some(format!("{dead_pid}").into_bytes()),
            true,
        );

        cleanup_marked_tempdirs_in(base.clone()).expect("sweep");
        assert!(!stale.exists(), "tempdir of reaped owner must be reclaimed");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg_attr(miri, ignore = "liveness probing is stubbed to true under Miri")]
    #[test]
    fn is_pid_alive_probes_known_pids() {
        assert!(!is_pid_alive(0), "PID 0 is never a live owner");
        assert!(is_pid_alive(std::process::id()), "own PID must read alive");
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    #[cfg_attr(miri, ignore = "spawns a probe process; blocked under Miri isolation")]
    #[test]
    fn is_pid_alive_reports_reaped_child_dead() {
        let mut child = {
            #[cfg(windows)]
            {
                std::process::Command::new("cmd")
                    .args(["/C", "exit 0"])
                    .spawn()
                    .expect("spawn probe")
            }
            #[cfg(not(windows))]
            {
                std::process::Command::new("sh")
                    .args(["-c", "exit 0"])
                    .spawn()
                    .expect("spawn probe")
            }
        };
        child.wait().expect("reap probe");
        assert!(
            !is_pid_alive(child.id()),
            "a reaped child's PID must report dead (modulo exotic PID reuse)"
        );
    }

    /// Contract test for what `tempdir_with` actually writes: caller-supplied
    /// builder configuration applies, the marker identifies ownership, and
    /// the lock carries our PID followed by a newline.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn tempdir_with_custom_configurator_writes_marker_and_pid_lock() {
        let temp = GuardedPath::tempdir_with(|builder| {
            builder.prefix("oxdock-contract-");
        })
        .expect("dir");
        let path = temp.as_guarded_path().to_path_buf();

        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        assert!(
            name.starts_with("oxdock-contract-"),
            "custom prefix applied: {name}"
        );

        let marker = std::fs::read(path.join(OXDOCK_TEMP_MARKER)).expect("marker readable");
        assert_eq!(marker, b"oxdock-tempdir".to_vec());

        let lock = std::fs::read_to_string(path.join(OXDOCK_TEMP_LOCK)).expect("lock readable");
        assert_eq!(lock, format!("{}\n", std::process::id()));
    }

    /// `into_parts` transfers persistence ownership to the caller: the
    /// directory survives losing the `GuardedTempDir`, and is removed only
    /// when the handed-out `TempDir` itself drops.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn into_parts_transfers_ownership_beyond_guard_drop() {
        let temp = GuardedPath::tempdir().expect("dir");
        let path = temp.as_guarded_path().to_path_buf();

        let (inner, guard) = temp.into_parts();
        drop(guard);

        let inner = inner.expect("host tempdir present");
        assert!(path.exists(), "survives guard drop while persisted");

        drop(inner);
        assert!(!path.exists(), "dropping the TempDir removes it");
    }
}
