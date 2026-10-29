//! Synchronous byte pumps between handoff queues and DSL pipes.
//!
//! All blocking pump funcs (`SSH_ACCEPT` / `SSH_PUMP_CHANNEL`,
//! `SSH_CONNECT`, `SSH_PUMP`) share one shape: one worker per
//! direction on scoped threads, an event-driven supervisor, and
//! `close_pipe` for prompt downstream EOF. The only async contact is
//! the bounded queue pair; blocking queue ops run exclusively on plain
//! engine-task threads, never on Tokio workers.
//!
//! Nothing here polls. Workers block in pipe reads (woken by data,
//! close, or `oxdock_pipe::PipeInner::cancel`) or in the queue receive (woken by a
//! message, a `Wake`, or sender drop); the supervisor blocks on the
//! task's [`CancelHub`] until a worker completion lands or cancel
//! fires. Flag checks after a wake are not polling: no wakeup is ever
//! spent on an empty check.
//!
//! `&StepCtx` is not `Sync`, so every context call (`pipe_reader`,
//! `pipe_writer`, `pipe_backend`, `close_pipe`, `cancel_hub`) happens
//! on the calling supervisor thread. Workers receive only owned,
//! thread-safe handles.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use oxdock_core::{StepCtx, Value};
use oxdock_process::{CancelHub, run_guarded as guard_worker};
use oxdock_process::{PUMP_CHUNK, ProcessManager, SharedInput, SharedOutput, read_pump_chunk};
use tokio::sync::mpsc;

use crate::state::{DownMsg, UpMsg};

/// Completion of one `pump_session` direction, by worker index (0 is
/// the pipe-to-wire worker, 1 the wire-to-pipe worker). Delivered
/// through [`guard_worker`], so a panicking worker reports instead of
/// stranding the supervisor past scope end.
enum SessionDone {
    InDone(std::thread::Result<Result<()>>),
    OutDone(std::thread::Result<Result<()>>),
}

