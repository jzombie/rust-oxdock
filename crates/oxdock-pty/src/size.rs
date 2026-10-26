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

/// Fallback geometry when neither the step nor the host names one:
/// CI logs, services, and redirects have no window to report.
pub const DEFAULT_TTY_ROWS: u16 = 24;
/// Fallback width, see [`DEFAULT_TTY_ROWS`].
pub const DEFAULT_TTY_COLS: u16 = 80;
/// Host-window repoll interval for the watch on platforms without a
/// window signal (Windows): prompt enough for interactive resizing
/// to feel live, sparse enough that one probe per live session is
/// noise. Unix learns through SIGWINCH instead and never ticks.
#[cfg(windows)]
pub const HOST_FOLLOW_POLL_MS: u64 = 250;

/// Merge explicit step geometry over a host reading: each named
/// dimension wins, unnamed dimensions take the host when it reports
/// one, and anything still unnamed falls back to the 24x80 default.
/// Pure, so unit tests pin every combination without a terminal.
fn merge_size(rows: Option<u16>, cols: Option<u16>, host: Option<PtySize>) -> PtySize {
    PtySize {
        rows: rows
            .or_else(|| host.map(|size| size.rows))
            .unwrap_or(DEFAULT_TTY_ROWS),
        cols: cols
            .or_else(|| host.map(|size| size.cols))
            .unwrap_or(DEFAULT_TTY_COLS),
        seq: 0,
    }
}

/// Merge explicit step geometry over the host terminal: each named
/// dimension wins, unnamed dimensions read the host console when one
/// sits behind our own stdio, and anything still unnamed falls back
/// to the 24x80 default. Pure over its inputs except for the host
/// probe, so unit tests pin the merge with explicit values while the
/// probe itself stays environment-bound.
pub fn resolve_size(rows: Option<u16>, cols: Option<u16>) -> PtySize {
    merge_size(rows, cols, host_terminal_size())
}

/// Our own window, when our stdio sits behind one: stdout first (the
/// rendering target), then stdin, then stderr. `None` under redirects,
/// services, and anywhere the probe fails, so callers always keep a
/// fallback. Read once per spawn, never watched: batch steps are
/// short-lived, and live window-following stays a later feature.
pub fn host_terminal_size() -> Option<PtySize> {
    #[cfg(unix)]
    return unix_terminal_size();
    #[cfg(windows)]
    return windows_terminal_size();
    #[cfg(not(any(unix, windows)))]
    return None;
}

/// Terminal size via `TIOCGWINSZ` over our own stdio. Zero dimensions
/// (a wedged descriptor reporting success with no window) read as no
/// terminal, same as a failed probe.
#[cfg(unix)]
fn unix_terminal_size() -> Option<PtySize> {
    for fd in [libc::STDOUT_FILENO, libc::STDIN_FILENO, libc::STDERR_FILENO] {
        // SAFETY: `ws` is a plain-integer struct fully written by the
        // kernel on success; zeroed first so no uninitialized memory
        // can leak into the read below on any platform.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: `TIOCGWINSZ` only writes `ws`; the fd is borrowed,
        // never owned or closed here.
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } != 0 {
            continue;
        }
        if ws.ws_row > 0 && ws.ws_col > 0 {
            return Some(PtySize {
                rows: ws.ws_row,
                cols: ws.ws_col,
                seq: 0,
            });
        }
    }
    None
}

/// Console window via the screen-buffer viewport: buffer dimensions
/// include scrollback, so only the visible `srWindow` rectangle
/// counts. Any failure (redirected stdio, no console, service
/// session) reads as no terminal.
#[cfg(windows)]
fn windows_terminal_size() -> Option<PtySize> {
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };
    for handle_id in [STD_OUTPUT_HANDLE, STD_INPUT_HANDLE] {
        // SAFETY: `GetStdHandle` returns a borrowed pseudo-handle;
        // never owned or closed here.
        let handle = unsafe { GetStdHandle(handle_id) };
        if handle.is_null() {
            continue;
        }
        // SAFETY: `info` is a plain-integer struct fully written on
        // success; zeroed first so a short write cannot leak
        // uninitialized memory into the read below.
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: only writes `info`; the handle is borrowed.
        if unsafe { GetConsoleScreenBufferInfo(handle, &mut info) } == 0 {
            continue;
        }
        let window = info.srWindow;
        let cols = window.Right - window.Left + 1;
        let rows = window.Bottom - window.Top + 1;
        if rows > 0 && cols > 0 {
            return Some(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                seq: 0,
            });
        }
    }
    None
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
    /// Re-read a host-tracked window and record it when it moved,
    /// returning whether the sequence bumped. Sources without a host
    /// window (remote sessions, static cells) keep the default no-op:
    /// only `WatchSize` cells with unnamed dimensions ever report
    /// true. The session supervisor calls this on window-change
    /// events (SIGWINCH on Unix, repoll ticks where no signal
    /// exists), never on a timer of its own.
    fn repoll(&self) -> bool {
        false
    }
    /// Whether any dimension tracks the host window: only then does
    /// the supervisor need a window-change watch (signal thread or
    /// repoll ticks) for this source. Static cells and remote
    /// sessions report false and run exactly as before.
    fn follows_host(&self) -> bool {
        false
    }
}

struct WatchState {
    size: PtySize,
    seq: u64,
    /// Pinned dimensions: `Some` sides never move under repoll, `None`
    /// sides re-read the host on every [`WatchSize::repoll`].
    want_rows: Option<u16>,
    want_cols: Option<u16>,
}

