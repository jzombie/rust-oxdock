//! Binary-safe process execution for SSH sessions: piped stdio with no
//! terminal, no translation, and no init bytes. Bytes travel untouched
//! both ways, so `scp`-style protocols survive; diagnostics ride their
//! own pipe and never pollute the byte stream.
//!
//! Environment and working directory match `SSH_PTY_RUN` (closed script
//! env, script cwd). Unlike the pty path there is no resize, no setup
//! handshake, and no drain wait: plain pipes EOF on their own once the
//! child is gone, so teardown is just reap plus close.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use oxdock_core::{StepCtx, Value};
use oxdock_fs::command_path;
use oxdock_process::{
    BackgroundHandle, ChildHandle, CommandBuilder, PUMP_CHUNK, PipedChild, ProcessManager,
    SharedOutput, run_guarded as guard_worker,
};

use crate::bridge::{
    ChildDone, ChildExited, DeferredClose, close_drained_outputs, note_failure, pump_pipe_to_sink,
};

/// Copy one child stream into a DSL pipe until EOF. Ends on child EOF;
/// a dead child closes its ends, so EOF always arrives without help.
fn pump_fd_to_pipe(
    stream: &mut Box<dyn Read + Send>,
    writer: &SharedOutput,
    name: &str,
) -> Result<()> {
    let mut buffer = [0u8; PUMP_CHUNK];
    loop {
        match stream.read(&mut buffer) {
            Err(err) => bail!("SSH run child {name} read failed: {err}"),
            Ok(0) => break,
            Ok(count) => {
                let mut guard = writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("SSH run output lock poisoned"))?;
                guard
                    .write_all(&buffer[..count])
                    .context("SSH run output pipe write failed")?;
                guard.flush().context("SSH run output pipe flush failed")?;
            }
        }
    }
    Ok(())
}

/// Backend half of the shared child-pump core: plain-pipe handles. No
/// extras: pipes EOF on their own, so teardown is just reap plus close.
/// The reaper owns the wait handle; the supervisor kills through a
/// clone (pid-based, valid mid-wait) and applies deferred closes. All
/// shared state is reference counted: the reaper runs detached, so
/// nothing here borrows the pump frame.
struct RunWaiter {
    handle: ChildHandle,
    peer_done: Arc<AtomicBool>,
    report: Option<Arc<dyn Fn(u32) + Send + Sync>>,
}

impl RunWaiter {
    /// Block until the child exits and report the wire status inside,
    /// returning the code. Signaled deaths and broken wait handles map
    /// to -1 (clients observe the close without a status). Runs on the
    /// reaper thread: the report always precedes the EOF closes the
    /// supervisor applies after, so a fast child never loses its status
    /// to the close.
    fn wait_exit(mut self) -> i64 {
        let code = match self.handle.wait() {
            Ok(status) => match status.code() {
                Some(code) if code >= 0 => {
                    if let Some(report) = self.report {
                        report(code as u32);
                    }
                    i64::from(code)
                }
                _ => -1,
            },
            Err(_) => -1,
        };
        self.peer_done.store(true, Ordering::SeqCst);
        code
    }
}

/// One supervisor event: a directional worker finished, or the reaper
/// observed child exit. A single channel carries both so the
/// supervisor parks on one blocking receive after servicing
/// cancellation, with zero CPU and no second queue to poll.
enum RunEvent {
    Done(ChildDone),
    Exited(ChildExited),
}

