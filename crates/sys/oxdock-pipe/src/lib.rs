//! Runtime-managed stream backends for OxDock: spillable script buffers, OS kernel
//! pairs, and the owned handle slots that bind them lazily.
//!
//! A pipe is a runtime-managed byte stream with exactly one interface:
//! declare it, wire it with `WITH_IO`, and pass or copy the handle
//! across calls, tasks, and module boundaries. Script code never
//! manages descriptors, temp files, or platform plumbing.
//!
//! This crate sits below `oxdock-parser` (whose `PIPE` values hold
//! [`PipeHandle`]s) and beside `oxdock-process` (whose stdio types the
//! backends plug into). No central pipe index lives here or anywhere:
//! resolution, keepers, and assertions all operate on handles already in
//! hand.

pub mod backend;
pub mod diagnostics;
pub mod slot;
pub mod spill;

pub use backend::{
    KeeperGuard, PipeInfo, PipeInner, PipeKindDesc, ScriptPipe, ScriptPipeEndpoint, inspect, peek,
    script_backend,
};
pub use slot::{
    Materialized, PipeHandle, SharedInput, SharedOutput, Slot, materialize, new_handle_in_task,
};
#[cfg(not(miri))]
pub use slot::{OsPipeEntry, OsPipeReader, OsPipeWriter, create_os_pipe};
pub use spill::{DEFAULT_MAX_BACKLOG, DEFAULT_SPILL_THRESHOLD, SpillBuffer};
