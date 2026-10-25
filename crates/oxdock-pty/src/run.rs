//! Blocking pty session runner: spawn under a pty, pump caller bytes
//! through the master, follow resizes, drain to quiescence, report the
//! exit code.
//!
//! One call runs the whole lifecycle on the calling thread and returns
//! when the child is gone and its output drained (or when cancelled).
//! Workers are detached by design: a thread parked in an
//! uninterruptible read can never strand the caller, which is what
//! lets cancellation return at once. The resize watcher is the only
//! joined thread, and its wake is bounded (stop flag plus poke).
//!
//! Callers supply plain byte sources and sinks plus a size source; no
//! run model, pipe backend, or task system crosses this boundary. The
//! SSH plugin adapts its pipe backends and session size cell onto
//! these shapes; a local `WITH_IO [tty]` runner would adapt script
//! pipes and a static size the same way.

use std::io::{Read, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::drain::{drain_overdue, elapsed_ms, quiet_remaining_ms};
use crate::filter::DsrFilter;
use crate::size::{PtySize, SizeSource};

/// Pump chunk size, mirroring the process pump so pipe and pty paths
/// move the same byte windows.
const PUMP_CHUNK: usize = 8192;
/// Supervisor poll for cancellation between channel events. Only the
/// cancel path depends on it (completions and drain deadlines arrive
/// as events); ten milliseconds keeps Ctrl-C prompt without a hot
/// loop or a wakeup primitive.
const SUPERVISOR_POLL_MS: u64 = 10;

/// Chunk reader for the input pump: fills `buf`, `Ok(0)` means EOF.
pub type ReadChunk = Box<dyn FnMut(&mut [u8]) -> std::io::Result<usize> + Send>;

/// Input bytes plus the hook that wakes a worker parked in a blocking
/// read. The worker owns the reader; the hook stays on the supervisor
/// thread, so neither half needs to cross threads.
pub struct PtyInput {
    /// Read the next chunk; `Ok(0)` means EOF. Moved into the worker.
    pub read_chunk: ReadChunk,
    /// Wake a worker parked in a blocking read. Stays on the
    /// supervisor thread; never sent anywhere.
    pub cancel: Box<dyn Fn()>,
}

/// Full session request. Borrowed hooks (`should_cancel`,
/// `on_output_drained`) run on the calling thread, so the request
/// borrows the caller for the (blocking) call instead of demanding
/// `'static` closures.
pub struct PtyRequest<'a> {
    /// Program plus arguments; empty bails naming the caller.
    pub argv: Vec<String>,
    /// Complete child environment (seeded by the caller, not inherited).
    pub env: Vec<(String, String)>,
    /// Child working directory, as a plain string: host path types
    /// stay inside this crate's conversion helper, never in the
    /// public request.
    pub cwd: String,
    /// Opening terminal size (rows, cols; the sequence is ignored).
    pub initial_size: PtySize,
    /// Stdin bytes, or `None` for a closed stdin (no input worker).
    pub input: Option<PtyInput>,
    /// Where master output lands. Shared so the DSR filter can answer
    /// ConPTY setup queries through the same handle the output worker
    /// writes: identical to `oxdock_pipe::SharedOutput` on purpose, so
    /// pipe backends pass straight through with no adapter.
    pub output: Arc<Mutex<dyn Write + Send>>,
    /// Window-change source followed for the session lifetime.
    pub size_source: Arc<dyn SizeSource>,
    /// Polled on the supervisor thread; true cancels the session.
    pub should_cancel: Box<dyn Fn() -> bool + 'a>,
    /// Fires exactly once with the learned exit code, ahead of the
    /// drain close, so a reporter never loses to EOF teardown.
    pub on_exit: Option<Arc<dyn Fn(u32) + Send + Sync>>,
    /// Runs once when the output worker finished and the exit is
    /// observed: the caller's deferred-close point.
    pub on_output_drained: Option<Box<dyn FnOnce() + 'a>>,
    /// Caller name for error context (mirrors the `func` convention
    /// of pipe-pump helpers so logs keep their origin).
    pub func: &'static str,
}

/// How a session ended. Worker failures surface as `Err`, never here.
#[derive(Debug)]
pub enum PtyOutcome {
    /// Child reaped with its exit code, output drained.
    Completed(i64),
    /// Cancelled mid-run; the child was killed.
    Cancelled,
}

