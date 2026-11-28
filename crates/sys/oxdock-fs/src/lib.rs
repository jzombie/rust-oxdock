use oxdock_diagnostics::{Diagnostic, DiagnosticResult};

use crate::diagnostics as fs_diag;

/// Opt into the once-per-process garbage-collection sweep of stale
/// `oxdock-*` tempdirs (PID-lock based). Idempotent; cheap after the first
/// call; errors are logged, never fatal.
///
/// Entry points (CLI binaries, build scripts) call this early instead
/// of relying on constructors to sweep implicitly.
pub fn init_temp_gc() {
    workspace_fs::path::run_temp_cleanup_once();
}

pub fn discover_workspace_root() -> DiagnosticResult<GuardedPath> {
    if let Ok(root) = std::env::var(env::oxdock::WORKSPACE_ROOT) {
        return GuardedPath::new_root_from_str(&root).map_err(|diag| {
            diag.with_contexts(vec![format!(
                "invalid {} {root}",
                env::oxdock::WORKSPACE_ROOT
            )])
        });
    }

    if let Ok(resolver) = PathResolver::from_manifest_env() {
        let manifest_root = resolver.root().as_path();
        if let Some(git_root) = workspace_fs::git::git_root_from_path(manifest_root) {
            return GuardedPath::new_root(&git_root).map_err(|diag| {
                diag.with_contexts(vec![format!(
                    "failed to guard workspace root {}",
                    git_root.display()
                )])
            });
        }
        return GuardedPath::new_root(manifest_root).map_err(|diag| {
            diag.with_contexts(vec![format!(
                "failed to guard manifest root {}",
                manifest_root.display()
            )])
        });
    }

    let cwd = std::env::current_dir().map_err(|e| fs_diag::cwd_determine_failed(&e))?;
    GuardedPath::new_root(&cwd).map_err(|diag| {
        diag.with_contexts(vec![format!(
            "failed to guard current directory {}",
            cwd.display()
        )])
    })
}

/// Transitional lift for bare `std` filesystem failures in the
/// `WorkspaceFs` trait impls below (`anyhow` is only the transport
/// into the single owner `from_anyhow`). These unguarded bodies never
/// carried historic contexts, so the lift preserves bytes exactly.
fn lift_io_err(err: std::io::Error) -> Diagnostic {
    fs_diag::from_anyhow(anyhow::Error::from(err))
}

pub mod diagnostics;
pub mod env;
pub mod workspace_fs;
pub use workspace_fs::git::{GitIdentity, current_head_commit, ensure_git_identity};
pub use workspace_fs::policy::{GuardPolicy, PolicyPath};
pub use workspace_fs::{
    CargoScratch, CopySourceRoot, DirEntry, EntryKind, GuardedPath, GuardedTempDir,
    LazyGuardedTempDir, PathResolver, WorkspaceSelection, reserve_cargo_scratch,
};
pub use workspace_fs::{
    SpillFile, command_path, embed_path, join_native, normalized_path, path_base, path_rel,
    to_forward_slashes,
};

#[allow(clippy::disallowed_types)]
pub use workspace_fs::UnguardedPath;

#[cfg(feature = "mock-fs")]
pub use workspace_fs::mock::MockFs;

/// Mode applied whenever the DSL marks a file executable (`INSTALL`,
/// `SET_EXECUTABLE`): owner rwx, group/others r-x on Unix; the bit is
/// a no-op elsewhere but a missing path fails on every platform
/// (see `set_permissions_mode_unix`).
pub const EXECUTABLE_MODE: u32 = 0o755;

/// Trait implemented by both `GuardedPath` and `UnguardedPath` so callers can
/// rely on a consistent set of path helper methods. This includes a small set
/// of constructors and a `root` accessor so code that needs to treat either
/// type homogenously can do so without ad-hoc helper functions.
pub trait PathLike: Sized + std::fmt::Display {
    /// Audited escape hatch: yields a raw std path reference.
    /// Prefer guarded operations; do not persist the value beyond the
    /// guarded operation that produced it.
    #[allow(clippy::disallowed_types)]
    fn as_path(&self) -> &std::path::Path;
    /// Audited escape hatch: yields a raw std path reference to the guard
    /// root. Prefer guarded operations; do not persist the value beyond the
    /// guarded operation that produced it.
    #[allow(clippy::disallowed_types)]
    fn root(&self) -> &std::path::Path;
    /// Audited escape hatch: yields an owned std path outside the guard.
    /// Prefer guarded operations; do not persist the value beyond the
    /// guarded operation that produced it.
    #[allow(clippy::disallowed_types)]
    fn to_path_buf(&self) -> std::path::PathBuf;
    fn join(&self, rel: &str) -> DiagnosticResult<Self>;
    fn parent(&self) -> Option<Self>;
    // Constructors analogous to those on `GuardedPath`.
    fn new_from_str(root: &str, candidate: &str) -> DiagnosticResult<Self>;
    #[allow(clippy::disallowed_types)]
    fn new_root(root: &std::path::Path) -> DiagnosticResult<Self>;
    fn new_root_from_str(root: &str) -> DiagnosticResult<Self> {
        #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
        Self::new_root(std::path::Path::new(root))
    }
}

