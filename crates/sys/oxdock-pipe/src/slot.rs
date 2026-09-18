//! Owned pipe handles: a mutex cell holding a lazily materializing slot.
//!
//! A [`PipeHandle`] starts [`Slot::Unbound`] and materializes on first
//! binding — never eagerly at declaration, so the backend choice always
//! has full usage context. Cloning the handle shares the backend (natural
//! fan-out for explicit sharing); the last drop closes. No central index
//! exists: resolution, keepers, and assertions all operate on handles
//! already in hand.

use std::sync::{Arc, Mutex};

#[cfg(not(miri))]
use oxdock_process::{OsPipeReader, OsPipeWriter, create_os_pipe};

use crate::backend::PipeInner;

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
pub type PipeHandle = Arc<Mutex<Slot>>;

/// Mint a fresh unbound handle, like bare `LET $p: PIPE`.
pub fn new_handle() -> PipeHandle {
    Arc::new(Mutex::new(Slot::Unbound))
}