/// Supervisor events. `Resize` and `DrainDue` come from watcher
/// threads; completions arrive through plain sends (a panicking
/// worker reports instead of stranding the supervisor).
enum PtyEvent {
    Done(ChildDone),
    Exited(ChildExited),
    Resize,
    DrainDue(u64),
}

/// Completion of one child-pump worker, by slot (0 is always input).
struct ChildDone(usize, Result<()>);

/// The reaper observed child exit, carrying the code. Produced by the
/// backend's blocking wait on its own thread: ConPTY holds the master
/// open past child death, so no pipe EOF can stand in for it.
struct ChildExited(i64);

/// Reaper half of the backend: owns the blocking wait. Reports the
/// exit code inside, then returns it; a broken wait handle reports
/// nothing (there is no status to report) and still yields -1 so the
/// drain wait forces the release. All shared state is reference
/// counted: the reaper runs detached, so nothing here borrows the
/// pump frame.
struct PtyWaiter {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    peer_done: Arc<AtomicBool>,
    report: Option<Arc<dyn Fn(u32) + Send + Sync>>,
}

impl PtyWaiter {
    /// Block until the child exits and report the wire status inside,
    /// returning the code. The report precedes the EOF the supervisor
    /// applies after, so a fast child never loses its status to the
    /// close.
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
/// teardown), live size tracking, drain bookkeeping. The reaper owns
/// the wait; this owns everything else, so no `&mut` is ever shared
/// across the wait.
struct PtyTerm {
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    applied_seq: u64,
    activity: Arc<AtomicU64>,
    epoch: Instant,
    exited_ms: Option<u64>,
    drain_generation: u64,
}

/// Release nudge replicating portable-pty's Unix master-writer drop:
/// EOT so a stdin-blocked child observes EOF and exits, led by a
/// newline only when the fed bytes end mid-line (canonical-mode VEOF
/// already submits a buffered partial line, while a leading newline
/// on a line-complete buffer would inject a phantom empty line).
/// Upstream only has this behavior on Unix (its Windows master writer
/// drop just closes the handle), so this is unix-only too: Windows
/// console children must never observe stray input bytes.
fn send_release_nudge(writer: &Arc<Mutex<Box<dyn Write + Send>>>, newline: bool) {
    #[cfg(unix)]
    if let Ok(mut guard) = writer.lock() {
        // Default termios VEOF (Ctrl-D); our ptys never customize it,
        // and the fd hides behind the trait object, so the byte is
        // fixed here rather than read back like upstream does.
        if newline {
            let _ = guard.write_all(b"\n");
        }
        let _ = guard.write_all(b"\x04");
        let _ = guard.flush();
    }
    #[cfg(not(unix))]
    let _ = (writer, newline);
}

/// Pump master-terminal output into the sink. Ends on master EOF
/// (child and its children are gone; portable-pty normalizes the
/// platform EIO-into-EOF kink for us) or on console teardown (the
/// supervisor releases the master when the drain is due, which closes
/// a ConPTY output pipe that would otherwise stay open). Answers
/// ConPTY setup queries in band while stripping them from the stream.
/// Stamps `activity` on every forwarded chunk so the supervisor can
/// tell a draining console from a stranded one.
#[allow(clippy::too_many_arguments)]
fn pump_master_out(
    reader: &mut Box<dyn Read + Send>,
    writer: &Arc<Mutex<dyn Write + Send>>,
    master_in: &Arc<Mutex<Box<dyn Write + Send>>>,
    cancel: &Arc<AtomicBool>,
    activity: &Arc<AtomicU64>,
    epoch: Instant,
    func: &'static str,
) -> Result<()> {
    let mut buffer = [0u8; PUMP_CHUNK];
    let mut queries = DsrFilter::new();
    loop {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        match reader.read(&mut buffer) {
            // A torn-down ConPTY pipe reads as broken rather than
            // clean EOF, depending on read versus teardown timing.
            // Either way no more bytes will ever arrive, so the drain
            // ends here.
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => break,
            Err(err) => bail!("{func} master read failed: {err}"),
            Ok(0) => break,
            Ok(count) => {
                let clean = queries.feed(&buffer[..count], master_in);
                if clean.is_empty() {
                    continue;
                }
                let mut guard = writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("{func} output lock poisoned"))?;
                guard
                    .write_all(&clean)
                    .with_context(|| format!("{func} output write failed"))?;
                guard
                    .flush()
                    .with_context(|| format!("{func} output flush failed"))?;
                activity.store(elapsed_ms(epoch), Ordering::SeqCst);
            }
        }
    }
    // Held query-prefix bytes never completed a query: genuine
    // output, released at EOF.
    let tail = queries.flush();
    if !tail.is_empty() {
        let mut guard = writer
            .lock()
            .map_err(|_| anyhow::anyhow!("{func} output lock poisoned"))?;
        guard
            .write_all(&tail)
            .with_context(|| format!("{func} output write failed"))?;
        guard
            .flush()
            .with_context(|| format!("{func} output flush failed"))?;
    }
    Ok(())
}

