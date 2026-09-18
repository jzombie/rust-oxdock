//! Anonymous pipe backends for OxDock: spillable script buffers, OS kernel
//! pairs, and the owned handle slots that bind them lazily.
//!
//! This crate sits below `oxdock-parser` (whose `PIPE` values hold
//! [`PipeHandle`]s) and beside `oxdock-process` (whose stdio types the
//! backends plug into). No central pipe index lives here or anywhere:
//! resolution, keepers, and assertions all operate on handles already in
//! hand.

pub mod backend;
pub mod slot;
pub mod spill;

pub use backend::{KeeperGuard, PipeInner, ScriptPipe, ScriptPipeEndpoint};
#[cfg(not(miri))]
pub use slot::OsPipeEntry;
pub use slot::{PipeHandle, Slot, new_handle};
pub use spill::{DEFAULT_MAX_BACKLOG, DEFAULT_SPILL_THRESHOLD, SpillBuffer};