/// Trait representing the workspace-scoped filesystem operations provided by
/// this crate. `PathResolver` implements this trait and existing behavior is
/// preserved; the trait exists to allow generic consumers or test doubles to
/// depend on the abstraction rather than the concrete type.
pub trait WorkspaceFs: Send + Sync {
    fn canonicalize(&self, path: &GuardedPath) -> DiagnosticResult<GuardedPath>;

    fn metadata(&self, path: &GuardedPath) -> DiagnosticResult<std::fs::Metadata>;
    #[allow(clippy::disallowed_types)]
    fn metadata_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<std::fs::Metadata>;
    /// Byte length plus real Unix mode bits for one archive member, for
    /// cases where a host `stat` is unavailable (the Miri synthetic
    /// backend has no host filesystem). `None` mode means the caller
    /// should use its conventional default. The default implementation
    /// stats the path, preserving exact native behavior.
    fn archive_member_stat(&self, path: &GuardedPath) -> DiagnosticResult<(u64, Option<u32>)> {
        let meta = self.metadata(path)?;
        let len = meta.len();
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(meta.permissions().mode() & 0o7777)
        };
        #[cfg(not(unix))]
        let mode: Option<u32> = None;
        Ok((len, mode))
    }

    /// Byte length of a file for size fast-paths (drift checks):
    /// `Some(len)` where the backend can stat, `None` where it cannot
    /// (synthetic and mock filesystems have no stat lengths), so the
    /// caller reads bytes instead. Missing files error exactly like
    /// `metadata`, so miss handling stays shared; `None` never means
    /// missing. The default implementation stats the path, preserving
    /// exact native behavior.
    fn file_len(&self, path: &GuardedPath) -> DiagnosticResult<Option<u64>> {
        Ok(Some(self.metadata(path)?.len()))
    }

    fn root(&self) -> &GuardedPath;
    fn build_context(&self) -> &GuardedPath;
    /// Whether this backend reads from the host OS disk. The import-only
    /// entry-tree fallback (`read_import_bytes`) probes the host through
    /// a fresh `PathResolver`, so it must only fire on host-backed
    /// filesystems; synthetic backends (mocks, embedded content) fail
    /// closed with the original error instead of leaking host IO into
    /// hermetic runs. Fail-closed default; `PathResolver` opts in.
    fn is_host_backed(&self) -> bool {
        false
    }
    /// Selection last applied through `switch_to_*`: the four-way
    /// workspace selector plus the cache flavor. Scope save/restore
    /// and module-context reset round-trip through this instead of
    /// re-deriving selection from a saved path.
    fn selection(&self) -> WorkspaceSelection;
    /// Restore a selection previously read from [`Self::selection`].
    /// Equivalent to re-running the matching `switch_to_*`.
    fn restore_selection(&mut self, selection: WorkspaceSelection);
    /// Project root recorded via `set_workspace_root`, when one is set.
    fn workspace_root(&self) -> Option<&GuardedPath>;

    /// Select the snapshot root without touching the disk (`WORKSPACE
    /// SNAPSHOT`). The pending anchor serves lexical joins until the first
    /// snapshot-targeted choke point swaps in the concrete root.
    fn switch_to_snapshot(&mut self);
    /// Select the local (build-context) root (`WORKSPACE LOCAL`). Never
    /// touches the snapshot handle.
    fn switch_to_local(&mut self);
    /// Select the persistent cache root (`WORKSPACE CACHE`). With `local`,
    /// the cache lives in the project tree instead of the OS user cache.
    /// The directory is created on first cache-targeted resolve and never
    /// deleted.
    fn switch_to_cache(&mut self, local: bool);
    /// Select full filesystem access (`WORKSPACE SYSTEM`). Resolution
    /// bypasses root-prefix containment; not hermetic.
    fn switch_to_system(&mut self);
    /// Entry working directory for `WORKSPACE SYSTEM`: rebase a
    /// snapshot-anchor-rooted `cwd` onto the concrete root (or the system
    /// anchor while pending) so unconfined resolution never materializes
    /// the never-created virtual anchor as a side effect. Passes every
    /// other `cwd` through untouched.
    fn system_entry_cwd(&self, cwd: &GuardedPath) -> GuardedPath;
    /// True while snapshot-selected but not yet materialized (drives the
    /// pending display sentinel defined as
    /// `oxdock_core::SNAPSHOT_PENDING_DISPLAY`).
    fn is_snapshot_pending(&self) -> bool;
    /// Map a possibly anchor-rooted path onto the shared concrete root when
    /// materialized; otherwise return it unchanged. Best-effort convergence
    /// for final-cwd reporting and display. Never creates.
    fn concretize_cwd(&self, cwd: &GuardedPath) -> GuardedPath;

    fn read_file(&self, path: &GuardedPath) -> DiagnosticResult<Vec<u8>>;
    #[allow(clippy::disallowed_types)]
    fn read_file_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<Vec<u8>>;

    fn open_read(&self, path: &GuardedPath) -> DiagnosticResult<Box<dyn std::io::Read + Send>>;
    fn open_write(&self, path: &GuardedPath) -> DiagnosticResult<Box<dyn std::io::Write + Send>>;
    fn open_append(&self, path: &GuardedPath) -> DiagnosticResult<Box<dyn std::io::Write + Send>>;
    /// Link-refusing opens for probe-then-act sequences (archive
    /// pack/unpack): a planted final-component symlink fails instead of
    /// redirecting bytes, and the opened descriptor's own path must
    /// stay under `expected_root`, closing the validation-to-open race
    /// on descriptor-pinned IO. Guard checks still run first.
    fn open_read_nofollow(
        &self,
        path: &GuardedPath,
        expected_root: &GuardedPath,
    ) -> DiagnosticResult<Box<dyn std::io::Read + Send>>;
    fn open_write_nofollow(
        &self,
        path: &GuardedPath,
        expected_root: &GuardedPath,
    ) -> DiagnosticResult<Box<dyn std::io::Write + Send>>;

    fn read_to_string(&self, path: &GuardedPath) -> DiagnosticResult<String>;
    #[allow(clippy::disallowed_types)]
    fn read_to_string_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<String>;

    fn read_dir_entries(&self, path: &GuardedPath) -> DiagnosticResult<Vec<DirEntry>>;
    #[allow(clippy::disallowed_types)]
    fn read_dir_entries_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<Vec<DirEntry>>;

    fn write_file(&self, path: &GuardedPath, contents: &[u8]) -> DiagnosticResult<()>;
    #[allow(clippy::disallowed_types)]
    fn write_file_unguarded(&self, path: &UnguardedPath, contents: &[u8]) -> DiagnosticResult<()>;

    fn append_file(&self, path: &GuardedPath, contents: &[u8]) -> DiagnosticResult<()>;
    #[allow(clippy::disallowed_types)]
    fn append_file_unguarded(&self, path: &UnguardedPath, contents: &[u8]) -> DiagnosticResult<()>;

    fn create_dir_all(&self, path: &GuardedPath) -> DiagnosticResult<()>;
    #[allow(clippy::disallowed_types)]
    fn create_dir_all_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<()>;
    fn ensure_parent_dir(&self, path: &GuardedPath) -> DiagnosticResult<()>;

    fn remove_file(&self, path: &GuardedPath) -> DiagnosticResult<()>;
    #[allow(clippy::disallowed_types)]
    fn remove_file_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<()>;

    fn remove_dir_all(&self, path: &GuardedPath) -> DiagnosticResult<()>;
    #[allow(clippy::disallowed_types)]
    fn remove_dir_all_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<()>;

    /// Remove an empty directory. Fails when the directory holds entries.
    fn remove_empty_dir(&self, path: &GuardedPath) -> DiagnosticResult<()>;

    /// Remove the symlink at the lexical path (`cwd`-relative `rel`) when
    /// one stands there, deleting the link itself, never its target.
    /// Returns whether a link was removed.
    fn remove_link_if_symlink(&self, cwd: &GuardedPath, rel: &str) -> DiagnosticResult<bool>;

    fn copy_file(&self, src: &GuardedPath, dst: &GuardedPath) -> DiagnosticResult<u64>;
    #[allow(clippy::disallowed_types)]
    fn copy_file_unguarded(
        &self,
        src: &UnguardedPath,
        dst: &UnguardedPath,
    ) -> DiagnosticResult<u64>;
    #[allow(clippy::disallowed_types)]
    fn copy_file_from_unguarded(
        &self,
        src: &UnguardedPath,
        dst: &GuardedPath,
    ) -> DiagnosticResult<u64>;
    #[allow(clippy::disallowed_types)]
    fn copy_file_to_unguarded(
        &self,
        src: &GuardedPath,
        dst: &UnguardedPath,
    ) -> DiagnosticResult<u64>;

    fn copy_dir_recursive(&self, src: &GuardedPath, dst: &GuardedPath) -> DiagnosticResult<()>;
    #[allow(clippy::disallowed_types)]
    fn copy_dir_from_unguarded(
        &self,
        src: &UnguardedPath,
        dst: &GuardedPath,
    ) -> DiagnosticResult<()>;

    /// Atomically place `src` at `dst` with unix `mode` (`INSTALL` passes
    /// `0o755`). Stages as a temp file in the dest dir, sets the mode,
    /// then renames, so the final path never carries the wrong mode.
    fn install_file_atomic(
        &self,
        src: &GuardedPath,
        dst: &GuardedPath,
        mode: u32,
    ) -> DiagnosticResult<()>;

    fn symlink(&self, src: &GuardedPath, dst: &GuardedPath) -> DiagnosticResult<()>;

    #[allow(clippy::disallowed_types)]
    fn open_file_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<std::fs::File>;
    fn set_permissions_mode_unix(&self, path: &GuardedPath, mode: u32) -> DiagnosticResult<()>;
    #[allow(clippy::disallowed_types)]
    fn set_permissions_mode_unix_unguarded(
        &self,
        path: &UnguardedPath,
        mode: u32,
    ) -> DiagnosticResult<()>;

    /// Mark `path` executable (`EXECUTABLE_MODE` on Unix, bit no-op
    /// elsewhere). Shared by `INSTALL`'s staging step and the
    /// `SET_EXECUTABLE` command so both name the intent instead of
    /// repeating the mode. A missing path fails everywhere: only the
    /// bit is platform-specific.
    fn set_executable(&self, path: &GuardedPath) -> DiagnosticResult<()> {
        self.set_permissions_mode_unix(path, EXECUTABLE_MODE)
    }

    fn resolve_workdir(
        &self,
        current: &GuardedPath,
        new_dir: &str,
    ) -> DiagnosticResult<GuardedPath>;
    fn resolve_read(&self, cwd: &GuardedPath, rel: &str) -> DiagnosticResult<GuardedPath>;
    fn resolve_write(&self, cwd: &GuardedPath, rel: &str) -> DiagnosticResult<GuardedPath>;
    fn resolve_copy_source(&self, from: &str) -> DiagnosticResult<GuardedPath>;
    fn resolve_copy_source_from_workspace(&self, from: &str) -> DiagnosticResult<GuardedPath>;
    fn resolve_copy_source_from_target(
        &self,
        root: CopySourceRoot,
        from: &str,
    ) -> DiagnosticResult<GuardedPath>;

    fn entry_kind(&self, path: &GuardedPath) -> DiagnosticResult<EntryKind>;
    #[allow(clippy::disallowed_types)]
    fn entry_kind_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<EntryKind>;

    /// No-follow variant of [`Self::entry_kind`]: reports symlinks as
    /// [`EntryKind::Symlink`] instead of resolving through them.
    fn entry_kind_no_follow(&self, path: &GuardedPath) -> DiagnosticResult<EntryKind>;

    fn copy_from_git(
        &self,
        rev: &str,
        from: &str,
        to: &GuardedPath,
        include_dirty: bool,
    ) -> DiagnosticResult<()>;

    /// Clone this filesystem handle into a boxed trait object.
    /// For `PathResolver`, produces an independent copy with its own root setting.
    /// For `MockFs`, produces a shared reference to the same in-memory state.
    fn clone_box(&self) -> Box<dyn WorkspaceFs>;
}