/// Pump caller stdin bytes into the master writer until EOF,
/// cancellation, or `peer_done` (child gone: bytes would have nowhere
/// to go). Reports whether the fed bytes end mid-line: the release
/// nudge must submit a pending partial line before signalling EOF,
/// but must not inject a newline when the buffer is already
/// line-complete (that newline would echo back as a phantom empty
/// line and hand the next reader a spurious blank instead of EOF).
fn pump_stdin_to_master(
    read_chunk: &mut (dyn FnMut(&mut [u8]) -> std::io::Result<usize> + Send),
    master: &Arc<Mutex<Box<dyn Write + Send>>>,
    cancel: &Arc<AtomicBool>,
    peer_done: &Arc<AtomicBool>,
    func: &'static str,
) -> Result<bool> {
    let mut buffer = [0u8; PUMP_CHUNK];
    let mut fed = false;
    let mut ends_newline = true;
    loop {
        if cancel.load(Ordering::SeqCst) || peer_done.load(Ordering::SeqCst) {
            break;
        }
        match read_chunk(&mut buffer) {
            Err(err) => bail!("{func} input read failed: {err}"),
            Ok(0) => break,
            Ok(count) => {
                let mut guard = master
                    .lock()
                    .map_err(|_| anyhow::anyhow!("{func} sink lock poisoned"))?;
                if guard.write_all(&buffer[..count]).is_err() {
                    break;
                }
                if guard.flush().is_err() {
                    break;
                }
                fed = true;
                ends_newline = buffer[count - 1] == b'\n';
            }
        }
    }
    Ok(fed && !ends_newline)
}

/// Spawn the Unix window-change watch: one thread blocked in SIGWINCH
/// delivery, repolling host-tracked dimensions on each signal and
/// asking the supervisor to apply real moves through the normal
/// resize path. Returns the close handle plus the thread when the
/// source tracks the host; `None` for static cells and remote
/// sessions (no watch, opener size stands), and on signal-registry
/// failure (same static fallback, never a session failure).
#[cfg(unix)]
fn spawn_sigwinch_watch(
    size_source: &Arc<dyn SizeSource>,
    event_tx: &mpsc::Sender<PtyEvent>,
) -> Option<(signal_hook::iterator::Handle, std::thread::JoinHandle<()>)> {
    if !size_source.follows_host() {
        return None;
    }
    let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH]).ok()?;
    let handle = signals.handle();
    let size = Arc::clone(size_source);
    let tx = event_tx.clone();
    let thread = std::thread::spawn(move || {
        for _ in signals.forever() {
            // Unchanged probes stay silent; only real moves reach the
            // supervisor, so a stray SIGWINCH never re-SIGWINCHes the
            // child for an identical size.
            if size.repoll() {
                let _ = tx.send(PtyEvent::Resize);
            }
        }
    });
    Some((handle, thread))
}

/// Spawn the repoll watch for platforms without a window signal: one
/// thread re-reading host-tracked dimensions on a bounded tick,
/// retired through the shared stop below. `None` for static cells
/// and remote sessions, like the SIGWINCH watch.
#[cfg(windows)]
fn spawn_repoll_watch(
    size_source: &Arc<dyn SizeSource>,
    stop: &Arc<AtomicBool>,
    event_tx: &mpsc::Sender<PtyEvent>,
) -> Option<std::thread::JoinHandle<()>> {
    if !size_source.follows_host() {
        return None;
    }
    let size = Arc::clone(size_source);
    let tx = event_tx.clone();
    let stop = Arc::clone(stop);
    Some(std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(
                crate::size::HOST_FOLLOW_POLL_MS,
            ));
            if stop.load(Ordering::SeqCst) {
                break;
            }
            if size.repoll() {
                let _ = tx.send(PtyEvent::Resize);
            }
        }
    }))
}

