use anyhow::{Context, Result};

/// Opt into the once-per-process garbage-collection sweep of stale
/// `oxdock-*` tempdirs (PID-lock based). Idempotent; cheap after the first
/// call; errors are logged, never fatal.
///
/// Composition roots (CLI binaries, build scripts) call this early instead
/// of relying on constructors to sweep implicitly.
pub fn init_temp_gc() {
    workspace_fs::path::run_temp_cleanup_once();
}

pub fn discover_workspace_root() -> Result<GuardedPath> {
    if let Ok(root) = std::env::var("OXDOCK_WORKSPACE_ROOT") {
        return GuardedPath::new_root_from_str(&root)
            .with_context(|| format!("invalid OXDOCK_WORKSPACE_ROOT {}", root));
    }

    if let Ok(resolver) = PathResolver::from_manifest_env() {
        let manifest_root = resolver.root().as_path();
        if let Some(git_root) = workspace_fs::git::git_root_from_path(manifest_root) {
            return GuardedPath::new_root(&git_root)
                .with_context(|| format!("failed to guard workspace root {}", git_root.display()));
        }
        return GuardedPath::new_root(manifest_root)
            .with_context(|| format!("failed to guard manifest root {}", manifest_root.display()));
    }

    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    GuardedPath::new_root(&cwd)
        .with_context(|| format!("failed to guard current directory {}", cwd.display()))
}

pub mod workspace_fs;
pub use workspace_fs::git::{GitIdentity, current_head_commit, ensure_git_identity};
pub use workspace_fs::policy::{GuardPolicy, PolicyPath};
pub use workspace_fs::{DirEntry, EntryKind, GuardedPath, GuardedTempDir, PathResolver};
pub use workspace_fs::{command_path, embed_path, normalized_path, to_forward_slashes};

#[allow(clippy::disallowed_types)]
pub use workspace_fs::UnguardedPath;

#[cfg(feature = "mock-fs")]
pub use workspace_fs::mock::MockFs;

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
    fn join(&self, rel: &str) -> Result<Self>;
    fn parent(&self) -> Option<Self>;
    // Constructors analogous to those on `GuardedPath`.
    fn new_from_str(root: &str, candidate: &str) -> Result<Self>;
    #[allow(clippy::disallowed_types)]
    fn new_root(root: &std::path::Path) -> Result<Self>;
    fn new_root_from_str(root: &str) -> Result<Self> {
        #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
        Self::new_root(std::path::Path::new(root))
    }
}

/// Trait representing the workspace-scoped filesystem operations provided by
/// this crate. `PathResolver` implements this trait and existing behavior is
/// preserved; the trait exists to allow generic consumers or test doubles to
/// depend on the abstraction rather than the concrete type.
pub trait WorkspaceFs: Send + Sync {
    fn canonicalize(&self, path: &GuardedPath) -> Result<GuardedPath>;

    fn metadata(&self, path: &GuardedPath) -> Result<std::fs::Metadata>;
    #[allow(clippy::disallowed_types)]
    fn metadata_unguarded(&self, path: &UnguardedPath) -> Result<std::fs::Metadata>;

    fn root(&self) -> &GuardedPath;
    fn build_context(&self) -> &GuardedPath;
    fn set_root(&mut self, root: &GuardedPath);

    fn read_file(&self, path: &GuardedPath) -> Result<Vec<u8>>;
    #[allow(clippy::disallowed_types)]
    fn read_file_unguarded(&self, path: &UnguardedPath) -> Result<Vec<u8>>;

