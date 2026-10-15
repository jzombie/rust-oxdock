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
use oxdock_process::run_guarded as guard_worker;
use oxdock_process::{PUMP_CHUNK, ProcessManager, SharedOutput};

use crate::bridge::{
    ChildDone, ChildExited, DeferredClose, InputSink, close_drained_outputs, note_failure,
    pump_pipe_to_sink,
};
use crate::state::{PtySize, PtySizeStamp, SharedPtySize, snapshot_pty_size, wait_size_changed};

/// Post-exit output silence that ends the drain wait: long enough to
/// cover ConPTY screen-buffer flush lag on loaded runners, short enough
/// that a stranded console costs little.
const DRAIN_QUIESCE_MS: u64 = 250;
/// Post-exit backstop: teardown runs no matter what past this point, so
/// a pathological console cannot strand the close. Enforced
/// opportunistically at drain-deadline arrivals (re-arms keep arriving
/// while bytes flow), so no long-lived timer thread is needed.
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
/// behalf. The query itself is consumed, never forwarded: a real
/// terminal would answer it too, and that answer lands in the shell as
/// stray input that eats the user's first keystroke. The reply reports
/// a fresh 1,1 cursor. Only conhost can emit this sequence toward us
/// (a child cursor query travels the opposite direction, into
/// conhost), so answering never injects stray input into the child.
/// Unix ptys never emit the query, so this is inert there.
const CONHOST_DSR_QUERY: &[u8] = b"\x1b[6n";
const CONHOST_CPR_REPLY: &[u8] = b"\x1b[1;1R";

/// Streaming `ESC[6n` filter over master output: answers each query
/// once through the master writer and releases the chunk minus query
/// bytes. A trailing query-prefix is held for the next chunk, so a
/// query split over two reads still matches and never reaches the wire
/// half. `flush` releases held bytes at EOF: they never completed a
/// query, so they are genuine output.
struct DsrFilter {
    carry: [u8; 3],
    len: usize,
}

impl DsrFilter {
    fn new() -> Self {
        Self {
            carry: [0u8; 3],
            len: 0,
        }
    }

    fn feed(&mut self, chunk: &[u8], master_in: &Arc<Mutex<Box<dyn Write + Send>>>) -> Vec<u8> {
        let mut probe = Vec::with_capacity(self.len + chunk.len());
        probe.extend_from_slice(&self.carry[..self.len]);
        probe.extend_from_slice(chunk);
        // Consumed bytes never rescan: a completed query leaves the
        // stream, so every match here is new and answered exactly once.
        let mut answers = 0;
        let mut kept = Vec::with_capacity(probe.len());
        let mut idx = 0;
        while idx + CONHOST_DSR_QUERY.len() <= probe.len() {
            if &probe[idx..idx + CONHOST_DSR_QUERY.len()] == CONHOST_DSR_QUERY {
                answers += 1;
                idx += CONHOST_DSR_QUERY.len();
            } else {
                kept.push(probe[idx]);
                idx += 1;
            }
        }
        kept.extend_from_slice(&probe[idx..]);
        // Hold back the longest trailing query-prefix: it may yet
        // complete a split query, and must not reach the wire half.
        let widest = kept.len().min(CONHOST_DSR_QUERY.len() - 1);
        let mut hold = 0;
        for width in (1..=widest).rev() {
            if kept[kept.len() - width..] == CONHOST_DSR_QUERY[..width] {
                hold = width;
                break;
            }
        }
        let emit = kept[..kept.len() - hold].to_vec();
        self.carry[..hold].copy_from_slice(&kept[kept.len() - hold..]);
        self.len = hold;
        if answers > 0
            && let Ok(mut guard) = master_in.lock()
        {
            for _ in 0..answers {
                let _ = guard.write_all(CONHOST_CPR_REPLY);
            }
            let _ = guard.flush();
        }
        emit
    }

    fn flush(&mut self) -> Vec<u8> {
        let tail = self.carry[..self.len].to_vec();
        self.len = 0;
        tail
    }
}

