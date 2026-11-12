//! Embedded pipeline files: the binary's scripts, served from memory.
//!
//! The pipeline entry plus every default lifecycle file lives in the
//! binary via `include_bytes!` and is served from memory through this
//! filesystem wrapper, so consumer repos never receive pipeline files
//! on disk. What runs is always the binary's version: no written
//! copies exist that could rot. An on-disk file shadowing an embedded
//! path is ignored; [`warn_on_shadowed_dist`] names it so repos
//! carrying stale copies from older releases know to delete them.

use anyhow::{Context, Result};
#[allow(clippy::disallowed_types)]
use oxdock_fs::UnguardedPath;
use oxdock_fs::{
    CopySourceRoot, DirEntry, EntryKind, GuardedPath, PathResolver, WorkspaceFs,
    WorkspaceSelection, to_forward_slashes,
};

/// Pipeline files the binary carries: entry, renderer, stage order,
/// shared types, and the default hook set, keyed by workspace-relative
/// path. Served from memory (see [`EmbeddedFiles`]), never written to disk.
const DIST: &[(&str, &[u8])] = &[
    ("oxdock-render/src/run.oxfile", include_bytes!("run.oxfile")),
    (
        "oxdock-render/src/render.oxfile",
        include_bytes!("render.oxfile"),
    ),
    (
        "oxdock-render/src/stages.oxfile",
        include_bytes!("stages.oxfile"),
    ),
    (
        "oxdock-render/src/types.oxfile",
        include_bytes!("types.oxfile"),
    ),
    (
        "oxdock-render/src/lifecycle/scanner.oxfile",
        include_bytes!("lifecycle/scanner.oxfile"),
    ),
    (
        "oxdock-render/src/lifecycle/generic.oxfile",
        include_bytes!("lifecycle/generic.oxfile"),
    ),
    (
        "oxdock-render/src/lifecycle/oxdock.oxfile",
        include_bytes!("lifecycle/oxdock.oxfile"),
    ),
];

/// Embedded bytes for a workspace-relative dist path, if it names one.
fn embedded(rel: &str) -> Option<&'static [u8]> {
    DIST.iter()
        .find_map(|(path, bytes)| (*path == rel).then_some(*bytes))
}

/// Embedded pipeline bytes for a guarded path, when it names a dist file.
fn embedded_for(path: &GuardedPath) -> Option<&'static [u8]> {
    let rel = path.as_path().strip_prefix(path.root()).ok()?;
    let rel = to_forward_slashes(&rel.to_string_lossy());
    embedded(rel.as_str())
}

/// Filesystem wrapper serving the binary's embedded pipeline files
/// from memory: reads of dist paths return the embedded bytes even
/// when the disk holds a stale copy. Everything else delegates to
/// the inner resolver, so consumer trees stay free of pipeline files.
#[derive(Clone)]
pub(crate) struct EmbeddedFiles {
    inner: PathResolver,
}

impl EmbeddedFiles {
    /// Wrap a configured resolver; the wrapper serves dist paths.
    pub(crate) fn new(inner: PathResolver) -> Self {
        Self { inner }
    }
}

/// Warn about on-disk files shadowing embedded dist paths: they are
/// ignored (the embedded copy is authoritative), and deleting them
/// silences the warning. Repos provisioned by older releases use this
/// to find their stale copies.
pub(crate) fn warn_on_shadowed_dist(resolver: &PathResolver, root: &GuardedPath) {
    for (rel, bytes) in DIST {
        let path = match root.join(rel) {
            Ok(path) => path,
            Err(_) => continue,
        };
        // Existence and length first: only read the file when a
        // byte-for-byte comparison is actually needed.
        let stale = match resolver.metadata(&path) {
            Err(_) => continue,
            Ok(meta) if meta.len() as usize != bytes.len() => true,
            Ok(_) => match resolver.read_file(&path) {
                Ok(disk) => disk.as_slice() != *bytes,
                Err(_) => continue,
            },
        };
        if stale {
            eprintln!(
                "render: ignoring on-disk '{rel}' (it differs from this binary's embedded pipeline, which is authoritative); delete it to silence this warning"
            );
        }
    }
}

impl WorkspaceFs for EmbeddedFiles {
    fn canonicalize(&self, path: &GuardedPath) -> Result<GuardedPath> {
        self.inner.canonicalize(path)
    }

    #[allow(clippy::disallowed_types)]
    fn metadata(&self, path: &GuardedPath) -> Result<std::fs::Metadata> {
        self.inner.metadata(path)
    }

    #[allow(clippy::disallowed_types)]
    fn metadata_unguarded(&self, path: &UnguardedPath) -> Result<std::fs::Metadata> {
        self.inner.metadata_unguarded(path)
    }

    fn root(&self) -> &GuardedPath {
        self.inner.root()
    }

    fn workspace_root(&self) -> Option<&GuardedPath> {
        self.inner.workspace_root()
    }