impl WorkspaceFs for PathResolver {
    fn canonicalize(&self, path: &GuardedPath) -> DiagnosticResult<GuardedPath> {
        PathResolver::canonicalize(self, path)
    }

    fn metadata(&self, path: &GuardedPath) -> DiagnosticResult<std::fs::Metadata> {
        PathResolver::metadata(self, path)
    }

    fn archive_member_stat(&self, path: &GuardedPath) -> DiagnosticResult<(u64, Option<u32>)> {
        PathResolver::archive_member_stat(self, path)
    }

    /// Miri has no host stat, and fabricated metadata cannot report
    /// true lengths: presence alone decides. Present entries answer
    /// `None` so the caller reads bytes; missing entries error like
    /// a failed read, so miss handling stays shared.
    #[cfg(miri)]
    fn file_len(&self, path: &GuardedPath) -> DiagnosticResult<Option<u64>> {
        if PathResolver::exists(self, path) {
            Ok(None)
        } else {
            Err(fs_diag::missing_file(path))
        }
    }

    #[allow(clippy::disallowed_types)]
    fn metadata_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<std::fs::Metadata> {
        PathResolver::metadata_unguarded(self, path)
    }

    fn root(&self) -> &GuardedPath {
        PathResolver::root(self)
    }

    fn is_host_backed(&self) -> bool {
        true
    }