/// Pump master-terminal output into the `out_pipe` writer. Ends on
/// master EOF (child and its children are gone; `portable-pty`
/// normalizes the platform EIO-into-EOF kink for us) or on console
/// teardown (the supervisor releases the master once the drain is due,
/// which closes a ConPTY output pipe that would otherwise stay open).
/// Also answers ConPTY setup queries in band while stripping them from
/// the stream. Stamps `activity` with the current millis on every
/// forwarded chunk so the supervisor can tell a draining console from
/// a stranded one.
fn pump_master_out(
    reader: &mut Box<dyn Read + Send>,
    writer: &SharedOutput,
    master_in: &Arc<Mutex<Box<dyn Write + Send>>>,
    cancel: &AtomicBool,
    activity: &AtomicU64,
    epoch: Instant,
) -> Result<()> {
    let mut buffer = [0u8; PUMP_CHUNK];
    let mut queries = DsrFilter::new();
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
                let clean = queries.feed(&buffer[..count], master_in);
                if clean.is_empty() {
                    continue;
                }
                let mut guard = writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("SSH pty output lock poisoned"))?;
                guard
                    .write_all(&clean)
                    .context("SSH pty output pipe write failed")?;
                guard.flush().context("SSH pty output pipe flush failed")?;
                activity.store(elapsed_ms(epoch), Ordering::SeqCst);
            }
        }
    }
    // Held query-prefix bytes never completed: genuine output, release.
    let tail = queries.flush();
    if !tail.is_empty() {
        let mut guard = writer
            .lock()
            .map_err(|_| anyhow::anyhow!("SSH pty output lock poisoned"))?;
        guard
            .write_all(&tail)
            .context("SSH pty output pipe write failed")?;
        guard.flush().context("SSH pty output pipe flush failed")?;
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

/// Reaper half of the pty backend: owns the blocking wait. The
/// supervisor kills through a split killer and applies resizes and
/// drain teardown; this only ever waits, reports, and announces the
/// exit. ConPTY holds the master open past child death, so no pipe EOF
/// can stand in for the wait: without it the supervisor could never
/// tell a draining console from a stranded one.
struct PtyWaiter<'a> {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    peer_done: &'a AtomicBool,
    report: Option<&'a (dyn Fn(u32) + Sync)>,
}

impl PtyWaiter<'_> {
    /// Block until the child exits and report the wire status inside,
    /// returning the code. A broken wait handle reports nothing (there
    /// is no status to report) and still yields -1, so the
    /// supervisor's drain wait forces the release. The report always
    /// precedes the EOF closes the supervisor applies after.
    fn wait_exit(mut self) -> i64 {
        let code = match self.child.wait() {
            Ok(status) => {
                let code = status.exit_code();
                if let Some(report) = self.report {
                    report(code);
                }
                i64::from(code)
            }
            Err(_) => -1,
        };
        self.peer_done.store(true, Ordering::SeqCst);
        code
    }
}

/// Supervisor-side terminal state: the console handle (dropped for
/// teardown), live size tracking, and drain bookkeeping. The reaper
/// owns the child; this owns everything else, so no `&mut` is ever
/// shared across the wait.
struct PtyTerm {
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    size_source: SharedPtySize,
    applied_seq: PtySizeStamp,
    activity: Arc<AtomicU64>,
    epoch: Instant,
    exited_ms: Option<u64>,
    drain_generation: u64,
}

/// Supervisor events. `Resize` and `DrainDue` come from watcher
/// threads; completions arrive through `guard_worker`.
enum PtyEvent {
    Done(ChildDone),
    Exited(ChildExited),
    Resize,
    DrainDue(u64),
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
    on_exit: Option<&(dyn Fn(u32) + Sync)>,
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

