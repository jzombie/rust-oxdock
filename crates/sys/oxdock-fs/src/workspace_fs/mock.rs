use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::path::Path;
use std::rc::Rc;

use anyhow::{Result, bail};

#[allow(clippy::disallowed_types)]
use crate::UnguardedPath;
use crate::{EntryKind, WorkspaceFs, to_forward_slashes};

use super::GuardedPath;

/// Write cursor for MockFs that buffers data and flushes to in-memory state on drop.
struct MockWriteCursor {
    state: Rc<RefCell<MockState>>,
    rel: String,
}

impl std::io::Write for MockWriteCursor {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Append to mock state
        let mut state = self.state.borrow_mut();
        if let Some(existing) = state.files.get_mut(&self.rel) {
            existing.extend_from_slice(buf);
        } else {
            state.files.insert(self.rel.clone(), buf.to_vec());
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // No-op for in-memory state
        Ok(())
    }
}

/// In-memory workspace filesystem for tests and Miri runs.
#[derive(Clone)]
pub struct MockFs {
    root: GuardedPath,
    build_context: GuardedPath,
    state: Rc<RefCell<MockState>>,
}

#[derive(Default)]
struct MockState {
    files: HashMap<String, Vec<u8>>,
    dirs: HashSet<String>,
}

impl MockFs {
    pub fn new() -> Self {
        let root = GuardedPath::new_root_from_str(".").unwrap();
        let build_context = root.clone();
        let mut dirs = HashSet::new();
        dirs.insert(String::new());
        Self {
            root,
            build_context,
            state: Rc::new(RefCell::new(MockState {
                files: HashMap::new(),
                dirs,
            })),
        }
    }

    pub fn snapshot(&self) -> HashMap<String, Vec<u8>> {
        self.state.borrow().files.clone()
    }

    fn normalize_rel(&self, base: &GuardedPath, rel: &str) -> Result<String> {
        let mut segments = if rel.starts_with('/') || rel.starts_with('\\') {
            Vec::new()
        } else {
            self.split_components(&self.relative_path(base))
        };
        for part in self.split_components(rel) {
            match part.as_str() {
                "" | "." => {}
                ".." => {
                    segments.pop();
                }
                other => segments.push(other.to_string()),
            }
        }
        Ok(segments.join("/"))
    }

    fn guard_from_rel(&self, rel: String) -> Result<GuardedPath> {
        if rel.is_empty() {
            return Ok(self.root.clone());
        }
        let native = if std::path::MAIN_SEPARATOR == '/' {
            rel
        } else {
            rel.replace('/', std::path::MAIN_SEPARATOR_STR)
        };
        self.root
            .join(native.as_str())
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }

    fn relative_path(&self, path: &GuardedPath) -> String {
        let rel = path
            .as_path()
            .strip_prefix(self.root.as_path())
            .unwrap_or_else(|_| path.as_path());
        let trimmed = rel
            .to_string_lossy()
            .trim_start_matches(std::path::MAIN_SEPARATOR)
            .to_string();
        to_forward_slashes(&trimmed)
    }

    fn split_components(&self, input: &str) -> Vec<String> {
        input
            .split(['/', '\\'])
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    }
}