/// Condvar-backed size cell. `set` records a new size and wakes
/// waiters; waiters sleep until the sequence moves or `stop` flips.
pub struct WatchSize {
    state: Mutex<WatchState>,
    changed: Condvar,
}

impl WatchSize {
    /// Cell holding `size` at sequence zero. Both dimensions pinned:
    /// [`WatchSize::repoll`] is a no-op and no watch is needed.
    pub fn new(rows: u16, cols: u16) -> Arc<Self> {
        Self::seeded(Some(rows), Some(cols), PtySize::new(rows, cols))
    }

    /// Cell seeded from the host terminal for unnamed dimensions:
    /// starts at the resolved size and records host moves through
    /// [`WatchSize::repoll`], which the session supervisor drives on
    /// window-change events. Explicit dimensions stay pinned; only
    /// unnamed sides move. Fully explicit geometry behaves exactly
    /// like [`WatchSize::new`].
    pub fn following_host(rows: Option<u16>, cols: Option<u16>) -> Arc<Self> {
        let initial = resolve_size(rows, cols);
        Self::seeded(rows, cols, initial)
    }

    /// [`WatchSize::following_host`] with an injectable probe, so unit
    /// tests pin the seed and repoll behavior without owning a
    /// terminal.
    #[cfg(test)]
    fn following(
        rows: Option<u16>,
        cols: Option<u16>,
        probe: fn() -> Option<PtySize>,
    ) -> Arc<Self> {
        let initial = merge_size(rows, cols, probe());
        Self::seeded(rows, cols, initial)
    }

    fn seeded(want_rows: Option<u16>, want_cols: Option<u16>, initial: PtySize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(WatchState {
                size: initial,
                seq: 0,
                want_rows,
                want_cols,
            }),
            changed: Condvar::new(),
        })
    }

    /// Re-read the host window for unnamed dimensions and record it
    /// when it moved, waking the resize watcher. Returns whether the
    /// sequence bumped. Same-size probes stay silent: applying an
    /// unchanged size would still SIGWINCH the child.
    pub fn repoll(&self) -> bool {
        self.repoll_with(host_terminal_size)
    }

    /// [`WatchSize::repoll`] with an injectable probe; test-only in
    /// practice, but shared with the real path so the tested merge is
    /// the shipped merge.
    fn repoll_with(&self, probe: fn() -> Option<PtySize>) -> bool {
        let (want_rows, want_cols) = {
            let state = self.state.lock().expect("size cell lock unpoisoned");
            (state.want_rows, state.want_cols)
        };
        let next = merge_size(want_rows, want_cols, probe());
        let mut state = self.state.lock().expect("size cell lock unpoisoned");
        if next.rows != state.size.rows || next.cols != state.size.cols {
            state.size = PtySize::new(next.rows, next.cols);
            state.seq += 1;
            self.changed.notify_all();
            true
        } else {
            false
        }
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

    fn repoll(&self) -> bool {
        WatchSize::repoll(self)
    }

    fn follows_host(&self) -> bool {
        let state = self.state.lock().expect("size cell lock unpoisoned");
        state.want_rows.is_none() || state.want_cols.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_geometry_wins_over_host() {
        // Explicit dims hold whatever the host reports (or not): the
        // merge never consults the probe for a named dimension.
        let size = resolve_size(Some(40), Some(100));
        assert_eq!((size.rows, size.cols), (40, 100));
    }

    #[test]
    fn unnamed_geometry_falls_back_without_host() {
        // Under test runners stdout is captured, never a terminal, so
        // the probe finds nothing and the default carries. A developer
        // terminal running this suite keeps explicit coverage above;
        // this pins the floor, not the window.
        let size = resolve_size(None, None);
        let host = host_terminal_size();
        match host {
            Some(seen) => assert_eq!((size.rows, size.cols), (seen.rows, seen.cols)),
            None => assert_eq!((size.rows, size.cols), (DEFAULT_TTY_ROWS, DEFAULT_TTY_COLS)),
        }
    }

    #[test]
    fn repoll_applies_host_changes_for_unnamed_dims() {
        // Seeded hostless, then the window arrives: the repoll must
        // merge explicit rows over probed cols and bump the sequence.
        // A repeat probe stays silent: same-size applies would still
        // SIGWINCH the child.
        fn absent() -> Option<PtySize> {
            None
        }
        fn window() -> Option<PtySize> {
            Some(PtySize::new(51, 123))
        }
        let cell = WatchSize::following(Some(40), None, absent);
        assert_eq!((cell.current().rows, cell.current().cols), (40, 80));
        assert_eq!(cell.current().seq, 0);
        assert!(cell.repoll_with(window));
        let current = cell.current();
        assert_eq!((current.rows, current.cols), (40, 123));
        assert!(current.seq >= 1);
        assert!(!cell.repoll_with(window));
    }

    #[test]
    fn repoll_ignores_unchanged_probes() {
        // A probe matching the cell must never bump the sequence.
        fn absent() -> Option<PtySize> {
            None
        }
        let cell = WatchSize::following(None, None, absent);
        assert!(!cell.repoll_with(absent));
        assert_eq!(cell.current().seq, 0);
    }

    #[test]
    fn explicit_cells_never_repoll() {
        // Both dims named: every probe merges back to the pinned size,
        // whatever the host reports, and nothing follows the host.
        fn window() -> Option<PtySize> {
            Some(PtySize::new(51, 123))
        }
        let cell = WatchSize::following(Some(40), Some(100), window);
        assert_eq!((cell.current().rows, cell.current().cols), (40, 100));
        assert!(!cell.repoll_with(window));
        assert_eq!(cell.current().seq, 0);
        assert!(!cell.follows_host());
    }
}