/// Pump `up_rx -> writer` until the wire half-closes. The bounded queue
/// is FIFO, so an [`UpMsg::Eof`] always arrives after every byte sent
/// before it: return without waiting for the sender to drop (it stays
/// alive until the channel itself closes, which this return helps cause
/// via the supervisor's force-close : waiting for it would deadlock).
/// The supervisor force-closes the output pipe right after this worker
/// finishes, so downstream observes EOF promptly.
/// A supervisor wake releases a parked wait for cancellation or a
/// finished peer: queued bytes still flush first (every `Data` writes
/// unconditionally); only the wait ends. The wake travels its own
/// queue, so the data queue's sender-drop still means exactly "the
/// wire side is gone", on graceful close and on transport loss alike.
/// When `break_on_peer_done`, also ends once the stdin direction
/// finishes first (`peer_done`): the response producers are gone, so
/// anything still arriving has nowhere to go : this is what releases a
/// server pump whose inner leg (a `RUN` subprocess, an exited remote)
/// died while the outer client idles. Server-side only: a client must
/// linger for the reply and the exit-status, which legitimately arrive
/// after its own stdin EOF, so client pumps always wait for the wire
/// EOF or close.
fn pump_out(
    writer: &SharedOutput,
    mut up_rx: mpsc::Receiver<UpMsg>,
    mut wake_rx: mpsc::Receiver<()>,
    runtime: tokio::runtime::Runtime,
    cancel: &AtomicBool,
    peer_done: &AtomicBool,
    break_on_peer_done: bool,
) -> Result<()> {
    /// One wait outcome: queue traffic or a supervisor wake.
    enum Waited {
        Up(Option<UpMsg>),
        WakeUp,
    }
    loop {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        // One blocking wait on both queues: no timeout, no spin. The
        // bare current-thread runtime parks the thread (no drivers, no
        // threads of its own); either queue wakes it. `biased` keeps
        // queued bytes ahead of wakes. A closed wake queue ends the
        // worker like a gone wire: the supervisor is tearing down.
        let waited = runtime.block_on(async {
            tokio::select! {
                biased;
                message = up_rx.recv() => Waited::Up(message),
                wake = wake_rx.recv() => match wake {
                    Some(()) => Waited::WakeUp,
                    None => Waited::Up(None),
                },
            }
        });
        match waited {
            Waited::Up(Some(UpMsg::Data(bytes))) => {
                let mut guard = writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("SSH pump output lock poisoned"))?;
                guard
                    .write_all(&bytes)
                    .context("SSH pump output pipe write failed")?;
                guard.flush().context("SSH pump output pipe flush failed")?;
            }
            Waited::Up(_) => break,
            Waited::WakeUp => {
                if break_on_peer_done && peer_done.load(Ordering::SeqCst) {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Pump `reader -> down_tx` until pipe EOF. Cancellation arrives as
/// EOF through [`oxdock_pipe::PipeInner::cancel`]: the supervisor cancels the input
/// backend when the hub fires, so no tick is needed. A cancelled EOF
/// still sends [`DownMsg::Eof`] (the wire half-close is the orderly
/// teardown on every path); only an explicit local cancel skips it,
/// matching the old flag-break wire behavior exactly.
fn pump_in(
    reader: &SharedInput,
    down_tx: &mpsc::Sender<DownMsg>,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut buffer = [0u8; PUMP_CHUNK];
    loop {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        match read_pump_chunk(reader, &mut buffer) {
            Err(err) => bail!("SSH pump input pipe read failed: {err}"),
            Ok(0) => {
                if !cancel.load(Ordering::SeqCst) {
                    let _ = down_tx.blocking_send(DownMsg::Eof);
                }
                break;
            }
            Ok(count) => {
                if down_tx
                    .blocking_send(DownMsg::Data(Bytes::copy_from_slice(&buffer[..count])))
                    .is_err()
                {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Record the first worker error. Later errors lose: the first failure
/// explains the teardown.
pub(crate) fn note_failure(
    failed: &mut Option<anyhow::Error>,
    outcome: std::thread::Result<Result<()>>,
    worker: &str,
) {
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            if failed.is_none() {
                *failed = Some(err);
            }
        }
        Err(_) => {
            if failed.is_none() {
                *failed = Some(anyhow::anyhow!("SSH pump {worker} panicked"));
            }
        }
    }
}

/// Pump a live session between explicit DSL pipes until both directions
/// end or the task cancels. `up_rx` carries wire bytes into `out_pipe`,
/// `in_pipe` bytes flow into `down_tx`. The output pipe is
/// force-closed as soon as the wire direction ends so downstream
/// observes EOF promptly instead of waiting out unrelated writer and
/// keeper lifetimes. The first worker error wins.
///
/// Workers run detached: the supervisor never joins them, so a worker
/// parked in an uninterruptible read (an orphaned grandchild holding
/// its pipe past every kill) cannot strand the task thread. Every
/// worker owns its captures outright (`Arc` clones, moved queue ends)
/// and reports through [`guard_worker`]; the frame keeps its own pipe
/// writers so backends detach at frame end. A stuck worker parks
/// quietly in kernel space holding its references until its pipe
/// severs; it never spins, and it never blocks teardown.
///
/// The wire worker waits on the data queue and a supervisor wake queue
/// together (one blocking wait, no timeout): wakes release it for
/// cancellation or a finished peer, while the data queue's sender-drop
/// still means exactly "the wire side is gone".
///
/// `break_on_peer_done` releases the output wait once the input side
/// ends: pass true for server pumps (a dead inner leg with an idling
/// outer client would strand the close), false for client pumps (the
/// reply and the exit-status arrive after the client's own stdin EOF).
pub fn pump_session<P: ProcessManager>(
    cx: &StepCtx<P>,
    in_pipe: &Value,
    out_pipe: &Value,
    up_rx: mpsc::Receiver<UpMsg>,
    down_tx: mpsc::Sender<DownMsg>,
    break_on_peer_done: bool,
) -> Result<()> {
    let reader = cx
        .pipe_reader(in_pipe)
        .context("SSH pump cannot borrow the input pipe")?;
    let writer = cx
        .pipe_writer(out_pipe)
        .context("SSH pump cannot borrow the output pipe")?;
    let in_backend = cx.pipe_backend(in_pipe);
    let hub = cx.cancel_hub();
    // Task-local cancellation shared with the detached workers. The
    // caller's token stays the source of truth (`cx.is_cancelled`);
    // this flag is the workers' copy of it.
    let cancel = Arc::new(AtomicBool::new(false));
    // Bare current-thread wait runtime for the wire worker's joint
    // queue wait: no drivers, no threads of its own, just a parking
    // spot. One per session; the client legs already pay more than
    // this for their multi-thread runtime.
    let wait_runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .context("SSH pump wait runtime failed")?;
    // Supervisor wake queue for the wire worker. Bounded and
    // best-effort: a full queue means queued wakes already guarantee
    // the worker releases.
    let (wake_tx, wake_rx) = mpsc::channel::<()>(8);
    let peer_done = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = std::sync::mpsc::channel::<SessionDone>();
    let mut failed: Option<anyhow::Error> = None;
    let mut in_done = false;
    let mut out_done = false;
    let mut out_closed = false;
    // Detached workers: the supervisor never joins them, so a worker
    // parked in an uninterruptible read cannot strand the task thread.
    // Every worker owns its captures outright (`Arc` clones, moved
    // queue ends); the frame keeps its own pipe writers so backends
    // detach at frame end.
    let in_tx = done_tx.clone();
    let out_tx = done_tx.clone();
    let peer_done_in = Arc::clone(&peer_done);
    let peer_done_out = Arc::clone(&peer_done);
    let reader_in = Arc::clone(&reader);
    let down_tx_in = down_tx.clone();
    let writer_out = Arc::clone(&writer);
    let cancel_in = Arc::clone(&cancel);
    let cancel_out = Arc::clone(&cancel);
    let in_hub = Arc::clone(&hub);
    let out_hub = Arc::clone(&hub);
    std::thread::spawn(move || {
        guard_worker(&in_hub, &in_tx, SessionDone::InDone, || {
            let result = pump_in(&reader_in, &down_tx_in, &cancel_in);
            if result.is_ok() {
                peer_done_in.store(true, Ordering::SeqCst);
            }
            result
        });
    });
    std::thread::spawn(move || {
        guard_worker(&out_hub, &out_tx, SessionDone::OutDone, || {
            pump_out(
                &writer_out,
                up_rx,
                wake_rx,
                wait_runtime,
                &cancel_out,
                &peer_done_out,
                break_on_peer_done,
            )
        });
    });
    // Supervisor: drain completions, translate hub events into worker
    // wakes, block on the hub otherwise. Every wake path is idempotent
    // so repeated wakes while cancelled stay harmless. Cancellation
    // breaks at once (after teardown): with detached workers there is
    // no join to wait out, so the task thread always returns.
    let mut seen = hub.sequence();
    loop {
        for event in done_rx.try_iter() {
            match event {
                SessionDone::InDone(outcome) => {
                    in_done = true;
                    note_failure(&mut failed, outcome, "input worker");
                    if break_on_peer_done {
                        let _ = wake_tx.try_send(());
                    }
                }
                SessionDone::OutDone(outcome) => {
                    out_done = true;
                    note_failure(&mut failed, outcome, "output worker");
                    if !out_closed {
                        out_closed = true;
                        // Best-effort EOF propagation: unbound/OS pipes cannot
                        // force-close, and their kernel halves close by drop.
                        let _ = cx.close_pipe(out_pipe);
                    }
                }
            }
        }
        if in_done && out_done {
            break;
        }
        if hub.is_cancelled() || cx.is_cancelled() {
            cancel.store(true, Ordering::SeqCst);
            if let Some(inner) = in_backend.as_ref() {
                inner.cancel();
            }
            let _ = wake_tx.try_send(());
            // Prompt downstream EOF on the way out: readers parked
            // past this see the close instead of hanging, and writers
            // still pushing fail out of their loops.
            let _ = cx.close_pipe(in_pipe);
            let _ = cx.close_pipe(out_pipe);
            break;
        }
        (_, seen) = hub.wait(seen);
    }
    match failed {
        Some(err) => Err(err),
        None => {
            if cancel.load(Ordering::SeqCst) {
                anyhow::bail!("SSH pump interrupted by cancellation");
            }
            Ok(())
        }
    }
}

/// Copy one pipe into another until EOF, then force-close the target so
/// downstream observes EOF. Returns the byte count. Single-threaded:
/// the caller decides task placement. Cancellation arrives through a
/// [`oxdock_process::CancelGuard`] that cancels the source backend
/// when the hub fires, so the blocking read ends without a tick.
pub fn pump_pipe_to_pipe<P: ProcessManager>(
    cx: &StepCtx<P>,
    from_pipe: &Value,
    to_pipe: &Value,
    cancel: &AtomicBool,
) -> Result<i64> {
    let reader = cx
        .pipe_reader(from_pipe)
        .context("SSH_PUMP cannot borrow the source pipe")?;
    let writer = cx
        .pipe_writer(to_pipe)
        .context("SSH_PUMP cannot borrow the target pipe")?;
    let hub: Arc<CancelHub> = cx.cancel_hub();
    let _guard = cx
        .pipe_backend(from_pipe)
        .map(|inner| oxdock_process::CancelGuard::arm(&hub, vec![inner]));
    let mut buffer = [0u8; PUMP_CHUNK];
    let mut total: i64 = 0;
    loop {
        if cancel.load(Ordering::SeqCst) || hub.is_cancelled() || cx.is_cancelled() {
            break;
        }
        match read_pump_chunk(&reader, &mut buffer) {
            Err(err) => bail!("SSH_PUMP source pipe read failed: {err}"),
            Ok(0) => break,
            Ok(count) => {
                {
                    let mut guard = writer
                        .lock()
                        .map_err(|_| anyhow::anyhow!("SSH_PUMP target lock poisoned"))?;
                    guard
                        .write_all(&buffer[..count])
                        .context("SSH_PUMP target pipe write failed")?;
                    guard.flush().context("SSH_PUMP target pipe flush failed")?;
                }
                total += count as i64;
            }
        }
    }
    let _ = cx.close_pipe(to_pipe);
    Ok(total)
}

// Shared child-pump core for the `SSH_PTY_RUN` and `SSH_RUN` backends.
// A terminal child and a piped child differ only in spawn, teardown
// extras, and input-sink ownership; exit-first reporting, deferred
// closes, and cancel-kill must never diverge between them, so they
// share this supervisor. The wire pumps above stay separate: channel
// half-close has no child to reap.

/// Completion of one child-pump worker, by slot index into the
/// supervisor's closes (0 is always the input worker). Delivered
/// through [`guard_worker`]: panics report instead of stranding the
/// supervisor.
pub(crate) struct ChildDone(pub usize, pub std::thread::Result<Result<()>>);

/// The reaper observed child exit, carrying the code. Produced by the
/// backend's blocking wait on its own thread (ConPTY holds the master
/// open past child death, so no pipe EOF can stand in for it), which
/// is also where the wire exit-status report fires: the report always
/// precedes the EOF closes the supervisor applies after, so a fast
/// child never loses its status to the close.
pub(crate) struct ChildExited(pub i64);

/// One DSL pipe closed after its worker drains, gated on an observed
/// exit so the wire status always precedes the EOF.
pub(crate) struct DeferredClose<'a> {
    pub pipe: &'a Value,
    pub closed: bool,
}

/// Pump DSL-pipe bytes into a child stdin sink until pipe EOF,
/// cancellation, or `peer_done` (child gone: bytes would have nowhere
/// to go). Cancellation arrives as EOF through [`oxdock_pipe::PipeInner::cancel`].
pub(crate) fn pump_pipe_to_sink(
    reader: &SharedInput,
    sink: &mut Box<dyn Write + Send>,
    cancel: &AtomicBool,
    peer_done: &AtomicBool,
    func: &str,
) -> Result<()> {
    let mut buffer = [0u8; PUMP_CHUNK];
    loop {
        if cancel.load(Ordering::SeqCst) || peer_done.load(Ordering::SeqCst) {
            break;
        }
        match read_pump_chunk(reader, &mut buffer) {
            Err(err) => bail!("{func} input pipe read failed: {err}"),
            Ok(0) => break,
            Ok(count) => {
                if sink.write_all(&buffer[..count]).is_err() {
                    break;
                }
                if sink.flush().is_err() {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Apply deferred closes for finished output workers once the exit is
/// observed, so the wire status always precedes the EOF. Each entry is
/// (worker finished, deferred close). Returns true when every output
/// worker finished.
pub(crate) fn close_drained_outputs<P: ProcessManager>(
    cx: &StepCtx<P>,
    outputs: &mut [(bool, DeferredClose<'_>)],
    exit_observed: bool,
) -> bool {
    let mut all_done = true;
    for (finished, close) in outputs.iter_mut() {
        if !*finished {
            all_done = false;
            continue;
        }
        if !close.closed && exit_observed {
            close.closed = true;
            let _ = cx.close_pipe(close.pipe);
        }
    }
    all_done
}
