//! Drain quiescence policy: how long the supervisor waits for a dying
//! console's last bytes before tearing it down.
//!
//! ConPTY holds the output pipe open past child death, so EOF cannot
//! stand in for the flush: the supervisor must distinguish a console
//! still flushing from a stranded one. The rule anchors on the later
//! of last output and death, waits one quiet window, and caps the
//! total wait: bytes in flight re-arm the deadline, silence plus the
//! cap ends it no matter what. Pure function of timestamps (monotonic
//! millis, never wall clock: NTP skew must not move a deadline), so
//! unit tests pin the policy with no threads or terminals.

use std::time::Instant;

/// Quiet window after the last byte before teardown may proceed.
pub const DRAIN_QUIESCE_MS: u64 = 250;
/// Hard cap on the post-exit drain: teardown runs past this point no
/// matter what, so a pathological console never strands the close.
pub const DRAIN_CAP_MS: u64 = 30_000;

/// Millis since a monotonic epoch. Monotonic by construction:
/// interval math must never read the wall clock.
pub fn elapsed_ms(epoch: Instant) -> u64 {
    epoch.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

/// Whether the console teardown is due: output has been silent for a
/// full quiet window anchored at the later of last output and death,
/// or the cap elapsed. No observed death means never due: a live
/// child keeps its console regardless of silence.
pub fn drain_overdue(last_output_ms: u64, exited_ms: Option<u64>, now_ms: u64) -> bool {
    let Some(exited) = exited_ms else {
        return false;
    };
    now_ms.saturating_sub(last_output_ms.max(exited)) >= DRAIN_QUIESCE_MS
        || now_ms.saturating_sub(exited) >= DRAIN_CAP_MS
}

/// Millis until the drain deadline from here: the quiescence window
/// past the later of last output and child death, floored at one so a
/// due drain still schedules (and observes) its release.
pub fn quiet_remaining_ms(activity_ms: u64, exited_ms: Option<u64>, now_ms: u64) -> u64 {
    let anchor = activity_ms.max(exited_ms.unwrap_or(now_ms));
    DRAIN_QUIESCE_MS
        .saturating_sub(now_ms.saturating_sub(anchor))
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_waits_for_quiescence_not_death() {
        // Fresh death with recent output: keep waiting for the flush.
        assert!(!drain_overdue(1000, Some(1000), 1000));
        assert!(!drain_overdue(1090, Some(1000), 1100));
        // Silent past the window: tear down even though the worker
        // never EOF'd on its own.
        assert!(drain_overdue(1000, Some(1000), 1000 + DRAIN_QUIESCE_MS));
        // Late flush resets the wait, until the cap.
        assert!(!drain_overdue(5000, Some(1000), 5100));
        assert!(drain_overdue(
            1000 + DRAIN_CAP_MS,
            Some(1000),
            1000 + DRAIN_CAP_MS
        ));
        // No death observed: never tear down a live child.
        assert!(!drain_overdue(1000, None, 1000 + DRAIN_CAP_MS + 1));
        // Silence before death must not bypass the post-exit wait: the
        // window anchors at the later of last output and death.
        assert!(!drain_overdue(500, Some(1000), 1000));
        assert!(!drain_overdue(500, Some(1000), 1000 + DRAIN_QUIESCE_MS - 1));
        assert!(drain_overdue(500, Some(1000), 1000 + DRAIN_QUIESCE_MS));
        // Output after death extends the window past the death mark.
        assert!(!drain_overdue(1100, Some(1000), 1200));
        assert!(drain_overdue(1100, Some(1000), 1100 + DRAIN_QUIESCE_MS));
    }
}
