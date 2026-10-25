//! Shared pseudo-terminal pump for OxDock.
//!
//! Spawns a child under a pty, pumps caller bytes through the master,
//! applies resizes, drains to quiescence, and reports the exit code.
//! Used by local `WITH_IO [tty]` runs and the SSH plugin's remote
//! terminal sessions alike, so both share one pump, one drain policy,
//! and one ConPTY handshake.
//!
//! The crate owns threads and OS handles but no run model: callers
//! supply plain byte sources and sinks plus a size source, and get a
//! blocking call back. It never touches `oxdock-core`, `oxdock-process`,
//! or any pipe backend, keeping the sync core and its Miri story
//! untouched. Miri cannot allocate a real pty, so tests that spawn one
//! live with the callers under `ignore`; everything here is either
//! pure or constructor-only.

mod drain;
mod filter;
mod run;
mod size;

pub use drain::{DRAIN_CAP_MS, DRAIN_QUIESCE_MS, drain_overdue};
pub use filter::DsrFilter;
pub use run::{PtyInput, PtyOutcome, PtyRequest, ReadChunk, run_pty_session};
pub use size::{PtySize, SizeSource, WatchSize};
