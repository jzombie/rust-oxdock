//! Synchronous byte pumps between handoff queues and DSL pipes.
//!
//! All blocking pump funcs (`SSH_ACCEPT` / `SSH_PUMP_CHANNEL`,
//! `SSH_CONNECT`, `SSH_PUMP`) share this shape: one worker per
//! direction on scoped threads, a supervisor tick for cancellation, and
//! `close_pipe` for prompt downstream EOF. The only async contact is the
//! bounded queue pair; blocking queue ops run exclusively on plain
//! engine-task threads, never on Tokio workers.
//!
//! `&StepCtx` is not `Sync`, so every context call (`pipe_reader`,
//! `pipe_writer`, `pipe_backend`, `close_pipe`, `is_cancelled`) happens
//! on the calling supervisor thread. Workers receive only owned,
//! thread-safe handles.

use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use oxdock_core::{StepCtx, Value};
use oxdock_pipe::PipeInner;
use oxdock_process::{ProcessManager, SharedInput, SharedOutput};
use tokio::sync::mpsc;

use crate::state::{DownMsg, UpMsg};

/// Copy buffer size: 8192 bytes per chunk.
pub(crate) const CHUNK: usize = 8192;
/// Supervisor/cancel poll interval, matching `SUPERVISOR_TICK`.
pub(crate) const TICK: Duration = Duration::from_millis(10);
/// Timeout-bounded pipe read backstop, matching `WORKER_BACKSTOP`.
pub(crate) const BACKSTOP: Duration = Duration::from_millis(10);

/// Read one chunk from a pipe: timeout-bounded for script backends (so
/// cancellation always wins within a tick), blocking otherwise.
pub(crate) fn read_pipe(
    reader: &SharedInput,
    backend: Option<&Arc<PipeInner>>,
    buffer: &mut [u8],
) -> std::io::Result<Option<usize>> {
    match backend {
        Some(inner) => inner.read_into_timeout(buffer, BACKSTOP),
        None => {
            let mut guard = reader
                .lock()
                .map_err(|_| std::io::Error::other("stdin lock poisoned"))?;
            guard.read(buffer).map(Some)
        }
    }
}

/// Owned pump endpoints, resolved on the supervisor thread.
struct PumpHandles {
    reader: SharedInput,
    backend: Option<Arc<PipeInner>>,
    writer: SharedOutput,
    up_rx: mpsc::Receiver<UpMsg>,
    down_tx: mpsc::Sender<DownMsg>,
}

/// Pump `up_rx -> writer` until the wire half-closes. The bounded queue
/// is FIFO, so an [`UpMsg::Eof`] always arrives after every byte sent
/// before it: return without waiting for the sender to drop (it stays
/// alive until the channel itself closes, which this return helps cause
/// via the supervisor's force-close : waiting for it would deadlock).
/// The supervisor force-closes the output pipe right after this worker
/// is reaped, so downstream observes EOF promptly.
/// When `break_on_peer_done`, also ends when the stdin direction
/// finishes first (`peer_done`): the response producers are gone, so
/// anything still arriving has nowhere to go : this is what releases a
/// server pump whose inner leg (a `RUN` subprocess, an exited remote)
/// died while the outer client idles. Queued bytes still flush first;
/// only the wait ends. Server-side only: a client must linger for the
/// reply and the exit-status, which legitimately arrive after its own
/// stdin EOF, so client pumps always wait for the wire EOF or close.
fn pump_out(
    writer: &SharedOutput,
    up_rx: &mut mpsc::Receiver<UpMsg>,
    cancel: &AtomicBool,
    peer_done: &AtomicBool,
    break_on_peer_done: bool,
) -> Result<()> {
    use tokio::sync::mpsc::error::TryRecvError;
    loop {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        match up_rx.try_recv() {
            Ok(UpMsg::Data(bytes)) => {
                let mut guard = writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("SSH pump output lock poisoned"))?;
                guard
                    .write_all(&bytes)
                    .context("SSH pump output pipe write failed")?;
                guard.flush().context("SSH pump output pipe flush failed")?;
            }
            Ok(UpMsg::Eof) | Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {
                if break_on_peer_done && peer_done.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(TICK);
            }
        }
    }
    Ok(())
}

