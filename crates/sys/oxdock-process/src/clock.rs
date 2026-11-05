//! Execution-metadata clock: seconds since the Unix epoch for load
//! times and status probes. Under Miri isolation there is no wall
//! clock (`clock_gettime` REALTIME is blocked), so the Miri build
//! answers from a process-local monotonic counter instead: ordering
//! is preserved, absolute values are synthetic. Display metadata
//! only: never use for timeouts or durations, those go through
//! `Instant`, which Miri supports.

/// Seconds since the Unix epoch, or a synthetic monotonic tick under
/// Miri isolation where no wall clock exists.
#[cfg(not(miri))]
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Synthetic replacement for [`unix_now`] under Miri isolation:
/// a process-local counter, so successive loads order correctly
/// while nothing reads the (unavailable) host clock.
#[cfg(miri)]
pub fn unix_now() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TICKS: AtomicU64 = AtomicU64::new(0);
    TICKS.fetch_add(1, Ordering::Relaxed)
}