    let applied = initial;
    let applied_seq = snapshot_pty_size(size_source);
    let pair = portable_pty::native_pty_system()
        .openpty(to_portable_size(applied))
        .context("SSH pty allocation failed")?;
    let master_reader = pair
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
    let master_opt = Some(pair.master);
    // Last forwarded output, for the drain wait. Bytes flowing after
    // child death mean ConPTY is still flushing: keep waiting.
    let epoch = Instant::now();
    let activity = Arc::new(AtomicU64::new(elapsed_ms(epoch)));

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
    let child = pair
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

    let peer_done = AtomicBool::new(false);
    // Split killer, valid while the reaper owns the wait handle:
    // `clone_killer` exists exactly so a thread blocked in `wait` can
    // still be signalled independently.
    let mut killer = child.clone_killer();
    let waiter = PtyWaiter {
        child,
        peer_done: &peer_done,
        report: on_exit,
    };
    let mut term = PtyTerm {
        master: master_opt,
        size_source: Arc::clone(size_source),
        applied_seq,
        activity: Arc::clone(&activity),
        epoch,
        exited_ms: None,
        drain_generation: 0,
    };
    let hub = cx.cancel_hub();
    let in_backend = cx.pipe_backend(in_pipe);
    let resize_stop = AtomicBool::new(false);
    let (event_tx, event_rx) = std::sync::mpsc::channel::<PtyEvent>();
    let mut failed: Option<anyhow::Error> = None;
    let mut in_done = false;
    let mut exit_code: Option<i64> = None;
    let mut outputs = [(
        false,
        DeferredClose {
            pipe: out_pipe,
            closed: false,
        },
    )];

    // Arm a one-shot drain deadline (inlined at both use sites: a
    // sleeper cannot spawn through a reborrowed scope): sleep `millis`,
    // send `DrainDue(generation)`, exit. Schedule-once, act-once: this
    // is a deadline, not a poll. Silence past the deadline means
    // ConPTY is done flushing (or stranded); bytes in flight re-arm
    // it. Sleeps at most one quiescence window, so a parked sleeper
    // never delays teardown past it.