/// Run one pty session to completion: spawn under a pty sized from
/// the size source, pump input and output, follow resizes, drain to
/// quiescence, and report the exit code.
///
/// The calling thread supervises; workers detach. Cancellation (via
/// `should_cancel`) tears down the console, kills the child, and
/// returns `Cancelled` without waiting out parked workers.
pub fn run_pty_session(mut req: PtyRequest<'_>) -> Result<PtyOutcome> {
    if req.argv.is_empty() {
        bail!("{} needs at least a program", req.func);
    }
    let func = req.func;
    let epoch = Instant::now();
    let activity = Arc::new(AtomicU64::new(elapsed_ms(epoch)));
    let peer_done = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));

    let pair = portable_pty::native_pty_system()
        .openpty(to_portable_size(
            req.initial_size.rows,
            req.initial_size.cols,
        ))
        .with_context(|| format!("{func} pty allocation failed"))?;
    let mut master_reader = pair
        .master
        .try_clone_reader()
        .with_context(|| format!("{func} pty master reader failed"))?;
    // Shared with the output worker, which answers ConPTY setup
    // queries through it while the input worker pumps pipe bytes.
    let master_writer = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .with_context(|| format!("{func} pty master writer failed"))?,
    ));
    // Console teardown handle. Released once the drain is due: ConPTY
    // holds the output pipe open until close, which lives in this
    // handle, so holding it to function end strands the output worker
    // in its blocking read; releasing it the instant the child dies
    // truncates the slow flush instead. Unix reports child death as
    // master EOF either way, so the worker usually beats the drain
    // wait there and the quiescence path stays inert.
    let master_opt = Some(pair.master);
    let mut builder = portable_pty::CommandBuilder::new(&req.argv[0]);
    builder.args(&req.argv[1..]);
    builder.cwd(cwd_path(&req.cwd));
    builder.env_clear();
    for (key, value) in &req.env {
        builder.env(key, value);
    }
    let child = pair
        .slave
        .spawn_command(builder)
        .with_context(|| format!("{func} pty spawn failed"))?;
    drop(pair.slave);

    let mut killer = child.clone_killer();
    let waiter = PtyWaiter {
        child,
        peer_done: Arc::clone(&peer_done),
        report: req.on_exit.take(),
    };
    let applied = req.size_source.current();
    let mut term = PtyTerm {
        master: master_opt,
        applied_seq: applied.seq,
        activity: Arc::clone(&activity),
        epoch,
        exited_ms: None,
        drain_generation: 0,
    };
    let (event_tx, event_rx) = mpsc::channel::<PtyEvent>();
    let mut failed: Option<anyhow::Error> = None;
    let mut in_done = req.input.is_none();
    let mut outputs_done = false;
    let mut exit_code: Option<i64> = None;
    let mut on_output_drained = req.on_output_drained.take();

    let input_tx = event_tx.clone();
    let output_tx = event_tx.clone();
    let reaper_tx = event_tx.clone();
    let resize_tx = event_tx.clone();
    // Split the reader (moved into the worker) from the wake hook
    // (stays on the supervisor thread).
    let (input_reader, input_cancel) = match req.input.take() {
        Some(slot) => (Some(slot.read_chunk), Some(slot.cancel)),
        None => (None, None),
    };
    let master_writer_in = Arc::clone(&master_writer);
    let master_writer_out = Arc::clone(&master_writer);
    let nudge_writer = Arc::clone(&master_writer);
    let cancel_in = Arc::clone(&cancel);
    let cancel_out = Arc::clone(&cancel);
    let peer_done_in = Arc::clone(&peer_done);
    let peer_done_out = Arc::clone(&peer_done);
    let activity_out = Arc::clone(&activity);
    let resize_stop = Arc::new(AtomicBool::new(false));
    let resize_stop_in = Arc::clone(&resize_stop);
    let resize_source = Arc::clone(&req.size_source);
    let mut resize_seen = applied.seq;
    // Detached workers: the supervisor never joins them, so a worker
    // parked in an uninterruptible read can never strand the caller.
    if let Some(mut read_chunk) = input_reader {
        std::thread::spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let result = pump_stdin_to_master(
                    &mut read_chunk,
                    &master_writer_in,
                    &cancel_in,
                    &peer_done_in,
                    func,
                );
                // Unix EOF nudge replicating portable-pty's master-writer
                // drop: the shared writer outlives this worker, so
                // without it a stdin-blocked child never observes EOF.
                // The pump reports whether a partial line is still
                // pending; only then does the nudge lead with a newline.
                match result {
                    Ok(partial) => {
                        send_release_nudge(&nudge_writer, partial);
                        Ok(())
                    }
                    Err(err) => {
                        send_release_nudge(&nudge_writer, true);
                        Err(err)
                    }
                }
            }));
            let outcome = match outcome {
                Ok(inner) => inner,
                Err(panic) => Err(anyhow::anyhow!("{func} input worker panicked: {panic:?}")),
            };
            let _ = input_tx.send(PtyEvent::Done(ChildDone(0, outcome)));
        });
    }
    let writer_out = Arc::clone(&req.output);
    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let result = pump_master_out(
                &mut master_reader,
                &writer_out,
                &master_writer_out,
                &cancel_out,
                &activity_out,
                epoch,
                func,
            );
            peer_done_out.store(true, Ordering::SeqCst);
            result
        }));
        let outcome = match outcome {
            Ok(inner) => inner,
            Err(panic) => Err(anyhow::anyhow!("{func} output worker panicked: {panic:?}")),
        };
        let _ = output_tx.send(PtyEvent::Done(ChildDone(1, outcome)));
    });
    std::thread::spawn(move || {
        let code = waiter.wait_exit();
        let _ = reaper_tx.send(PtyEvent::Exited(ChildExited(code)));
    });
    // Resize watcher: sleeps in the size source until a window-change
    // lands, then asks the supervisor to apply it. Detached like the
    // workers; retired explicitly below (stop plus poke, then join:
    // the wake is prompt, so the join is bounded).
    let resize_watcher = std::thread::spawn(move || {
        loop {
            match resize_source.wait_changed(resize_seen, &resize_stop_in) {
                None => break,
                Some(next) => {
                    resize_seen = next.seq;
                    let _ = resize_tx.send(PtyEvent::Resize);
                }
            }
        }
    });
    // Host-window watch: repoll host-tracked dimensions on
    // window-change events so unnamed sides follow the host while the
    // step runs. Unix learns promptly through SIGWINCH; platforms
    // without a window signal repoll on a bounded tick instead.
    // Sources without host-tracked dimensions (static cells, remote
    // sessions) need no watch and run exactly as before. Retired
    // explicitly below alongside the resize watcher.
    #[cfg(unix)]
    let sigwinch_watch = spawn_sigwinch_watch(&req.size_source, &event_tx);
    #[cfg(windows)]
    let repoll_watch = spawn_repoll_watch(&req.size_source, &resize_stop, &event_tx);
    // Supervisor: drain completions, apply resizes, arm the drain
    // deadline, and break at once on cancellation. Every wake path is
    // idempotent so repeated wakes while cancelled stay harmless.
    // A single blocking receive (never try_iter plus a discarding
    // recv): every queued event must reach the match below, since a
    // dropped completion strands the session and a dropped resize is
    // silently lost.
    loop {
        match event_rx.recv_timeout(std::time::Duration::from_millis(SUPERVISOR_POLL_MS)) {
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Ok(event) => match event {
                PtyEvent::Done(done) => {
                    if done.0 == 0 {
                        in_done = true;
                        if let Some(err) = done.1.err() {
                            failed
                                .get_or_insert(err.context(format!("{func} input worker failed")));
                        }
                    } else {
                        outputs_done = true;
                        if let Some(err) = done.1.err() {
                            failed
                                .get_or_insert(err.context(format!("{func} output worker failed")));
                        }
                    }
                }
                PtyEvent::Exited(exited) => {
                    if exit_code.is_none() {
                        exit_code = Some(exited.0);
                        // The child is gone: input pumps nowhere, so
                        // wake a worker blocked in its chunk read at
                        // once. Without this an idle input pipe never
                        // EOFs and the session strands on `in_done`.
                        if let Some(wake) = input_cancel.as_ref() {
                            (**wake)();
                        }
                        // The child is gone: note when, but the console
                        // lives until the drain is due. Releasing it
                        // here would truncate a slow ConPTY flush; the
                        // quiet deadline below threads between them.
                        term.exited_ms = Some(elapsed_ms(term.epoch));
                        term.drain_generation += 1;
                        let generation = term.drain_generation;
                        let quiet = quiet_remaining_ms(
                            term.activity.load(Ordering::SeqCst),
                            term.exited_ms,
                            elapsed_ms(term.epoch),
                        );
                        let drain_tx = event_tx.clone();
                        // Detached one-shot sleeper (never joined):
                        // it fires once within a quiescence window, so
                        // no join is needed and no timer thread lingers.
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(quiet));
                            let _ = drain_tx.send(PtyEvent::DrainDue(generation));
                        });
                    }
                }
                PtyEvent::Resize => {
                    // Live resize: a window change bumped the cell's
                    // sequence; apply it so the kernel SIGWINCHes the
                    // child. The master is gone once released: a dead
                    // child has no terminal left to resize.
                    let current = req.size_source.current();
                    if current.seq != term.applied_seq {
                        term.applied_seq = current.seq;
                        if let Some(master) = term.master.as_ref() {
                            let _ = master.resize(to_portable_size(current.rows, current.cols));
                        }
                    }
                }
                PtyEvent::DrainDue(generation) => {
                    // Due drain with a live output worker: release the
                    // console so its blocked read observes EOF, then the
                    // close lands after the flushed tail. Bytes still in
                    // flight re-arm the deadline instead; the cap inside
                    // `drain_overdue` bounds the pathological console no
                    // matter what. A finished worker needs no release:
                    // it already saw its EOF.
                    if generation != term.drain_generation || term.master.is_none() || outputs_done
                    {
                        continue;
                    }
                    if drain_overdue(
                        term.activity.load(Ordering::SeqCst),
                        term.exited_ms,
                        elapsed_ms(term.epoch),
                    ) {
                        term.master = None;
                        if let Some(close) = on_output_drained.take() {
                            close();
                        }
                    } else {
                        term.drain_generation += 1;
                        let next_generation = term.drain_generation;
                        let quiet = quiet_remaining_ms(
                            term.activity.load(Ordering::SeqCst),
                            term.exited_ms,
                            elapsed_ms(term.epoch),
                        );
                        let drain_tx = event_tx.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(quiet));
                            let _ = drain_tx.send(PtyEvent::DrainDue(next_generation));
                        });
                    }
                }
            },
        }
        if in_done && outputs_done && exit_code.is_some() {
            break;
        }
        if (req.should_cancel)() {
            cancel.store(true, Ordering::SeqCst);
            if let Some(wake) = input_cancel.as_ref() {
                (**wake)();
            }
            // Release the console so a worker parked in the master read
            // ends at once; teardown drops it at function end either
            // way. Then kill: with detached workers there is no join to
            // wait out, so the caller returns at once.
            term.master = None;
            let _ = killer.kill();
            break;
        }
    }
    // Retire the resize watcher, then join: the wake is prompt, so the
    // join is bounded (unlike a worker parked in an uninterruptible
    // read, which is never joined).
    resize_stop.store(true, Ordering::SeqCst);
    req.size_source.poke();
    let _ = resize_watcher.join();
    // Retire the host-window watch: closing wakes the blocked
    // iterator at once on Unix, so the join is prompt like the
    // watcher above; the shared stop plus the tick bound it
    // everywhere else.
    #[cfg(unix)]
    if let Some((handle, thread)) = sigwinch_watch {
        handle.close();
        let _ = thread.join();
    }
    #[cfg(windows)]
    if let Some(thread) = repoll_watch {
        let _ = thread.join();
    }
    if let Some(err) = failed {
        return Err(err);
    }
    if cancel.load(Ordering::SeqCst) {
        return Ok(PtyOutcome::Cancelled);
    }
    Ok(PtyOutcome::Completed(exit_code.unwrap_or(-1)))
}

/// Host path conversion at the single spawn boundary: portable-pty
/// takes native paths, and this crate owns that translation so no
/// host path type crosses the public request.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn cwd_path(cwd: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(cwd)
}

/// Native pty size for the open call.
fn to_portable_size(rows: u16, cols: u16) -> portable_pty::PtySize {
    portable_pty::PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}
