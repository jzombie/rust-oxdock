//! Synchronous byte pumps between TCP sockets and DSL pipes, plus the
//! memory-session pump between in-process pipe backends.
//!
//! Ported from core's retired `net_bridge`, retargeted from ambient
//! `WITH_IO` streams to explicit pipe values (the only shape a host
//! function can receive). One worker per direction on scoped threads,
//! an event-driven supervisor, and `close_pipe` for prompt downstream
//! EOF. Blocking socket ops run exclusively on plain engine-task
//! threads.
//!
//! Nothing here polls. Workers block in pipe reads (woken by data,
//! close, or `PipeInner::cancel`) or socket reads (woken by traffic,
//! EOF, or shutdown); the supervisor blocks on the task's
//! [`CancelHub`] until a worker completion lands or cancel fires. Flag
//! checks after a wake are not polling: no wakeup is ever spent on an
//! empty check.
//!
//! `&StepCtx` is not `Sync`, so every context call (`pipe_reader`,
//! `pipe_writer`, `pipe_backend`, `close_pipe`, `cancel_hub`) happens
//! on the calling supervisor thread. Workers receive only owned,
//! thread-safe handles.
//!
//! Unlike the builtin, pumps are always full-duplex: explicit pipes have
//! no `Null` spelling, so there is no read-only mode. Stdin EOF still
//! half-closes the socket write side by default; memory legs mirror that
//! with writer-drop EOF (or a lingered writer under `no_half_close`).

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow};
use oxdock_core::{StepCtx, Value};
use oxdock_pipe::PipeInner;
use oxdock_process::{
    CancelHub, PUMP_CHUNK, ProcessManager, SharedInput, SharedOutput, read_pump_chunk,
    run_guarded as guard_worker,
};

/// Whether an I/O error means "peer already gone": routine during teardown,
/// never a task failure.
fn is_peer_gone(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe
    )
}

/// Run `op` (a socket shutdown) and swallow only the peer-gone class.
/// Anything else propagates: an unexpectedly broken shutdown is a real
/// task failure, not routine teardown.
fn suppress_peer_gone(op: std::io::Result<()>) -> std::io::Result<()> {
    match op {
        Ok(()) => Ok(()),
        Err(err) if is_peer_gone(&err) => Ok(()),
        Err(err) => Err(err),
    }
}

/// Completion of one pump worker, by direction index (0 is always the
/// input worker). Delivered through [`guard_worker`], so a panicking
/// worker reports instead of stranding the supervisor past scope end.
struct NetDone<T>(usize, std::thread::Result<Result<T>>);

/// Record the first worker error. Later errors lose: the first failure
/// explains the teardown.
fn note_failure<T>(
    failed: &mut Option<anyhow::Error>,
    outcome: std::thread::Result<Result<T>>,
    worker: &str,
    func: &str,
) {
    match outcome {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            if failed.is_none() {
                *failed = Some(err);
            }
        }
        Err(_) => {
            if failed.is_none() {
                *failed = Some(anyhow!("{func} {worker} panicked"));
            }
        }
    }
}

/// `stdin-pipe -> socket` direction. Stdin EOF ends the direction; with
/// half-close enabled it also shuts down the socket write side first (the
/// peer sees request EOF), otherwise the socket stays fully open.
/// Cancellation arrives as EOF through [`PipeInner::cancel`]: the
/// supervisor cancels the input backend when the hub fires.
fn pump_in(
    func: &str,
    reader: SharedInput,
    mut stream: TcpStream,
    cancel: &AtomicBool,
    half_close: bool,
) -> Result<()> {
    let mut buf = [0u8; PUMP_CHUNK];
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        match read_pump_chunk(&reader, &mut buf)
            .with_context(|| format!("{func} stdin pipe read failed"))?
        {
            0 => {
                if half_close {
                    suppress_peer_gone(stream.shutdown(Shutdown::Write))
                        .map_err(|err| anyhow!("{func} socket shutdown failed: {err}"))?;
                }
                return Ok(());
            }
            n => stream
                .write_all(&buf[..n])
                .with_context(|| format!("{func} socket write failed"))?,
        }
    }
}

/// `socket -> stdout-pipe` direction. Socket EOF ends the direction; the
/// backend is force-closed so downstream observes EOF promptly instead of
/// waiting out unrelated writer and keeper lifetimes.
fn pump_out(
    func: &str,
    writer: SharedOutput,
    out_inner: Option<Arc<PipeInner>>,
    mut stream: TcpStream,
    cancel: &AtomicBool,
) -> Result<()> {
    let result = pump_out_loop(func, &writer, &mut stream, cancel);
    if let Some(inner) = out_inner {
        inner.force_close();
    }
    result
}

