//! Local terminal bridge: spawn a subprocess under a correctly-sized
//! pseudo-terminal and pump explicit DSL pipes through its master side.
//!
//! This is what makes fullscreen apps work through the proxy. A byte
//! pump alone cannot carry terminal dimensions: the inner `ssh` sizes
//! its remote pty from its *local* stdin tty, and pipes have no size
//! (0x0), so without a local pty every remote fullscreen app lays out
//! for a nonexistent screen. Spawning under a local pty sized from the
//! outer session : and resizing it live as outer window-change requests
//! arrive : closes the loop: the kernel SIGWINCHes the child, which
//! forwards the new size to the remote end itself.
//!
//! Implemented on `portable-pty` (the same substrate term-wm uses), so
//! Unix PTY and Windows ConPTY share one code path : no per-platform
//! branches here. Environment is inherited from the host process, layered
//! with the script environment like `RUN`: block-scoped `ENV` (such as a
//! session's `SSH_*` relay) reaches the child and reverts at scope exit.
//! The working directory comes from the script.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use oxdock_core::{StepCtx, Value};
use oxdock_fs::command_path;
use oxdock_pipe::PipeInner;
use oxdock_process::{ProcessManager, SharedInput, SharedOutput};

use crate::bridge::{CHUNK, TICK, read_pipe};
use crate::state::{PtySize, SharedPtySize, snapshot_pty_size};

/// Post-exit output silence that ends the drain wait: long enough to
/// cover ConPTY screen-buffer flush lag on loaded runners, short enough
/// that a stranded console costs little.
const DRAIN_QUIESCE_MS: u64 = 250;
/// Post-exit backstop: teardown runs no matter what past this point, so
/// a pathological console cannot strand the close.
const DRAIN_CAP_MS: u64 = 30_000;

