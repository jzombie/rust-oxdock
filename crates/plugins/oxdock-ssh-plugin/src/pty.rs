//! Local terminal bridge: spawn a subprocess under a correctly-sized
//! pseudo-terminal and pump explicit DSL pipes through its master side.
//!
//! This is what makes fullscreen apps work through the proxy. A byte
//! pump alone cannot carry terminal dimensions: the inner `ssh` sizes
//! its remote pty from its *local* stdin tty, and pipes have no size
//! (0x0), so without a local pty every remote fullscreen app lays out
//! for a nonexistent screen. Spawning under a local pty sized from the
//! outer session — and resizing it live as outer window-change requests
//! arrive — closes the loop: the kernel SIGWINCHes the child, which
//! forwards the new size to the remote end itself.
//!
//! Implemented on `portable-pty` (the same substrate term-wm uses), so
//! Unix PTY and Windows ConPTY share one code path — no per-platform
//! branches here. Environment is inherited from the host process, layered
//! with the script environment like `RUN`: block-scoped `ENV` (such as a
//! session's `SSH_*` relay) reaches the child and reverts at scope exit.
//! The working directory comes from the script.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use oxdock_core::{StepCtx, Value};
use oxdock_pipe::PipeInner;
use oxdock_process::{ProcessManager, SharedInput, SharedOutput};

use crate::bridge::{CHUNK, TICK, read_pipe};
use crate::state::{PtySize, SharedPtySize, snapshot_pty_size};

/// ConPTY console-setup handshake. portable-pty builds the console with
/// `PSEUDOCONSOLE_INHERIT_CURSOR`, so when the parent has no console of
/// its own (CI runners, services) conhost emits a cursor-position query
/// and stalls console setup, and the client with it, until the query is
/// answered. A terminal emulator answers in band as part of VT handling;
/// this pump is the only reader, so the output worker answers on its
/// behalf. Query bytes are still forwarded untouched: they are genuine
/// console output and substring assertions must observe the full stream.
/// The reply reports a fresh 1,1 cursor. Only conhost can emit this
/// sequence toward us (a child cursor query travels the opposite
/// direction, into conhost), so answering never injects stray input into
/// the child. Unix ptys never emit the query, so this is inert there.
const CONHOST_DSR_QUERY: &[u8] = b"\x1b[6n";
const CONHOST_CPR_REPLY: &[u8] = b"\x1b[1;1R";

/// Answer every ConPTY cursor query in `chunk`, carrying a short tail
/// across reads so a query split over two reads still matches. The reply
/// is best effort: setup may complete, or teardown race, concurrently.
fn answer_conhost_queries(
    chunk: &[u8],
    tail: &mut [u8; 3],
    tail_len: &mut usize,
    master_in: &Arc<Mutex<Box<dyn Write + Send>>>,
) {
    let mut probe = Vec::with_capacity(*tail_len + chunk.len());
    probe.extend_from_slice(&tail[..*tail_len]);
    probe.extend_from_slice(chunk);
    // Answer matches that involve new bytes; matches fully inside the
    // old tail were answered on the previous call.
    let mut answers = 0;
    let mut idx = 0;
    while idx + CONHOST_DSR_QUERY.len() <= probe.len() {
        if &probe[idx..idx + CONHOST_DSR_QUERY.len()] == CONHOST_DSR_QUERY
            && idx + CONHOST_DSR_QUERY.len() > *tail_len
        {
            answers += 1;
            idx += CONHOST_DSR_QUERY.len();
        } else {
            idx += 1;
        }
    }
    if answers > 0
        && let Ok(mut guard) = master_in.lock()
    {
        for _ in 0..answers {
            let _ = guard.write_all(CONHOST_CPR_REPLY);
        }
        let _ = guard.flush();
    }
    *tail_len = (*tail_len + chunk.len()).min(tail.len());
    tail[..*tail_len].copy_from_slice(&probe[probe.len() - *tail_len..]);
}

/// Pump master-terminal output into the `out_pipe` writer. Ends on
/// master EOF (child and its children are gone; `portable-pty`
/// normalizes the platform EIO-into-EOF kink for us) or on console
/// teardown (the supervisor releases the master once the child is
/// observed gone, which closes a ConPTY output pipe that would
/// otherwise stay open). Also answers ConPTY setup queries in band.
fn pump_master_out(
    reader: &mut Box<dyn Read + Send>,
    writer: &SharedOutput,
    master_in: &Arc<Mutex<Box<dyn Write + Send>>>,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut buffer = [0u8; CHUNK];
    let mut query_tail = [0u8; 3];
    let mut query_tail_len = 0usize;
    loop {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        match reader.read(&mut buffer) {
            // Windows reports a torn-down ConPTY pipe as broken rather
            // than clean EOF, depending on read versus teardown timing.
            // Either way no more bytes will ever arrive, so drain ends.
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => break,
            Err(err) => bail!("SSH pty master read failed: {err}"),
            Ok(0) => break,
            Ok(count) => {
                answer_conhost_queries(
                    &buffer[..count],
                    &mut query_tail,
                    &mut query_tail_len,
                    master_in,
                );
                let mut guard = writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("SSH pty output lock poisoned"))?;
                guard
                    .write_all(&buffer[..count])
                    .context("SSH pty output pipe write failed")?;
                guard.flush().context("SSH pty output pipe flush failed")?;
            }
        }
    }
    Ok(())
}