fn pump_out_loop(
    func: &str,
    writer: &SharedOutput,
    stream: &mut TcpStream,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut buf = [0u8; PUMP_CHUNK];
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        match stream.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                let mut guard = writer
                    .lock()
                    .map_err(|_| anyhow!("{func} stdout lock poisoned"))?;
                guard
                    .write_all(&buf[..n])
                    .with_context(|| format!("{func} stdout pipe write failed"))?;
                guard
                    .flush()
                    .with_context(|| format!("{func} stdout pipe flush failed"))?;
            }
            Err(err) => {
                return Err(anyhow!("{func} socket read failed: {err}"));
            }
        }
    }
}

/// Pump bytes bidirectionally between a socket and explicit DSL pipes on
/// the calling (supervisor) thread: one worker per direction, then
/// event-driven supervisor duties (completion vs cancellation) until
/// every worker reports. Worker errors cascade through a local token so
/// siblings release promptly; the first error wins. External
/// cancellation (the task token) reports as cancelled.
pub fn pump_stream<P: ProcessManager>(
    cx: &StepCtx<P>,
    func: &str,
    in_pipe: &Value,
    out_pipe: &Value,
    stream: TcpStream,
    half_close: bool,
) -> Result<()> {
    let reader = cx
        .pipe_reader(in_pipe)
        .context("NET pump cannot borrow the input pipe")?;
    let writer = cx
        .pipe_writer(out_pipe)
        .context("NET pump cannot borrow the output pipe")?;
    let in_backend = cx.pipe_backend(in_pipe);
    let out_inner = cx.pipe_backend(out_pipe);
    let sock_in = stream
        .try_clone()
        .with_context(|| format!("{func} failed to clone socket"))?;
    let sock_out = stream
        .try_clone()
        .with_context(|| format!("{func} failed to clone socket"))?;
    let hub: Arc<CancelHub> = cx.cancel_hub();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<NetDone<()>>();
    let cancel = AtomicBool::new(false);
    let mut failed: Option<anyhow::Error> = None;
    let mut in_done = false;
    let mut out_done = false;
    std::thread::scope(|scope| {
        let in_tx = done_tx.clone();
        let out_tx = done_tx.clone();
        let in_hub = Arc::clone(&hub);
        let out_hub = Arc::clone(&hub);
        let cancel_flag = &cancel;
        scope.spawn(move || {
            guard_worker(
                &in_hub,
                &in_tx,
                |outcome| NetDone(0, outcome),
                || pump_in(func, reader, sock_in, cancel_flag, half_close),
            );
        });
        scope.spawn(move || {
            guard_worker(
                &out_hub,
                &out_tx,
                |outcome| NetDone(1, outcome),
                || pump_out(func, writer, out_inner, sock_out, cancel_flag),
            );
        });
        // Supervisor: drain completions, translate hub events into
        // worker wakes, block otherwise. Every wake path is idempotent
        // so repeated wakes while cancelled stay harmless. Cancelling
        // the input backend ends a worker parked in its pipe read; the
        // backend is task-scoped, so no live session observes it.
        let mut seen = hub.sequence();
        loop {
            for event in done_rx.try_iter() {
                if event.0 == 0 {
                    in_done = true;
                    note_failure(&mut failed, event.1, "input worker", func);
                } else {
                    out_done = true;
                    note_failure(&mut failed, event.1, "output worker", func);
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
                let _ = suppress_peer_gone(stream.shutdown(Shutdown::Both));
            }
            (_, seen) = hub.wait(seen);
        }
    });
    match failed {
        Some(err) => Err(err),
        None => {
            if cancel.load(Ordering::SeqCst) {
                anyhow::bail!("{func} interrupted by cancellation");
            }
            Ok(())
        }
    }
}

/// `stdin-pipe -> memory` direction. Stdin EOF ends the direction; with
/// half-close enabled the writer drops so the peer observes EOF, otherwise
/// the writer returns to the supervisor to linger until the peer closes
/// (mirroring `no_half_close` socket semantics). Cancellation arrives as
/// EOF through [`PipeInner::cancel`].
fn memory_pump_in(
    func: &str,
    reader: SharedInput,
    writer: SharedOutput,
    cancel: &AtomicBool,
    half_close: bool,
) -> Result<Option<SharedOutput>> {
    let mut buf = [0u8; PUMP_CHUNK];
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(None);
        }
        match read_pump_chunk(&reader, &mut buf)
            .with_context(|| format!("{func} stdin pipe read failed"))?
        {
            0 => {
                if half_close {
                    return Ok(None);
                }
                return Ok(Some(writer));
            }
            n => {
                let mut guard = writer
                    .lock()
                    .map_err(|_| anyhow!("{func} memory pipe lock poisoned"))?;
                guard
                    .write_all(&buf[..n])
                    .with_context(|| format!("{func} memory pipe write failed"))?;
                guard
                    .flush()
                    .with_context(|| format!("{func} memory pipe flush failed"))?;
            }
        }
    }
}

