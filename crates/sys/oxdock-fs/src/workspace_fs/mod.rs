// Workspace-scoped path resolver with guarded file operations.
// Methods are split across submodules by concern (access checks, IO, copy, git, resolve helpers).

use anyhow::{Context, Result};

use backend::Backend;
use std::sync::{Arc, OnceLock};

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

/// Reserved `CARGO_TARGET_DIR` location (issue #131).
///
/// Cargo gets dedicated handling because OxDock runs inside the Cargo build
/// lifecycle: `build.rs` scripts, procedural macros, and Cargo workflows.
/// DSL scripts frequently invoke `cargo` inside `RUN` steps, and without
/// explicit redirection a nested `cargo` writes into the host `target/`
/// directory. That risks `Cargo.lock` contention, build deadlocks, and
/// corruption of the host compilation state.
///
/// `cargo` natively honors `CARGO_TARGET_DIR` as a global override across
/// every subcommand (`build`, `check`, `test`), so redirecting through this
/// single variable needs no argument injection and no config mutation. Other
/// toolchains need no equivalent: `npm` keeps dependencies in the in-tree
/// `node_modules/` hierarchy, `pip` resolves into virtual environments or
/// site packages, and `go` defaults to user level caches outside the project
/// root. Only `cargo` shares the host build environment with the OxDock host
/// process itself, so only `cargo` needs this isolation. Non cargo tools
/// follow root selection as usual: under `WORKSPACE LOCAL` their artifacts
/// land in the live tree, under `WORKSPACE SNAPSHOT` in the isolated
/// snapshot directory.
///
/// Opaque by design: the inner guarded path is not exposed, so neither
/// snapshot style `ensure()` creation nor `create_dir_all` can name it.
/// Passing a `CargoScratch` where a `&GuardedPath` is required is a compile
/// error. The only operations are rendering it for the child environment
/// (`display`, `command_path`) and recording it in test doubles
/// (`to_path_buf`). Creation, if ever needed, is performed on demand by the
/// child `cargo` invocation itself. Host code never creates it.
#[derive(Clone, Debug)]
pub struct CargoScratch(GuardedPath);

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
impl CargoScratch {
    /// Render for `CARGO_TARGET_DIR` environment bindings.
    pub fn display(&self) -> String {
        self.0.display()
    }

    /// Platform-adjusted form for process spawning (verbatim-prefix
    /// stripping on Windows), mirroring [`command_path`].
    pub fn command_path(&self) -> std::borrow::Cow<'_, std::path::Path> {
        command_path(&self.0)
    }

    /// Owned raw path for test-double recording. Must never be created
    /// through; it exists so mocks can assert on the value.
    pub fn to_path_buf(&self) -> std::path::PathBuf {
        self.0.to_path_buf()
    }
}

/// Reserve a [`CargoScratch`] name inside the system temp hierarchy.
///
/// Pure name reservation: containment is verified against the real temp root,
/// but the directory is created on demand by the child `cargo` invocation
/// itself. Never by us. Plain `RUN` steps therefore perform zero filesystem
/// I/O for this while build outputs stay a managed, guarded location instead
/// of an arbitrary relative string (or the live workspace tree). The
/// high-entropy leaf makes the name unguessable; since we never create or
/// follow it, reservation alone presents no TOCTOU surface.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub fn reserve_cargo_scratch() -> Result<CargoScratch> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;
    static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);
    static SCRATCH_EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let id = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
    // Monotonic clock: available under Miri isolation (unlike wall-clock
    // time), unique per process via the counter, unguessable enough that a
    // reserved-but-never-created name presents no TOCTOU surface.
    let nanos = SCRATCH_EPOCH.get_or_init(Instant::now).elapsed().as_nanos();
    let leaf = format!("oxdock-cargo-{id}-{nanos:x}");
    let temp_root = std::env::temp_dir();
    // The temp root exists, so guarding performs no creation: the missing
    // candidate is normalized lexically and containment-verified.
    GuardedPath::new(&temp_root, &temp_root.join(leaf)).map(CargoScratch)
}

pub mod path;
pub use path::{GuardedPath, GuardedTempDir, LazyGuardedTempDir};
pub use path::{command_path, embed_path, normalized_path, to_forward_slashes};
pub(crate) mod io;
pub use io::SpillFile;

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

/// Which workspace root the resolver currently addresses (issue #131).
/// `WORKSPACE SNAPSHOT` selects the lazily-created snapshot dir, `WORKSPACE
/// LOCAL` selects the build context. Mutated only through the `&mut` switch
/// methods; resolution itself stays `&self`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CurrentRoot {
    Snapshot,
    Local,
}