/// Release nudge replicating portable-pty's Unix master-writer drop:
/// newline plus EOT so a stdin-blocked child observes EOF and exits.
/// Upstream only has this behavior on Unix (its Windows writer drop
/// just closes the handle), so this is unix-only too: Windows console
/// children must never observe stray input bytes.
fn send_release_nudge(writer: &Arc<Mutex<Box<dyn Write + Send>>>) {
    #[cfg(unix)]
    if let Ok(mut guard) = writer.lock() {
        // Default termios VEOF (^D): our ptys never customize it, and
        // the fd is hidden behind the trait object, so the byte is
        // fixed here instead of read back like upstream does.
        let _ = guard.write_all(b"\n\x04");
        let _ = guard.flush();
    }
    #[cfg(not(unix))]
    let _ = writer;
}

/// Sends [`send_release_nudge`] when the input pump ends, on every exit
/// path. The shared writer outlives this pump (the output worker keeps
/// a clone for ConPTY answers), so without this guard a stdin-driven
/// child like `cat` never observes EOF after its pipe closes and the
/// session strands.
struct ReleaseNudge<'a> {
    writer: &'a Arc<Mutex<Box<dyn Write + Send>>>,
}

impl Drop for ReleaseNudge<'_> {
    fn drop(&mut self) {
        send_release_nudge(self.writer);
    }
}

/// Pump `in_pipe` bytes into the master side (toward the child). Ends on
/// pipe EOF, cancellation, or `peer_done` (child gone: bytes would have
/// nowhere to go). The master writer is shared with the output worker,
/// which uses it to answer ConPTY setup queries.
fn pump_master_in(
    reader: &SharedInput,
    backend: Option<&Arc<PipeInner>>,
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    cancel: &AtomicBool,
    peer_done: &AtomicBool,
) -> Result<()> {
    let _nudge = ReleaseNudge { writer };
    let mut buffer = [0u8; CHUNK];
    loop {
        if cancel.load(Ordering::SeqCst) || peer_done.load(Ordering::SeqCst) {
            break;
        }
        match read_pipe(reader, backend, &mut buffer) {
            Err(err) => bail!("SSH pty input pipe read failed: {err}"),
            Ok(None) => continue,
            Ok(Some(0)) => {
                break;
            }
            Ok(Some(count)) => {
                let mut guard = writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("SSH pty master lock poisoned"))?;
                if guard.write_all(&buffer[..count]).is_err() {
                    break;
                }
                if guard.flush().is_err() {
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
                *failed = Some(anyhow::anyhow!("SSH pty worker panicked"));
            }
        }
    }
}

