//! Synchronous byte pumps between handoff queues and DSL pipes.
//!
//! All three blocking funcs (`SSH_ACCEPT`, `SSH_CONNECT`, `SSH_PUMP`)
//! share this shape, mirroring `net_bridge::pump`: one worker per
//! direction on scoped threads, a supervisor tick for cancellation, and
//! `close_pipe` for prompt downstream EOF. The only async contact is the
//! bounded queue pair; blocking queue ops run exclusively on plain
//! engine-task threads, never on Tokio workers.
//!
//! `&StepCtx` is not `Sync`, so every context call (`pipe_reader`,
//! `pipe_writer`, `pipe_backend`, `close_pipe`, `is_cancelled`) happens
//! on the calling supervisor thread. Workers receive only owned,
//! thread-safe handles.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use oxdock_core::{StepCtx, Value};
use oxdock_pipe::PipeInner;
use oxdock_process::{ProcessManager, SharedInput, SharedOutput};
use tokio::sync::mpsc;

use crate::state::{DownMsg, UpMsg};

/// Copy buffer size, matching `net_bridge::CHUNK`.
const CHUNK: usize = 8192;
/// Supervisor/cancel poll interval, matching `SUPERVISOR_TICK`.
const TICK: Duration = Duration::from_millis(10);
/// Timeout-bounded pipe read backstop, matching `WORKER_BACKSTOP`.
const BACKSTOP: Duration = Duration::from_millis(10);

/// Read one chunk from a pipe: timeout-bounded for script backends (so
/// cancellation always wins within a tick), blocking otherwise.
fn read_pipe(
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
/// via the supervisor's force-close — waiting for it would deadlock).
/// The supervisor force-closes the output pipe right after this worker
/// is reaped, so downstream observes EOF promptly.
fn pump_out(
    writer: &SharedOutput,
    up_rx: &mut mpsc::Receiver<UpMsg>,
    cancel: &AtomicBool,
) -> Result<()> {
    loop {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        match up_rx.blocking_recv() {
            Some(UpMsg::Data(bytes)) => {
                let mut guard = writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("SSH pump output lock poisoned"))?;
                guard
                    .write_all(&bytes)
                    .context("SSH pump output pipe write failed")?;
                guard.flush().context("SSH pump output pipe flush failed")?;
            }
            Some(UpMsg::Eof) | None => break,
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

/// Reap one finished worker, recording the first error.
fn reap(
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
pub fn pump_session<P: ProcessManager>(
    cx: &StepCtx<P>,
    in_pipe: &Value,
    out_pipe: &Value,
    up_rx: mpsc::Receiver<UpMsg>,
    down_tx: mpsc::Sender<DownMsg>,
    cancel: &AtomicBool,
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
    std::thread::scope(|scope| {
        let mut worker_in =
            Some(scope.spawn(|| pump_in(&reader, backend.as_ref(), &down_tx, cancel)));
        let mut worker_out = Some(scope.spawn(|| pump_out(&writer, &mut up_rx, cancel)));
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