    fn build_context(&self) -> &GuardedPath {
        self.inner.build_context()
    }

    fn selection(&self) -> WorkspaceSelection {
        self.inner.selection()
    }

    fn restore_selection(&mut self, selection: WorkspaceSelection) {
        self.inner.restore_selection(selection)
    }

    fn switch_to_snapshot(&mut self) {
        self.inner.switch_to_snapshot()
    }

    fn switch_to_local(&mut self) {
        self.inner.switch_to_local()
    }

    fn switch_to_cache(&mut self, local: bool) {
        self.inner.switch_to_cache(local)
    }

    fn switch_to_system(&mut self) {
        self.inner.switch_to_system()
    }

    fn system_entry_cwd(&self, cwd: &GuardedPath) -> GuardedPath {
        self.inner.system_entry_cwd(cwd)
    }

    fn is_snapshot_pending(&self) -> bool {
        self.inner.is_snapshot_pending()
    }

    fn concretize_cwd(&self, cwd: &GuardedPath) -> GuardedPath {
        self.inner.concretize_cwd(cwd)
    }

    fn read_file(&self, path: &GuardedPath) -> Result<Vec<u8>> {
        if let Some(bytes) = embedded_for(path) {
            return Ok(bytes.to_vec());
        }
        self.inner.read_file(path)
    }

    #[allow(clippy::disallowed_types)]
    fn read_file_unguarded(&self, path: &UnguardedPath) -> Result<Vec<u8>> {
        self.inner.read_file_unguarded(path)
    }

