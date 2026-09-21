//! Server lifetime state: session queue, shutdown teardown, handle Drop.
//!
//! There is deliberately no global registry: every `SSH_SERVER` DSL value
//! carries an `Arc<ServerState>`, so server lifetime follows handle
//! lifetime. When the last handle drops, [`ServerState::drop`] signals the
//! runtime thread and wakes every waiter; nothing leaks.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::mpsc;

/// How long `SSH_CLOSE` waits for the runtime thread before detaching it.
pub const CLOSE_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounded handoff depth between the russh runtime and sync pump threads.
/// Bounds memory under a slow consumer; the async side applies SSH
/// backpressure instead of growing a queue.
pub const CHANNEL_CAPACITY: usize = 64;

/// One direction of the pump-to-wire handoff.
#[derive(Debug)]
pub enum DownMsg {
    Data(Bytes),
    Eof,
}

/// Wire bytes toward the DSL pipes. `Eof` is a half-close: no more bytes
/// will arrive, but the channel itself stays open until the pump drains
/// and tears down. Queued before any later messages (FIFO), so bytes
/// sent before the EOF still flush first.
#[derive(Debug)]
pub enum UpMsg {
    Data(Bytes),
    Eof,
}

/// A freshly authenticated channel waiting for an `SSH_DEQUEUE` call.
/// The wire-writer task is already spawned; the pump side of the queue
/// pairs it with explicit DSL pipes.
#[derive(Debug)]
pub struct PendingSession {
    /// Command from an `exec` request, if the client used one.
    pub exec_command: Option<String>,
    /// Authenticated username, captured at auth time.
    pub username: Option<String>,
    /// Peer socket address, captured at connection time.
    pub peer_addr: Option<SocketAddr>,
    /// Terminal dimensions for this connection's channels: the handler
    /// records pty and window-change requests here, and the session's pty
    /// pump polls it. Owned per connection, never shared across guests.
    pub pty_size: SharedPtySize,
    /// Wire bytes toward the DSL pipes. Bounded ([`CHANNEL_CAPACITY`]):
    /// the async sender applies SSH backpressure instead of growing a
    /// queue. Closed by the wire side when the channel goes away, so the
    /// pump observes the end by drain.
    pub up_rx: mpsc::Receiver<UpMsg>,
    /// DSL bytes toward the wire. The pump sends [`DownMsg::Eof`] on
    /// stdin EOF, then drops the sender.
    pub down_tx: mpsc::Sender<DownMsg>,
}

/// Dequeue state shared between the russh runtime thread (producers) and
/// `SSH_ACCEPT` worker threads (consumers).
#[derive(Debug)]
pub struct QueueState {
    sessions: VecDeque<PendingSession>,
    shutdown: bool,
}

/// Dequeue outcome for one wait tick.
pub enum Dequeue {
    Session(PendingSession),
    Shutdown,
    Empty,
}

/// Multi-consumer session queue: any number of concurrent `SSH_ACCEPT`
/// calls pop distinct sessions FIFO.
#[derive(Debug)]
pub struct SessionQueue {
    state: Mutex<QueueState>,
    cvar: Condvar,
}

impl SessionQueue {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(QueueState {
                sessions: VecDeque::new(),
                shutdown: false,
            }),
            cvar: Condvar::new(),
        }
    }

    /// Enqueue an authenticated session from the runtime thread.
    /// Never blocks, never holds the lock across an await.
    pub fn push(&self, session: PendingSession) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if guard.shutdown {
            return;
        }
        guard.sessions.push_back(session);
        self.cvar.notify_one();
    }

    /// Signal teardown and wake every waiter. Poison-tolerant and
    /// non-blocking: safe to call from `Drop`.
    pub fn signal_shutdown(&self) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        guard.shutdown = true;
        self.cvar.notify_all();
    }

    /// Non-blocking dequeue attempt.
    pub fn try_pop(&self) -> Dequeue {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(session) = guard.sessions.pop_front() {
            Dequeue::Session(session)
        } else if guard.shutdown {
            Dequeue::Shutdown
        } else {
            Dequeue::Empty
        }
    }

    /// Wait for a session up to `timeout`. Returns [`Dequeue::Empty`] on
    /// tick expiry so the caller can poll cancellation.
    pub fn wait_for_session(&self, timeout: Duration) -> Dequeue {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (mut guard, _timeout_result) = self
            .cvar
            .wait_timeout_while(guard, timeout, |state| {
                state.sessions.is_empty() && !state.shutdown
            })
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(session) = guard.sessions.pop_front() {
            Dequeue::Session(session)
        } else if guard.shutdown {
            Dequeue::Shutdown
        } else {
            Dequeue::Empty
        }
    }
}