    fn workspace_root(&self) -> Option<&GuardedPath> {
        PathResolver::workspace_root(self)
    }

    fn build_context(&self) -> &GuardedPath {
        PathResolver::build_context(self)
    }

    fn selection(&self) -> WorkspaceSelection {
        PathResolver::selection(self)
    }

    fn restore_selection(&mut self, selection: WorkspaceSelection) {
        PathResolver::restore_selection(self, selection)
    }

    fn switch_to_snapshot(&mut self) {
        PathResolver::switch_to_snapshot(self)
    }

    fn switch_to_local(&mut self) {
        PathResolver::switch_to_local(self)
    }

    fn switch_to_cache(&mut self, local: bool) {
        PathResolver::switch_to_cache(self, local)
    }

    fn switch_to_system(&mut self) {
        PathResolver::switch_to_system(self)
    }

    fn system_entry_cwd(&self, cwd: &GuardedPath) -> GuardedPath {
        PathResolver::system_entry_cwd(self, cwd)
    }

    fn is_snapshot_pending(&self) -> bool {
        PathResolver::is_snapshot_pending(self)
    }

    fn concretize_cwd(&self, cwd: &GuardedPath) -> GuardedPath {
        PathResolver::concretize_cwd(self, cwd)
    }

    fn read_file(&self, path: &GuardedPath) -> DiagnosticResult<Vec<u8>> {
        PathResolver::read_file(self, path)
    }