    fn open_read(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Read + Send>> {
        if let Some(bytes) = embedded_for(path) {
            return Ok(Box::new(std::io::Cursor::new(bytes)));
        }
        self.inner.open_read(path)
    }

    fn open_write(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write + Send>> {
        self.inner.open_write(path)
    }

    fn open_append(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write + Send>> {
        self.inner.open_append(path)
    }

    fn open_read_nofollow(
        &self,
        path: &GuardedPath,
        expected_root: &GuardedPath,
    ) -> Result<Box<dyn std::io::Read + Send>> {
        if let Some(bytes) = embedded_for(path) {
            return Ok(Box::new(std::io::Cursor::new(bytes)));
        }
        self.inner.open_read_nofollow(path, expected_root)
    }

    fn open_write_nofollow(
        &self,
        path: &GuardedPath,
        expected_root: &GuardedPath,
    ) -> Result<Box<dyn std::io::Write + Send>> {
        self.inner.open_write_nofollow(path, expected_root)
    }

    fn read_to_string(&self, path: &GuardedPath) -> Result<String> {
        if let Some(bytes) = embedded_for(path) {
            return String::from_utf8(bytes.to_vec()).context("embedded pipeline is not UTF-8");
        }
        self.inner.read_to_string(path)
    }

    #[allow(clippy::disallowed_types)]
    fn read_to_string_unguarded(&self, path: &UnguardedPath) -> Result<String> {
        self.inner.read_to_string_unguarded(path)
    }

    fn read_dir_entries(&self, path: &GuardedPath) -> Result<Vec<DirEntry>> {
        self.inner.read_dir_entries(path)
    }

    #[allow(clippy::disallowed_types)]
    fn read_dir_entries_unguarded(&self, path: &UnguardedPath) -> Result<Vec<DirEntry>> {
        self.inner.read_dir_entries_unguarded(path)
    }

    fn write_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()> {
        self.inner.write_file(path, contents)
    }

    #[allow(clippy::disallowed_types)]
    fn write_file_unguarded(&self, path: &UnguardedPath, contents: &[u8]) -> Result<()> {
        self.inner.write_file_unguarded(path, contents)
    }

    fn append_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()> {
        self.inner.append_file(path, contents)
    }

    #[allow(clippy::disallowed_types)]
    fn append_file_unguarded(&self, path: &UnguardedPath, contents: &[u8]) -> Result<()> {
        self.inner.append_file_unguarded(path, contents)
    }

    fn create_dir_all(&self, path: &GuardedPath) -> Result<()> {
        self.inner.create_dir_all(path)
    }

    #[allow(clippy::disallowed_types)]
    fn create_dir_all_unguarded(&self, path: &UnguardedPath) -> Result<()> {
        self.inner.create_dir_all_unguarded(path)
    }

    fn ensure_parent_dir(&self, path: &GuardedPath) -> Result<()> {
        self.inner.ensure_parent_dir(path)
    }

    fn remove_file(&self, path: &GuardedPath) -> Result<()> {
        self.inner.remove_file(path)
    }

    #[allow(clippy::disallowed_types)]
    fn remove_file_unguarded(&self, path: &UnguardedPath) -> Result<()> {
        self.inner.remove_file_unguarded(path)
    }

    fn remove_dir_all(&self, path: &GuardedPath) -> Result<()> {
        self.inner.remove_dir_all(path)
    }

    #[allow(clippy::disallowed_types)]
    fn remove_dir_all_unguarded(&self, path: &UnguardedPath) -> Result<()> {
        self.inner.remove_dir_all_unguarded(path)
    }

    fn remove_empty_dir(&self, path: &GuardedPath) -> Result<()> {
        self.inner.remove_empty_dir(path)
    }

    fn remove_link_if_symlink(&self, cwd: &GuardedPath, rel: &str) -> Result<bool> {
        self.inner.remove_link_if_symlink(cwd, rel)
    }

    fn copy_file(&self, src: &GuardedPath, dst: &GuardedPath) -> Result<u64> {
        self.inner.copy_file(src, dst)
    }

    #[allow(clippy::disallowed_types)]
    fn copy_file_unguarded(&self, src: &UnguardedPath, dst: &UnguardedPath) -> Result<u64> {
        self.inner.copy_file_unguarded(src, dst)
    }

    #[allow(clippy::disallowed_types)]
    fn copy_file_from_unguarded(&self, src: &UnguardedPath, dst: &GuardedPath) -> Result<u64> {
        self.inner.copy_file_from_unguarded(src, dst)
    }

    #[allow(clippy::disallowed_types)]
    fn copy_file_to_unguarded(&self, src: &GuardedPath, dst: &UnguardedPath) -> Result<u64> {
        self.inner.copy_file_to_unguarded(src, dst)
    }

    fn copy_dir_recursive(&self, src: &GuardedPath, dst: &GuardedPath) -> Result<()> {
        self.inner.copy_dir_recursive(src, dst)
    }

    #[allow(clippy::disallowed_types)]
    fn copy_dir_from_unguarded(&self, src: &UnguardedPath, dst: &GuardedPath) -> Result<()> {
        self.inner.copy_dir_from_unguarded(src, dst)
    }

    fn symlink(&self, src: &GuardedPath, dst: &GuardedPath) -> Result<()> {
        self.inner.symlink(src, dst)
    }

    fn install_file_atomic(&self, src: &GuardedPath, dst: &GuardedPath, mode: u32) -> Result<()> {
        self.inner.install_file_atomic(src, dst, mode)
    }

    #[allow(clippy::disallowed_types)]
    fn open_file_unguarded(&self, path: &UnguardedPath) -> Result<std::fs::File> {
        self.inner.open_file_unguarded(path)
    }

    fn set_permissions_mode_unix(&self, path: &GuardedPath, mode: u32) -> Result<()> {
        self.inner.set_permissions_mode_unix(path, mode)
    }

    #[allow(clippy::disallowed_types)]
    fn set_permissions_mode_unix_unguarded(&self, path: &UnguardedPath, mode: u32) -> Result<()> {
        self.inner.set_permissions_mode_unix_unguarded(path, mode)
    }

    fn resolve_workdir(&self, current: &GuardedPath, new_dir: &str) -> Result<GuardedPath> {
        self.inner.resolve_workdir(current, new_dir)
    }

    fn resolve_read(&self, cwd: &GuardedPath, rel: &str) -> Result<GuardedPath> {
        self.inner.resolve_read(cwd, rel)
    }

    fn resolve_write(&self, cwd: &GuardedPath, rel: &str) -> Result<GuardedPath> {
        self.inner.resolve_write(cwd, rel)
    }

    fn resolve_copy_source(&self, from: &str) -> Result<GuardedPath> {
        self.inner.resolve_copy_source(from)
    }

    fn resolve_copy_source_from_workspace(&self, from: &str) -> Result<GuardedPath> {
        self.inner.resolve_copy_source_from_workspace(from)
    }

    fn resolve_copy_source_from_target(
        &self,
        root: CopySourceRoot,
        from: &str,
    ) -> Result<GuardedPath> {
        self.inner.resolve_copy_source_from_target(root, from)
    }

    fn entry_kind(&self, path: &GuardedPath) -> Result<EntryKind> {
        if embedded_for(path).is_some() {
            return Ok(EntryKind::File);
        }
        self.inner.entry_kind(path)
    }

    #[allow(clippy::disallowed_types)]
    fn entry_kind_unguarded(&self, path: &UnguardedPath) -> Result<EntryKind> {
        self.inner.entry_kind_unguarded(path)
    }

    fn entry_kind_no_follow(&self, path: &GuardedPath) -> Result<EntryKind> {
        if embedded_for(path).is_some() {
            return Ok(EntryKind::File);
        }
        self.inner.entry_kind_no_follow(path)
    }

    fn copy_from_git(
        &self,
        rev: &str,
        from: &str,
        to: &GuardedPath,
        include_dirty: bool,
    ) -> Result<()> {
        self.inner.copy_from_git(rev, from, to, include_dirty)
    }

    fn clone_box(&self) -> Box<dyn WorkspaceFs> {
        Box::new(self.clone())
    }
}
