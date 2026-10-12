use anyhow::{Context, Result, bail};
use std::fs;

use super::{DirEntry, GuardedPath};

/// Private trait describing the backend IO interface. Kept private to avoid
/// expanding the public API surface; used to ensure host/miri implementations
/// remain in sync.
trait BackendImpl: Send + Sync {
    fn create_dir_all(&self, root: &GuardedPath, path: &GuardedPath) -> Result<()>;
    fn read_dir_entries(&self, path: &GuardedPath) -> Result<Vec<DirEntry>>;
    fn read_file(&self, path: &GuardedPath) -> Result<Vec<u8>>;
    fn write_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()>;
    fn append_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()>;
    fn open_read(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Read + Send>>;
    fn open_write(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write + Send>>;
    fn open_append(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write + Send>>;
    /// Link-refusing opens for TOCTOU-exposed paths: a final-component
    /// symlink fails instead of redirecting the IO. Archive pack/unpack
    /// use these after their listing probes so a planted link cannot
    /// redirect bytes mid-run.
    fn open_read_nofollow(
        &self,
        path: &GuardedPath,
        expected_root: &GuardedPath,
    ) -> Result<Box<dyn std::io::Read + Send>>;
    fn open_write_nofollow(
        &self,
        path: &GuardedPath,
        expected_root: &GuardedPath,
    ) -> Result<Box<dyn std::io::Write + Send>>;
    fn canonicalize(&self, path: GuardedPath) -> Result<GuardedPath>;
    fn metadata(&self, path: &GuardedPath) -> Result<std::fs::Metadata>;
    fn entry_kind(&self, path: &GuardedPath) -> Result<super::EntryKind>;
    fn entry_kind_no_follow(&self, path: &GuardedPath) -> Result<super::EntryKind>;
    fn resolve_workdir(&self, resolved: GuardedPath) -> Result<GuardedPath>;
    fn resolve_copy_source(&self, guarded: GuardedPath) -> Result<GuardedPath>;
    fn remove_file(&self, path: &GuardedPath) -> Result<()>;
    fn remove_dir_all(&self, path: &GuardedPath) -> Result<()>;
    fn remove_empty_dir(&self, path: &GuardedPath) -> Result<()>;
}

// Host implementation (used when not under Miri)
#[cfg(not(miri))]
#[derive(Clone, Copy)]
pub(in crate::workspace_fs) struct HostBackend;

/// Win32 `FILE_FLAG_OPEN_REPARSE_POINT` for
/// `OpenOptionsExt::custom_flags`: open the link itself so subsequent
/// IO fails instead of following. Stable API constant, spelled out to
/// avoid a dependency for one flag.
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// True path of an open file descriptor: Linux reads its `/proc`
/// self-link, macOS asks `fcntl(F_GETPATH)`. The descriptor is
/// pinned, so later swaps cannot redirect its IO: checking this path
/// closes the validation-to-open race on that side completely.
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
#[cfg_attr(miri, allow(dead_code))]
fn fd_path(file: &fs::File) -> Result<std::path::PathBuf> {
    use std::os::unix::io::AsRawFd;
    let link = format!("/proc/self/fd/{}", file.as_raw_fd());
    std::fs::read_link(&link).with_context(|| "failed to resolve open file path".to_string())
}

/// True path of an open file descriptor via `fcntl(F_GETPATH)`.
#[cfg(target_os = "macos")]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
#[cfg_attr(miri, allow(dead_code))]
fn fd_path(file: &fs::File) -> Result<std::path::PathBuf> {
    use std::os::unix::io::AsRawFd;
    let mut buf = vec![0 as libc::c_char; libc::MAXPATHLEN as usize];
    // fcntl on a live descriptor with a caller-owned buffer: no
    // aliasing, returns -1 with errno on failure.
    let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) };
    if ret == -1 {
        bail!(
            "failed to resolve open file path: {}",
            std::io::Error::last_os_error()
        );
    }
    let path = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    Ok(std::path::PathBuf::from(path))
}

/// True path of an open file handle via `GetFinalPathNameByHandleW`,
/// returned verbatim (extended-length form, uppercase drive). No
/// normalization: `std::fs::canonicalize`, which produced the expected
/// root, uses the same API with the same flags, so both sides already
/// agree on case and prefixes for the same file.
#[cfg(windows)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
#[cfg_attr(miri, allow(dead_code))]
fn fd_path(file: &fs::File) -> Result<std::path::PathBuf> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
    };

    let handle = file.as_raw_handle();
    let mut buf = vec![0u16; 1024];
    let mut len = unsafe {
        GetFinalPathNameByHandleW(
            handle as _,
            buf.as_mut_ptr(),
            buf.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    if len as usize >= buf.len() {
        buf.resize(len as usize + 1, 0);
        len = unsafe {
            GetFinalPathNameByHandleW(
                handle as _,
                buf.as_mut_ptr(),
                buf.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
    }
    if len == 0 {
        bail!(
            "failed to resolve open file path: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(std::path::PathBuf::from(String::from_utf16_lossy(
        &buf[..len as usize],
    )))
}

/// Verify an opened file still lives under its expected tree, using
/// the descriptor's own path. The expected root is canonicalized
/// through the same API first: guard roots arrive in mixed forms
/// (verbatim canonical under confinement, lexical under `SYSTEM`),
/// while descriptor paths are always fully resolved, so comparing
/// without converging the domain false-rejects legitimate opens.
/// Linux, macOS, and Windows: other hosts keep the `lstat` +
/// open-flag layers, which close pre-planting but leave a swap-race
/// microsecond.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
#[cfg_attr(miri, allow(dead_code))]
fn verify_fd_within(
    file: &fs::File,
    expected_root: &GuardedPath,
    path: &GuardedPath,
) -> Result<()> {
    let actual = fd_path(file)?;
    let root = std::fs::canonicalize(expected_root.as_path()).with_context(|| {
        format!(
            "failed to canonicalize expected tree {}",
            expected_root.display()
        )
    })?;
    if cmp_form(&actual).starts_with(cmp_form(&root)) {
        Ok(())
    } else {
        bail!(
            "opened {} outside its expected tree {} (descriptor resolves to {})",
            path.display(),
            expected_root.display(),
            actual.display()
        )
    }
}

/// Comparison form for descriptor verification. macOS folds case:
/// `realpath` preserves caller spelling while `F_GETPATH` reports
/// on-disk case, and default volumes are case-insensitive, so exact
/// compare false-rejects divergent spellings of one file. Folding
/// cannot accept a distinct file here: the guard already pinned the
/// exact file pre-swap, and a same-letters doppelganger is either the
/// same file or unreachable through guard-resolved paths. All other
/// hosts compare exact: both sides resolve identically there
/// (Windows shares one API for both; Linux kernels agree).
#[cfg(target_os = "macos")]
#[allow(clippy::disallowed_types)]
#[cfg_attr(miri, allow(dead_code))]
fn cmp_form(path: &std::path::Path) -> std::path::PathBuf {
    std::path::PathBuf::from(path.as_os_str().to_string_lossy().to_lowercase())
}

/// Comparison form for descriptor verification (exact outside macOS).
#[cfg(not(target_os = "macos"))]
#[allow(clippy::disallowed_types)]
#[cfg_attr(miri, allow(dead_code))]
fn cmp_form(path: &std::path::Path) -> std::path::PathBuf {
    path.to_path_buf()
}

/// Other hosts cannot resolve descriptor paths with std + libc
/// alone, so verification stays a no-op there (documented gap, not
/// silent: the `lstat` + flag layers still run).
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
#[allow(clippy::disallowed_types)]
#[cfg_attr(miri, allow(dead_code))]
fn verify_fd_within(
    file: &fs::File,
    expected_root: &GuardedPath,
    path: &GuardedPath,
) -> Result<()> {
    let _ = (file, expected_root, path);
    Ok(())
}

#[cfg(not(miri))]
impl HostBackend {
    fn new(_root: &GuardedPath, _build: &GuardedPath) -> Result<Self> {
        Ok(Self {})
    }
}

#[cfg(not(miri))]
#[allow(clippy::disallowed_methods)]
impl BackendImpl for HostBackend {
    fn create_dir_all(&self, root: &GuardedPath, path: &GuardedPath) -> Result<()> {
        if !root.as_path().exists() {
            fs::create_dir_all(root.as_path())
                .with_context(|| format!("creating resolver root {}", root.display()))?;
        }
        fs::create_dir_all(path.as_path())
            .with_context(|| format!("creating dir {}", path.display()))?;
        Ok(())
    }

    fn read_dir_entries(&self, path: &GuardedPath) -> Result<Vec<DirEntry>> {
        let entries = fs::read_dir(path.as_path())
            .with_context(|| format!("failed to read dir {}", path.display()))?;

        let mut result = Vec::new();
        for entry in entries {
            let entry = entry?;
            #[cfg(not(miri))]
            result.push(entry);

            #[cfg(miri)]
            {
                let entry_path = entry.path();
                let file_type = entry.file_type()?;
                let kind = if file_type.is_dir() {
                    super::EntryKind::Dir
                } else {
                    super::EntryKind::File
                };
                result.push(DirEntry::new(entry_path, kind));
            }
        }
        Ok(result)
    }

    fn read_file(&self, path: &GuardedPath) -> Result<Vec<u8>> {
        let data = fs::read(path.as_path())
            .with_context(|| format!("failed to read {}", path.display()))?;
        Ok(data)
    }

    fn write_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()> {
        if let Some(parent) = path.as_path().parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating dir {}", parent.display()))?;
        }
        fs::write(path.as_path(), contents)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn append_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()> {
        if let Some(parent) = path.as_path().parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating dir {}", parent.display()))?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.as_path())
            .with_context(|| format!("opening {} for append", path.display()))?;
        std::io::Write::write_all(&mut file, contents)
            .with_context(|| format!("appending to {}", path.display()))?;
        Ok(())
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn open_read(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Read + Send>> {
        let file = fs::File::open(path.as_path())
            .with_context(|| format!("failed to open {} for reading", path.display()))?;
        Ok(Box::new(file))
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn open_write(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write + Send>> {
        if let Some(parent) = path.as_path().parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating dir {}", parent.display()))?;
        }
        let file = fs::File::create(path.as_path())
            .with_context(|| format!("failed to open {} for writing", path.display()))?;
        Ok(Box::new(file))
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn open_append(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write + Send>> {
        if let Some(parent) = path.as_path().parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating dir {}", parent.display()))?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.as_path())
            .with_context(|| format!("failed to open {} for appending", path.display()))?;
        Ok(Box::new(file))
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn open_read_nofollow(
        &self,
        path: &GuardedPath,
        expected_root: &GuardedPath,
    ) -> Result<Box<dyn std::io::Read + Send>> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(windows)]
        use std::os::windows::fs::OpenOptionsExt;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        // Unix refuses a link final component at open (ELOOP); Windows
        // opens the reparse point itself, so the subsequent IO fails
        // instead of following. Either way a swapped-in link errors
        // atomically rather than redirecting.
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        #[cfg(windows)]
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options
            .open(path.as_path())
            .with_context(|| format!("failed to open {} for reading", path.display()))?;
        verify_fd_within(&file, expected_root, path)?;
        Ok(Box::new(file))
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn open_write_nofollow(
        &self,
        path: &GuardedPath,
        expected_root: &GuardedPath,
    ) -> Result<Box<dyn std::io::Write + Send>> {
        if let Some(parent) = path.as_path().parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating dir {}", parent.display()))?;
        }
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(windows)]
        use std::os::windows::fs::OpenOptionsExt;
        let mut options = fs::OpenOptions::new();
        // No truncate here: truncation lands on the pinned descriptor
        // only after verification, so a redirected open cannot destroy
        // bytes before the check runs.
        options.write(true).create(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        #[cfg(windows)]
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        // Existence before the open decides rollback later: only files
        // this open creates may be removed on mismatch.
        let pre_existing = path.as_path().exists();
        let file = options
            .open(path.as_path())
            .with_context(|| format!("failed to open {} for writing", path.display()))?;
        if let Err(e) = verify_fd_within(&file, expected_root, path) {
            drop(file);
            // Roll back only files this open created: removing a
            // pre-existing redirect target would destroy the bytes
            // verification exists to protect. A concurrent create
            // racing the precheck can only strand an empty file.
            if !pre_existing {
                let _ = fs::remove_file(path.as_path());
            }
            return Err(e);
        }
        // Truncate the verified descriptor itself: later swaps cannot
        // redirect an open handle.
        file.set_len(0)
            .with_context(|| format!("failed to truncate {}", path.display()))?;
        Ok(Box::new(file))
    }

    fn canonicalize(&self, path: GuardedPath) -> Result<GuardedPath> {
        Ok(path)
    }

    fn metadata(&self, path: &GuardedPath) -> Result<std::fs::Metadata> {
        let m = fs::metadata(path.as_path())
            .with_context(|| format!("failed to stat {}", path.display()))?;
        Ok(m)
    }

    fn entry_kind(&self, path: &GuardedPath) -> Result<super::EntryKind> {
        let meta = self.metadata(path)?;
        if meta.is_dir() {
            Ok(super::EntryKind::Dir)
        } else if meta.is_file() {
            Ok(super::EntryKind::File)
        } else {
            bail!("unsupported file type: {}", path.display());
        }
    }

    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    fn entry_kind_no_follow(&self, path: &GuardedPath) -> Result<super::EntryKind> {
        let meta = fs::symlink_metadata(path.as_path())
            .with_context(|| format!("failed to stat {}", path.display()))?;
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            Ok(super::EntryKind::Symlink)
        } else if file_type.is_dir() {
            Ok(super::EntryKind::Dir)
        } else if file_type.is_file() {
            Ok(super::EntryKind::File)
        } else {
            bail!("unsupported file type: {}", path.display());
        }
    }

    fn resolve_workdir(&self, resolved: GuardedPath) -> Result<GuardedPath> {
        if let Ok(meta) = fs::metadata(resolved.as_path()) {
            if !meta.is_dir() {
                bail!("WORKDIR path is not a directory: {}", resolved.display());
            }
            return Ok(resolved);
        }

        fs::create_dir_all(resolved.as_path())
            .with_context(|| format!("failed to create WORKDIR {}", resolved.display()))?;
        Ok(resolved)
    }

    fn resolve_copy_source(&self, guarded: GuardedPath) -> Result<GuardedPath> {
        if !guarded.as_path().exists() {
            bail!(
                "COPY source missing in build context: {}",
                guarded.display()
            );
        }
        Ok(guarded)
    }

    fn remove_file(&self, path: &GuardedPath) -> Result<()> {
        match std::fs::remove_file(path.as_path()) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("failed to remove file {}", path.display()));
            }
        }
        Ok(())
    }

    fn remove_dir_all(&self, path: &GuardedPath) -> Result<()> {
        std::fs::remove_dir_all(path.as_path())
            .with_context(|| format!("failed to remove dir {}", path.display()))?;
        Ok(())
    }

    fn remove_empty_dir(&self, path: &GuardedPath) -> Result<()> {
        std::fs::remove_dir(path.as_path())
            .with_context(|| format!("failed to remove dir {}", path.display()))?;
        Ok(())
    }
}

// Miri implementation (keeps synthetic state + mirrors to host)
#[cfg(miri)]
#[allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::collapsible_if,
    clippy::needless_return,
    clippy::duplicated_attributes
)]
mod miri_backend {
    use super::super::EntryKind;
    use super::*;
    use anyhow::anyhow;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::io::Write;
    use std::path::Component;
    use std::sync::{Arc, Mutex, OnceLock};

