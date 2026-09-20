//! Owned pipe handles: a mutex cell holding a lazily materializing slot.
//!
//! A [`PipeHandle`] starts [`Slot::Unbound`] and materializes on first
//! binding — never eagerly at declaration, so the backend choice always
//! has full usage context. Cloning the handle shares the backend (natural
//! fan-out for explicit sharing); the last drop closes. No central index
//! exists: resolution, keepers, and assertions all operate on handles
//! already in hand.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;

use crate::backend::{PipeInner, ScriptPipe};

/// Shared reader half: mutex-guarded `Read` behind an `Arc`, so every
/// binding aliases one channel.
pub type SharedInput = Arc<Mutex<dyn std::io::Read + Send>>;

/// Shared writer half: mutex-guarded `Write` behind an `Arc`.
pub type SharedOutput = Arc<Mutex<dyn std::io::Write + Send>>;

/// Owned OS kernel pipe reader half behind a single use slot. `Clone`
/// shares the slot; `take` transfers the handle exactly once so no parent
/// copy survives spawn to starve the consumer of EOF. Backed by
/// `std::io::pipe` (stable since Rust 1.87): `pipe` on Unix, `CreatePipe`
/// on Windows. Moved verbatim from `oxdock-process` so handle slots can
/// own kernel pairs without a dependency cycle; behavior is unchanged.
#[cfg(not(miri))]
#[derive(Clone)]
pub struct OsPipeReader {
    inner: Arc<Mutex<Option<std::io::PipeReader>>>,
}

/// Owned OS kernel pipe writer half behind a single use slot. See
/// [`OsPipeReader`] for the shared slot semantics. Moved verbatim from
/// `oxdock-process`; behavior is unchanged.
#[cfg(not(miri))]
#[derive(Clone)]
pub struct OsPipeWriter {
    inner: Arc<Mutex<Option<std::io::PipeWriter>>>,
}

#[cfg(not(miri))]
impl OsPipeReader {
    fn new(reader: std::io::PipeReader) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(reader))),
        }
    }

    /// Take the handle for `Stdio::from`. Bails deterministically if the
    /// descriptor was already consumed so a second spawn can never reuse a
    /// spent pipe or leave stdio unbound.
    pub fn take(&self) -> Result<std::io::PipeReader> {
        self.inner
            .lock()
            .map_err(|_| anyhow::anyhow!("os pipe reader lock poisoned"))?
            .take()
            .ok_or_else(|| {
                anyhow::anyhow!("os pipe handle has already been consumed by another process")
            })
    }

    /// Whether this half was already taken. A poisoned slot reports live
    /// so callers never recycle what they cannot inspect.
    pub fn is_consumed(&self) -> bool {
        self.inner.lock().map(|g| g.is_none()).unwrap_or(false)
    }
}

#[cfg(not(miri))]
impl OsPipeWriter {
    fn new(writer: std::io::PipeWriter) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(writer))),
        }
    }

    /// Take the handle for `Stdio::from`. Bails deterministically if the
    /// descriptor was already consumed so a second spawn can never reuse a
    /// spent pipe or leave stdio unbound.
    pub fn take(&self) -> Result<std::io::PipeWriter> {
        self.inner
            .lock()
            .map_err(|_| anyhow::anyhow!("os pipe writer lock poisoned"))?
            .take()
            .ok_or_else(|| {
                anyhow::anyhow!("os pipe handle has already been consumed by another process")
            })
    }

    /// Whether this half was already taken. A poisoned slot reports live
    /// so callers never recycle what they cannot inspect.
    pub fn is_consumed(&self) -> bool {
        self.inner.lock().map(|g| g.is_none()).unwrap_or(false)
    }
}

/// Create a cross platform anonymous OS pipe pair for concurrent `ASYNC`
/// pipelines. The caller moves each half into a spawn and drops any other
/// copies immediately after spawning, otherwise the reader never sees EOF.
/// Moved verbatim from `oxdock-process`; behavior is unchanged.
#[cfg(not(miri))]
pub fn create_os_pipe() -> Result<(OsPipeReader, OsPipeWriter)> {
    let (reader, writer) = std::io::pipe()?;
    Ok((OsPipeReader::new(reader), OsPipeWriter::new(writer)))
}

/// One anonymous OS kernel pipe pair behind take-once slots. The first
/// producer and the first consumer each take their half; any further
/// binding to the same handle bails deterministically instead of
/// interleaving bytes or stealing the descriptor.
#[cfg(not(miri))]
#[derive(Clone)]
pub struct OsPipeEntry {
    /// Writer half. Only `RUN` consumes this directly; DSL commands
    /// observe bridged shared handles instead.
    pub writer: OsPipeWriter,
    /// Reader half. Only `RUN` consumes this directly; DSL commands
    /// observe bridged shared handles instead.
    pub reader: OsPipeReader,
}

#[cfg(not(miri))]
impl OsPipeEntry {
    /// Mint a fresh kernel pair.
    pub fn new() -> anyhow::Result<Self> {
        let (reader, writer) = create_os_pipe()?;
        Ok(Self { writer, reader })
    }