/// `memory -> stdout-pipe` direction. Peer EOF (writer dropped or
/// force-closed, buffer drained) ends the direction; the script backend is
/// force-closed so downstream observes EOF promptly. The incoming read
/// blocks without a backstop: cancellation arrives through the
/// supervisor's force-close of the peer backends.
fn memory_pump_out(
    func: &str,
    writer: SharedOutput,
    out_inner: Option<Arc<PipeInner>>,
    incoming: SharedInput,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut buf = [0u8; PUMP_CHUNK];
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        match read_pump_chunk(&incoming, &mut buf) {
            Ok(0) => {
                if let Some(inner) = out_inner {
                    inner.force_close();
                }
                return Ok(());
            }
            Ok(n) => {
                let mut guard = writer
                    .lock()
                    .map_err(|_| anyhow!("{func} stdout lock poisoned"))?;
                guard
                    .write_all(&buf[..n])
                    .with_context(|| format!("{func} stdout pipe write failed"))?;
                guard
                    .flush()
                    .with_context(|| format!("{func} stdout pipe flush failed"))?;
            }
            Err(err) => {
                return Err(anyhow!("{func} memory pipe read failed: {err}"));
            }
        }
    }
}

/// Pump bytes bidirectionally between two in-memory pipe backends and
/// explicit DSL pipes: one memory session with zero sockets. `incoming`
/// feeds the stdout direction, `outgoing` takes the stdin direction;
/// each side EOFs by writer drop (half-close on) or lingers (half-close
/// off), worker errors cascade, and cancellation force-closes both peer
/// backends so a stranded counterpart releases instead of hanging.
pub fn pump_memory<P: ProcessManager>(
    cx: &StepCtx<P>,
    func: &str,
    in_pipe: &Value,
    out_pipe: &Value,
    incoming: &Arc<PipeInner>,
    outgoing: &Arc<PipeInner>,
    half_close: bool,
) -> Result<()> {
    let reader = cx
        .pipe_reader(in_pipe)
        .context("NET pump cannot borrow the input pipe")?;
    let writer = cx
        .pipe_writer(out_pipe)
        .context("NET pump cannot borrow the output pipe")?;
    let in_backend = cx.pipe_backend(in_pipe);
    let out_inner = cx.pipe_backend(out_pipe);
    let peer_writer = outgoing.writer_handle();
    let incoming_reader = incoming.reader_handle();
    let hub: Arc<CancelHub> = cx.cancel_hub();
    let (in_tx, in_rx) = std::sync::mpsc::channel::<NetDone<Option<SharedOutput>>>();
    let (out_tx, out_rx) = std::sync::mpsc::channel::<NetDone<()>>();
    let cancel = AtomicBool::new(false);
    let mut failed: Option<anyhow::Error> = None;
    let mut in_done = false;
    let mut out_done = false;
    // A lingered writer (half-close off) stays alive until both
    // workers are done, then drops with the scope.
    let mut lingered: Option<SharedOutput> = None;
    std::thread::scope(|scope| {
        let in_send = in_tx.clone();
        let out_send = out_tx.clone();
        let in_hub = Arc::clone(&hub);
        let out_hub = Arc::clone(&hub);
        let cancel_flag = &cancel;
        scope.spawn(move || {
            guard_worker(
                &in_hub,
                &in_send,
                |outcome| NetDone(0, outcome),
                || memory_pump_in(func, reader, peer_writer, cancel_flag, half_close),
            );
        });
        scope.spawn(move || {
            guard_worker(
                &out_hub,
                &out_send,
                |outcome| NetDone(1, outcome),
                || memory_pump_out(func, writer, out_inner, incoming_reader, cancel_flag),
            );
        });
        // Supervisor: same event shape as [`pump_stream`], plus the
        // lingered writer retrieved from the input completion.
        let mut seen = hub.sequence();
        loop {
            for event in in_rx.try_iter() {
                in_done = true;
                match event.1 {
                    Ok(Ok(value)) => {
                        lingered = value;
                    }
                    outcome => note_failure(&mut failed, outcome, "input worker", func),
                }
            }
            for event in out_rx.try_iter() {
                out_done = true;
                note_failure(&mut failed, event.1, "output worker", func);
            }
            if in_done && out_done {
                break;
            }
            if hub.is_cancelled() || cx.is_cancelled() {
                cancel.store(true, Ordering::SeqCst);
                if let Some(inner) = in_backend.as_ref() {
                    inner.cancel();
                }
                incoming.force_close();
                outgoing.force_close();
            }
            (_, seen) = hub.wait(seen);
        }
    });
    drop(lingered);
    match failed {
        Some(err) => Err(err),
        None => {
            if cancel.load(Ordering::SeqCst) {
                anyhow::bail!("{func} interrupted by cancellation");
            }
            Ok(())
        }
    }
}
