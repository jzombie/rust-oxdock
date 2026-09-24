//! Capture-sink construction over the shared spill buffer.
//!
//! The buffer implementation lives in `oxdock-pipe` (pipe backlogs and
//! capture sinks share it); this module keeps the crate's spill/backlog
//! thresholds : small under `cfg(test)` so tests exercise the spill path
//! without multi-megabyte payloads : and the threshold-aware constructor
//! production and tests share.

/// Memory threshold before spilling to disk.
#[cfg(not(test))]
#[cfg_attr(miri, allow(dead_code))]
pub(super) const SPILL_THRESHOLD: usize = 8 * 1024 * 1024; // 8 MiB
#[cfg(test)]
#[cfg_attr(miri, allow(dead_code))]
pub(super) const SPILL_THRESHOLD: usize = 1024 * 1024; // 1 MiB for tests

/// Maximum active backlog before returning an error.
#[cfg(not(test))]
#[cfg_attr(miri, allow(dead_code))]
pub(super) const MAX_BACKLOG: u64 = 100 * 1024 * 1024; // 100 MiB
#[cfg(test)]
#[cfg_attr(miri, allow(dead_code))]
pub(super) const MAX_BACKLOG: u64 = 2 * 1024 * 1024; // 2 MiB for tests

pub(super) use oxdock_pipe::SpillBuffer;

/// Spill buffer with this crate's thresholds (see above). Production and
/// tests construct through here so both honor the same cfg-selected
/// values; the pipe backend constructs through `oxdock-pipe` directly.
pub(super) fn new_spill_buffer() -> SpillBuffer {
    SpillBuffer::with_thresholds(SPILL_THRESHOLD, MAX_BACKLOG)
}
