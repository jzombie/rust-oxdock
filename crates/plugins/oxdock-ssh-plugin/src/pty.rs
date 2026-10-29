//! Local terminal bridge: spawn a subprocess under a correctly-sized
//! pseudo-terminal and pump explicit DSL pipes through its master side.
//!
//! This is what makes fullscreen apps work through the proxy: an inner
//! `ssh` sizes its remote pty from its local stdin's terminal size, and
//! pipes have no size (0x0), so without a local pty every remote
//! fullscreen app lays out for a nonexistent screen. Spawning under a
//! local pty sized from the session cell and applying window changes
//! live closes the loop: the kernel SIGWINCHes the child, which
//! forwards the new size to the remote end itself.
//!
//! Thin adapter over `oxdock_pty`: the pump, drain policy, ConPTY
//! handshake, and teardown live in the shared crate (with its unit
//! tests); this file only routes script pipes, the session size cell,
//! and cancellation into a `PtyRequest`, then maps the outcome back
//! onto this caller's pipe-close and report contract.

use std::sync::Arc;

use anyhow::{Context, Result};
use oxdock_core::{StepCtx, Value};
use oxdock_fs::command_path;
use oxdock_process::{self, ProcessManager, read_pump_chunk};
use oxdock_pty::{PtyInput, PtyOutcome, PtyRequest, SizeSource, run_pty_session};

use crate::state::{SharedPtySize, snapshot_pty_size};

/// `oxdock_pty::SizeSource` over the session size cell: snapshots carry
/// the cell sequence, so an opener never has its explicit dimensions
/// clobbered by a stale default on the first poll.
struct SessionSize(SharedPtySize);

impl SizeSource for SessionSize {
    fn current(&self) -> oxdock_pty::PtySize {
        let snap = snapshot_pty_size(&self.0);
        let (rows, cols) = snap.size.effective();
        oxdock_pty::PtySize {
            rows,
            cols,
            seq: snap.seq,
        }
    }

    fn wait_changed(
        &self,
        after_seq: u64,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Option<oxdock_pty::PtySize> {
        let snap = crate::state::wait_size_changed(&self.0, after_seq, stop);
        if snap.seq == after_seq {
            return None;
        }
        let (rows, cols) = snap.size.effective();
        Some(oxdock_pty::PtySize {
            rows,
            cols,
            seq: snap.seq,
        })
    }

    fn poke(&self) {
        crate::state::poke_size(&self.0);
    }
}

/// Run `argv` under a local terminal sized from `initial` (live: outer
/// window-change requests land in `size_source` and are applied as
/// they arrive), pumping `in_pipe` into it and its output into
/// `out_pipe`. Blocks until the child exits and its output drains,
/// then returns the exit code. Cancellation kills the child.
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
    initial: crate::state::PtySize,
    size_source: &SharedPtySize,
    in_pipe: &Value,
    out_pipe: &Value,
    on_exit: Option<Arc<dyn Fn(u32) + Send + Sync>>,
) -> Result<i64> {
    let reader = cx
        .pipe_reader(in_pipe)
        .context("SSH pty cannot borrow the input pipe")?;
    let writer = cx
        .pipe_writer(out_pipe)
        .context("SSH pty cannot borrow the output pipe")?;
    // Interruptible chunk reads with the backend's keeper semantics,
    // unchanged from the inlined pump: `read_pump_chunk` carries the
    // timeout and wake contract, so the shared runner sees the exact
    // same byte stream and cancellation behavior.
    let read_chunk = Box::new(move |buf: &mut [u8]| read_pump_chunk(&reader, buf));
    let in_backend = cx.pipe_backend(in_pipe);
    let cancel_input = Box::new(move || {
        if let Some(inner) = in_backend.as_ref() {
            inner.cancel();
        }
    });
    cx.ensure_scratch_materialized()?;
    let staged = cx.env_snapshot();
    oxdock_process::check_spawn_anchors(&argv[0], &staged, false)?;
    let (rows, cols) = initial.effective();
    let size = Arc::new(SessionSize(Arc::clone(size_source)));
    // Win32-OpenSSH parity: every console session opens cleared and
    // homed so the child renders onto a known blank screen instead of
    // whatever the client held. Synchronously first, ahead of any
    // console output, exactly as before: the crate never clears.
    {
        let mut guard = writer
            .lock()
            .map_err(|_| anyhow::anyhow!("SSH pty output lock poisoned"))?;
        guard
            .write_all(b"\x1b[2J\x1b[H")
            .context("SSH pty session init write failed")?;
        guard.flush().context("SSH pty session init flush failed")?;
    }
    let hub = cx.cancel_hub();
    let outcome = run_pty_session(PtyRequest {
        argv: argv.to_vec(),
        env: staged.into_iter().collect(),
        cwd: command_path(cx.cwd()).to_string_lossy().into_owned(),
        initial_size: oxdock_pty::PtySize { rows, cols, seq: 0 },
        input: Some(PtyInput {
            read_chunk,
            cancel: cancel_input,
        }),
        output: writer,
        size_source: size,
        should_cancel: Box::new(|| hub.is_cancelled() || cx.is_cancelled()),
        on_exit,
        // Deferred close, unchanged from the inlined supervisor: the
        // output pipe closes only once its worker finished *and* the
        // exit is observed, so the wire status always precedes EOF.
        on_output_drained: Some(Box::new(|| {
            let _ = cx.close_pipe(out_pipe);
        })),
        func: "SSH pty",
    })?;
    // Post-return closes mirror the inlined supervisor exactly: the
    // drain callback above already handled the output close, and the
    // input pipe is only ever closed on cancellation. Cancellation
    // flags latch, so re-reading them here is exact, not racy.
    match outcome {
        PtyOutcome::Completed(code) => Ok(code),
        PtyOutcome::Cancelled => {
            let _ = cx.close_pipe(in_pipe);
            let _ = cx.close_pipe(out_pipe);
            anyhow::bail!("SSH pty run interrupted by cancellation")
        }
    }
}
