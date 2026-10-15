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
use oxdock_process::{BackgroundHandle, CommandBuilder, PipedChild, ProcessManager, SharedOutput};

use crate::bridge::{CHUNK, TICK, read_pipe};

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
                *failed = Some(anyhow::anyhow!("SSH run worker panicked"));
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
        mut handle,
        stdin,
        stdout,
        stderr,
    } = piped;
    let mut child_stdin: Box<dyn Write + Send> = Box::new(stdin);
    let mut child_stdout: Box<dyn Read + Send> = Box::new(stdout);
    let Some(stderr) = stderr else {
        bail!("SSH run spawn lost stderr");
    };
    let mut child_stderr: Box<dyn Read + Send> = Box::new(stderr);

    let mut failed: Option<anyhow::Error> = None;
    let mut out_closed = false;
    let mut err_closed = false;
    let peer_done = AtomicBool::new(false);
    let mut exit_code: Option<i64> = None;
    std::thread::scope(|scope| {
        // Owned by the worker, not borrowed: thread end drops the
        // handle, which closes the child stdin. A borrowed handle
        // would outlive the worker in this frame and the child would
        // never observe EOF. Shared flags ride in by reference.
        let peer_done_in = &peer_done;
        let mut worker_in = Some(scope.spawn(move || {
            let mut buffer = [0u8; CHUNK];
            loop {
                if cancel.load(Ordering::SeqCst) || peer_done_in.load(Ordering::SeqCst) {
                    break;
                }
                match read_pipe(&reader, backend.as_ref(), &mut buffer) {
                    Err(err) => bail!("SSH run input pipe read failed: {err}"),
                    Ok(None) => continue,
                    // DSL EOF closes the child stdin: the child observes
                    // EOF then. Not peer-done: the child may still
                    // produce output after its input ends.
                    Ok(Some(0)) => break,
                    Ok(Some(count)) => {
                        if child_stdin.write_all(&buffer[..count]).is_err() {
                            break;
                        }
                        if child_stdin.flush().is_err() {
                            break;
                        }
                    }
                }
            }
            Ok(())
        }));
        let mut worker_out =
            Some(scope.spawn(|| pump_fd_to_pipe(&mut child_stdout, &out_writer, "stdout")));
        let mut worker_err =
            Some(scope.spawn(|| pump_fd_to_pipe(&mut child_stderr, &err_writer, "stderr")));
        loop {
            // Child exit first, ahead of worker reaping below: the
            // reporter (the SSH wire exit-status) must queue before the
            // EOF closes trigger the pump EOF, or a fast child wins the
            // race against it. Never poll past the first observed exit:
            // the wait handle is spent after reaping.
            if exit_code.is_none() {
                match handle.try_wait() {
                    Ok(Some(status)) => {
                        peer_done.store(true, Ordering::SeqCst);
                        match status.code() {
                            Some(code) if code >= 0 => {
                                exit_code = Some(i64::from(code));
                                if let Some(report) = on_exit {
                                    report(code as u32);
                                }
                            }
                            // Signaled: no meaningful code to report.
                            _ => exit_code = Some(-1),
                        }
                    }
                    Ok(None) => {}
                    Err(_) => {
                        peer_done.store(true, Ordering::SeqCst);
                        exit_code = Some(-1);
                    }
                }
            }
            reap(&mut worker_in, &mut failed);
            let out_was_live = worker_out.is_some();
            reap(&mut worker_out, &mut failed);
            let err_was_live = worker_err.is_some();
            reap(&mut worker_err, &mut failed);
            // Deferred closes: each DSL pipe closes only after the exit
            // is observed, so the report above always precedes them.
            if out_was_live && worker_out.is_none() && !out_closed && exit_code.is_some() {
                out_closed = true;
                let _ = cx.close_pipe(out_pipe);
            }
            if err_was_live && worker_err.is_none() && !err_closed && exit_code.is_some() {
                err_closed = true;
                let _ = cx.close_pipe(err_pipe);
            }
            if cx.is_cancelled() {
                cancel.store(true, Ordering::SeqCst);
                let _ = handle.kill();
            }
            if worker_in.is_none() && worker_out.is_none() && worker_err.is_none() {
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
            // Workers drained without an observed exit: reap the
            // authoritative status blocking.
            match handle.wait() {
                Ok(status) => match status.code() {
                    Some(code) if code >= 0 => {
                        if let Some(report) = on_exit {
                            report(code as u32);
                        }
                        Ok(i64::from(code))
                    }
                    _ => Ok(-1),
                },
                Err(err) => bail!("SSH run reaped with error: {err}"),
            }
        }
    }
}