/// Pump `reader -> down_tx` until stdin EOF. Sends one [`DownMsg::Eof`]
/// so the wire half-closes.
fn pump_in(
    reader: &SharedInput,
    backend: Option<&Arc<PipeInner>>,
    down_tx: &mpsc::Sender<DownMsg>,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut buffer = [0u8; CHUNK];
    loop {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        match read_pipe(reader, backend, &mut buffer) {
            Err(err) => bail!("SSH pump input pipe read failed: {err}"),
            Ok(None) => continue,
            Ok(Some(0)) => {
                let _ = down_tx.blocking_send(DownMsg::Eof);
                break;
            }
            Ok(Some(count)) => {
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

/// Reap one finished worker, recording the first error. Shared by the
/// wire pumps above and the child pumps below: never block-join live.
pub(crate) fn reap(
    slot: &mut Option<std::thread::ScopedJoinHandle<'_, Result<()>>>,
    failed: &mut Option<anyhow::Error>,
) {
    if !slot.as_ref().is_some_and(|handle| handle.is_finished()) {
        return;
    }
    let Some(handle) = slot.take() else {
        return;
    };
    match handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            if failed.is_none() {
                *failed = Some(err);
            }
        }
        Err(_) => {
            if failed.is_none() {
                *failed = Some(anyhow::anyhow!("SSH pump worker panicked"));
            }
        }
    }
}

/// Pump a live session between explicit DSL pipes until both directions
/// end or `cancel` fires. `up_rx` carries wire bytes into `out_pipe`,
/// `in_pipe` bytes flow into `down_tx`. The output pipe is force-closed
/// as soon as the wire direction ends so downstream observes EOF
/// promptly instead of waiting out unrelated writer and keeper
/// lifetimes. The first worker error wins.
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
    cancel: &AtomicBool,
    break_on_peer_done: bool,
) -> Result<()> {
    let reader = cx
        .pipe_reader(in_pipe)
        .context("SSH pump cannot borrow the input pipe")?;
    let writer = cx
        .pipe_writer(out_pipe)
        .context("SSH pump cannot borrow the output pipe")?;
    let backend = cx.pipe_backend(in_pipe);
    let handles = PumpHandles {
        reader,
        backend,
        writer,
        up_rx,
        down_tx,
    };
    let PumpHandles {
        reader,
        backend,
        writer,
        mut up_rx,
        down_tx,
    } = handles;
    let mut failed: Option<anyhow::Error> = None;
    let mut out_closed = false;
    let peer_done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let mut worker_in = Some(scope.spawn(|| {
            let result = pump_in(&reader, backend.as_ref(), &down_tx, cancel);
            if result.is_ok() {
                peer_done.store(true, Ordering::SeqCst);
            }
            result
        }));
        let mut worker_out = Some(
            scope.spawn(|| pump_out(&writer, &mut up_rx, cancel, &peer_done, break_on_peer_done)),
        );
        loop {
            reap(&mut worker_in, &mut failed);
            let out_was_live = worker_out.is_some();
            reap(&mut worker_out, &mut failed);
            if out_was_live && worker_out.is_none() && !out_closed {
                out_closed = true;
                // Best-effort EOF propagation: unbound/OS pipes cannot
                // force-close, and their kernel halves close by drop.
                let _ = cx.close_pipe(out_pipe);
            }
            if worker_in.is_none() && worker_out.is_none() {
                break;
            }
            if cx.is_cancelled() {
                cancel.store(true, Ordering::SeqCst);
            }
            std::thread::sleep(TICK);
        }
    });
    match failed {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Copy one pipe into another until EOF, then force-close the target so
/// downstream observes EOF. Returns the byte count. Single-threaded: the
/// caller decides task placement. Reads use the same timeout backstop as
/// [`pump_session`] so cancellation always wins within a tick.
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
    let backend = cx.pipe_backend(from_pipe);
    let mut buffer = [0u8; CHUNK];
    let mut total: i64 = 0;
    loop {
        if cancel.load(Ordering::SeqCst) || cx.is_cancelled() {
            break;
        }
        match read_pipe(&reader, backend.as_ref(), &mut buffer) {
            Err(err) => bail!("SSH_PUMP source pipe read failed: {err}"),
            Ok(None) => continue,
            Ok(Some(0)) => break,
            Ok(Some(count)) => {
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

/// One supervised worker slot on a scoped thread.
pub(crate) type WorkerSlot<'s> = Option<std::thread::ScopedJoinHandle<'s, Result<()>>>;

/// Backend half of the shared core: spawn-specific pieces the core
/// cannot know. Poll observes once and reports inside, so the wire
/// status always precedes the EOF closes the core applies after.
pub(crate) trait ChildCtl {
    /// Poll child exit once. On first observation queue the wire status
    /// and record the code. Returns the code once observed (a signaled
    /// death still counts: clients observe a codeless close).
    fn poll_exit(&mut self) -> Option<i64>;
    /// Kill on cancellation.
    fn kill(&mut self);
    /// Blocking reap fallback when workers drained unseen.
    fn wait_code(&mut self) -> Result<i64>;
    /// Per-tick backend duties (live resize, drain teardown).
    fn on_tick(&mut self, tick: TickInfo);
}

/// Liveness snapshot for [`ChildCtl::on_tick`].
pub(crate) struct TickInfo {
    /// Whether any output worker is still alive this round.
    pub out_alive: bool,
}

/// One DSL pipe closed after its worker drains, gated on an observed
/// exit so the wire status always precedes the EOF.
pub(crate) struct DeferredClose<'a> {
    pub pipe: &'a Value,
    pub closed: bool,
}

/// Input sink ownership: a terminal master writer is shared with the
/// output worker (setup answers flow through it) and outlives the
/// input worker; a piped child stdin is owned by the input worker, so
/// thread end drops it and the child observes EOF.
pub(crate) enum InputSink {
    Owned(Box<dyn Write + Send>),
    Shared(Arc<Mutex<Box<dyn Write + Send>>>),
}

/// Pump DSL-pipe bytes into a child stdin sink until pipe EOF,
/// cancellation, or `peer_done` (child gone: bytes would have nowhere
/// to go).
pub(crate) fn pump_pipe_to_sink(
    reader: &SharedInput,
    backend: Option<&Arc<PipeInner>>,
    mut sink: InputSink,
    cancel: &AtomicBool,
    peer_done: &AtomicBool,
    func: &str,
) -> Result<()> {
    let mut buffer = [0u8; CHUNK];
    loop {
        if cancel.load(Ordering::SeqCst) || peer_done.load(Ordering::SeqCst) {
            break;
        }
        match read_pipe(reader, backend, &mut buffer) {
            Err(err) => bail!("{func} input pipe read failed: {err}"),
            Ok(None) => continue,
            Ok(Some(0)) => break,
            Ok(Some(count)) => match &mut sink {
                InputSink::Owned(handle) => {
                    if handle.write_all(&buffer[..count]).is_err() {
                        break;
                    }
                    if handle.flush().is_err() {
                        break;
                    }
                }
                InputSink::Shared(shared) => {
                    let mut guard = shared
                        .lock()
                        .map_err(|_| anyhow::anyhow!("{func} sink lock poisoned"))?;
                    if guard.write_all(&buffer[..count]).is_err() {
                        break;
                    }
                    if guard.flush().is_err() {
                        break;
                    }
                }
            },
        }
    }
    Ok(())
}

/// Supervise spawned workers around one child: exit-first reporting,
/// deferred pipe closes, cancel-kill. Returns the observed exit code,
/// if any; the caller runs the blocking [`ChildCtl::wait_code`]
/// fallback when workers drained unseen.
pub(crate) fn supervise_child_pump<'s, 'v, P: ProcessManager>(
    cx: &StepCtx<P>,
    cancel: &AtomicBool,
    child: &mut dyn ChildCtl,
    input: &mut WorkerSlot<'s>,
    outputs: &mut [(WorkerSlot<'s>, DeferredClose<'v>)],
) -> Result<Option<i64>> {
    let mut failed: Option<anyhow::Error> = None;
    let mut exit_code: Option<i64> = None;
    loop {
        // Child exit first, ahead of reaping: the reporter (the SSH
        // wire exit-status) must queue before the EOF closes trigger
        // the pump EOF, or a fast child wins the race against it.
        if exit_code.is_none() {
            exit_code = child.poll_exit();
        }
        reap(input, &mut failed);
        let mut out_alive = false;
        for (worker, close) in outputs.iter_mut() {
            let was_live = worker.is_some();
            reap(worker, &mut failed);
            out_alive = out_alive || was_live;
            if was_live && worker.is_none() && !close.closed && exit_code.is_some() {
                close.closed = true;
                let _ = cx.close_pipe(close.pipe);
            }
        }
        if cx.is_cancelled() {
            cancel.store(true, Ordering::SeqCst);
            child.kill();
        }
        child.on_tick(TickInfo { out_alive });
        if input.is_none() && outputs.iter().all(|(worker, _)| worker.is_none()) {
            break;
        }
        std::thread::sleep(TICK);
    }
    if let Some(err) = failed {
        return Err(err);
    }
    Ok(exit_code)
}