impl Default for SessionQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Shutdown signal from the DSL-visible handle to the runtime thread.
#[derive(Debug)]
pub enum ShutdownSignal {
    Close,
}

/// Terminal dimensions for one connection's channels, requested by the
/// outer client. Each connection owns its cell (minted in `new_client`,
/// attached to the session at announce), so concurrent guests never see
/// each other's sizes. Zero rows/cols (clients that report none) clamp to
/// the 24x80 default at apply time, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

impl PtySize {
    pub fn new(rows: u32, cols: u32) -> Self {
        Self {
            rows: rows.try_into().unwrap_or(u16::MAX),
            cols: cols.try_into().unwrap_or(u16::MAX),
        }
    }

    /// Kernel-ready size: zeros fall back to 24x80 since a 0x0 pty
    /// breaks fullscreen apps (they lay out for a nonexistent screen).
    pub fn effective(&self) -> (u16, u16) {
        (
            if self.rows == 0 { 24 } else { self.rows },
            if self.cols == 0 { 80 } else { self.cols },
        )
    }
}

impl Default for PtySize {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

/// Shareable terminal-size cell: one instance per connection, held by
/// the russh handler (writer: pty and window-change requests) and the
/// dequeued session (reader: the session's pty pump polls it).
///
/// The sequence number distinguishes "outer requested this" from "nobody
/// asked yet": a pump that opened on explicit dimensions must not have
/// them clobbered by a stale default on its first poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySizeStamp {
    pub size: PtySize,
    pub(crate) seq: u64,
}

impl PtySizeStamp {
    fn bump(&mut self, size: PtySize) {
        self.size = size;
        self.seq = self.seq.wrapping_add(1);
    }
}

pub type SharedPtySize = Arc<Mutex<PtySizeStamp>>;

/// Fresh size cell at the default dimensions, sequence zero.
pub fn shared_pty_size() -> SharedPtySize {
    Arc::new(Mutex::new(PtySizeStamp {
        size: PtySize::default(),
        seq: 0,
    }))
}

/// Record dimensions on a shared cell (poison-tolerant).
pub fn set_shared_pty_size(cell: &SharedPtySize, size: PtySize) {
    if let Ok(mut slot) = cell.lock() {
        slot.bump(size);
    }
}

/// Snapshot a shared cell with its sequence (poison-tolerant).
pub fn snapshot_pty_size(cell: &SharedPtySize) -> PtySizeStamp {
    cell.lock()
        .map(|slot| *slot)
        .unwrap_or_else(|poison| *poison.into_inner())
}

/// Lifetime state behind one `SSH_SERVER` value.
#[derive(Debug)]
pub struct ServerState {
    id: String,
    local_addr: SocketAddr,
    /// Display/MAP address text: the physical bind for socket servers, the
    /// virtual endpoint echo when socketless (`--offline`).
    addr_text: String,
    queue: Arc<SessionQueue>,
    /// Signal to the runtime thread. `None` once consumed by `SSH_CLOSE`.
    /// Plain std channel: `Sender` is `Send + Sync` and `send` never blocks.
    shutdown_tx: Mutex<Option<std::sync::mpsc::Sender<ShutdownSignal>>>,
    /// Runtime thread handle, taken by `SSH_CLOSE` for a bounded join.
    /// Dropped (detached) if the handle value dies first; the thread then
    /// self-reaps when sessions drain. `None` for socketless servers,
    /// which spawn no thread.
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// The endpoint this server was acquired for, plus the registry
    /// holding its slot: close/drop frees the slot for re-serve loops.
    registry: Arc<oxdock_net_plugin::EndpointRegistry>,
    endpoint: oxdock_net_plugin::VirtualEndpoint,
}

