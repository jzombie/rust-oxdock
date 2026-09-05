// Workspace-scoped path resolver with guarded file operations.
// Methods are split across submodules by concern (access checks, IO, copy, git, resolve helpers).

use anyhow::{Context, Result};

use backend::Backend;

#[cfg(not(miri))]
pub type DirEntry = std::fs::DirEntry;
#[cfg(not(miri))]
pub type FileType = std::fs::FileType;

#[cfg(miri)]
#[allow(clippy::disallowed_types)]
mod synthetic_entry {
    use super::EntryKind;
    use std::ffi::OsString;
    use std::io;
    use std::path::PathBuf;

    #[derive(Clone, Copy)]
    pub struct FileType {
        kind: EntryKind,
    }

    impl FileType {
        pub fn is_dir(&self) -> bool {
            matches!(self.kind, EntryKind::Dir)
        }

        pub fn is_file(&self) -> bool {
            matches!(self.kind, EntryKind::File)
        }

        pub fn is_symlink(&self) -> bool {
            false
        }
    }

    #[derive(Clone)]
    pub struct DirEntry {
        path: PathBuf,
        file_name: OsString,
        file_type: FileType,
    }

    impl DirEntry {
        pub(crate) fn new(path: PathBuf, kind: EntryKind) -> Self {
            let file_name = path
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default();
            Self {
                path,
                file_name,
                file_type: FileType { kind },
            }
        }

        pub fn path(&self) -> PathBuf {
            self.path.clone()
        }

        pub fn file_name(&self) -> OsString {
            self.file_name.clone()
        }

        pub fn file_type(&self) -> io::Result<FileType> {
            Ok(self.file_type)
        }
    }

    impl std::fmt::Debug for DirEntry {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("DirEntry")
                .field("path", &self.path)
                .finish()
        }
    }

    impl std::fmt::Debug for FileType {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("FileType")
                .field("kind", &self.kind)
                .finish()
        }
    }
}

#[cfg(miri)]
pub use synthetic_entry::{DirEntry, FileType};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Dir,
}

pub mod path;
pub use path::{GuardedPath, GuardedTempDir};
pub use path::{command_path, embed_path, normalized_path, to_forward_slashes};

#[allow(clippy::disallowed_types)]
pub use path::UnguardedPath;

pub mod policy;
pub use policy::{GuardPolicy, PolicyPath};

pub mod git;

#[allow(clippy::disallowed_types)]
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub(crate) enum AccessMode {
    Read,
    Write,
}

impl AccessMode {
    fn name(&self) -> &'static str {
        match self {
            AccessMode::Read => "READ",
            AccessMode::Write => "WRITE",
        }
    }
}

/// Resolves and validates filesystem paths within a confined workspace and build context.
#[derive(Clone)]
pub struct PathResolver {
    root: GuardedPath,
    build_context: GuardedPath,
    workspace_root: Option<GuardedPath>,
    backend: Backend,
}

impl PathResolver {
    /// Build a resolver rooted at `CARGO_MANIFEST_DIR`, using that same
    /// directory as the build context. This centralizes env lookup and path
    /// creation so callers avoid ad-hoc path construction.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub fn from_manifest_env() -> Result<Self> {
        let manifest_dir =
            std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR missing")?;
        let path = Path::new(&manifest_dir);
        Self::new(path, path)
    }

    #[allow(clippy::disallowed_types)]
    pub fn new(root: &Path, build_context: &Path) -> Result<Self> {
        let root_guard = GuardedPath::new_root(root)?;
        let build_guard = GuardedPath::new_root(build_context)?;
        let backend = Backend::new(&root_guard, &build_guard)?;
        Ok(Self {
            root: root_guard,
            build_context: build_guard,
            workspace_root: None,
            backend,
        })
    }

    pub fn new_guarded(root: GuardedPath, build_context: GuardedPath) -> Result<Self> {
        let backend = Backend::new(&root, &build_context)?;
        Ok(Self {
            root,
            build_context,
            workspace_root: None,
            backend,
        })
    }

    pub fn root(&self) -> &GuardedPath {
        &self.root
    }

    pub fn build_context(&self) -> &GuardedPath {
        &self.build_context
    }

    pub fn workspace_root(&self) -> Option<&GuardedPath> {
        self.workspace_root.as_ref()
    }

    pub fn set_workspace_root(&mut self, root: GuardedPath) {
        self.workspace_root = Some(root);
    }

    pub fn set_root(&mut self, root: &GuardedPath) {
        self.root = root.clone();
    }
}
pub(crate) mod access;
mod backend;
mod copy;
mod io;
mod resolve;
use access::guard_path;

#[cfg(feature = "mock-fs")]
pub mod mock;