/// Cross-clone synchronized snapshot backing (issue #131). Fork clones share
/// the same `Arc<SharedSnapshot>`, so the first snapshot-targeted choke point
/// on any clone materializes the ONE physical directory and every clone
/// observes it through `concrete` without per-clone remapping.
pub(crate) struct SharedSnapshot {
    lazy: Arc<LazyGuardedTempDir>,
    concrete: OnceLock<GuardedPath>,
}

impl SharedSnapshot {
    fn new() -> Self {
        Self {
            lazy: Arc::new(LazyGuardedTempDir::new()),
            concrete: OnceLock::new(),
        }
    }
}

/// Resolves and validates filesystem paths within a confined workspace and build context.
#[derive(Clone)]
pub struct PathResolver {
    /// Snapshot root anchor: the virtual (never created) lexical base while
    /// pending on native, the reserved synthetic path on Miri, the eager
    /// snapshot root for legacy constructors. Never mutated after
    /// construction and never passed to a syscall (see `effective_root`).
    root: GuardedPath,
    current: CurrentRoot,
    shared: Arc<SharedSnapshot>,
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
        Self::from_eager_roots(root_guard, build_guard)
    }

    pub fn new_guarded(root: GuardedPath, build_context: GuardedPath) -> Result<Self> {
        Self::from_eager_roots(root, build_context)
    }

    /// Shared eager-constructor core: the given root is already a real
    /// directory, so it is published as the concrete snapshot root up front
    /// and resolution never triggers lazy creation (legacy behavior kept).
    fn from_eager_roots(root: GuardedPath, build_context: GuardedPath) -> Result<Self> {
        let backend = Backend::new(&root, &build_context)?;
        let shared = Arc::new(SharedSnapshot::new());
        let _ = shared.concrete.set(root.clone());
        Ok(Self {
            root,
            current: CurrentRoot::Snapshot,
            shared,
            build_context,
            workspace_root: None,
            backend,
        })
    }

    /// Build a resolver whose snapshot root starts pending: no directory is
    /// created until the first snapshot-targeted resolution (issue #131).
    /// The build context (local root) is used as-is.
    pub fn new_lazy(build_context: GuardedPath) -> Result<Self> {
        let shared = Arc::new(SharedSnapshot::new());
        #[cfg(not(miri))]
        let root = virtual_snapshot_anchor();
        #[cfg(miri)]
        let root = shared
            .lazy
            .reserved_path()
            .expect("miri lazy holder reserves its synthetic path at construction");
        let backend = Backend::new(&root, &build_context)?;
        Ok(Self {
            root,
            current: CurrentRoot::Snapshot,
            shared,
            build_context,
            workspace_root: None,
            backend,
        })
    }

    /// Share ownership of the snapshot backing dir (execution results, shell
    /// entry materialization). The directory lives as long as the last clone.
    pub fn snapshot_handle(&self) -> Arc<LazyGuardedTempDir> {
        Arc::clone(&self.shared.lazy)
    }

    /// The root currently in force for guard checks and I/O: the build
    /// context while local, the shared concrete snapshot root once
    /// materialized, otherwise the (never created) virtual anchor. The anchor
    /// only ever feeds lexical joins and the pending display. Never a
    /// syscall, because every snapshot-targeted choke point materializes
    /// before any guard or I/O runs.
    pub(crate) fn effective_root(&self) -> &GuardedPath {
        match self.current {
            CurrentRoot::Local => &self.build_context,
            CurrentRoot::Snapshot => self.shared.concrete.get().unwrap_or(&self.root),
        }
    }

    /// The never-created virtual anchor (native) for lexical fail-closed
    /// checks. Never passed to a syscall.
    #[allow(clippy::disallowed_types)]
    pub(crate) fn anchor_path(&self) -> &Path {
        self.root.as_path()
    }

    /// Choke point (issue #131): the single place that may demand a concrete
    /// snapshot path. Returns the cross-clone shared concrete root,
    /// materializing exactly once on first call. Callers must invoke this
    /// before any join/guard/I/O when snapshot-targeted.
    pub(crate) fn snapshot_concrete(&self) -> Result<&GuardedPath> {
        if let Some(concrete) = self.shared.concrete.get() {
            return Ok(concrete);
        }
        let live = self.shared.lazy.ensure()?;
        let _ = self.shared.concrete.set(live.clone());
        Ok(self
            .shared
            .concrete
            .get()
            .expect("snapshot concrete published by choke point"))
    }

    /// Rebase a possibly anchor-rooted `cwd` onto the shared concrete root.
    /// Snapshot-targeted callers must route `cwd` through here first: it
    /// materializes when needed, passes through already-concrete paths, and
    /// performs purely lexical prefix surgery otherwise (no creation).
    /// Local callers pass through untouched.
    fn rebased_cwd(&self, cwd: &GuardedPath) -> Result<GuardedPath> {
        if !matches!(self.current, CurrentRoot::Snapshot) {
            return Ok(cwd.clone());
        }
        let concrete = self.snapshot_concrete()?;
        if cwd.root() == concrete.root() {
            return Ok(cwd.clone());
        }
        let rel = cwd
            .as_path()
            .strip_prefix(self.root.as_path())
            .map_err(|_| anyhow::anyhow!("cwd outside snapshot root: {}", cwd.display()))?;
        GuardedPath::new(concrete.root(), &concrete.as_path().join(rel))
    }

    /// Select the snapshot root without touching the disk (`WORKSPACE
    /// SNAPSHOT`). The pending anchor serves lexical joins until the first
    /// snapshot-targeted choke point swaps in the concrete root.
    pub fn switch_to_snapshot(&mut self) {
        self.current = CurrentRoot::Snapshot;
    }

    /// Select the local (build-context) root (`WORKSPACE LOCAL`). Never
    /// touches the snapshot handle.
    pub fn switch_to_local(&mut self) {
        self.current = CurrentRoot::Local;
    }

    /// True while snapshot-selected but with no concrete root published yet
    /// (drives the `<snapshot:pending>` display sentinel). Keyed off the
    /// shared concrete publication. Eager constructors pre-publish their
    /// root, so they never read pending. Never off the lazy flag alone.
    pub fn is_snapshot_pending(&self) -> bool {
        matches!(self.current, CurrentRoot::Snapshot) && self.shared.concrete.get().is_none()
    }

    /// Map a possibly anchor-rooted path onto the shared concrete root when
    /// materialized; otherwise return it unchanged. Best-effort convergence
    /// for final-cwd reporting and display. Never creates.
    pub fn concretize_cwd(&self, cwd: &GuardedPath) -> GuardedPath {
        if !matches!(self.current, CurrentRoot::Snapshot) {
            return cwd.clone();
        }
        let Some(concrete) = self.shared.concrete.get() else {
            return cwd.clone();
        };
        if cwd.root() == concrete.root() {
            return cwd.clone();
        }
        cwd.as_path()
            .strip_prefix(self.root.as_path())
            .ok()
            .and_then(|rel| GuardedPath::new(concrete.root(), &concrete.as_path().join(rel)).ok())
            .unwrap_or_else(|| cwd.clone())
    }

    pub fn root(&self) -> &GuardedPath {
        self.effective_root()
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

    /// Replace the build context (local root) before execution starts. Used by
    /// test harnesses that select the context per case. Safe for both
    /// backends: the native backend is stateless and the Miri backend keys
    /// build state by role, not by path. Must not be called mid-run.
    pub fn set_build_context(&mut self, build_context: GuardedPath) {
        self.build_context = build_context;
    }

    pub fn set_root(&mut self, root: &GuardedPath) {
        // Scope-restore compatibility: only the root SELECTION is restored.
        // The concrete publication (if any) stays the source of truth for
        // I/O paths, so a stale anchor or concrete value can never regress
        // resolution. The anchor field itself is never adopted here.
        if root == &self.build_context {
            self.current = CurrentRoot::Local;
        } else {
            self.current = CurrentRoot::Snapshot;
        }
    }
}

/// Display-only lexical base for the pending snapshot root (native only).
/// Never created, opened, or followed: it feeds purely in-memory joins until
/// the choke point swaps in the atomically-created concrete root, and
/// pending-state display renders `<snapshot:pending>` instead of this value.
/// Squatting it gains an attacker nothing. No syscall of ours ever names it.
#[cfg(not(miri))]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn virtual_snapshot_anchor() -> GuardedPath {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ANCHOR_COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = ANCHOR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let leaf = format!("oxdock-pending-{}-{id}", std::process::id());
    let abs = std::env::temp_dir().join(leaf);
    GuardedPath::from_guarded_parts(abs.clone(), abs)
}
pub(crate) mod access;
mod backend;
mod copy;
mod resolve;
use access::guard_path;

#[cfg(feature = "mock-fs")]
pub mod mock;
