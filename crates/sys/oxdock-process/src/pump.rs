//! Event-driven pump mechanism shared by host plugins.
//!
//! Every byte pump in the tree (SSH legs, NET sockets, child stdio)
//! follows one choreography: worker threads move bytes between pipes
//! and sinks, and a supervisor thread ends the session when the workers
//! finish, the child exits, or the task cancels. The mechanism here is
//! the part that never varies: a cancellation fan-out hub, a blocking
//! pipe read, and an exactly-once completion delivery that survives
//! worker panics. Supervisors keep their own event loops (pty drain
//! waits and memory linger semantics genuinely differ), but no
//! supervisor anywhere polls: workers wake on pipe data or close, the
//! hub wakes on cancel, and completions arrive as messages.
//!
//! The one rule senders must honor: queue the message first, then poke
//! the hub. A supervisor drains its queue before every hub wait, so a
//! poke can never arrive for a message it has not queued yet, and no
//! wakeup is ever lost.

use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use oxdock_pipe::{PipeInner, SharedInput};

/// Copy buffer size: 8192 bytes per chunk. Sized for interactive
/// latency, not bulk throughput; bulk legs chunk above this.
pub const PUMP_CHUNK: usize = 8192;

/// Cancellation fan-out for one pump session. One hub per supervised
/// call, shared by its workers and supervisor: `mark_cancelled` ends
/// every waiter at once, `poke` wakes the supervisor for a queued
/// event without cancelling. Sequence-guarded, so a poke that lands
/// between the supervisor's drain and its wait still wakes it.
#[derive(Debug)]
pub struct CancelHub {
    state: Mutex<bool>,
    woken: Condvar,
    sequence: AtomicU64,
}

impl CancelHub {
    /// Fresh hub: not cancelled, sequence zero.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(false),
            woken: Condvar::new(),
            sequence: AtomicU64::new(0),
        }
    }

    /// Current sequence. A supervisor records it, drains its queue,
    /// then waits: any poke since the drain changes the sequence and
    /// the wait returns at once.
    pub fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::SeqCst)
    }

    /// Whether cancellation was marked. Checked after every wake;
    /// flag checks on a wake are not polling.
    pub fn is_cancelled(&self) -> bool {
        self.state.lock().map(|flag| *flag).unwrap_or(true)
    }

    /// Mark cancellation and wake every waiter. Idempotent. Holds
    /// the state lock across the bump and the notify: a waiter holds
    /// the same lock across its check-to-sleep transition, so the wake
    /// can never slip between the check and the sleep.
    pub fn mark_cancelled(&self) {
        let mut flag = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *flag = true;
        self.sequence.fetch_add(1, Ordering::SeqCst);
        self.woken.notify_all();
    }

    /// Wake the supervisor for a queued event. No state change. Holds
    /// the state lock for the same lost-wakeup reasoning as
    /// [`CancelHub::mark_cancelled`]: the bump and the notify are one
    /// atomic step against every waiter's check-to-sleep.
    pub fn poke(&self) {
        let flag = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.sequence.fetch_add(1, Ordering::SeqCst);
        self.woken.notify_all();
        drop(flag);
    }

    /// Block until cancellation or a poke past `seen`. Returns the
    /// cancellation flag with the current sequence. Spurious wakes
    /// re-check the predicate, never escape.
    pub fn wait(&self, seen: u64) -> (bool, u64) {
        let mut flag = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while !*flag && self.sequence.load(Ordering::SeqCst) == seen {
            flag = self
                .woken
                .wait(flag)
                .unwrap_or_else(|poison| poison.into_inner());
        }
        let cancelled = *flag;
        drop(flag);
        (cancelled, self.sequence.load(Ordering::SeqCst))
    }
}

impl Default for CancelHub {
    fn default() -> Self {
        Self::new()
    }
}

/// One blocking chunk read for pump workers. Script backends wake on
/// data, close, or `PipeInner::cancel`; anything else blocks in the OS
/// read until its own EOF. Never times out: cancellation arrives as
/// EOF through `cancel`, not as a backstop tick.
pub fn read_pump_chunk(reader: &SharedInput, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut guard = reader
        .lock()
        .map_err(|_| std::io::Error::other("pump input lock poisoned"))?;
    guard.read(buf)
}