    std::thread::scope(|scope| {
        // Pre-bound shared borrows (all `Copy`) plus owned values: the
        // `move` closure below takes these and borrows nothing else
        // from this frame.
        let reader_in = &reader;
        let peer_done_in = &peer_done;
        let master_writer_in = Arc::clone(&master_writer);
        let nudge_writer = Arc::clone(&master_writer);
        let input_tx = event_tx.clone();
        let input_hub = Arc::clone(&hub);
        scope.spawn(move || {
            guard_worker(
                &input_hub,
                &input_tx,
                |outcome| PtyEvent::Done(ChildDone(0, outcome)),
                || {
                    let result = pump_pipe_to_sink(
                        reader_in,
                        InputSink::Shared(master_writer_in),
                        cancel,
                        peer_done_in,
                        "SSH pty",
                    );
                    // Unix EOF nudge replicating portable-pty's master-writer
                    // drop: the shared writer outlives this worker, so without
                    // it a stdin-blocked child never observes EOF.
                    send_release_nudge(&nudge_writer);
                    result
                },
            );
        });
        let output_tx = event_tx.clone();
        let output_hub = Arc::clone(&hub);
        let peer_done_out = &peer_done;
        // Shared writer, cloned outside the `move` closure (see the
        // capture rule in `bridge::pump_session`): the frame keeps its
        // own Arc so the backend detaches at frame end, after the
        // drain close, never from a worker thread exit mid-session.
        let writer_out = Arc::clone(&writer);
        scope.spawn(move || {
            let mut master_reader = master_reader;
            guard_worker(
                &output_hub,
                &output_tx,
                |outcome| PtyEvent::Done(ChildDone(1, outcome)),
                || {
                    let result = pump_master_out(
                        &mut master_reader,
                        &writer_out,
                        &master_writer,
                        cancel,
                        &activity,
                        epoch,
                    );
                    peer_done_out.store(true, Ordering::SeqCst);
                    result
                },
            );
        });
        let reaper_tx = event_tx.clone();
        let reaper_hub = Arc::clone(&hub);
        scope.spawn(move || {
            let code = waiter.wait_exit();
            let _ = reaper_tx.send(PtyEvent::Exited(ChildExited(code)));
            reaper_hub.poke();
        });
        // Resize watcher: sleeps in the size cell until a
        // window-change lands, then asks the supervisor to apply it.
        // No sequence polling: the cell notifies on every record.
        let resize_tx = event_tx.clone();
        let resize_hub = Arc::clone(&hub);
        let resize_cell = Arc::clone(size_source);
        let resize_stop_in = &resize_stop;
        let mut resize_seen = applied_seq.seq;
        scope.spawn(move || {
            loop {
                let snapshot = wait_size_changed(&resize_cell, resize_seen, resize_stop_in);
                if resize_stop_in.load(Ordering::SeqCst) {
                    break;
                }
                resize_seen = snapshot.seq;
                let _ = resize_tx.send(PtyEvent::Resize);
                resize_hub.poke();
            }
        });
        // Supervisor: drain events, translate hub events into worker
        // wakes, block otherwise. Every wake path is idempotent so
        // repeated wakes while cancelled stay harmless.
        let mut seen = hub.sequence();
        loop {
            for event in event_rx.try_iter() {
                match event {
                    PtyEvent::Done(done) => {
                        if done.0 == 0 {
                            in_done = true;
                            note_failure(&mut failed, done.1, "input worker");
                        } else {
                            outputs[0].0 = true;
                            note_failure(&mut failed, done.1, "output worker");
                        }
                    }
                    PtyEvent::Exited(exited) => {
                        if exit_code.is_none() {
                            exit_code = Some(exited.0);
                            // The child is gone: note when, but the
                            // console lives until the drain is due so a
                            // slow flush still lands. Releasing it here
                            // would truncate the tail; holding it to
                            // function end strands the output worker
                            // (ConPTY holds the pipe open until
                            // ClosePseudoConsole runs). The quiet
                            // deadline below threads between them.
                            // Input ends at once (bytes pump nowhere):
                            // the old tick observed `peer_done`; without
                            // a tick the wake must be explicit.
                            if let Some(inner) = in_backend.as_ref() {
                                inner.cancel();
                            }
                            term.exited_ms = Some(elapsed_ms(term.epoch));
                            term.drain_generation += 1;
                            let generation = term.drain_generation;
                            let quiet = quiet_remaining(&term);
                            let drain_tx = event_tx.clone();
                            let drain_hub = Arc::clone(&hub);
                            scope.spawn(move || {
                                std::thread::sleep(std::time::Duration::from_millis(quiet));
                                let _ = drain_tx.send(PtyEvent::DrainDue(generation));
                                drain_hub.poke();
                            });
                        }
                    }
                    PtyEvent::Resize => {
                        // Live resize: an outer window-change bumps the
                        // shared cell's sequence; apply it so the kernel
                        // SIGWINCHes the child, which forwards it to the
                        // remote end itself. Sequenced, so an explicit
                        // open size is never clobbered by a stale
                        // default. The master is gone once released: a
                        // dead child has no terminal left to resize.
                        let current = snapshot_pty_size(&term.size_source);
                        if current.seq != term.applied_seq.seq {
                            term.applied_seq = current;
                            if let Some(master) = term.master.as_ref() {
                                let _ = master.resize(to_portable_size(current.size));
                            }
                        }
                    }
                    PtyEvent::DrainDue(generation) => {
                        // Due drain with a live output worker: release
                        // the console so its blocking read observes EOF,
                        // then the close below lands after the flushed
                        // tail. Bytes still flowing re-arm the deadline
                        // instead; the cap inside `drain_overdue` bounds
                        // the pathological console no matter what.
                        if generation != term.drain_generation
                            || term.master.is_none()
                            || outputs[0].0
                        {
                            continue;
                        }
                        if drain_overdue(
                            term.activity.load(Ordering::SeqCst),
                            term.exited_ms,
                            elapsed_ms(term.epoch),
                        ) {
                            term.master = None;
                        } else {
                            term.drain_generation += 1;
                            let next_generation = term.drain_generation;
                            let quiet = quiet_remaining(&term);
                            let drain_tx = event_tx.clone();
                            let drain_hub = Arc::clone(&hub);
                            scope.spawn(move || {
                                std::thread::sleep(std::time::Duration::from_millis(quiet));
                                let _ = drain_tx.send(PtyEvent::DrainDue(next_generation));
                                drain_hub.poke();
                            });
                        }
                    }
                }
            }
            let outputs_done = close_drained_outputs(cx, &mut outputs, exit_code.is_some());
            if in_done && outputs_done && exit_code.is_some() {
                break;
            }
            if hub.is_cancelled() || cx.is_cancelled() {
                cancel.store(true, Ordering::SeqCst);
                if let Some(inner) = in_backend.as_ref() {
                    inner.cancel();
                }
                // Release the console so a worker parked in the master
                // read ends at once; teardown drops it at function end
                // either way.
                term.master = None;
                let _ = killer.kill();
            }
            (_, seen) = hub.wait(seen);
        }
        // Retire the resize watcher before scope end joins it.
        resize_stop.store(true, Ordering::SeqCst);
        crate::state::poke_size(size_source);
    });
    if let Some(err) = failed {
        return Err(err);
    }
    Ok(exit_code.unwrap_or(-1))
}

