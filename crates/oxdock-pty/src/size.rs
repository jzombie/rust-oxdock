//! Terminal size plumbing.
//!
//! The pty starts at an explicit size and follows window changes
//! through a shared cell: whoever owns the window bumps the sequence
//! and pokes, the resize watcher applies the newest size to the
//! master. Sequencing keeps a stale default from clobbering an
//! explicit open size. `WatchSize` is the local form (static size set
//! once, or bumped by the owner); interactive hosts back the same
//! trait with their own window-change source.

use std::sync::{Arc, Condvar, Mutex, atomic::AtomicBool};

/// Terminal dimensions plus a bump-on-change sequence number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtySize {
    /// Character rows.
    pub rows: u16,
    /// Character columns.
    pub cols: u16,
    /// Bumped on every record so watchers can tell new from seen.
    pub seq: u64,
}

impl PtySize {
    /// Sized terminal with a zero sequence: the opener's size, applied
    /// before any watcher traffic can supersede it.
    pub fn new(rows: u16, cols: u16) -> Self {
        Self { rows, cols, seq: 0 }
    }
}

/// Window-change source for the resize watcher. Implementations block
/// in `wait_changed` until a newer sequence arrives or `stop` flips;
/// `poke` wakes a parked waiter so retirement never hangs.
pub trait SizeSource: Send + Sync {
    /// Newest size with its sequence.
    fn current(&self) -> PtySize;
    /// Newest size when its sequence passes `after_seq`, or `None`
    /// once `stop` is set.
    fn wait_changed(&self, after_seq: u64, stop: &AtomicBool) -> Option<PtySize>;
    /// Wake any parked waiter (retirement, or a check that cannot wait).
    fn poke(&self);
}

struct WatchState {
    size: PtySize,
    seq: u64,
}

/// Condvar-backed size cell. `set` records a new size and wakes
/// waiters; waiters sleep until the sequence moves or `stop` flips.
pub struct WatchSize {
    state: Mutex<WatchState>,
    changed: Condvar,
}

impl WatchSize {
    /// Cell holding `size` at sequence zero.
    pub fn new(rows: u16, cols: u16) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(WatchState {
                size: PtySize::new(rows, cols),
                seq: 0,
            }),
            changed: Condvar::new(),
        })
    }

    /// Record a new size, bumping the sequence and waking waiters.
    /// A `set` concurrent with retirement still notifies; the waiter
    /// re-checks `stop` before acting, so a late size never applies
    /// past teardown.
    pub fn set(&self, rows: u16, cols: u16) {
        let mut state = self.state.lock().expect("size cell lock unpoisoned");
        state.size = PtySize::new(rows, cols);
        state.seq += 1;
        self.changed.notify_all();
    }
}

impl SizeSource for WatchSize {
    fn current(&self) -> PtySize {
        let state = self.state.lock().expect("size cell lock unpoisoned");
        let mut size = state.size;
        size.seq = state.seq;
        size
    }

    fn wait_changed(&self, after_seq: u64, stop: &AtomicBool) -> Option<PtySize> {
        let mut state = self.state.lock().expect("size cell lock unpoisoned");
        while state.seq == after_seq && !stop.load(std::sync::atomic::Ordering::SeqCst) {
            state = self.changed.wait(state).expect("size cell wait unpoisoned");
        }
        if state.seq == after_seq {
            return None;
        }
        let mut size = state.size;
        size.seq = state.seq;
        Some(size)
    }

    fn poke(&self) {
        self.changed.notify_all();
    }
}