/// Construction bundle for [`ServerState::new`]: identity, runtime
/// halves, and endpoint bookkeeping in one argument (stays under the
/// argument-count lint as the surface grows).
///
/// [`ServerState::new`]: ServerState::new
pub struct ServerConfig {
    pub id: String,
    pub local_addr: SocketAddr,
    pub addr_text: String,
    pub queue: Arc<SessionQueue>,
    pub shutdown_tx: std::sync::mpsc::Sender<ShutdownSignal>,
    pub thread: Option<std::thread::JoinHandle<()>>,
    pub registry: Arc<oxdock_net_plugin::EndpointRegistry>,
    pub endpoint: oxdock_net_plugin::VirtualEndpoint,
}

impl ServerState {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            id: config.id,
            local_addr: config.local_addr,
            addr_text: config.addr_text,
            queue: config.queue,
            shutdown_tx: Mutex::new(Some(config.shutdown_tx)),
            thread: Mutex::new(config.thread),
            registry: config.registry,
            endpoint: config.endpoint,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Address text for display and result MAPs: the physical bind for
    /// socket servers, the virtual endpoint echo when socketless.
    pub fn addr_text(&self) -> &str {
        &self.addr_text
    }

    pub fn queue(&self) -> &Arc<SessionQueue> {
        &self.queue
    }

    /// Best-effort shutdown: wake queue waiters, nudge the runtime
    /// thread, and free the registry slot. Shared by `SSH_CLOSE` and
    /// `Drop`; never blocks, never joins.
    pub fn request_shutdown(&self) {
        self.queue.signal_shutdown();
        if let Ok(mut slot) = self.shutdown_tx.lock()
            && let Some(tx) = slot.take()
        {
            let _ = tx.send(ShutdownSignal::Close);
        }
        self.registry.release(&self.endpoint);
    }

    /// Bounded join of the runtime thread. Returns true when the thread
    /// exited within `timeout`; on timeout the thread is detached and
    /// self-reaps once sessions drain.
    pub fn join_thread(&self, timeout: Duration) -> bool {
        let handle = self
            .thread
            .lock()
            .map(|mut slot| slot.take())
            .unwrap_or(None);
        let Some(handle) = handle else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        loop {
            if handle.is_finished() {
                let _ = handle.join();
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ServerState {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_pair() -> (PendingSession, mpsc::Sender<DownMsg>) {
        let (_up_tx, up_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (down_tx, _down_rx) = mpsc::channel(1);
        let session = PendingSession {
            exec_command: None,
            username: None,
            peer_addr: None,
            pty_size: shared_pty_size(),
            up_rx,
            down_tx: down_tx.clone(),
        };
        (session, down_tx)
    }

    #[test]
    fn shutdown_wakes_empty_waiter() {
        let queue = SessionQueue::new();
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| queue.wait_for_session(Duration::from_secs(30)));
            std::thread::sleep(Duration::from_millis(50));
            queue.signal_shutdown();
            assert!(matches!(
                waiter.join().expect("waiter joins"),
                Dequeue::Shutdown
            ));
        });
    }

    #[test]
    fn fifo_ordering() {
        let queue = SessionQueue::new();
        let (first, _) = pending_pair();
        let (second, _) = pending_pair();
        queue.push(first);
        queue.push(second);
        assert!(matches!(queue.try_pop(), Dequeue::Session(_)));
        assert!(matches!(queue.try_pop(), Dequeue::Session(_)));
        assert!(matches!(queue.try_pop(), Dequeue::Empty));
    }

    #[test]
    fn push_after_shutdown_dropped() {
        let queue = SessionQueue::new();
        queue.signal_shutdown();
        let (session, _) = pending_pair();
        queue.push(session);
        assert!(matches!(queue.try_pop(), Dequeue::Shutdown));
    }
}
