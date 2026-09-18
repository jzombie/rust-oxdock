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

/// A freshly authenticated channel waiting for an `SSH_ACCEPT` call.
/// The wire-writer task is already spawned; the pump side of the queue
/// pairs it with explicit DSL pipes.
#[derive(Debug)]
pub struct PendingSession {
    /// Command from an `exec` request, if the client used one.
    pub exec_command: Option<String>,
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

/// Lifetime state behind one `SSH_SERVER` value.
#[derive(Debug)]
pub struct ServerState {
    id: String,
    local_addr: SocketAddr,
    queue: Arc<SessionQueue>,
    /// Signal to the runtime thread. `None` once consumed by `SSH_CLOSE`.
    /// Plain std channel: `Sender` is `Send + Sync` and `send` never blocks.
    shutdown_tx: Mutex<Option<std::sync::mpsc::Sender<ShutdownSignal>>>,
    /// Runtime thread handle, taken by `SSH_CLOSE` for a bounded join.
    /// Dropped (detached) if the handle value dies first; the thread then
    /// self-reaps when sessions drain.
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl ServerState {
    pub fn new(
        id: String,
        local_addr: SocketAddr,
        queue: Arc<SessionQueue>,
        shutdown_tx: std::sync::mpsc::Sender<ShutdownSignal>,
        thread: std::thread::JoinHandle<()>,
    ) -> Self {
        Self {
            id,
            local_addr,
            queue,
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            thread: Mutex::new(Some(thread)),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn queue(&self) -> &Arc<SessionQueue> {
        &self.queue
    }

    /// Best-effort shutdown: wake queue waiters and nudge the runtime
    /// thread. Shared by `SSH_CLOSE` and `Drop`; never blocks, never joins.
    pub fn request_shutdown(&self) {
        self.queue.signal_shutdown();
        if let Ok(mut slot) = self.shutdown_tx.lock()
            && let Some(tx) = slot.take()
        {
            let _ = tx.send(ShutdownSignal::Close);
        }
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