    #[allow(clippy::disallowed_types)]
    fn read_file_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<Vec<u8>> {
        #[allow(clippy::disallowed_methods)]
        std::fs::read(path.as_path()).map_err(lift_io_err)
    }

    fn open_read(&self, path: &GuardedPath) -> DiagnosticResult<Box<dyn std::io::Read + Send>> {
        PathResolver::open_read(self, path)
    }

    fn open_write(&self, path: &GuardedPath) -> DiagnosticResult<Box<dyn std::io::Write + Send>> {
        PathResolver::open_write(self, path)
    }

    fn open_append(&self, path: &GuardedPath) -> DiagnosticResult<Box<dyn std::io::Write + Send>> {
        PathResolver::open_append(self, path)
    }

    fn open_read_nofollow(
        &self,
        path: &GuardedPath,
        expected_root: &GuardedPath,
    ) -> DiagnosticResult<Box<dyn std::io::Read + Send>> {
        PathResolver::open_read_nofollow(self, path, expected_root)
    }

    fn open_write_nofollow(
        &self,
        path: &GuardedPath,
        expected_root: &GuardedPath,
    ) -> DiagnosticResult<Box<dyn std::io::Write + Send>> {
        PathResolver::open_write_nofollow(self, path, expected_root)
    }

    fn read_to_string(&self, path: &GuardedPath) -> DiagnosticResult<String> {
        PathResolver::read_to_string(self, path)
    }

    #[allow(clippy::disallowed_types)]
    fn read_to_string_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<String> {
        #[allow(clippy::disallowed_methods)]
        std::fs::read_to_string(path.as_path()).map_err(lift_io_err)
    }