/// Apply one supervisor event: completion bookkeeping and deferred
/// closes stay gated on an observed exit, exactly as before. Shared by
/// the drain loop and the post-cancel park, so both paths observe the
/// same ordering and neither duplicates the arms.
#[allow(clippy::too_many_arguments)]
fn handle_run_event(
    event: RunEvent,
    in_done: &mut bool,
    outputs: &mut [(bool, DeferredClose<'_>)],
    failed: &mut Option<anyhow::Error>,
    exit_code: &mut Option<i64>,
    in_backend: &Option<Arc<oxdock_pipe::PipeInner>>,
) {
    match event {
        RunEvent::Done(done) => {
            let name = match done.0 {
                0 => {
                    *in_done = true;
                    "input worker"
                }
                1 => {
                    outputs[0].0 = true;
                    "stdout worker"
                }
                2 => {
                    outputs[1].0 = true;
                    "stderr worker"
                }
                _ => "unknown worker",
            };
            note_failure(failed, done.1, name);
        }
        RunEvent::Exited(exited) => {
            if exit_code.is_none() {
                *exit_code = Some(exited.0);
                // The child is gone: bytes pump nowhere, so end a
                // worker parked in its pipe read. The old tick
                // observed `peer_done`; without a tick the wake
                // must be explicit.
                if let Some(inner) = in_backend.as_ref() {
                    inner.cancel();
                }
            }
        }
    }
}

/// Run `argv` with piped stdio through explicit DSL pipes until the
/// child exits. Returns the exit code, or -1 when the child died by
/// signal (nothing reportable then: clients observe the close without
/// a status). The wire `on_exit` report fires the instant the exit is
/// observed, always ahead of the EOF pipe closes below, so a fast
/// child never loses its status to the close.
#[allow(clippy::too_many_arguments)]
pub fn pump_run_session<P: ProcessManager>(
    cx: &StepCtx<P>,
    argv: &[String],
    in_pipe: &Value,
    out_pipe: &Value,
    err_pipe: &Value,
    on_exit: Option<Arc<dyn Fn(u32) + Send + Sync>>,
) -> Result<i64> {
    if argv.is_empty() {
        bail!("SSH_RUN needs at least a program");
    }
    let reader = cx
        .pipe_reader(in_pipe)
        .context("SSH run cannot borrow the input pipe")?;
    let out_writer = cx
        .pipe_writer(out_pipe)
        .context("SSH run cannot borrow the output pipe")?;
    let err_writer = cx
        .pipe_writer(err_pipe)
        .context("SSH run cannot borrow the error pipe")?;

    cx.ensure_scratch_materialized()?;
    let staged = cx.env_snapshot();
    oxdock_process::check_spawn_anchors(&argv[0], &staged, false)?;
    let mut builder = CommandBuilder::new(&argv[0]);
    builder.args(&argv[1..]);
    // Native form, not the guarded verbatim path: a `\\?\` startup
    // directory trips console apps into UNC handling, the same reason
    // process spawn goes through `command_path`.
    let cwd = command_path(cx.cwd());
    builder.current_dir(cwd.as_ref());
    // Closed child env like `RUN`: the script map verbatim, never host
    // inherit.
    builder.env_clear();
    for (key, value) in &staged {
        builder.env(key, value);
    }
    let piped = builder
        .spawn_piped_with_stderr()
        .context("SSH run spawn failed")?;
    let PipedChild {
        handle,
        stdin,
        stdout,
        stderr,
    } = piped;
    let child_stdin: Box<dyn Write + Send> = Box::new(stdin);
    let child_stdout: Box<dyn Read + Send> = Box::new(stdout);
    let Some(stderr) = stderr else {
        bail!("SSH run spawn lost stderr");
    };
    let child_stderr: Box<dyn Read + Send> = Box::new(stderr);

    let peer_done = Arc::new(AtomicBool::new(false));
    let waiter = RunWaiter {
        handle,
        peer_done: Arc::clone(&peer_done),
        report: on_exit,
    };
    // Pid-based kill handle, valid while the reaper owns the wait
    // handle: killing never needs the `&mut` the blocking wait holds.
    let mut killer = waiter.handle.clone();
    let hub = cx.cancel_hub();
    // Task-local cancellation shared with the detached workers (see
    // `bridge::pump_session` for the capture rule).
    let cancel = Arc::new(AtomicBool::new(false));
    let in_backend = cx.pipe_backend(in_pipe);
    let (event_tx, event_rx) = std::sync::mpsc::channel::<RunEvent>();
    let mut failed: Option<anyhow::Error> = None;
    let mut in_done = false;
    let mut exit_code: Option<i64> = None;
    let mut outputs = [
        (
            false,
            DeferredClose {
                pipe: out_pipe,
                closed: false,
            },
        ),
        (
            false,
            DeferredClose {
                pipe: err_pipe,
                closed: false,
            },
        ),
    ];
    // Detached workers: the supervisor never joins them, so a worker
    // parked in an uninterruptible read (an orphaned grandchild
    // holding its stdio past every kill) cannot strand the task
    // thread. Capture rule: clone every shared handle HERE, in plain
    // (non-`move`) context, and move only the clones into the
    // workers. A `move` closure moves everything it touches, even
    // through `&`: cloning inside would steal the frame's own Arc,
    // and pipe writers must stay attached until frame end. Owned
    // stdin drops at its worker's end, closing the child stdin; a
    // borrowed handle would outlive the worker and the child would
    // never observe EOF.
    let in_tx = event_tx.clone();
    let out_tx = event_tx.clone();
    let err_tx = event_tx.clone();
    let in_hub = Arc::clone(&hub);
    let out_hub = Arc::clone(&hub);
    let err_hub = Arc::clone(&hub);
    let reaper_hub = Arc::clone(&hub);
    let reader_in = Arc::clone(&reader);
    let peer_done_in = Arc::clone(&peer_done);
    let cancel_in = Arc::clone(&cancel);
    let out_writer_out = Arc::clone(&out_writer);
    let err_writer_out = Arc::clone(&err_writer);
    std::thread::spawn(move || {
        guard_worker(
            &in_hub,
            &in_tx,
            |outcome| RunEvent::Done(ChildDone(0, outcome)),
            || {
                let mut stdin_holder = child_stdin;
                pump_pipe_to_sink(
                    &reader_in,
                    &mut stdin_holder,
                    &cancel_in,
                    &peer_done_in,
                    "SSH run",
                )
            },
        );
    });
    std::thread::spawn(move || {
        let mut child_stdout = child_stdout;
        guard_worker(
            &out_hub,
            &out_tx,
            |outcome| RunEvent::Done(ChildDone(1, outcome)),
            || pump_fd_to_pipe(&mut child_stdout, &out_writer_out, "stdout"),
        );
    });
    std::thread::spawn(move || {
        let mut child_stderr = child_stderr;
        guard_worker(
            &err_hub,
            &err_tx,
            |outcome| RunEvent::Done(ChildDone(2, outcome)),
            || pump_fd_to_pipe(&mut child_stderr, &err_writer_out, "stderr"),
        );
    });
    std::thread::spawn(move || {
        let code = waiter.wait_exit();
        let _ = event_tx.send(RunEvent::Exited(ChildExited(code)));
        reaper_hub.poke();
    });
    // Supervisor: drain completions, translate hub events into worker
    // wakes, block on the hub otherwise. Every wake path is idempotent
    // so repeated wakes while cancelled stay harmless. Cancellation
    // breaks at once (after teardown): with detached workers there is
    // no join to wait out, so the task thread always returns.
    let mut seen = hub.sequence();
    loop {
        for event in event_rx.try_iter() {
            handle_run_event(
                event,
                &mut in_done,
                &mut outputs,
                &mut failed,
                &mut exit_code,
                &in_backend,
            );
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
            let _ = killer.kill();
            // Prompt downstream EOF on the way out: readers parked
            // past this see the close instead of hanging, and writers
            // still pushing fail out of their loops.
            let _ = cx.close_pipe(in_pipe);
            let _ = cx.close_pipe(out_pipe);
            let _ = cx.close_pipe(err_pipe);
            break;
        }
        (_, seen) = hub.wait(seen);
    }
    match failed {
        Some(err) => Err(err),
        None => {
            if cancel.load(Ordering::SeqCst) {
                anyhow::bail!("SSH run interrupted by cancellation");
            }
            Ok(exit_code.unwrap_or(-1))
        }
    }
}
