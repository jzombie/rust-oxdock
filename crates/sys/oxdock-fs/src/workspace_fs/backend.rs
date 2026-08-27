use anyhow::{Context, Result, bail};
use std::fs;

use super::{DirEntry, GuardedPath};

/// Private trait describing the backend IO interface. Kept private to avoid
/// expanding the public API surface; used to ensure host/miri implementations
/// remain in sync.
trait BackendImpl {
    fn create_dir_all(&self, root: &GuardedPath, path: &GuardedPath) -> Result<()>;
    fn read_dir_entries(&self, path: &GuardedPath) -> Result<Vec<DirEntry>>;
    fn read_file(&self, path: &GuardedPath) -> Result<Vec<u8>>;
    fn write_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()>;
    fn append_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()>;
    fn canonicalize(&self, path: GuardedPath) -> Result<GuardedPath>;
    fn metadata(&self, path: &GuardedPath) -> Result<std::fs::Metadata>;
    fn entry_kind(&self, path: &GuardedPath) -> Result<super::EntryKind>;
    fn resolve_workdir(&self, resolved: GuardedPath) -> Result<GuardedPath>;
    fn resolve_copy_source(&self, guarded: GuardedPath) -> Result<GuardedPath>;
    fn remove_file(&self, path: &GuardedPath) -> Result<()>;
    fn remove_dir_all(&self, path: &GuardedPath) -> Result<()>;
}

// Host implementation (used when not under Miri)
#[cfg(not(miri))]
pub(in crate::workspace_fs) struct HostBackend;

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
    use std::path::Component;
    use std::sync::{Arc, Mutex, OnceLock};

    static STATE_REGISTRY: OnceLock<
        Mutex<HashMap<std::path::PathBuf, Arc<Mutex<SyntheticRootState>>>>,
    > = OnceLock::new();

    pub(in crate::workspace_fs) struct MiriBackend {
        root_state: Option<Arc<Mutex<SyntheticRootState>>>,
        build_state: Option<Arc<Mutex<SyntheticRootState>>>,
        root_path: std::path::PathBuf,
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
            bail!("synthetic metadata unsupported under miri on this platform")
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

    pub(super) fn canonicalize(&self, path: GuardedPath) -> Result<GuardedPath> {
        self.as_impl().canonicalize(path)
    }

    pub(super) fn metadata(&self, path: &GuardedPath) -> Result<std::fs::Metadata> {
        self.as_impl().metadata(path)
    }

    pub(super) fn entry_kind(&self, path: &GuardedPath) -> Result<super::EntryKind> {
        self.as_impl().entry_kind(path)
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
}
