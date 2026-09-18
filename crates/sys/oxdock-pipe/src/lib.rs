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

pub use backend::{
    KeeperGuard, PipeInfo, PipeInner, PipeKindDesc, ScriptPipe, ScriptPipeEndpoint, inspect, peek,
    script_backend,
};
pub use slot::{
    Materialized, PipeHandle, SharedInput, SharedOutput, Slot, materialize, new_handle,
};
#[cfg(not(miri))]
pub use slot::{OsPipeEntry, OsPipeReader, OsPipeWriter, create_os_pipe};
pub use spill::{DEFAULT_MAX_BACKLOG, DEFAULT_SPILL_THRESHOLD, SpillBuffer};