impl Default for MockFs {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceFs for MockFs {
    fn canonicalize(&self, path: &GuardedPath) -> Result<GuardedPath> {
        Ok(path.clone())
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn metadata(&self, _path: &GuardedPath) -> Result<std::fs::Metadata> {
        bail!("metadata not supported in mock fs");
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn metadata_unguarded(&self, _path: &crate::UnguardedPath) -> Result<std::fs::Metadata> {
        bail!("metadata not supported in mock fs");
    }

    fn root(&self) -> &GuardedPath {
        &self.root
    }

    fn build_context(&self) -> &GuardedPath {
        &self.build_context
    }

    fn set_root(&mut self, root: &GuardedPath) {
        self.root = root.clone();
    }

    fn read_file(&self, path: &GuardedPath) -> Result<Vec<u8>> {
        let rel = self.relative_path(path);
        self.state
            .borrow()
            .files
            .get(&rel)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing file {}", path.display()))
    }

    #[allow(clippy::disallowed_types)]
    fn read_file_unguarded(&self, _path: &UnguardedPath) -> Result<Vec<u8>> {
        bail!("unguarded operations not supported in mock fs");
    }

    fn read_to_string(&self, path: &GuardedPath) -> Result<String> {
        let bytes = self.read_file(path)?;
        String::from_utf8(bytes).map_err(|e| anyhow::anyhow!(e))
    }

    #[allow(clippy::disallowed_types)]
    fn read_to_string_unguarded(&self, _path: &UnguardedPath) -> Result<String> {
        bail!("unguarded operations not supported in mock fs");
    }

    fn read_dir_entries(&self, _path: &GuardedPath) -> Result<Vec<crate::DirEntry>> {
        bail!("read_dir unsupported in mock fs");
    }

    #[allow(clippy::disallowed_types)]
    fn read_dir_entries_unguarded(&self, _path: &UnguardedPath) -> Result<Vec<crate::DirEntry>> {
        bail!("unguarded operations not supported in mock fs");
    }

    fn write_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()> {
        let rel = self.relative_path(path);
        self.state.borrow_mut().files.insert(rel, contents.to_vec());
        Ok(())
    }

    #[allow(clippy::disallowed_types)]
    fn write_file_unguarded(&self, _path: &UnguardedPath, _contents: &[u8]) -> Result<()> {
        bail!("unguarded operations not supported in mock fs");
    }

    fn append_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()> {
        let rel = self.relative_path(path);
        let mut state = self.state.borrow_mut();
        if let Some(existing) = state.files.get_mut(&rel) {
            existing.extend_from_slice(contents);
        } else {
            state.files.insert(rel, contents.to_vec());
        }
        Ok(())
    }

    #[allow(clippy::disallowed_types)]
    fn append_file_unguarded(&self, _path: &UnguardedPath, _contents: &[u8]) -> Result<()> {
        bail!("unguarded operations not supported in mock fs");
    }

    fn open_read(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Read>> {
        let data = self.read_file(path)?;
        Ok(Box::new(std::io::Cursor::new(data)))
    }

    fn open_write(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write>> {
        let rel = self.relative_path(path);
        // Create an empty file first
        self.state.borrow_mut().files.insert(rel.clone(), Vec::new());
        // Return a cursor that will be flushed on drop
        let state = self.state.clone();
        Ok(Box::new(MockWriteCursor { state, rel }))
    }

    fn open_append(&self, path: &GuardedPath) -> Result<Box<dyn std::io::Write>> {
        let rel = self.relative_path(path);
        let _existing = self
            .state
            .borrow()
            .files
            .get(&rel)
            .cloned()
            .unwrap_or_default();
        let state = self.state.clone();
        Ok(Box::new(MockWriteCursor { state, rel }))
    }

    fn create_dir_all(&self, path: &GuardedPath) -> Result<()> {
        let rel = self.relative_path(path);
        let mut state = self.state.borrow_mut();
        state.dirs.insert(String::new());
        let mut prefix: Vec<String> = Vec::new();
        for comp in self.split_components(&rel) {
            prefix.push(comp.clone());
            state.dirs.insert(prefix.join("/"));
        }
        Ok(())
    }

    #[allow(clippy::disallowed_types)]
    fn create_dir_all_unguarded(&self, _path: &UnguardedPath) -> Result<()> {
        bail!("unguarded operations not supported in mock fs");
    }

    fn ensure_parent_dir(&self, path: &GuardedPath) -> Result<()> {
        if let Some(parent) = path.parent() {
            self.create_dir_all(&parent)?;
        }
        Ok(())
    }

    fn remove_file(&self, _path: &GuardedPath) -> Result<()> {
        bail!("remove unsupported")
    }

    #[allow(clippy::disallowed_types)]
    fn remove_file_unguarded(&self, _path: &UnguardedPath) -> Result<()> {
        bail!("unguarded operations not supported in mock fs");
    }

    fn remove_dir_all(&self, _path: &GuardedPath) -> Result<()> {
        bail!("remove unsupported")
    }

    #[allow(clippy::disallowed_types)]
    fn remove_dir_all_unguarded(&self, _path: &UnguardedPath) -> Result<()> {
        bail!("unguarded operations not supported in mock fs");
    }

    fn copy_file(&self, _src: &GuardedPath, _dst: &GuardedPath) -> Result<u64> {
        bail!("copy unsupported")
    }

    #[allow(clippy::disallowed_types)]
    fn copy_file_unguarded(&self, _src: &UnguardedPath, _dst: &UnguardedPath) -> Result<u64> {
        bail!("unguarded operations not supported in mock fs");
    }

    fn copy_dir_recursive(&self, _src: &GuardedPath, _dst: &GuardedPath) -> Result<()> {
        bail!("copy unsupported")
    }

    #[allow(clippy::disallowed_types)]
    fn copy_dir_from_unguarded(
        &self,
        _src: &crate::UnguardedPath,
        _dst: &GuardedPath,
    ) -> Result<()> {
        bail!("copy unsupported")
    }

    #[allow(clippy::disallowed_types)]
    fn copy_file_from_unguarded(
        &self,
        _src: &crate::UnguardedPath,
        _dst: &GuardedPath,
    ) -> Result<u64> {
        bail!("copy unsupported")
    }

    #[allow(clippy::disallowed_types)]
    fn copy_file_to_unguarded(&self, _src: &GuardedPath, _dst: &UnguardedPath) -> Result<u64> {
        bail!("unguarded operations not supported in mock fs");
    }

    fn symlink(&self, _src: &GuardedPath, _dst: &GuardedPath) -> Result<()> {
        bail!("symlink unsupported")
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn open_file_unguarded(&self, _path: &crate::UnguardedPath) -> Result<std::fs::File> {
        bail!("open unsupported")
    }

    fn set_permissions_mode_unix(&self, _path: &GuardedPath, _mode: u32) -> Result<()> {
        bail!("perms unsupported")
    }

    #[allow(clippy::disallowed_types)]
    fn set_permissions_mode_unix_unguarded(&self, _path: &UnguardedPath, _mode: u32) -> Result<()> {
        bail!("unguarded operations not supported in mock fs");
    }

    fn entry_kind(&self, path: &GuardedPath) -> Result<EntryKind> {
        let rel = self.relative_path(path);
        let state = self.state.borrow();
        if rel.is_empty() || state.dirs.contains(&rel) {
            Ok(EntryKind::Dir)
        } else if state.files.contains_key(&rel) {
            Ok(EntryKind::File)
        } else {
            bail!("missing path {}", path.display())
        }
    }

    #[allow(clippy::disallowed_types)]
    fn entry_kind_unguarded(&self, _path: &UnguardedPath) -> Result<EntryKind> {
        bail!("unguarded operations not supported in mock fs");
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn resolve_workdir(&self, current: &GuardedPath, new_dir: &str) -> Result<GuardedPath> {
        let candidate = Path::new(new_dir);
        if candidate.is_absolute() {
            // Let `GuardedPath::new` (and its `guard_path`) decide whether the
            // absolute candidate escapes the allowed root. Previously we silently
            // remapped absolute paths into the mock root which allowed Windows
            // drive-prefixed paths (e.g. `C:\...`) to bypass the guard.
            return GuardedPath::new(self.root.root(), candidate);
        }
        if new_dir == "/" {
            return Ok(self.root.clone());
        }
        let target = self.normalize_rel(current, new_dir)?;
        self.guard_from_rel(target)
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn resolve_read(&self, cwd: &GuardedPath, rel: &str) -> Result<GuardedPath> {
        let candidate = Path::new(rel);
        if candidate.is_absolute() {
            return GuardedPath::new(self.root.root(), candidate);
        }
        let target = self.normalize_rel(cwd, rel)?;
        self.guard_from_rel(target)
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn resolve_write(&self, cwd: &GuardedPath, rel: &str) -> Result<GuardedPath> {
        let candidate = Path::new(rel);
        if candidate.is_absolute() {
            return GuardedPath::new(self.root.root(), candidate);
        }
        let target = self.normalize_rel(cwd, rel)?;
        self.guard_from_rel(target)
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn resolve_copy_source(&self, from: &str) -> Result<GuardedPath> {
        let candidate = Path::new(from);
        if candidate.is_absolute() {
            return GuardedPath::new(self.build_context.root(), candidate);
        }
        let rel = self.split_components(from).join("/");
        self.guard_from_rel(rel)
    }

    fn resolve_copy_source_from_workspace(&self, from: &str) -> Result<GuardedPath> {
        // Mock has no separate workspace root; treat workspace as build_context.
        self.resolve_copy_source(from)
    }

    fn copy_from_git(
        &self,
        _rev: &str,
        _from: &str,
        _to: &GuardedPath,
        _include_dirty: bool,
    ) -> Result<()> {
        bail!("git copy unsupported")
    }
}