/// Millis since a session epoch. Monotonic by construction: interval
/// math must never read the wall clock (NTP steps would skew quiescence
/// and the cap in either direction).
fn elapsed_ms(epoch: Instant) -> u64 {
    epoch.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

/// Whether the console teardown is due: output has been silent since
/// the child died for longer than the quiescence window, or the cap
/// elapsed. Silence anchors at the LATER of last output and death: a
/// child silent before exit still gets its full post-exit drain wait.
/// Pure function of timestamps, so unit tests pin the policy without
/// threads or terminals.
fn drain_overdue(last_output_ms: u64, exited_ms: Option<u64>, now_ms: u64) -> bool {
    let Some(exited) = exited_ms else {
        return false;
    };
    now_ms.saturating_sub(last_output_ms.max(exited)) >= DRAIN_QUIESCE_MS
        || now_ms.saturating_sub(exited) >= DRAIN_CAP_MS
}

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
/// teardown (the supervisor releases the master once the drain is due,
/// which closes a ConPTY output pipe that would otherwise stay open).
/// Also answers ConPTY setup queries in band. Stamps `activity` with
/// the current millis on every forwarded chunk so the supervisor can
/// tell a draining console from a stranded one.
fn pump_master_out(
    reader: &mut Box<dyn Read + Send>,
    writer: &SharedOutput,
    master_in: &Arc<Mutex<Box<dyn Write + Send>>>,
    cancel: &AtomicBool,
    activity: &AtomicU64,
    epoch: Instant,
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
                activity.store(elapsed_ms(epoch), Ordering::SeqCst);
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
///
/// `on_exit` fires exactly once with the learned exit code, at child
/// exit observation: ahead of the drain close, so a reporter (the SSH
/// wire `exit-status`) queues before the pump EOF that the close
/// triggers. A fast child would otherwise win the race against a
/// script-level report issued after this function returns.
#[allow(clippy::too_many_arguments)]
pub fn pump_pty_session<P: ProcessManager>(
    cx: &StepCtx<P>,
    argv: &[String],
    initial: PtySize,
    size_source: &SharedPtySize,
    in_pipe: &Value,
    out_pipe: &Value,
    cancel: &AtomicBool,
    on_exit: Option<&dyn Fn(u32)>,
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
    // Console teardown handle. Released once the output drain is due
    // (see the supervisor loop): ConPTY keeps the output pipe open
    // until ClosePseudoConsole runs, which lives in this handle, so
    // holding it to function end strands the output worker in its
    // blocking read after the child exits (EOF waits for teardown,
    // teardown waits for the scope join, the join waits for the
    // worker). Releasing it the instant the child dies truncates the
    // drain instead: a slow ConPTY flush loses to teardown and the
    // tail bytes never reach the pipe. Unix reports child death as
    // master EOF either way, so the worker usually beats the drain
    // wait there and the quiescence path stays inert.
    let mut master_opt = Some(pair.master);
    // Last forwarded output, for the drain wait. Bytes flowing after
    // child death mean ConPTY is still flushing: keep waiting.
    let epoch = Instant::now();
    let activity = AtomicU64::new(elapsed_ms(epoch));

    let mut builder = portable_pty::CommandBuilder::new(&argv[0]);
    builder.args(&argv[1..]);
    // Native form, not the guarded verbatim path: a `\\?\` startup
    // directory trips console apps into UNC handling (`cmd` warns and
    // silently defaults elsewhere), the same reason process spawn
    // goes through `command_path`.
    let cwd = command_path(cx.cwd());
    builder.cwd(cwd.as_ref());
    // Seeded child env like `RUN`: the script map verbatim (seeded at
    // session start, uniformly overwritten by `ENV`), never host
    // inherit. Terminal definitions ride the seed with a host-or-`dumb`
    // default: a leg needing curses capability lists `INHERIT_ENV
    // [TERM]` (or sets it) and the snapshot carries it.
    cx.ensure_scratch_materialized()?;
    let staged = cx.env_snapshot();
    oxdock_process::check_spawn_anchors(&argv[0], &staged, false)?;
    builder.env_clear();
    for (key, value) in &staged {
        builder.env(key, value);
    }
    let mut child = pair
        .slave
        .spawn_command(builder)
        .context("SSH pty spawn failed")?;
    drop(pair.slave);

    // Win32-OpenSSH parity: every console session opens cleared and
    // homed, so the child renders onto a known blank screen instead of
    // whatever the client held. Synchronously first, ahead of all
    // console output (their paint and ours never reorder past this).
    {
        let mut guard = writer
            .lock()
            .map_err(|_| anyhow::anyhow!("SSH pty output lock poisoned"))?;
        guard
            .write_all(b"\x1b[2J\x1b[H")
            .context("SSH pty session init write failed")?;
        guard.flush().context("SSH pty session init flush failed")?;
    }

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
            let result = pump_master_out(
                &mut master_reader,
                &writer,
                &master_writer,
                cancel,
                &activity,
                epoch,
            );
            peer_done.store(true, Ordering::SeqCst);
            result
        }));
        // Child-death timestamp for the drain wait. Set once, on the
        // first observation below; teardown follows quiescence, never
        // the death itself.
        let mut exited_ms: Option<u64> = None;
        loop {
            // Child exit first, ahead of worker reaping below: the
            // reporter (the SSH wire exit-status) must queue before
            // the drain close triggers the pump EOF, or a fast child
            // wins the race against it. Never poll a reaped child:
            // past the first observed exit the wait handle is spent
            // (Unix answers further polls with ECHILD), and
            // re-stamping the death would push the drain wait out
            // from under quiescence.
            if exit_code.is_none() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        peer_done.store(true, Ordering::SeqCst);
                        let code = status.exit_code();
                        exit_code = Some(i64::from(code));
                        if let Some(report) = on_exit {
                            report(code);
                        }
                        // The child is gone: note when, but keep the
                        // console until the drain is due so a slow flush
                        // still lands. Deferred to function end this
                        // deadlocks on Windows, where the ConPTY host
                        // holds the output pipe open until
                        // ClosePseudoConsole runs; torn down the instant
                        // the child dies it truncates the tail instead.
                        exited_ms = Some(elapsed_ms(epoch));
                    }
                    Ok(None) => {}
                    Err(_) => {
                        peer_done.store(true, Ordering::SeqCst);
                        // The wait handle is broken: no exit will ever be
                        // observed, so note it once and let the drain wait
                        // below force the release. Workers drain to EOF
                        // and the reaper below reports.
                        if exited_ms.is_none() {
                            exited_ms = Some(elapsed_ms(epoch));
                        }
                    }
                }
            }
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
            // Due drain with a live output worker: release the console
            // so its blocking read observes EOF, then reaping closes the
            // pipe after the flushed tail.
            if worker_out.is_some()
                && master_opt.is_some()
                && drain_overdue(
                    activity.load(Ordering::SeqCst),
                    exited_ms,
                    elapsed_ms(epoch),
                )
            {
                master_opt = None;
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
                Ok(status) => {
                    let code = status.exit_code();
                    if let Some(report) = on_exit {
                        report(code);
                    }
                    Ok(i64::from(code))
                }
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
    fn drain_waits_for_quiescence_not_death() {
        // Fresh death with recent output: keep waiting for the flush.
        assert!(!drain_overdue(1000, Some(1000), 1000));
        assert!(!drain_overdue(1090, Some(1000), 1100));
        // Silent past the window: tear down even though the worker
        // never EOF'd on its own.
        assert!(drain_overdue(1000, Some(1000), 1000 + DRAIN_QUIESCE_MS));
        // Late flush resets the wait, until the cap.
        assert!(!drain_overdue(5000, Some(1000), 5100));
        assert!(drain_overdue(
            1000 + DRAIN_CAP_MS,
            Some(1000),
            1000 + DRAIN_CAP_MS
        ));
        // No death observed: never tear down.
        assert!(!drain_overdue(1000, None, 1000 + DRAIN_CAP_MS + 1));
        // Silence before death must not bypass the post-exit wait: the
        // window anchors at the later of last output and death.
        assert!(!drain_overdue(500, Some(1000), 1000));
        assert!(!drain_overdue(500, Some(1000), 1000 + DRAIN_QUIESCE_MS - 1));
        assert!(drain_overdue(500, Some(1000), 1000 + DRAIN_QUIESCE_MS));
        // Output after death extends the window past the death mark.
        assert!(!drain_overdue(1100, Some(1000), 1200));
        assert!(drain_overdue(1100, Some(1000), 1100 + DRAIN_QUIESCE_MS));
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
