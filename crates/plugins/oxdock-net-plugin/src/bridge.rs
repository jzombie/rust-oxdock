//! Synchronous byte pumps between TCP sockets and DSL pipes, plus the
//! memory-session pump between in-process pipe backends.
//!
//! Ported from core's retired `net_bridge`, retargeted from ambient
//! `WITH_IO` streams to explicit pipe values (the only shape a host
//! function can receive). One worker per direction on scoped threads, a
//! supervisor tick for cancellation, and `close_pipe` for prompt
//! downstream EOF. Blocking socket ops run exclusively on plain
//! engine-task threads.
//!
//! `&StepCtx` is not `Sync`, so every context call (`pipe_reader`,
//! `pipe_writer`, `pipe_backend`, `close_pipe`, `is_cancelled`) happens
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
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use oxdock_core::{StepCtx, Value};
use oxdock_pipe::PipeInner;
use oxdock_process::{ProcessManager, SharedInput, SharedOutput};

/// Copy buffer size, matching the retired bridge.
pub(crate) const CHUNK: usize = 8192;
/// Supervisor/cancel poll interval.
pub(crate) const TICK: Duration = Duration::from_millis(10);
/// Timeout-bounded pipe read backstop: cancellation and teardown resolve
/// within a few ticks with no notify machinery.
pub(crate) const BACKSTOP: Duration = Duration::from_millis(10);

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

/// Read one chunk from a pipe: timeout-bounded for script pipes (backend
/// found by handle identity), blocking for anything else. Returns `None`
/// on a backstop tick with no data and no close.
fn read_pipe(
    reader: &SharedInput,
    inner: Option<&Arc<PipeInner>>,
    buf: &mut [u8],
) -> std::io::Result<Option<usize>> {
    match inner {
        Some(inner) => inner.read_into_timeout(buf, BACKSTOP),
        None => {
            let mut guard = reader
                .lock()
                .map_err(|_| std::io::Error::other("stdin lock poisoned"))?;
            guard.read(buf).map(Some)
        }
    }
}

/// `stdin-pipe -> socket` direction. Stdin EOF ends the direction; with
/// half-close enabled it also shuts down the socket write side first (the
/// peer sees request EOF), otherwise the socket stays fully open.
/// Cancellation exits at the next tick.
fn pump_in(
    func: &str,
    reader: SharedInput,
    inner: Option<Arc<PipeInner>>,
    mut stream: TcpStream,
    cancel: &AtomicBool,
    half_close: bool,
) -> Result<()> {
    let mut buf = [0u8; CHUNK];
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        match read_pipe(&reader, inner.as_ref(), &mut buf)
            .with_context(|| format!("{func} stdin pipe read failed"))?
        {
            None => continue,
            Some(0) => {
                if half_close {
                    suppress_peer_gone(stream.shutdown(Shutdown::Write))
                        .map_err(|err| anyhow!("{func} socket shutdown failed: {err}"))?;
                }
                return Ok(());
            }
            Some(n) => stream
                .write_all(&buf[..n])
                .with_context(|| format!("{func} socket write failed"))?,
        }
    }
}

/// `socket -> stdout-pipe` direction. Socket EOF ends the direction; the
/// backend is force-closed so downstream observes EOF promptly instead of
/// waiting out unrelated writer and keeper lifetimes. Cancellation exits
/// at the next tick.
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
    let mut buf = [0u8; CHUNK];
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

/// Reap a finished worker without ever block-joining a live one.
/// Returns the worker's value on success (the memory pump retrieves a
/// lingered writer this way); stream pumps ignore it.
fn reap<T>(
    slot: &mut Option<std::thread::ScopedJoinHandle<'_, Result<T>>>,
    failed: &mut Option<anyhow::Error>,
) -> Option<T> {
    if !slot.as_ref().is_some_and(|h| h.is_finished()) {
        return None;
    }
    let handle = slot.take()?;
    match handle.join() {
        Ok(Ok(value)) => Some(value),
        Ok(Err(err)) => {
            if failed.is_none() {
                *failed = Some(err);
            }
            None
        }
        Err(_) => {
            if failed.is_none() {
                *failed = Some(anyhow!("NET pump worker thread panicked"));
            }
            None
        }
    }
}