/// Run a worker body to completion, delivering `done` to `tx` exactly
/// once whether the body returns or panics, then poking `hub` so a
/// supervisor parked in [`CancelHub::wait`] ends its wait and drains
/// the queue. A supervisor drains before every wait and the hub is
/// sequence-guarded, so the completion can never arrive after the wait
/// starts yet go unseen: send-then-poke is the one rule. A supervisor
/// that waits on messages instead of joining handles must observe
/// panics too, or a panicking worker strands it past scope end. The
/// outcome (value or panic) rides in the message; the join handle just
/// returns unit.
pub fn run_guarded<T, M: Send>(
    hub: &CancelHub,
    tx: &Sender<M>,
    done: impl FnOnce(std::thread::Result<T>) -> M,
    body: impl FnOnce() -> T,
) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
    let _ = tx.send(done(outcome));
    hub.poke();
}

/// RAII cancellation bridge for single-threaded pumps (no supervisor
/// thread of their own to translate the hub). Arms a waiter that
/// cancels each pipe when the hub fires; dropping the guard releases
/// the waiter. The pump loop itself stays a blocking read: cancel
/// arrives as EOF, never as a tick.
pub struct CancelGuard {
    hub: Arc<CancelHub>,
    waiter: Option<std::thread::JoinHandle<()>>,
    /// Rendezvous: the waiter signals after reading its sequence and
    /// before parking, so a release that lands before the wait still
    /// wakes it. A bare poke is edge-triggered and would be lost.
    started: std::sync::mpsc::Receiver<()>,
}

impl CancelGuard {
    /// Watch `hub`, cancelling every pipe in `pipes` on fire. The hub
    /// must outlive the guard; pipes are usually task-scoped.
    pub fn arm(hub: &Arc<CancelHub>, pipes: Vec<Arc<PipeInner>>) -> Self {
        let watched = Arc::clone(hub);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let seen = watched.sequence();
            let _ = started_tx.send(());
            let (cancelled, _) = watched.wait(seen);
            if cancelled {
                for pipe in &pipes {
                    pipe.cancel();
                }
            }
        });
        Self {
            hub: Arc::clone(hub),
            waiter: Some(waiter),
            started: started_rx,
        }
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        // Rendezvous first (the waiter reached its wait), then poke
        // (never cancel: the pump may have ended cleanly), then join:
        // no thread outlives the pump. The poke also wakes any hub
        // supervisor; its predicate re-check absorbs it.
        let _ = self.started.recv();
        self.hub.poke();
        if let Some(waiter) = self.waiter.take() {
            let _ = waiter.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_wait_returns_on_poke_without_cancelling() {
        let hub = CancelHub::new();
        let seen = hub.sequence();
        hub.poke();
        let (cancelled, next) = hub.wait(seen);
        assert!(!cancelled);
        assert_ne!(next, seen);
    }

    #[test]
    fn hub_wait_wakes_on_cancel_from_another_thread() {
        // Same contract the supervisors rely on: a thread parked in
        // `wait` returns once `mark_cancelled` runs elsewhere.
        let hub = Arc::new(CancelHub::new());
        let parked = Arc::clone(&hub);
        let waiter = std::thread::spawn(move || parked.wait(parked.sequence()));
        std::thread::sleep(std::time::Duration::from_millis(50));
        hub.mark_cancelled();
        let (cancelled, _) = waiter.join().expect("waiter joins");
        assert!(cancelled);
    }

    #[test]
    fn guarded_delivery_survives_panic() {
        let hub = CancelHub::new();
        let (tx, rx) = std::sync::mpsc::channel();
        run_guarded(
            &hub,
            &tx,
            |outcome: std::thread::Result<u32>| outcome.is_err(),
            || -> u32 { panic!("boom") },
        );
        assert!(rx.recv().expect("exactly one message"), "panic delivered");
    }

    #[test]
    fn guard_cancels_pipes_on_fire_and_joins_on_drop() {
        use oxdock_pipe::ScriptPipe;
        let pipe = ScriptPipe::new();
        let inner = pipe.pipe_inner();
        let hub = Arc::new(CancelHub::new());
        {
            let _guard = CancelGuard::arm(&hub, vec![inner.clone()]);
            hub.mark_cancelled();
        }
        assert!(inner.is_cancelled(), "waiter cancelled the pipe");
    }
}