    fn open_read(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Read + Send>>;
    fn open_write(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write + Send>>;
    fn open_append(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write + Send>>;

    fn read_to_string(&self, path: &GuardedPath) -> Result<String>;
    #[allow(clippy::disallowed_types)]
    fn read_to_string_unguarded(&self, path: &UnguardedPath) -> Result<String>;

    fn read_dir_entries(&self, path: &GuardedPath) -> Result<Vec<DirEntry>>;
    #[allow(clippy::disallowed_types)]
    fn read_dir_entries_unguarded(&self, path: &UnguardedPath) -> Result<Vec<DirEntry>>;

    fn write_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()>;
    #[allow(clippy::disallowed_types)]
    fn write_file_unguarded(&self, path: &UnguardedPath, contents: &[u8]) -> Result<()>;

    fn append_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()>;
    #[allow(clippy::disallowed_types)]
    fn append_file_unguarded(&self, path: &UnguardedPath, contents: &[u8]) -> Result<()>;

    fn create_dir_all(&self, path: &GuardedPath) -> Result<()>;
    #[allow(clippy::disallowed_types)]
    fn create_dir_all_unguarded(&self, path: &UnguardedPath) -> Result<()>;
    fn ensure_parent_dir(&self, path: &GuardedPath) -> Result<()>;

    fn remove_file(&self, path: &GuardedPath) -> Result<()>;
    #[allow(clippy::disallowed_types)]
    fn remove_file_unguarded(&self, path: &UnguardedPath) -> Result<()>;

    fn remove_dir_all(&self, path: &GuardedPath) -> Result<()>;
    #[allow(clippy::disallowed_types)]
    fn remove_dir_all_unguarded(&self, path: &UnguardedPath) -> Result<()>;

    fn copy_file(&self, src: &GuardedPath, dst: &GuardedPath) -> Result<u64>;
    #[allow(clippy::disallowed_types)]
    fn copy_file_unguarded(&self, src: &UnguardedPath, dst: &UnguardedPath) -> Result<u64>;
    #[allow(clippy::disallowed_types)]
    fn copy_file_from_unguarded(&self, src: &UnguardedPath, dst: &GuardedPath) -> Result<u64>;
    #[allow(clippy::disallowed_types)]
    fn copy_file_to_unguarded(&self, src: &GuardedPath, dst: &UnguardedPath) -> Result<u64>;

    fn copy_dir_recursive(&self, src: &GuardedPath, dst: &GuardedPath) -> Result<()>;
    #[allow(clippy::disallowed_types)]
    fn copy_dir_from_unguarded(&self, src: &UnguardedPath, dst: &GuardedPath) -> Result<()>;

    fn symlink(&self, src: &GuardedPath, dst: &GuardedPath) -> Result<()>;

    #[allow(clippy::disallowed_types)]
    fn open_file_unguarded(&self, path: &UnguardedPath) -> Result<std::fs::File>;
    fn set_permissions_mode_unix(&self, path: &GuardedPath, mode: u32) -> Result<()>;
    #[allow(clippy::disallowed_types)]
    fn set_permissions_mode_unix_unguarded(&self, path: &UnguardedPath, mode: u32) -> Result<()>;

    fn resolve_workdir(&self, current: &GuardedPath, new_dir: &str) -> Result<GuardedPath>;
    fn resolve_read(&self, cwd: &GuardedPath, rel: &str) -> Result<GuardedPath>;
    fn resolve_write(&self, cwd: &GuardedPath, rel: &str) -> Result<GuardedPath>;
    fn resolve_copy_source(&self, from: &str) -> Result<GuardedPath>;
    fn resolve_copy_source_from_workspace(&self, from: &str) -> Result<GuardedPath>;

    fn entry_kind(&self, path: &GuardedPath) -> Result<EntryKind>;
    #[allow(clippy::disallowed_types)]
    fn entry_kind_unguarded(&self, path: &UnguardedPath) -> Result<EntryKind>;

    fn copy_from_git(
        &self,
        rev: &str,
        from: &str,
        to: &GuardedPath,
        include_dirty: bool,
    ) -> Result<()>;

    /// Clone this filesystem handle into a boxed trait object.
    /// For `PathResolver`, produces an independent copy with its own root setting.
    /// For `MockFs`, produces a shared reference to the same in-memory state.
    fn clone_box(&self) -> Box<dyn WorkspaceFs>;
}

impl WorkspaceFs for PathResolver {
    fn canonicalize(&self, path: &GuardedPath) -> Result<GuardedPath> {
        PathResolver::canonicalize(self, path)
    }

    fn metadata(&self, path: &GuardedPath) -> Result<std::fs::Metadata> {
        PathResolver::metadata(self, path)
    }

    #[allow(clippy::disallowed_types)]
    fn metadata_unguarded(&self, path: &UnguardedPath) -> Result<std::fs::Metadata> {
        PathResolver::metadata_unguarded(self, path)
    }

    fn root(&self) -> &GuardedPath {
        PathResolver::root(self)
    }

    fn build_context(&self) -> &GuardedPath {
        PathResolver::build_context(self)
    }

    fn set_root(&mut self, root: &GuardedPath) {
        PathResolver::set_root(self, root)
    }

    fn read_file(&self, path: &GuardedPath) -> Result<Vec<u8>> {
        PathResolver::read_file(self, path)
    }

    #[allow(clippy::disallowed_types)]
    fn read_file_unguarded(&self, path: &UnguardedPath) -> Result<Vec<u8>> {
        #[allow(clippy::disallowed_methods)]
        Ok(std::fs::read(path.as_path())?)
    }

    fn open_read(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Read + Send>> {
        PathResolver::open_read(self, path)
    }

    fn open_write(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write + Send>> {
        PathResolver::open_write(self, path)
    }

    fn open_append(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write + Send>> {
        PathResolver::open_append(self, path)
    }

    fn read_to_string(&self, path: &GuardedPath) -> Result<String> {
        PathResolver::read_to_string(self, path)
    }

    #[allow(clippy::disallowed_types)]
    fn read_to_string_unguarded(&self, path: &UnguardedPath) -> Result<String> {
        #[allow(clippy::disallowed_methods)]
        Ok(std::fs::read_to_string(path.as_path())?)
    }

    fn read_dir_entries(&self, path: &GuardedPath) -> Result<Vec<DirEntry>> {
        PathResolver::read_dir_entries(self, path)
    }

    #[allow(clippy::disallowed_types)]
    fn read_dir_entries_unguarded(&self, path: &UnguardedPath) -> Result<Vec<DirEntry>> {
        #[cfg(not(miri))]
        {
            #[allow(clippy::disallowed_methods)]
            let entries = std::fs::read_dir(path.as_path())?.collect::<Result<Vec<_>, _>>()?;
            Ok(entries)
        }
        #[cfg(miri)]
        {
            let _ = path;
            anyhow::bail!("Unguarded read_dir not supported on Miri")
        }
    }

    fn write_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()> {
        PathResolver::write_file(self, path, contents)
    }

    #[allow(clippy::disallowed_types)]
    fn write_file_unguarded(&self, path: &UnguardedPath, contents: &[u8]) -> Result<()> {
        #[allow(clippy::disallowed_methods)]
        Ok(std::fs::write(path.as_path(), contents)?)
    }

    fn append_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()> {
        PathResolver::append_file(self, path, contents)
    }

    #[allow(clippy::disallowed_types)]
    fn append_file_unguarded(&self, path: &UnguardedPath, contents: &[u8]) -> Result<()> {
        #[allow(clippy::disallowed_methods)]
        {
            use std::fs::OpenOptions;
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path.as_path())?;
            std::io::Write::write_all(&mut file, contents)?;
        }
        Ok(())
    }

    fn create_dir_all(&self, path: &GuardedPath) -> Result<()> {
        PathResolver::create_dir_all(self, path)
    }

    #[allow(clippy::disallowed_types)]
    fn create_dir_all_unguarded(&self, path: &UnguardedPath) -> Result<()> {
        #[allow(clippy::disallowed_methods)]
        Ok(std::fs::create_dir_all(path.as_path())?)
    }

    fn ensure_parent_dir(&self, path: &GuardedPath) -> Result<()> {
        PathResolver::ensure_parent_dir(self, path)
    }

    fn remove_file(&self, path: &GuardedPath) -> Result<()> {
        PathResolver::remove_file(self, path)
    }

    #[allow(clippy::disallowed_types)]
    fn remove_file_unguarded(&self, path: &UnguardedPath) -> Result<()> {
        #[allow(clippy::disallowed_methods)]
        Ok(std::fs::remove_file(path.as_path())?)
    }

    fn remove_dir_all(&self, path: &GuardedPath) -> Result<()> {
        PathResolver::remove_dir_all(self, path)
    }

    #[allow(clippy::disallowed_types)]
    fn remove_dir_all_unguarded(&self, path: &UnguardedPath) -> Result<()> {
        #[allow(clippy::disallowed_methods)]
        Ok(std::fs::remove_dir_all(path.as_path())?)
    }

    fn copy_file(&self, src: &GuardedPath, dst: &GuardedPath) -> Result<u64> {
        PathResolver::copy_file(self, src, dst)
    }

    #[allow(clippy::disallowed_types)]
    fn copy_file_unguarded(&self, src: &UnguardedPath, dst: &UnguardedPath) -> Result<u64> {
        #[allow(clippy::disallowed_methods)]
        Ok(std::fs::copy(src.as_path(), dst.as_path())?)
    }

    #[allow(clippy::disallowed_types)]
    fn copy_file_from_unguarded(&self, src: &UnguardedPath, dst: &GuardedPath) -> Result<u64> {
        PathResolver::copy_file_from_unguarded(self, src, dst)
    }

    #[allow(clippy::disallowed_types)]
    fn copy_file_to_unguarded(&self, src: &GuardedPath, dst: &UnguardedPath) -> Result<u64> {
        #[allow(clippy::disallowed_methods)]
        Ok(std::fs::copy(src.as_path(), dst.as_path())?)
    }

    fn copy_dir_recursive(&self, src: &GuardedPath, dst: &GuardedPath) -> Result<()> {
        PathResolver::copy_dir_recursive(self, src, dst)
    }

    #[allow(clippy::disallowed_types)]
    fn copy_dir_from_unguarded(&self, src: &UnguardedPath, dst: &GuardedPath) -> Result<()> {
        PathResolver::copy_dir_from_unguarded(self, src, dst)
    }

    fn symlink(&self, src: &GuardedPath, dst: &GuardedPath) -> Result<()> {
        PathResolver::symlink(self, src, dst)
    }

    #[allow(clippy::disallowed_types)]
    fn open_file_unguarded(&self, path: &UnguardedPath) -> Result<std::fs::File> {
        PathResolver::open_file_unguarded(self, path)
    }

    fn set_permissions_mode_unix(&self, path: &GuardedPath, mode: u32) -> Result<()> {
        PathResolver::set_permissions_mode_unix(self, path, mode)
    }

    #[allow(clippy::disallowed_types)]
    fn set_permissions_mode_unix_unguarded(&self, path: &UnguardedPath, mode: u32) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            #[allow(clippy::disallowed_methods)]
            let metadata = std::fs::metadata(path.as_path())?;
            let mut perms = metadata.permissions();
            perms.set_mode(mode);
            #[allow(clippy::disallowed_methods)]
            std::fs::set_permissions(path.as_path(), perms)?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
            Ok(())
        }
    }

    fn resolve_workdir(&self, current: &GuardedPath, new_dir: &str) -> Result<GuardedPath> {
        PathResolver::resolve_workdir(self, current, new_dir)
    }

    fn resolve_read(&self, cwd: &GuardedPath, rel: &str) -> Result<GuardedPath> {
        PathResolver::resolve_read(self, cwd, rel)
    }

    fn resolve_write(&self, cwd: &GuardedPath, rel: &str) -> Result<GuardedPath> {
        PathResolver::resolve_write(self, cwd, rel)
    }

    fn resolve_copy_source(&self, from: &str) -> Result<GuardedPath> {
        PathResolver::resolve_copy_source(self, from)
    }

    fn resolve_copy_source_from_workspace(&self, from: &str) -> Result<GuardedPath> {
        PathResolver::resolve_copy_source_from_workspace(self, from)
    }

    fn entry_kind(&self, path: &GuardedPath) -> Result<EntryKind> {
        PathResolver::entry_kind(self, path)
    }

    #[allow(clippy::disallowed_types)]
    fn entry_kind_unguarded(&self, path: &UnguardedPath) -> Result<EntryKind> {
        #[allow(clippy::disallowed_methods)]
        let metadata = std::fs::metadata(path.as_path())?;
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
    ) -> Result<()> {
        PathResolver::copy_from_git(self, rev, from, to, include_dirty)
    }

    fn clone_box(&self) -> Box<dyn WorkspaceFs> {
        Box::new(self.clone())
    }
}