fn to_portable_size(size: PtySize) -> portable_pty::PtySize {
    let (rows, cols) = size.effective();
    portable_pty::PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Run `argv` under a local terminal sized from `initial` (live:
/// outer window-change requests land in `size_source` and are applied
/// each tick), pumping `in_pipe` into it and its output into `out_pipe`.
/// Blocks until the child exits and its output drains, then returns the
/// exit code. Cancellation kills the child.
#[allow(clippy::too_many_arguments)]
pub fn pump_pty_session<P: ProcessManager>(
    cx: &StepCtx<P>,
    argv: &[String],
    initial: PtySize,
    size_source: &SharedPtySize,
    in_pipe: &Value,
    out_pipe: &Value,
    cancel: &AtomicBool,
) -> Result<i64> {
    if argv.is_empty() {
        bail!("SSH_PTY_RUN needs at least a program");
    }
    let reader = cx
        .pipe_reader(in_pipe)
        .context("SSH pty cannot borrow the input pipe")?;
    let writer = cx
        .pipe_writer(out_pipe)
        .context("SSH pty cannot borrow the output pipe")?;
    let backend = cx.pipe_backend(in_pipe);

    let mut applied = initial;
    let mut applied_seq = snapshot_pty_size(size_source);
    let pair = portable_pty::native_pty_system()
        .openpty(to_portable_size(applied))
        .context("SSH pty allocation failed")?;
    let mut master_reader = pair
        .master
        .try_clone_reader()
        .context("SSH pty master reader failed")?;
    // Shared with the output worker, which answers ConPTY setup
    // queries through it while the input worker pumps pipe bytes.
    let master_writer = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .context("SSH pty master writer failed")?,
    ));
    // Console teardown handle. Released once the child is observed
    // gone (see the supervisor loop): ConPTY keeps the output pipe
    // open until ClosePseudoConsole runs, which lives in this handle,
    // so holding it to function end strands the output worker in its
    // blocking read after the child exits (EOF waits for teardown,
    // teardown waits for the scope join, the join waits for the
    // worker). Unix reports child death as master EOF either way, so
    // the early release changes nothing there.
    let mut master_opt = Some(pair.master);

    let mut builder = portable_pty::CommandBuilder::new(&argv[0]);
    builder.args(&argv[1..]);
    builder.cwd(cx.cwd().as_path());
    // Script ENV layers over the inherited host environment (the `RUN`
    // contract): block-scoped assignments like the session's `SSH_*`
    // relay reach the child, and scope exit reverts them.
    for (key, value) in cx.env_snapshot() {
        builder.env(&key, &value);
    }
    let mut child = pair
        .slave
        .spawn_command(builder)
        .context("SSH pty spawn failed")?;
    drop(pair.slave);

    let mut failed: Option<anyhow::Error> = None;
    let mut out_closed = false;
    let peer_done = AtomicBool::new(false);
    let mut exit_code: Option<i64> = None;
    std::thread::scope(|scope| {
        let mut worker_in = Some(scope.spawn(|| {
            pump_master_in(
                &reader,
                backend.as_ref(),
                &master_writer,
                cancel,
                &peer_done,
            )
        }));
        let mut worker_out = Some(scope.spawn(|| {
            let result = pump_master_out(&mut master_reader, &writer, &master_writer, cancel);
            peer_done.store(true, Ordering::SeqCst);
            result
        }));
        loop {
            reap(&mut worker_in, &mut failed);
            let out_was_live = worker_out.is_some();
            reap(&mut worker_out, &mut failed);
            if out_was_live && worker_out.is_none() && !out_closed {
                out_closed = true;
                let _ = cx.close_pipe(out_pipe);
            }
            // Live resize: an outer window-change bumps the shared cell's
            // sequence; apply it so the kernel SIGWINCHes the child,
            // which forwards it to the remote end itself. Sequenced, so
            // an explicit open size is never clobbered by a stale default.
            let current = snapshot_pty_size(size_source);
            if current.seq != applied_seq.seq {
                applied = current.size;
                applied_seq = current;
                // The master is gone once the child exits (see below):
                // a dead child has no terminal left to resize.
                if let Some(master) = master_opt.as_ref() {
                    let _ = master.resize(to_portable_size(applied));
                }
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    peer_done.store(true, Ordering::SeqCst);
                    if exit_code.is_none() {
                        exit_code = Some(i64::from(status.exit_code()));
                    }
                    // The child is gone: tear down the console now so the
                    // output worker's blocking read observes EOF. Deferred
                    // to function end this deadlocks on Windows, where the
                    // ConPTY host holds the output pipe open until
                    // ClosePseudoConsole runs.
                    master_opt = None;
                }
                Ok(None) => {}
                Err(_) => {
                    peer_done.store(true, Ordering::SeqCst);
                    // The wait handle is broken: no exit will ever be
                    // observed, so release the console the same way.
                    // Workers drain to EOF and the reaper below reports.
                    master_opt = None;
                }
            }
            if cx.is_cancelled() {
                cancel.store(true, Ordering::SeqCst);
                let _ = child.kill();
            }
            if worker_in.is_none() && worker_out.is_none() {
                break;
            }
            std::thread::sleep(TICK);
        }
    });
    if let Some(err) = failed {
        return Err(err);
    }
    match exit_code {
        Some(code) => Ok(code),
        None => {
            // Workers drained without an observed exit (cancellation or
            // master EOF race): reap the authoritative status blocking.
            match child.wait() {
                Ok(status) => Ok(i64::from(status.exit_code())),
                Err(err) => bail!("SSH pty reaped with error: {err}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;
    type SeenBytes = Arc<Mutex<Vec<u8>>>;

    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("sink lock unpoisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn harness() -> (SharedWriter, SeenBytes) {
        let seen: SeenBytes = Arc::new(Mutex::new(Vec::new()));
        let writer: Box<dyn Write + Send> = Box::new(Sink(Arc::clone(&seen)));
        (Arc::new(Mutex::new(writer)), seen)
    }

    #[test]
    fn answers_split_query_once() {
        let (master_in, seen) = harness();
        let mut tail = [0u8; 3];
        let mut tail_len = 0;
        answer_conhost_queries(b"abc\x1b[", &mut tail, &mut tail_len, &master_in);
        assert!(seen.lock().expect("sink readable").is_empty());
        answer_conhost_queries(b"6nrest", &mut tail, &mut tail_len, &master_in);
        assert_eq!(
            seen.lock().expect("sink readable").as_slice(),
            b"\x1b[1;1R".as_slice()
        );
        // Replaying the boundary afterwards must not double answer.
        answer_conhost_queries(b"more", &mut tail, &mut tail_len, &master_in);
        assert_eq!(
            seen.lock().expect("sink readable").as_slice(),
            b"\x1b[1;1R".as_slice()
        );
    }

    #[test]
    fn silent_without_query() {
        let (master_in, seen) = harness();
        let mut tail = [0u8; 3];
        let mut tail_len = 0;
        answer_conhost_queries(
            b"plain output, no query",
            &mut tail,
            &mut tail_len,
            &master_in,
        );
        assert!(seen.lock().expect("sink readable").is_empty());
    }
}