/// Pump bytes bidirectionally between a socket and explicit DSL pipes on
/// the calling (supervisor) thread: one worker per direction, then tick
/// supervisor duties (completion vs cancellation) until every worker is
/// reaped. Worker errors cascade through a local token so siblings
/// release promptly; the first error wins. External cancellation (the
/// task token) reports as cancelled.
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
    let backend = cx.pipe_backend(in_pipe);
    let out_inner = cx.pipe_backend(out_pipe);
    let sock_in = stream
        .try_clone()
        .with_context(|| format!("{func} failed to clone socket"))?;
    let sock_out = stream
        .try_clone()
        .with_context(|| format!("{func} failed to clone socket"))?;
    let cancel = AtomicBool::new(false);
    let mut failed: Option<anyhow::Error> = None;
    std::thread::scope(|scope| {
        let mut worker_in =
            Some(scope.spawn(|| pump_in(func, reader, backend, sock_in, &cancel, half_close)));
        let mut worker_out =
            Some(scope.spawn(|| pump_out(func, writer, out_inner, sock_out, &cancel)));
        loop {
            reap(&mut worker_in, &mut failed);
            reap(&mut worker_out, &mut failed);
            if worker_in.is_none() && worker_out.is_none() {
                break;
            }
            if failed.is_some() || cx.is_cancelled() {
                cancel.store(true, Ordering::SeqCst);
                let _ = suppress_peer_gone(stream.shutdown(Shutdown::Both));
            }
            std::thread::sleep(TICK);
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
/// (mirroring `no_half_close` socket semantics). Cancellation exits at the
/// next tick.
fn memory_pump_in(
    func: &str,
    reader: SharedInput,
    inner: Option<Arc<PipeInner>>,
    writer: SharedOutput,
    cancel: &AtomicBool,
    half_close: bool,
) -> Result<Option<SharedOutput>> {
    let mut buf = [0u8; CHUNK];
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(None);
        }
        match read_pipe(&reader, inner.as_ref(), &mut buf)
            .with_context(|| format!("{func} stdin pipe read failed"))?
        {
            None => continue,
            Some(0) => {
                if half_close {
                    return Ok(None);
                }
                return Ok(Some(writer));
            }
            Some(n) => {
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
/// force-closed so downstream observes EOF promptly. Reads use the timeout
/// backstop so cancellation resolves on a tick. Cancellation exits at the
/// next tick.
fn memory_pump_out(
    func: &str,
    writer: SharedOutput,
    out_inner: Option<Arc<PipeInner>>,
    incoming: &Arc<PipeInner>,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut buf = [0u8; CHUNK];
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        match incoming.read_into_timeout(&mut buf, BACKSTOP) {
            Ok(None) => continue,
            Ok(Some(0)) => {
                if let Some(inner) = out_inner {
                    inner.force_close();
                }
                return Ok(());
            }
            Ok(Some(n)) => {
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
/// carries peer-to-us bytes, `outgoing` us-to-peer bytes. Same worker and
/// supervisor shape as [`pump_stream`]: EOF propagates by writer drop
/// (half-close on) or lingers (half-close off), worker errors cascade, and
/// cancellation force-closes both peer backends so a stranded counterpart
/// releases instead of hanging.
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
    let backend = cx.pipe_backend(in_pipe);
    let out_inner = cx.pipe_backend(out_pipe);
    let peer_writer = outgoing.writer_handle();
    let cancel = AtomicBool::new(false);
    let mut failed: Option<anyhow::Error> = None;
    std::thread::scope(|scope| {
        let mut worker_in = Some(
            scope.spawn(|| memory_pump_in(func, reader, backend, peer_writer, &cancel, half_close)),
        );
        let mut worker_out =
            Some(scope.spawn(|| memory_pump_out(func, writer, out_inner, incoming, &cancel)));
        // A lingered writer (half-close off) stays alive until both
        // workers are done, then drops with the scope.
        let mut _lingered: Option<SharedOutput> = None;
        loop {
            if let Some(lingered) = reap(&mut worker_in, &mut failed) {
                _lingered = lingered;
            }
            reap::<()>(&mut worker_out, &mut failed);
            if worker_in.is_none() && worker_out.is_none() {
                break;
            }
            if failed.is_some() || cx.is_cancelled() {
                cancel.store(true, Ordering::SeqCst);
                incoming.force_close();
                outgoing.force_close();
            }
            std::thread::sleep(TICK);
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