    /// Both halves taken. The takers hold raw descriptors outside the
    /// entry, so a spent entry can never serve another resolve.
    pub fn is_spent(&self) -> bool {
        self.reader.is_consumed() && self.writer.is_consumed()
    }
}

/// Backend state behind a [`PipeHandle`]. Starts unbound; the first
/// binding decides the kind under the cell lock and later bindings adapt
/// through the existing resolution machinery instead of failing or
/// upgrading in place.
pub enum Slot {
    /// Declared but never bound: no backend, no bytes.
    Unbound,
    /// Store-and-forward buffer shared by every binding.
    Script {
        /// Live backend. Cloned out for keepers, peeks, and diagnostics.
        backend: Arc<PipeInner>,
    },
    /// Zero-copy OS kernel pair behind take-once slots.
    #[cfg(not(miri))]
    Os {
        /// Live kernel pair. Takes are single-use per end.
        entry: OsPipeEntry,
    },
}

impl std::fmt::Debug for Slot {
    /// Opaque by design: backend internals (buffers, fds) never enter
    /// debug output; only the materialization kind shows.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Slot::Unbound => write!(f, "Unbound"),
            Slot::Script { .. } => write!(f, "Script(..)"),
            #[cfg(not(miri))]
            Slot::Os { .. } => write!(f, "Os(..)"),
        }
    }
}

/// Owned pipe handle: cheap to clone, shared backend, no registry.
/// `LET $p: PIPE` mints one; `LET $q: PIPE = $p` clones it.
///
/// Clones share everything: the backend cell, the declaring task id, and
/// the escape flag. Identity is the cell (`Arc::ptr_eq` on it), exactly
/// like the old bare-`Arc` handle.
#[derive(Clone, Debug)]
pub struct PipeHandle {
    cell: Arc<Mutex<Slot>>,
    declaring_task_id: u64,
    escaped_to_child: Arc<AtomicBool>,
}

/// Mint a fresh unbound handle declared by `task_id` (`0` = root flow).
/// The id travels with every clone so promotion checks always see the
/// declaration origin, no matter how far the value aliases.
pub fn new_handle_in_task(task_id: u64) -> PipeHandle {
    PipeHandle {
        cell: Arc::new(Mutex::new(Slot::Unbound)),
        declaring_task_id: task_id,
        escaped_to_child: Arc::new(AtomicBool::new(false)),
    }
}

impl PipeHandle {
    /// Task that executed the `LET $p: PIPE` declaration (`0` = root flow).
    pub fn declaring_task(&self) -> u64 {
        self.declaring_task_id
    }

    /// Whether any spawned child task can observe this handle. Set at
    /// `ASYNC` fork time for every pipe visible in scope; sticky, since a
    /// share, once possible, never un-happens.
    pub fn has_escaped(&self) -> bool {
        self.escaped_to_child.load(Ordering::SeqCst)
    }

    /// Mark escaped (see [`PipeHandle::has_escaped`]). Idempotent.
    pub fn mark_escaped(&self) {
        self.escaped_to_child.store(true, Ordering::SeqCst);
    }

    /// Borrow the backend cell for materialization and lock-step transitions.
    pub(crate) fn cell(&self) -> &Arc<Mutex<Slot>> {
        &self.cell
    }

    /// Handle identity: true iff both handles share the same backend cell
    /// (aliases of one declaration). Never compares bytes.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cell, &other.cell)
    }
}

/// Decided backend cloned out from under the cell lock. OS entries clone
/// their take-once slots (takes still race deterministically at take
/// time); script backends clone their `Arc`.
pub enum Materialized {
    /// Store-and-forward backend.
    Script(Arc<PipeInner>),
    /// Kernel pair behind take-once slots.
    #[cfg(not(miri))]
    Os(OsPipeEntry),
}

/// First-binding-wins materialization under the cell lock: an unbound
/// handle decides its kind from `promote` (the caller supplies full usage
/// context — RUN-terminated ⇒ OS, else script); a decided handle returns
/// its kind unchanged. Later bindings with different needs adapt through
/// the caller's resolution machinery instead of failing or upgrading here.
pub fn materialize(handle: &PipeHandle, promote: bool) -> anyhow::Result<Materialized> {
    let mut guard = handle
        .cell()
        .lock()
        .map_err(|_| anyhow::anyhow!("pipe handle lock poisoned"))?;
    match &*guard {
        Slot::Script { backend } => Ok(Materialized::Script(Arc::clone(backend))),
        #[cfg(not(miri))]
        Slot::Os { entry } => Ok(Materialized::Os(entry.clone())),
        Slot::Unbound => {
            #[cfg(not(miri))]
            if promote {
                let entry = OsPipeEntry::new()?;
                let out = entry.clone();
                *guard = Slot::Os { entry };
                return Ok(Materialized::Os(out));
            }
            #[cfg(miri)]
            let _ = promote;
            let pipe = ScriptPipe::new();
            let backend = pipe.pipe_inner();
            *guard = Slot::Script {
                backend: Arc::clone(&backend),
            };
            Ok(Materialized::Script(backend))
        }
    }
}
