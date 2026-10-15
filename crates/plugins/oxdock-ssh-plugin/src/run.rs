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
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use oxdock_core::{StepCtx, Value};
use oxdock_fs::command_path;
use oxdock_process::{
    BackgroundHandle, ChildHandle, CommandBuilder, PipedChild, ProcessManager, SharedOutput,
};

use crate::bridge::{
    CHUNK, ChildCtl, DeferredClose, InputSink, TickInfo, pump_pipe_to_sink, supervise_child_pump,
};

/// Copy one child stream into a DSL pipe until EOF. Ends on child EOF;
/// a dead child closes its ends, so EOF always arrives without help.
fn pump_fd_to_pipe(
    stream: &mut Box<dyn Read + Send>,
    writer: &SharedOutput,
    name: &str,
) -> Result<()> {
    let mut buffer = [0u8; CHUNK];
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
struct RunChild<'a> {
    handle: ChildHandle,
    peer_done: &'a AtomicBool,
    report: Option<&'a dyn Fn(u32)>,
}

impl ChildCtl for RunChild<'_> {
    fn poll_exit(&mut self) -> Option<i64> {
        match self.handle.try_wait() {
            Ok(Some(status)) => {
                self.peer_done.store(true, Ordering::SeqCst);
                match status.code() {
                    Some(code) if code >= 0 => {
                        if let Some(report) = self.report {
                            report(code as u32);
                        }
                        Some(i64::from(code))
                    }
                    // Signaled: no meaningful code to report.
                    _ => Some(-1),
                }
            }
            Ok(None) => None,
            Err(_) => {
                self.peer_done.store(true, Ordering::SeqCst);
                Some(-1)
            }
        }
    }

    fn kill(&mut self) {
        let _ = self.handle.kill();
    }

    fn wait_code(&mut self) -> Result<i64> {
        // Workers drained without an observed exit: reap the
        // authoritative status blocking.
        match self.handle.wait() {
            Ok(status) => match status.code() {
                Some(code) if code >= 0 => {
                    if let Some(report) = self.report {
                        report(code as u32);
                    }
                    Ok(i64::from(code))
                }
                _ => Ok(-1),
            },
            Err(err) => bail!("SSH run reaped with error: {err}"),
        }
    }

    fn on_tick(&mut self, _tick: TickInfo) {}
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
    cancel: &AtomicBool,
    on_exit: Option<&dyn Fn(u32)>,
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
    let backend = cx.pipe_backend(in_pipe);

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
    let mut child_stdout: Box<dyn Read + Send> = Box::new(stdout);
    let Some(stderr) = stderr else {
        bail!("SSH run spawn lost stderr");
    };
    let mut child_stderr: Box<dyn Read + Send> = Box::new(stderr);

    let peer_done = AtomicBool::new(false);
    let mut run_child = RunChild {
        handle,
        peer_done: &peer_done,
        report: on_exit,
    };
    let exit_code = std::thread::scope(|scope| {
        // Pre-bound shared borrows (all `Copy`) plus owned values: the
        // `move` closure below takes these and borrows nothing else
        // from this frame. Owned stdin drops at thread end, closing
        // the child stdin; a borrowed handle would outlive the worker
        // and the child would never observe EOF.
        let reader_in = &reader;
        let backend_in = backend.as_ref();
        let peer_done_in = &peer_done;
        let mut worker_in = Some(scope.spawn(move || {
            pump_pipe_to_sink(
                reader_in,
                backend_in,
                InputSink::Owned(child_stdin),
                cancel,
                peer_done_in,
                "SSH run",
            )
        }));
        let worker_out =
            Some(scope.spawn(|| pump_fd_to_pipe(&mut child_stdout, &out_writer, "stdout")));
        let worker_err =
            Some(scope.spawn(|| pump_fd_to_pipe(&mut child_stderr, &err_writer, "stderr")));
        let mut outputs = [
            (
                worker_out,
                DeferredClose {
                    pipe: out_pipe,
                    closed: false,
                },
            ),
            (
                worker_err,
                DeferredClose {
                    pipe: err_pipe,
                    closed: false,
                },
            ),
        ];
        supervise_child_pump(cx, cancel, &mut run_child, &mut worker_in, &mut outputs)
    })?;
    match exit_code {
        Some(code) => Ok(code),
        None => run_child.wait_code(),
    }
}