    static STATE_REGISTRY: OnceLock<
        Mutex<HashMap<std::path::PathBuf, Arc<Mutex<SyntheticRootState>>>>,
    > = OnceLock::new();

    /// A write cursor that buffers data and flushes to synthetic state on drop.
    struct MiriWriteCursor {
        state: Arc<Mutex<SyntheticRootState>>,
        rel: String,
        cursor: std::io::Cursor<Vec<u8>>,
    }

    impl std::io::Write for MiriWriteCursor {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.cursor.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.cursor.flush()?;
            // Flush to synthetic state
            let data = self.cursor.get_ref().clone();
            self.state
                .lock()
                .expect("miri state poisoned")
                .write_file(&self.rel, &data);
            Ok(())
        }
    }

    impl Drop for MiriWriteCursor {
        fn drop(&mut self) {
            // Ensure data is flushed to synthetic state
            let _ = self.flush();
        }
    }

    pub(in crate::workspace_fs) struct MiriBackend {
        root_state: Option<Arc<Mutex<SyntheticRootState>>>,
        build_state: Option<Arc<Mutex<SyntheticRootState>>>,
        root_path: std::path::PathBuf,
    }

    impl Clone for MiriBackend {
        fn clone(&self) -> Self {
            Self {
                root_state: self.root_state.as_ref().map(Arc::clone),
                build_state: self.build_state.as_ref().map(Arc::clone),
                root_path: self.root_path.clone(),
            }
        }
    }

    impl MiriBackend {
        pub(super) fn new(root: &GuardedPath, build: &GuardedPath) -> Result<Self> {
            let root_state = shared_state_for(root.as_path());

            if root.as_path() == build.as_path() {
                Ok(Self {
                    root_state: Some(root_state.clone()),
                    build_state: Some(root_state),
                    root_path: root.as_path().to_path_buf(),
                })
            } else {
                let build_state = shared_state_for(build.as_path());
                Ok(Self {
                    root_state: Some(root_state),
                    build_state: Some(build_state),
                    root_path: root.as_path().to_path_buf(),
                })
            }
        }

        fn state_for_guard_rc(
            &self,
            guard: &GuardedPath,
        ) -> Result<Arc<Mutex<SyntheticRootState>>> {
            if guard.root() == self.root_path.as_path() {
                self.root_state
                    .clone()
                    .ok_or_else(|| anyhow!("synthetic state missing for root"))
            } else {
                self.build_state
                    .clone()
                    .ok_or_else(|| anyhow!("synthetic state missing for build context"))
            }
        }
    }

    fn shared_state_for(path: &std::path::Path) -> Arc<Mutex<SyntheticRootState>> {
        let registry = STATE_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = registry.lock().expect("miri state registry poisoned");
        guard
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(SyntheticRootState::new())))
            .clone()
    }

    impl BackendImpl for MiriBackend {
        fn create_dir_all(&self, _root: &GuardedPath, path: &GuardedPath) -> Result<()> {
            let state = self.state_for_guard_rc(path)?;
            let rel = normalize_rel(path);
            state.lock().expect("miri state poisoned").ensure_dir(&rel);
            Ok(())
        }

        fn read_dir_entries(&self, path: &GuardedPath) -> Result<Vec<DirEntry>> {
            let rel = normalize_rel(path);
            let state_ref = self.state_for_guard_rc(path)?;
            let state = state_ref.lock().expect("miri state poisoned");
            if !state.dir_exists(&rel) {
                bail!("directory does not exist: {}", path.display());
            }
            let children = state.list_children(&rel);
            Ok(children
                .into_iter()
                .map(|(name, kind)| DirEntry::new(path.as_path().join(name), kind))
                .collect())
        }

        fn read_file(&self, path: &GuardedPath) -> Result<Vec<u8>> {
            let rel = normalize_rel(path);
            let data = self
                .state_for_guard_rc(path)?
                .lock()
                .expect("miri state poisoned")
                .read_file(&rel)?;
            Ok(data)
        }

        fn write_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()> {
            let rel = normalize_rel(path);
            let binding = self.state_for_guard_rc(path)?;
            binding
                .lock()
                .expect("miri state poisoned")
                .write_file(&rel, contents);
            Ok(())
        }

        fn append_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()> {
            let rel = normalize_rel(path);
            let binding = self.state_for_guard_rc(path)?;
            binding
                .lock()
                .expect("miri state poisoned")
                .append_file(&rel, contents);
            Ok(())
        }

        fn open_read(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Read + Send>> {
            let data = self.read_file(path)?;
            Ok(Box::new(std::io::Cursor::new(data)))
        }

        // Synthetic state holds no links, so plain opens are exact.
        // The expected root still typechecks the caller's intent.
        fn open_read_nofollow(
            &self,
            path: &GuardedPath,
            expected_root: &GuardedPath,
        ) -> Result<Box<dyn std::io::Read + Send>> {
            let _ = expected_root;
            self.open_read(path)
        }

        fn open_write_nofollow(
            &self,
            path: &GuardedPath,
            expected_root: &GuardedPath,
        ) -> Result<Box<dyn std::io::Write + Send>> {
            let _ = expected_root;
            self.open_write(path)
        }

        fn open_write(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write + Send>> {
            // Under Miri, we write to a Cursor and then flush to synthetic state
            let rel = normalize_rel(path);
            let state = self.state_for_guard_rc(path)?;
            // Create an empty file first, then return a Cursor that will be flushed
            state
                .lock()
                .expect("miri state poisoned")
                .write_file(&rel, &[]);
            Ok(Box::new(MiriWriteCursor {
                state,
                rel,
                cursor: std::io::Cursor::new(Vec::new()),
            }))
        }

        fn open_append(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write + Send>> {
            let rel = normalize_rel(path);
            let state = self.state_for_guard_rc(path)?;
            // Read existing content
            let existing = state
                .lock()
                .expect("miri state poisoned")
                .read_file(&rel)
                .unwrap_or_default();
            let mut cursor = std::io::Cursor::new(existing);
            cursor.set_position(cursor.get_ref().len() as u64); // Seek to end
            Ok(Box::new(MiriWriteCursor { state, rel, cursor }))
        }

        fn canonicalize(&self, path: GuardedPath) -> Result<GuardedPath> {
            Ok(path)
        }

        fn metadata(&self, path: &GuardedPath) -> Result<std::fs::Metadata> {
            let rel = normalize_rel(path);
            let binding = self.state_for_guard_rc(path)?;
            let state = binding.lock().expect("miri state poisoned");
            if let Some(kind) = state.entry_kind(&rel) {
                synthetic_metadata(kind)
            } else {
                bail!("failed to stat {}", path.display())
            }
        }

        fn entry_kind(&self, path: &GuardedPath) -> Result<EntryKind> {
            let rel = normalize_rel(path);
            let binding = self.state_for_guard_rc(path)?;
            let state = binding.lock().expect("miri state poisoned");
            state
                .entry_kind(&rel)
                .ok_or_else(|| anyhow::anyhow!("missing entry kind for {}", path.display()))
        }

        fn entry_kind_no_follow(&self, path: &GuardedPath) -> Result<EntryKind> {
            // The synthetic Miri filesystem has no symlinks, so no-follow
            // inspection coincides with following inspection.
            self.entry_kind(path)
        }

        fn resolve_workdir(&self, resolved: GuardedPath) -> Result<GuardedPath> {
            let rel = normalize_rel(&resolved);
            let state = self.state_for_guard_rc(&resolved)?;

            {
                let mut state_mut = state.lock().expect("miri state poisoned");
                if state_mut.entry_kind(&rel).is_none() {
                    state_mut.ensure_dir(&rel);
                }
            }

            Ok(resolved)
        }

        fn resolve_copy_source(&self, guarded: GuardedPath) -> Result<GuardedPath> {
            let rel = normalize_rel(&guarded);
            let kind = self
                .state_for_guard_rc(&guarded)?
                .lock()
                .expect("miri state poisoned")
                .entry_kind(&rel)
                .ok_or_else(|| {
                    anyhow!(
                        "COPY source missing in build context: {}",
                        guarded.display()
                    )
                })?;
            if matches!(kind, EntryKind::Dir | EntryKind::File) {
                Ok(guarded)
            } else {
                bail!("COPY source unsupported kind: {}", guarded.display());
            }
        }

        fn remove_file(&self, path: &GuardedPath) -> Result<()> {
            let rel = normalize_rel(path);
            self.state_for_guard_rc(path)?
                .lock()
                .expect("miri state poisoned")
                .remove_file(&rel);
            Ok(())
        }

        fn remove_dir_all(&self, path: &GuardedPath) -> Result<()> {
            let rel = normalize_rel(path);
            self.state_for_guard_rc(path)?
                .lock()
                .expect("miri state poisoned")
                .remove_dir_all(&rel);
            Ok(())
        }

        fn remove_empty_dir(&self, path: &GuardedPath) -> Result<()> {
            let rel = normalize_rel(path);
            self.state_for_guard_rc(path)?
                .lock()
                .expect("miri state poisoned")
                .remove_empty_dir(&rel)
                .with_context(|| format!("failed to remove dir {}", path.display()))?;
            Ok(())
        }
    }

    fn synthetic_metadata(_kind: EntryKind) -> Result<std::fs::Metadata> {
        #[cfg(unix)]
        {
            // Use a stable FD (/dev/null) to obtain metadata without path-based statx.
            let f = fs::File::open("/dev/null")
                .with_context(|| "failed to open /dev/null for synthetic metadata")?;
            f.metadata()
                .with_context(|| "failed to fetch synthetic metadata")
        }

        #[cfg(not(unix))]
        {
            // Conservative fallback: metadata is not available under Miri on this platform.
            let _ = _kind;
            bail!("synthetic metadata unsupported under Miri on this platform")
        }
    }

    #[cfg(miri)]
    fn normalize_rel(guard: &GuardedPath) -> String {
        let rel = guard
            .as_path()
            .strip_prefix(guard.root())
            .unwrap_or(guard.as_path())
            .to_path_buf();
        let mut parts: Vec<String> = Vec::new();
        for part in rel.components() {
            match part {
                Component::RootDir | Component::CurDir => {}
                Component::ParentDir => {
                    parts.pop();
                }
                Component::Normal(s) => parts.push(s.to_string_lossy().to_string()),
                Component::Prefix(p) => parts.push(p.as_os_str().to_string_lossy().to_string()),
            }
        }
        parts.join("/")
    }

    #[derive(Default)]
    struct SyntheticRootState {
        files: HashMap<String, Vec<u8>>,
        dirs: HashSet<String>,
    }

    #[cfg(miri)]
    impl SyntheticRootState {
        fn new() -> Self {
            let mut dirs = HashSet::new();
            dirs.insert(String::new());
            Self {
                files: HashMap::new(),
                dirs,
            }
        }

        fn ensure_dir(&mut self, rel: &str) {
            self.dirs.insert(String::new());
            if rel.is_empty() {
                return;
            }
            let mut current = Vec::new();
            for comp in rel.split('/') {
                if comp.is_empty() {
                    continue;
                }
                current.push(comp.to_string());
                self.dirs.insert(current.join("/"));
            }
        }

        fn write_file(&mut self, rel: &str, contents: &[u8]) {
            if let Some((parent, _)) = rel.rsplit_once('/') {
                self.ensure_dir(parent);
            } else {
                self.ensure_dir("");
            }
            self.files.insert(rel.to_string(), contents.to_vec());
        }

        fn append_file(&mut self, rel: &str, contents: &[u8]) {
            if let Some((parent, _)) = rel.rsplit_once('/') {
                self.ensure_dir(parent);
            } else {
                self.ensure_dir("");
            }
            if let Some(existing) = self.files.get_mut(rel) {
                existing.extend_from_slice(contents);
            } else {
                self.files.insert(rel.to_string(), contents.to_vec());
            }
        }

        fn read_file(&self, rel: &str) -> Result<Vec<u8>> {
            self.files
                .get(rel)
                .cloned()
                .ok_or_else(|| anyhow!("missing file {rel}"))
        }

        fn remove_file(&mut self, rel: &str) {
            self.files.remove(rel);
        }

        fn remove_dir_all(&mut self, rel: &str) {
            let prefix = if rel.is_empty() {
                String::new()
            } else {
                format!("{rel}/")
            };
            self.files
                .retain(|path, _| !path.eq(rel) && !path.starts_with(&prefix));
            self.dirs
                .retain(|dir| !dir.eq(rel) && !dir.starts_with(&prefix));
        }

        fn remove_empty_dir(&mut self, rel: &str) -> Result<()> {
            if !self.dir_exists(rel) {
                bail!("directory does not exist: {rel}");
            }
            if !self.list_children(rel).is_empty() {
                bail!("directory not empty: {rel}");
            }
            self.dirs.remove(rel);
            Ok(())
        }

        fn dir_exists(&self, rel: &str) -> bool {
            rel.is_empty() || self.dirs.contains(rel)
        }

        fn entry_kind(&self, rel: &str) -> Option<EntryKind> {
            if rel.is_empty() {
                return Some(EntryKind::Dir);
            }
            if self.files.contains_key(rel) {
                Some(EntryKind::File)
            } else if self.dirs.contains(rel) {
                Some(EntryKind::Dir)
            } else {
                None
            }
        }

        fn list_children(&self, rel: &str) -> Vec<(String, EntryKind)> {
            let mut entries: BTreeMap<String, EntryKind> = BTreeMap::new();
            let prefix = if rel.is_empty() {
                None
            } else {
                Some(format!("{rel}/"))
            };

            let mut push_child = |child: &str, kind: EntryKind| {
                if child.is_empty() {
                    return;
                }
                entries
                    .entry(child.to_string())
                    .and_modify(|existing| {
                        if matches!(kind, EntryKind::Dir) {
                            *existing = EntryKind::Dir;
                        }
                    })
                    .or_insert(kind);
            };

            for dir in self.dirs.iter() {
                if rel.is_empty() {
                    if dir.is_empty() {
                        continue;
                    }
                    if let Some(child) = dir.split('/').next() {
                        push_child(child, EntryKind::Dir);
                    }
                } else if let Some(prefix) = prefix.as_ref()
                    && let Some(rest) = dir.strip_prefix(prefix)
                {
                    if let Some(child) = rest.split('/').next() {
                        push_child(child, EntryKind::Dir);
                    }
                }
            }

            for file in self.files.keys() {
                if rel.is_empty() {
                    if let Some(child) = file.split('/').next() {
                        push_child(child, EntryKind::File);
                    }
                } else if let Some(prefix) = prefix.as_ref()
                    && let Some(rest) = file.strip_prefix(prefix)
                {
                    if let Some(child) = rest.split('/').next() {
                        push_child(child, EntryKind::File);
                    }
                }
            }

            entries.into_iter().collect()
        }
    }

    pub(super) fn cleanup_root(path: &std::path::Path) {
        if let Some(registry) = STATE_REGISTRY.get() {
            let mut guard = registry.lock().expect("miri state registry poisoned");
            guard.remove(path);
        }
    }
}

#[cfg(miri)]
#[allow(clippy::disallowed_types)]
pub(super) fn cleanup_synthetic_root(root: &std::path::Path) {
    miri_backend::cleanup_root(root);
}

/// Public backend delegator used by `PathResolver`. It holds either a host
/// or miri backend and forwards calls to the concrete implementation.
pub(super) enum Backend {
    #[cfg(not(miri))]
    Host(HostBackend),
    #[cfg(miri)]
    Miri(miri_backend::MiriBackend),
}

impl Clone for Backend {
    fn clone(&self) -> Self {
        match self {
            #[cfg(not(miri))]
            Backend::Host(h) => Backend::Host(*h),
            #[cfg(miri)]
            Backend::Miri(m) => Backend::Miri(m.clone()),
        }
    }
}

impl Backend {
    pub(super) fn new(root: &GuardedPath, build: &GuardedPath) -> Result<Self> {
        #[cfg(miri)]
        {
            Ok(Backend::Miri(miri_backend::MiriBackend::new(root, build)?))
        }
        #[cfg(not(miri))]
        {
            Ok(Backend::Host(HostBackend::new(root, build)?))
        }
    }

    fn as_impl(&self) -> &dyn BackendImpl {
        match self {
            #[cfg(not(miri))]
            Backend::Host(h) => h,
            #[cfg(miri)]
            Backend::Miri(m) => m,
        }
    }

    pub(super) fn create_dir_all(&self, root: &GuardedPath, path: &GuardedPath) -> Result<()> {
        self.as_impl().create_dir_all(root, path)
    }

    pub(super) fn read_dir_entries(&self, path: &GuardedPath) -> Result<Vec<DirEntry>> {
        self.as_impl().read_dir_entries(path)
    }

    pub(super) fn read_file(&self, path: &GuardedPath) -> Result<Vec<u8>> {
        self.as_impl().read_file(path)
    }

    pub(super) fn write_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()> {
        self.as_impl().write_file(path, contents)
    }

    pub(super) fn append_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()> {
        self.as_impl().append_file(path, contents)
    }

    pub(super) fn open_read(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Read + Send>> {
        self.as_impl().open_read(path)
    }

    pub(super) fn open_write(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write + Send>> {
        self.as_impl().open_write(path)
    }

    pub(super) fn open_append(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write + Send>> {
        self.as_impl().open_append(path)
    }

    pub(super) fn open_read_nofollow(
        &self,
        path: &GuardedPath,
        expected_root: &GuardedPath,
    ) -> Result<Box<dyn std::io::Read + Send>> {
        self.as_impl().open_read_nofollow(path, expected_root)
    }

    pub(super) fn open_write_nofollow(
        &self,
        path: &GuardedPath,
        expected_root: &GuardedPath,
    ) -> Result<Box<dyn std::io::Write + Send>> {
        self.as_impl().open_write_nofollow(path, expected_root)
    }

    pub(super) fn canonicalize(&self, path: GuardedPath) -> Result<GuardedPath> {
        self.as_impl().canonicalize(path)
    }

    pub(super) fn metadata(&self, path: &GuardedPath) -> Result<std::fs::Metadata> {
        self.as_impl().metadata(path)
    }

    pub(super) fn entry_kind(&self, path: &GuardedPath) -> Result<super::EntryKind> {
        self.as_impl().entry_kind(path)
    }

    pub(super) fn entry_kind_no_follow(&self, path: &GuardedPath) -> Result<super::EntryKind> {
        self.as_impl().entry_kind_no_follow(path)
    }

    pub(super) fn resolve_workdir(&self, resolved: GuardedPath) -> Result<GuardedPath> {
        self.as_impl().resolve_workdir(resolved)
    }

    pub(super) fn resolve_copy_source(&self, guarded: GuardedPath) -> Result<GuardedPath> {
        self.as_impl().resolve_copy_source(guarded)
    }

    pub(super) fn remove_file(&self, path: &GuardedPath) -> Result<()> {
        self.as_impl().remove_file(path)
    }

    pub(super) fn remove_dir_all(&self, path: &GuardedPath) -> Result<()> {
        self.as_impl().remove_dir_all(path)
    }

    pub(super) fn remove_empty_dir(&self, path: &GuardedPath) -> Result<()> {
        self.as_impl().remove_empty_dir(path)
    }
}