    fn read_dir_entries(&self, path: &GuardedPath) -> DiagnosticResult<Vec<DirEntry>> {
        PathResolver::read_dir_entries(self, path)
    }

    #[allow(clippy::disallowed_types)]
    fn read_dir_entries_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<Vec<DirEntry>> {
        #[cfg(not(miri))]
        {
            #[allow(clippy::disallowed_methods)]
            let entries = std::fs::read_dir(path.as_path())
                .map_err(lift_io_err)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(lift_io_err)?;
            Ok(entries)
        }
        #[cfg(miri)]
        {
            let _ = path;
            Err(fs_diag::unguarded_read_dir_miri())
        }
    }

    fn write_file(&self, path: &GuardedPath, contents: &[u8]) -> DiagnosticResult<()> {
        PathResolver::write_file(self, path, contents)
    }

    #[allow(clippy::disallowed_types)]
    fn write_file_unguarded(&self, path: &UnguardedPath, contents: &[u8]) -> DiagnosticResult<()> {
        #[allow(clippy::disallowed_methods)]
        std::fs::write(path.as_path(), contents).map_err(lift_io_err)
    }

    fn append_file(&self, path: &GuardedPath, contents: &[u8]) -> DiagnosticResult<()> {
        PathResolver::append_file(self, path, contents)
    }