/// Millis until the drain deadline from here: the quiescence window
/// past the later of last output and child death, floored at one so a
/// due drain still schedules (and observes) its release.
fn quiet_remaining(term: &PtyTerm) -> u64 {
    let now = elapsed_ms(term.epoch);
    let anchor = term
        .activity
        .load(Ordering::SeqCst)
        .max(term.exited_ms.unwrap_or(now));
    DRAIN_QUIESCE_MS
        .saturating_sub(now.saturating_sub(anchor))
        .max(1)
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
    fn query_answered_once_and_stripped() {
        let (master_in, seen) = harness();
        let mut filter = DsrFilter::new();
        assert_eq!(filter.feed(b"ab\x1b[6ncd", &master_in).as_slice(), b"abcd");
        assert_eq!(
            seen.lock().expect("sink readable").as_slice(),
            b"\x1b[1;1R".as_slice()
        );
        assert!(filter.flush().is_empty());
    }

    #[test]
    fn split_query_answered_once_and_stripped() {
        let (master_in, seen) = harness();
        let mut filter = DsrFilter::new();
        assert_eq!(filter.feed(b"abc\x1b[", &master_in).as_slice(), b"abc");
        assert!(seen.lock().expect("sink readable").is_empty());
        assert_eq!(filter.feed(b"6nrest", &master_in).as_slice(), b"rest");
        assert_eq!(
            seen.lock().expect("sink readable").as_slice(),
            b"\x1b[1;1R".as_slice()
        );
        // Replay past the boundary: the consumed query never rescans.
        assert_eq!(filter.feed(b"more", &master_in).as_slice(), b"more");
        assert_eq!(
            seen.lock().expect("sink readable").as_slice(),
            b"\x1b[1;1R".as_slice()
        );
        assert!(filter.flush().is_empty());
    }

    #[test]
    fn trailing_prefix_flushed_at_eof() {
        let (master_in, seen) = harness();
        let mut filter = DsrFilter::new();
        assert_eq!(
            filter.feed(b"ends with \x1b[", &master_in).as_slice(),
            b"ends with "
        );
        assert!(seen.lock().expect("sink readable").is_empty());
        assert_eq!(filter.flush().as_slice(), b"\x1b[");
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
        let mut filter = DsrFilter::new();
        assert_eq!(
            filter
                .feed(b"plain output, no query", &master_in)
                .as_slice(),
            b"plain output, no query"
        );
        assert!(seen.lock().expect("sink readable").is_empty());
        assert!(filter.flush().is_empty());
    }
}