    #[allow(clippy::disallowed_types)]
    fn append_file_unguarded(&self, path: &UnguardedPath, contents: &[u8]) -> DiagnosticResult<()> {
        #[allow(clippy::disallowed_methods)]
        {
            use std::fs::OpenOptions;
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path.as_path())
                .map_err(lift_io_err)?;
            std::io::Write::write_all(&mut file, contents).map_err(lift_io_err)?;
        }
        Ok(())
    }

    fn create_dir_all(&self, path: &GuardedPath) -> DiagnosticResult<()> {
        PathResolver::create_dir_all(self, path)
    }

    #[allow(clippy::disallowed_types)]
    fn create_dir_all_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<()> {
        #[allow(clippy::disallowed_methods)]
        std::fs::create_dir_all(path.as_path()).map_err(lift_io_err)
    }

    fn ensure_parent_dir(&self, path: &GuardedPath) -> DiagnosticResult<()> {
        PathResolver::ensure_parent_dir(self, path)
    }

    fn remove_file(&self, path: &GuardedPath) -> DiagnosticResult<()> {
        PathResolver::remove_file(self, path)
    }

    #[allow(clippy::disallowed_types)]
    fn remove_file_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<()> {
        #[allow(clippy::disallowed_methods)]
        std::fs::remove_file(path.as_path()).map_err(lift_io_err)
    }

    fn remove_dir_all(&self, path: &GuardedPath) -> DiagnosticResult<()> {
        PathResolver::remove_dir_all(self, path)
    }

    fn remove_empty_dir(&self, path: &GuardedPath) -> DiagnosticResult<()> {
        PathResolver::remove_empty_dir(self, path)
    }

    fn remove_link_if_symlink(&self, cwd: &GuardedPath, rel: &str) -> DiagnosticResult<bool> {
        PathResolver::remove_link_if_symlink(self, cwd, rel)
    }

    #[allow(clippy::disallowed_types)]
    fn remove_dir_all_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<()> {
        #[allow(clippy::disallowed_methods)]
        std::fs::remove_dir_all(path.as_path()).map_err(lift_io_err)
    }

    fn copy_file(&self, src: &GuardedPath, dst: &GuardedPath) -> DiagnosticResult<u64> {
        PathResolver::copy_file(self, src, dst)
    }

    #[allow(clippy::disallowed_types)]
    fn copy_file_unguarded(
        &self,
        src: &UnguardedPath,
        dst: &UnguardedPath,
    ) -> DiagnosticResult<u64> {
        #[allow(clippy::disallowed_methods)]
        std::fs::copy(src.as_path(), dst.as_path()).map_err(lift_io_err)
    }

    #[allow(clippy::disallowed_types)]
    fn copy_file_from_unguarded(
        &self,
        src: &UnguardedPath,
        dst: &GuardedPath,
    ) -> DiagnosticResult<u64> {
        PathResolver::copy_file_from_unguarded(self, src, dst)
    }

    #[allow(clippy::disallowed_types)]
    fn copy_file_to_unguarded(
        &self,
        src: &GuardedPath,
        dst: &UnguardedPath,
    ) -> DiagnosticResult<u64> {
        #[allow(clippy::disallowed_methods)]
        std::fs::copy(src.as_path(), dst.as_path()).map_err(lift_io_err)
    }

    fn copy_dir_recursive(&self, src: &GuardedPath, dst: &GuardedPath) -> DiagnosticResult<()> {
        PathResolver::copy_dir_recursive(self, src, dst)
    }

    #[allow(clippy::disallowed_types)]
    fn copy_dir_from_unguarded(
        &self,
        src: &UnguardedPath,
        dst: &GuardedPath,
    ) -> DiagnosticResult<()> {
        PathResolver::copy_dir_from_unguarded(self, src, dst)
    }

    fn symlink(&self, src: &GuardedPath, dst: &GuardedPath) -> DiagnosticResult<()> {
        PathResolver::symlink(self, src, dst)
    }

    fn install_file_atomic(
        &self,
        src: &GuardedPath,
        dst: &GuardedPath,
        mode: u32,
    ) -> oxdock_diagnostics::DiagnosticResult<()> {
        PathResolver::install_file_atomic(self, src, dst, mode)
    }

    #[allow(clippy::disallowed_types)]
    fn open_file_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<std::fs::File> {
        PathResolver::open_file_unguarded(self, path)
    }

    fn set_permissions_mode_unix(&self, path: &GuardedPath, mode: u32) -> DiagnosticResult<()> {
        PathResolver::set_permissions_mode_unix(self, path, mode)
    }

    #[allow(clippy::disallowed_types)]
    fn set_permissions_mode_unix_unguarded(
        &self,
        path: &UnguardedPath,
        mode: u32,
    ) -> DiagnosticResult<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            #[allow(clippy::disallowed_methods)]
            let metadata = std::fs::metadata(path.as_path()).map_err(lift_io_err)?;
            let mut perms = metadata.permissions();
            perms.set_mode(mode);
            #[allow(clippy::disallowed_methods)]
            std::fs::set_permissions(path.as_path(), perms).map_err(lift_io_err)?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
            Ok(())
        }
    }

    fn resolve_workdir(
        &self,
        current: &GuardedPath,
        new_dir: &str,
    ) -> DiagnosticResult<GuardedPath> {
        PathResolver::resolve_workdir(self, current, new_dir)
    }

    fn resolve_read(&self, cwd: &GuardedPath, rel: &str) -> DiagnosticResult<GuardedPath> {
        PathResolver::resolve_read(self, cwd, rel)
    }

    fn resolve_write(&self, cwd: &GuardedPath, rel: &str) -> DiagnosticResult<GuardedPath> {
        PathResolver::resolve_write(self, cwd, rel)
    }

    fn resolve_copy_source(&self, from: &str) -> DiagnosticResult<GuardedPath> {
        PathResolver::resolve_copy_source(self, from)
    }

    fn resolve_copy_source_from_workspace(&self, from: &str) -> DiagnosticResult<GuardedPath> {
        PathResolver::resolve_copy_source_from_workspace(self, from)
    }

    fn resolve_copy_source_from_target(
        &self,
        root: CopySourceRoot,
        from: &str,
    ) -> DiagnosticResult<GuardedPath> {
        PathResolver::resolve_copy_source_from_target(self, root, from)
    }

    fn entry_kind(&self, path: &GuardedPath) -> DiagnosticResult<EntryKind> {
        PathResolver::entry_kind(self, path)
    }

    fn entry_kind_no_follow(&self, path: &GuardedPath) -> DiagnosticResult<EntryKind> {
        PathResolver::entry_kind_no_follow(self, path)
    }

    #[allow(clippy::disallowed_types)]
    fn entry_kind_unguarded(&self, path: &UnguardedPath) -> DiagnosticResult<EntryKind> {
        #[allow(clippy::disallowed_methods)]
        let metadata = std::fs::metadata(path.as_path()).map_err(lift_io_err)?;
        if metadata.is_dir() {
            Ok(EntryKind::Dir)
        } else {
            Ok(EntryKind::File)
        }
    }

    fn copy_from_git(
        &self,
        rev: &str,
        from: &str,
        to: &GuardedPath,
        include_dirty: bool,
    ) -> DiagnosticResult<()> {
        PathResolver::copy_from_git(self, rev, from, to, include_dirty)
    }

    fn clone_box(&self) -> Box<dyn WorkspaceFs> {
        Box::new(self.clone())
    }
}
