use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Result, bail};
use oxdock_fs::GuardedPath;
use oxdock_parser::{Arg, Step, StepKind, Value, guard_option_allows};
use oxdock_process::{BackgroundHandle, CommandStdin, ProcessManager};

/// Create an ExitStatus from a raw exit code. Cross-platform.
fn exit_status_from_code(code: i32) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }
}

use super::capture::SpillBuffer;
use super::handlers;
use super::io::{SlidingWindow, StreamHandle};
use super::state::{ExecState, TaskEntry, TaskPhase};

/// A background handle wrapping a `std::thread::JoinHandle` for ASYNC blocks
/// that execute commands in a background thread.
pub(super) struct ThreadJoinHandle {
    join: Option<std::thread::JoinHandle<Result<()>>>,
    cancel_token: Arc<AtomicBool>,
    active_process: Arc<Mutex<Option<Box<dyn BackgroundHandle>>>>,
    /// Preserved error from the child thread, if any.
    thread_error: Option<anyhow::Error>,
}

impl ThreadJoinHandle {
    pub(super) fn new(
        join: std::thread::JoinHandle<Result<()>>,
        cancel_token: Arc<AtomicBool>,
        active_process: Arc<Mutex<Option<Box<dyn BackgroundHandle>>>>,
    ) -> Self {
        Self {
            join: Some(join),
            cancel_token,
            active_process,
            thread_error: None,
        }
    }

    /// Reap the thread if finished, preserving any error.
    fn reap(&mut self) {
        if self.join.is_none() {
            return;
        }
        let handle = self.join.take().unwrap();
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                self.thread_error = Some(e);
            }
            Err(panic) => {
                let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "thread panicked".to_string()
                };
                self.thread_error = Some(anyhow::anyhow!("{msg}"));
            }
        }
    }
}

impl BackgroundHandle for ThreadJoinHandle {
    fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        if let Some(join) = &self.join {
            if join.is_finished() {
                self.reap();
            } else {
                return Ok(None);
            }
        }
        if let Some(ref err) = self.thread_error {
            Err(anyhow::anyhow!("{err}"))
        } else {
            Ok(Some(exit_status_from_code(0)))
        }
    }

    fn kill(&mut self) -> Result<()> {
        // Signal cancellation
        self.cancel_token.store(true, Ordering::SeqCst);
        // Kill any active OS process to interrupt blocking wait
        if let Ok(mut guard) = self.active_process.lock()
            && let Some(ref mut proc) = *guard
        {
            let _ = proc.kill();
        }
        // Join the thread to ensure it completes before returning
        self.reap();
        Ok(())
    }

    fn wait(&mut self) -> Result<ExitStatus> {
        self.reap();
        if let Some(ref err) = self.thread_error {
            Err(anyhow::anyhow!("{err}"))
        } else {
            Ok(exit_status_from_code(0))
        }
    }
}

impl Drop for ThreadJoinHandle {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

/// Monotonically increasing generation counter for assert_windows key scoping.
/// Each execute_steps invocation gets a unique generation, preventing key
/// collisions between nested scopes (for_loop bodies, WithIo blocks).
static ASSERT_GENERATION: AtomicUsize = AtomicUsize::new(0);

/// Intra-thread control-flow signal (`BREAK`/`CONTINUE`/`RETURN`).
/// Produced by steps, consumed by the nearest loop (`Break`/`Continue`) or
/// `call_func` (`Return`). Anything reaching a thread boundary (`ASYNC`
/// spawn, `await` reaping) or the pipeline top becomes a step-numbered
/// error. `idx` is the 0-based index of the originating step in its own
/// body, so boundary errors can name it.
#[derive(Debug)]
pub(super) enum Flow {
    Done,
    Break { idx: usize },
    Continue { idx: usize },
    Return { idx: usize, value: Value },
}

pub(super) fn allocate_assert_generation() -> usize {
    ASSERT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Extract the AssertStdout needle from a StepKind, handling both top-level
/// and WITH_IO-wrapped variants.
fn extract_assert_stdout_needle(kind: &StepKind) -> Option<&Arg> {
    match kind {
        StepKind::AssertStdout(needle) => Some(needle),
        StepKind::WithIo { cmd, .. } => match cmd.as_ref() {
            StepKind::AssertStdout(needle) => Some(needle),
            _ => None,
        },
        _ => None,
    }
}

/// Pre-register `ASSERT_STDOUT` window observers so the tee writer can feed
/// them data before the step executes. Uses `args::resolve_arg_state` for
/// actual template expansion. Handles both top-level and WITH_IO-wrapped
/// assertions via `extract_assert_stdout_needle`.
pub(super) fn pre_register_assertions<P: ProcessManager>(
    state: &mut ExecState<P>,
    steps: &[Step],
    generation: usize,
) -> Result<()> {
    let mut windows = match state.assert_windows.lock() {
        Ok(guard) => guard,
        Err(_) => bail!("assert_windows poisoned"),
    };
    for (idx, step) in steps.iter().enumerate() {
        if let Some(arg) = extract_assert_stdout_needle(&step.kind) {
            let resolved = super::args::resolve_arg_state(arg, state)?;
            windows.insert((generation, idx), SlidingWindow::new(resolved.into_bytes()));
        }
    }
    Ok(())
}

/// After an environment mutation (ENV or INHERIT_ENV), re-expand all assertion
/// window needles for the current generation to reflect new env values.
/// Preserves ring buffer history via `update_needle`. Handles both top-level
/// and WITH_IO-wrapped assertions.
#[allow(clippy::collapsible_if)]
pub(super) fn sync_iteration_assert_needles<P: ProcessManager>(
    state: &ExecState<P>,
    steps: &[Step],
    generation: usize,
) -> Result<()> {
    let mut windows = match state.assert_windows.lock() {
        Ok(guard) => guard,
        Err(_) => bail!("assert_windows poisoned"),
    };
    for (idx, step) in steps.iter().enumerate() {
        if let Some(arg) = extract_assert_stdout_needle(&step.kind) {
            if let Some(w) = windows.get_mut(&(generation, idx)) {
                let resolved = super::args::resolve_arg_state(arg, state)?;
                w.update_needle(resolved.into_bytes());
            }
        }
    }
    Ok(())
}

/// Per-step execution context handed to every command handler.
///
/// Output contract (load-bearing for `LET`-capture, pipes, and `ASSERT_STDOUT`):
/// handlers must emit stdout/stderr ONLY through `out`/`err` — via
/// `write_stdout` or `StreamHandle::to_stdout`/`to_stderr` — and never write
/// to host stdout directly. The step runner swaps these handles per context:
/// `LET $x: STRING = <command>` installs a spillable capture sink, `WITH_IO`
/// installs named-pipe endpoints, and the root installs the `ASSERT_STDOUT`
/// tee. A handler that bypasses its context handles silently breaks all three.
pub struct StepCtx<'a, P: ProcessManager> {
    pub(super) state: &'a mut ExecState<P>,
    pub(super) process: &'a mut P,
    pub(super) snapshot_root: GuardedPath,
    pub(super) build_context: GuardedPath,
    pub(super) stdin: CommandStdin,
    pub(super) expose_stdin: bool,
    pub(super) out: Option<StreamHandle>,
    pub(super) err: Option<StreamHandle>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn execute_steps<P: ProcessManager>(
    state: &mut ExecState<P>,
    process: &mut P,
    steps: &[Step],
    stdin: CommandStdin,
    expose_stdin: bool,
    out: Option<StreamHandle>,
    err: Option<StreamHandle>,
    wait_at_end: bool,
) -> Result<Flow> {
    let generation = allocate_assert_generation();
    let flow = execute_steps_inner(
        state,
        process,
        generation,
        steps,
        stdin,
        expose_stdin,
        out,
        err,
        wait_at_end,
    )?;
    // Cleanup: remove all windows for this generation
    let mut windows = match state.assert_windows.lock() {
        Ok(guard) => guard,
        Err(_) => bail!("assert_windows poisoned"),
    };
    windows.retain(|(g, _), _| *g != generation);
    Ok(flow)
}

/// Execute a single step with an explicit generation and index.
/// Used by `with_io` to preserve the parent step's index for assertion window keys.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_single_step_with_generation<P: ProcessManager>(
    state: &mut ExecState<P>,
    process: &mut P,
    cmd: &StepKind,
    generation: usize,
    idx: usize,
    stdin: CommandStdin,
    expose_stdin: bool,
    out: Option<StreamHandle>,
    err: Option<StreamHandle>,
) -> Result<Flow> {
    let snapshot_root = state.fs.root().clone();
    let build_context = state.fs.build_context().clone();

    let mut cx = StepCtx {
        state,
        process,
        snapshot_root,
        build_context,
        stdin,
        expose_stdin,
        out,
        err,
    };
    // Compound steps (loops, functions, scoped wrappers) participate in
    // Flow and dispatch through the Flow path; every other variant runs
    // the leaf pipeline below and yields Done.
    match cmd {
        StepKind::FuncDef { .. }
        | StepKind::Call { .. }
        | StepKind::Return { .. }
        | StepKind::While { .. }
        | StepKind::Break
        | StepKind::Continue
        | StepKind::For { .. }
        | StepKind::If { .. }
        | StepKind::Timeout { .. }
        | StepKind::WithIo { .. }
        | StepKind::AssignCapture { .. } => {
            return dispatch_flow_step(cmd, &mut cx, generation, idx);
        }
        _ => {}
    }
    match cmd {
        StepKind::Run(arg) => {
            let cmd = super::args::resolve_arg(arg, &mut cx)?;
            let cmd = super::args::expand_dsl_vars(&cmd, cx.state);
            handlers::run(&mut cx, idx, &cmd)
        }
        StepKind::RunExec { argv } => {
            let resolved = handlers::resolve_run_exec_argv(argv, &mut cx)?;
            handlers::run_argv(&mut cx, idx, &resolved)
        }
        StepKind::Echo(arg) => {
            let msg = super::args::resolve_arg(arg, &mut cx)?;
            handlers::echo(&mut cx, &msg)
        }
        StepKind::AsyncBlock { .. } => handlers::dispatch_async_block(cmd, &mut cx),
        StepKind::Workdir(arg) => {
            let path = super::args::resolve_arg(arg, &mut cx)?;
            handlers::workdir(&mut cx, idx, &path)
        }
        StepKind::Workspace(target) => handlers::workspace(&mut cx, target),
        StepKind::Env { key, value } => {
            let resolved = super::args::resolve_arg(value, &mut cx)?;
            handlers::env(&mut cx, key, &resolved)
        }
        StepKind::InheritEnv { keys } => {
            handlers::inherit_env(&mut cx, keys)?;
            sync_iteration_assert_needles(
                cx.state,
                &[Step {
                    guard: None,
                    kind: cmd.clone(),
                    scope_enter: 0,
                    scope_exit: 0,
                }],
                generation,
            )?;
            Ok(())
        }
        StepKind::Copy {
            from_current_workspace,
            from,
            to,
        } => {
            let from_resolved = super::args::resolve_arg(from, &mut cx)?;
            let to_resolved = super::args::resolve_arg(to, &mut cx)?;
            handlers::copy(
                &mut cx,
                idx,
                *from_current_workspace,
                &from_resolved,
                &to_resolved,
            )
        }
        StepKind::CopyGit {
            rev,
            from,
            to,
            include_dirty,
        } => {
            let rev_resolved = super::args::resolve_arg(rev, &mut cx)?;
            let from_resolved = super::args::resolve_arg(from, &mut cx)?;
            let to_resolved = super::args::resolve_arg(to, &mut cx)?;
            handlers::copy_git(
                &mut cx,
                idx,
                &rev_resolved,
                &from_resolved,
                &to_resolved,
                *include_dirty,
            )
        }
        StepKind::HashSha256 { path } => {
            let path_resolved = super::args::resolve_arg(path, &mut cx)?;
            handlers::hash_sha256(&mut cx, idx, &path_resolved)
        }
        StepKind::Symlink { from, to } => {
            let from_resolved = super::args::resolve_arg(from, &mut cx)?;
            let to_resolved = super::args::resolve_arg(to, &mut cx)?;
            handlers::symlink(&mut cx, idx, &from_resolved, &to_resolved)
        }
        StepKind::Mkdir(arg) => {
            let path = super::args::resolve_arg(arg, &mut cx)?;
            handlers::mkdir(&mut cx, idx, &path)
        }
        StepKind::Ls(arg) => {
            let resolved = super::args::resolve_arg_opt(arg, &mut cx)?;
            handlers::ls(&mut cx, idx, &resolved)
        }
        StepKind::Cwd => handlers::cwd(&mut cx, idx),
        StepKind::Read(arg) => {
            let resolved = super::args::resolve_arg_opt(arg, &mut cx)?;
            handlers::read(&mut cx, idx, &resolved)
        }
        StepKind::ReadLine { var } => handlers::read_line(&mut cx, idx, var),
        StepKind::Write { path, contents } => {
            let path_resolved = super::args::resolve_arg(path, &mut cx)?;
            let contents_resolved = super::args::resolve_arg_opt(contents, &mut cx)?;
            handlers::write(&mut cx, idx, &path_resolved, contents_resolved.as_deref())
        }
        StepKind::Append { path, contents } => {
            let path_resolved = super::args::resolve_arg(path, &mut cx)?;
            let contents_resolved = super::args::resolve_arg_opt(contents, &mut cx)?;
            handlers::append(&mut cx, idx, &path_resolved, contents_resolved.as_deref())
        }
        StepKind::Expand { path, overrides } => {
            let path_resolved = super::args::resolve_arg_opt(path, &mut cx)?;
            let overrides_resolved = super::args::resolve_overrides(overrides, &mut cx)?;
            handlers::replace(&mut cx, idx, &path_resolved, &overrides_resolved)
        }
        StepKind::AssertFile {
            hash,
            path,
            contents,
        } => {
            let path_resolved = super::args::resolve_arg(path, &mut cx)?;
            let contents_resolved = super::args::resolve_arg_opt(contents, &mut cx)?;
            handlers::assert_file(
                &mut cx,
                idx,
                hash,
                &path_resolved,
                contents_resolved.as_deref(),
            )
        }
        StepKind::AssertDir(arg) => {
            let path = super::args::resolve_arg(arg, &mut cx)?;
            handlers::assert_dir(&mut cx, idx, &path)
        }
        StepKind::AssertAbsent(arg) => {
            let path = super::args::resolve_arg(arg, &mut cx)?;
            handlers::assert_absent(&mut cx, idx, &path)
        }
        StepKind::AssertStdout(arg) => {
            let needle = super::args::resolve_arg(arg, &mut cx)?;
            handlers::assert_stdout(&mut cx, idx, generation, idx, &needle)
        }
        StepKind::WithIoBlock { .. } => {
            bail!("WITH_IO block should have been expanded during parsing")
        }
        StepKind::Exit(code) => {
            let code = super::args::resolve_arg_as_int(code, &mut cx)?;
            handlers::exit(&mut cx, code)
        }
        StepKind::Assign {
            var,
            decl_type,
            expr,
        } => handlers::assign(&mut cx, var, *decl_type, expr),
        StepKind::Set { var, expr } => handlers::set_var_value(&mut cx, var, expr),
        StepKind::AssignAsync {
            var,
            decl_type,
            body,
        } => handlers::dispatch_assign_async(var, *decl_type, body, &mut cx),
        StepKind::Await { var } => handlers::dispatch_await(var, &mut cx),
        StepKind::AwaitCapture {
            out_var,
            out_type,
            task_var,
        } => handlers::dispatch_await_capture(out_var, *out_type, task_var, &mut cx),
        StepKind::Cancel { var } => handlers::dispatch_cancel(var, &mut cx),
        StepKind::Sleep { duration } => {
            let duration = super::args::resolve_arg_as_duration(duration, &mut cx)?;
            handlers::sleep(&mut cx, idx, &duration)
        }
        StepKind::FuncDef { .. }
        | StepKind::Call { .. }
        | StepKind::Return { .. }
        | StepKind::While { .. }
        | StepKind::Break
        | StepKind::Continue
        | StepKind::For { .. }
        | StepKind::If { .. }
        | StepKind::Timeout { .. }
        | StepKind::WithIo { .. }
        | StepKind::AssignCapture { .. } => {
            unreachable!("compound steps dispatch before this match")
        }
    }?;
    Ok(Flow::Done)
}

#[allow(clippy::too_many_arguments)]
fn execute_steps_inner<P: ProcessManager>(
    state: &mut ExecState<P>,
    process: &mut P,
    generation: usize,
    steps: &[Step],
    stdin: CommandStdin,
    expose_stdin: bool,
    out: Option<StreamHandle>,
    err: Option<StreamHandle>,
    wait_at_end: bool,
) -> Result<Flow> {
    let snapshot_root = state.fs.root().clone();
    let build_context = state.fs.build_context().clone();

    // Pre-register assertion windows for this generation
    pre_register_assertions(state, steps, generation)?;

    for (idx, step) in steps.iter().enumerate() {
        // Check for cancellation before each step
        if state.cancel_token.load(Ordering::SeqCst) {
            bail!("ASYNC task cancelled");
        }
        if step.scope_enter > 0 {
            for _ in 0..step.scope_enter {
                state.push_scope();
            }
        }

        let should_run = guard_option_allows(step.guard.as_ref(), &state.envs);
        let flow_result: Result<Flow> = if !should_run {
            Ok(Flow::Done)
        } else {
            let mut cx = StepCtx {
                state,
                process,
                snapshot_root: snapshot_root.clone(),
                build_context: build_context.clone(),
                stdin: stdin.clone(),
                expose_stdin,
                out: out.clone(),
                err: err.clone(),
            };
            // Function/loop control steps dispatch through the Flow path;
            // every other variant runs the leaf pipeline and yields Done.
            let flow_result: Result<Flow> = match &step.kind {
                StepKind::FuncDef { .. }
                | StepKind::Call { .. }
                | StepKind::Return { .. }
                | StepKind::While { .. }
                | StepKind::Break
                | StepKind::Continue
                | StepKind::For { .. }
                | StepKind::If { .. }
                | StepKind::Timeout { .. }
                | StepKind::WithIo { .. }
                | StepKind::AssignCapture { .. } => {
                    dispatch_flow_step(&step.kind, &mut cx, generation, idx)
                }
                _ => {
                    match &step.kind {
                        StepKind::InheritEnv { keys } => {
                            handlers::inherit_env(&mut cx, keys)?;
                            sync_iteration_assert_needles(cx.state, steps, generation)?;
                            Ok(())
                        }
                        StepKind::Workdir(arg) => {
                            let path = super::args::resolve_arg(arg, &mut cx)?;
                            handlers::workdir(&mut cx, idx, &path)
                        }
                        StepKind::Workspace(target) => handlers::workspace(&mut cx, target),
                        StepKind::Env { key, value } => {
                            let resolved = super::args::resolve_arg(value, &mut cx)?;
                            handlers::env(&mut cx, key, &resolved)?;
                            sync_iteration_assert_needles(cx.state, steps, generation)?;
                            Ok(())
                        }
                        StepKind::Run(arg) => {
                            let cmd = super::args::resolve_arg(arg, &mut cx)?;
                            let cmd = super::args::expand_dsl_vars(&cmd, cx.state);
                            handlers::run(&mut cx, idx, &cmd)
                        }
                        StepKind::RunExec { argv } => {
                            let resolved = handlers::resolve_run_exec_argv(argv, &mut cx)?;
                            handlers::run_argv(&mut cx, idx, &resolved)
                        }
                        StepKind::Echo(arg) => {
                            let msg = super::args::resolve_arg(arg, &mut cx)?;
                            handlers::echo(&mut cx, &msg)
                        }
                        StepKind::AsyncBlock { .. } => {
                            handlers::dispatch_async_block(&step.kind, &mut cx)
                        }
                        StepKind::Copy {
                            from_current_workspace,
                            from,
                            to,
                        } => {
                            let from_resolved = super::args::resolve_arg(from, &mut cx)?;
                            let to_resolved = super::args::resolve_arg(to, &mut cx)?;
                            handlers::copy(
                                &mut cx,
                                idx,
                                *from_current_workspace,
                                &from_resolved,
                                &to_resolved,
                            )
                        }
                        StepKind::CopyGit {
                            rev,
                            from,
                            to,
                            include_dirty,
                        } => {
                            let rev_resolved = super::args::resolve_arg(rev, &mut cx)?;
                            let from_resolved = super::args::resolve_arg(from, &mut cx)?;
                            let to_resolved = super::args::resolve_arg(to, &mut cx)?;
                            handlers::copy_git(
                                &mut cx,
                                idx,
                                &rev_resolved,
                                &from_resolved,
                                &to_resolved,
                                *include_dirty,
                            )
                        }
                        StepKind::HashSha256 { path } => {
                            let path_resolved = super::args::resolve_arg(path, &mut cx)?;
                            handlers::hash_sha256(&mut cx, idx, &path_resolved)
                        }
                        StepKind::Symlink { from, to } => {
                            let from_resolved = super::args::resolve_arg(from, &mut cx)?;
                            let to_resolved = super::args::resolve_arg(to, &mut cx)?;
                            handlers::symlink(&mut cx, idx, &from_resolved, &to_resolved)
                        }
                        StepKind::Mkdir(arg) => {
                            let path = super::args::resolve_arg(arg, &mut cx)?;
                            handlers::mkdir(&mut cx, idx, &path)
                        }
                        StepKind::Ls(arg) => {
                            let resolved = super::args::resolve_arg_opt(arg, &mut cx)?;
                            handlers::ls(&mut cx, idx, &resolved)
                        }
                        StepKind::Cwd => handlers::cwd(&mut cx, idx),
                        StepKind::Read(arg) => {
                            let resolved = super::args::resolve_arg_opt(arg, &mut cx)?;
                            handlers::read(&mut cx, idx, &resolved)
                        }
                        StepKind::ReadLine { var } => handlers::read_line(&mut cx, idx, var),
                        StepKind::Write { path, contents } => {
                            let path_resolved = super::args::resolve_arg(path, &mut cx)?;
                            let contents_resolved =
                                super::args::resolve_arg_opt(contents, &mut cx)?;
                            handlers::write(
                                &mut cx,
                                idx,
                                &path_resolved,
                                contents_resolved.as_deref(),
                            )
                        }
                        StepKind::Append { path, contents } => {
                            let path_resolved = super::args::resolve_arg(path, &mut cx)?;
                            let contents_resolved =
                                super::args::resolve_arg_opt(contents, &mut cx)?;
                            handlers::append(
                                &mut cx,
                                idx,
                                &path_resolved,
                                contents_resolved.as_deref(),
                            )
                        }
                        StepKind::Expand { path, overrides } => {
                            let path_resolved = super::args::resolve_arg_opt(path, &mut cx)?;
                            let overrides_resolved =
                                super::args::resolve_overrides(overrides, &mut cx)?;
                            handlers::replace(&mut cx, idx, &path_resolved, &overrides_resolved)
                        }
                        StepKind::AssertFile {
                            hash,
                            path,
                            contents,
                        } => {
                            let path_resolved = super::args::resolve_arg(path, &mut cx)?;
                            let contents_resolved =
                                super::args::resolve_arg_opt(contents, &mut cx)?;
                            handlers::assert_file(
                                &mut cx,
                                idx,
                                hash,
                                &path_resolved,
                                contents_resolved.as_deref(),
                            )
                        }
                        StepKind::AssertDir(arg) => {
                            let path = super::args::resolve_arg(arg, &mut cx)?;
                            handlers::assert_dir(&mut cx, idx, &path)
                        }
                        StepKind::AssertAbsent(arg) => {
                            let path = super::args::resolve_arg(arg, &mut cx)?;
                            handlers::assert_absent(&mut cx, idx, &path)
                        }
                        StepKind::AssertStdout(arg) => {
                            let needle = super::args::resolve_arg(arg, &mut cx)?;
                            handlers::assert_stdout(&mut cx, idx, generation, idx, &needle)
                        }
                        StepKind::WithIoBlock { .. } => {
                            bail!("WITH_IO block should have been expanded during parsing")
                        }
                        StepKind::Exit(code) => {
                            let code = super::args::resolve_arg_as_int(code, &mut cx)?;
                            handlers::exit(&mut cx, code)
                        }
                        StepKind::Assign {
                            var,
                            decl_type,
                            expr,
                        } => handlers::assign(&mut cx, var, *decl_type, expr),
                        StepKind::Set { var, expr } => handlers::set_var_value(&mut cx, var, expr),
                        StepKind::AssignAsync {
                            var,
                            decl_type,
                            body,
                        } => handlers::dispatch_assign_async(var, *decl_type, body, &mut cx),
                        StepKind::Await { var } => handlers::dispatch_await(var, &mut cx),
                        StepKind::AwaitCapture {
                            out_var,
                            out_type,
                            task_var,
                        } => {
                            handlers::dispatch_await_capture(out_var, *out_type, task_var, &mut cx)
                        }
                        StepKind::Cancel { var } => handlers::dispatch_cancel(var, &mut cx),
                        StepKind::Sleep { duration } => {
                            let duration = super::args::resolve_arg_as_duration(duration, &mut cx)?;
                            handlers::sleep(&mut cx, idx, &duration)
                        }
                        StepKind::FuncDef { .. }
                        | StepKind::Call { .. }
                        | StepKind::Return { .. }
                        | StepKind::While { .. }
                        | StepKind::Break
                        | StepKind::Continue
                        | StepKind::For { .. }
                        | StepKind::If { .. }
                        | StepKind::Timeout { .. }
                        | StepKind::WithIo { .. }
                        | StepKind::AssignCapture { .. } => {
                            unreachable!("compound steps dispatch in the outer match")
                        }
                    }?;
                    Ok(Flow::Done)
                }
            };
            flow_result
        };

        let restore_result = restore_scopes(state, step.scope_exit);
        // Keeper expiry: drop spawn-time pins whose final producer step
        // just completed, so later consumer steps in the same task observe
        // EOF. Gated on slice identity, so nested bodies executing through
        // this same loop never discharge the worker's top-level map.
        let expiry_drained = if let Some(expiry) = state.keeper_expiry.as_mut() {
            expiry.expire_step(steps, idx)
        } else {
            false
        };
        if expiry_drained {
            state.keeper_expiry = None;
        }
        let flow = flow_result?;
        restore_result?;
        match flow {
            Flow::Done => {}
            Flow::Break { .. } | Flow::Continue { .. } | Flow::Return { .. } => {
                return Ok(flow);
            }
        }
    }

    // Poll anonymous background handles at end-of-pipeline. The shared
    // named_tasks entries are reaped only by the root context: task threads
    // must never block on sibling tasks, which may depend on this thread
    // via AWAIT (three-way deadlock). Un-awaited named tasks are still
    // reaped by the root end-poll, and explicitly awaited tasks join via
    // AWAIT. Entries are retained as Cancelled/Completed tombstones so
    // later AWAIT/CANCEL report precise errors.
    let reap_named = !state.inside_async;
    let has_bg = !state.bg_children.is_empty();
    let named_pending = |state: &ExecState<P>| {
        reap_named
            && state
                .named_tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .any(|entry| !entry.state.lock().unwrap_or_else(|e| e.into_inner()).reaped)
    };
    let has_named = named_pending(state);
    if wait_at_end && (has_bg || has_named) {
        loop {
            let mut failed_status: Option<anyhow::Error> = None;

            // Cancellation (deadline watcher or parent teardown) must break
            // the poll loop: without this, a stuck background handle would
            // hang the reaper forever. Flows into the shared fail-fast
            // teardown below.
            if failed_status.is_none() && state.cancel_token.load(Ordering::SeqCst) {
                failed_status = Some(anyhow::anyhow!("ASYNC task cancelled"));
            }

            // 1. Poll anonymous background handles
            let mut i = 0;
            while i < state.bg_children.len() {
                match state.bg_children[i].try_wait() {
                    Ok(Some(status)) => {
                        if !status.success() && failed_status.is_none() {
                            failed_status =
                                Some(anyhow::anyhow!("ASYNC process exited with status {status}"));
                            break;
                        }
                        state.bg_children.swap_remove(i);
                    }
                    Ok(None) => {
                        i += 1;
                    }
                    Err(e) => {
                        if failed_status.is_none() {
                            failed_status = Some(e);
                        }
                        break;
                    }
                }
            }

            // 2. Poll un-awaited named tasks (root context only). Each entry
            // is probed under a short entry lock; terminal entries are
            // retained as tombstones, never removed.
            if failed_status.is_none() && reap_named {
                let entries: Vec<(u64, Arc<TaskEntry>)> = {
                    let named = state.named_tasks.lock().unwrap_or_else(|e| e.into_inner());
                    named
                        .iter()
                        .map(|(id, entry)| (*id, Arc::clone(entry)))
                        .collect()
                };
                for (id, entry) in &entries {
                    enum Poll {
                        Pending,
                        CompletedOk { sink: Option<Arc<SpillBuffer>> },
                        CompletedErr(anyhow::Error),
                    }
                    let poll = {
                        let mut guard = entry.state.lock().unwrap_or_else(|e| e.into_inner());
                        match guard.phase {
                            TaskPhase::Running | TaskPhase::Awaiting => {
                                // Only take the sink for tasks that were never
                                // awaited (`Running`): an `Awaiting` entry has
                                // an awaiter that owns output handling.
                                let take_sink = matches!(guard.phase, TaskPhase::Running);
                                match guard.handle.as_mut() {
                                    Some(handle) => match handle.try_wait() {
                                        Ok(Some(status)) => {
                                            let _ = guard.handle.take();
                                            guard.phase = TaskPhase::Completed;
                                            let sink =
                                                if take_sink { guard.sink.take() } else { None };
                                            if status.success() {
                                                Poll::CompletedOk { sink }
                                            } else {
                                                Poll::CompletedErr(anyhow::anyhow!(
                                                    "named ASYNC task {id} exited with status {status}"
                                                ))
                                            }
                                        }
                                        Ok(None) => Poll::Pending,
                                        Err(e) => {
                                            let _ = guard.handle.take();
                                            guard.phase = TaskPhase::Completed;
                                            Poll::CompletedErr(e)
                                        }
                                    },
                                    // Handle taken by a concurrent CANCEL/AWAIT
                                    // teardown; the barrier below rendezvouses.
                                    None => Poll::Pending,
                                }
                            }
                            TaskPhase::Cancelled | TaskPhase::Completed => Poll::Pending,
                        }
                    };
                    match poll {
                        Poll::Pending => {}
                        Poll::CompletedOk { sink } => {
                            entry.finish_teardown();
                            if let Some(sink) = sink
                                && let Err(e) = forward_task_sink(&sink, &out, *id)
                            {
                                if failed_status.is_none() {
                                    failed_status = Some(e);
                                }
                                break;
                            }
                        }
                        Poll::CompletedErr(e) => {
                            entry.finish_teardown();
                            if failed_status.is_none() {
                                failed_status = Some(e);
                            }
                            break;
                        }
                    }
                }
            }

            // 3. Fail-fast teardown (named entries are root-owned; task
            // threads only tear down their own anonymous children).
            // Handles are taken under short locks and killed outside every
            // lock; tombstones are retained.
            if let Some(err) = failed_status {
                for survivor in state.bg_children.iter_mut() {
                    let _ = survivor.kill();
                }
                if reap_named {
                    let entries: Vec<Arc<TaskEntry>> = {
                        let named = state.named_tasks.lock().unwrap_or_else(|e| e.into_inner());
                        named.values().cloned().collect()
                    };
                    let mut to_kill: Vec<(Arc<TaskEntry>, Box<dyn BackgroundHandle>)> = Vec::new();
                    for entry in &entries {
                        let mut guard = entry.state.lock().unwrap_or_else(|e| e.into_inner());
                        match guard.phase {
                            TaskPhase::Running | TaskPhase::Awaiting => {
                                guard.phase = TaskPhase::Cancelled;
                                if let Some(handle) = guard.handle.take() {
                                    to_kill.push((Arc::clone(entry), handle));
                                }
                            }
                            TaskPhase::Cancelled | TaskPhase::Completed => {}
                        }
                    }
                    for (entry, mut handle) in to_kill {
                        let _ = handle.kill();
                        entry.finish_teardown();
                    }
                }
                state.bg_children.clear();
                return Err(err);
            }

            let bg_empty = state.bg_children.is_empty();
            if bg_empty && !named_pending(state) {
                return Ok(Flow::Done);
            }
            // Rendezvous: a concurrent CANCEL/AWAIT on another thread may
            // own teardown of a Cancelled-but-unreaped entry. Wait for it
            // instead of spinning, so this thread never outruns the join.
            if reap_named {
                let unreaped: Vec<Arc<TaskEntry>> = {
                    let named = state.named_tasks.lock().unwrap_or_else(|e| e.into_inner());
                    named
                        .values()
                        .filter(|entry| {
                            let guard = entry.state.lock().unwrap_or_else(|e| e.into_inner());
                            matches!(guard.phase, TaskPhase::Cancelled) && !guard.reaped
                        })
                        .cloned()
                        .collect()
                };
                for entry in &unreaped {
                    entry.wait_reaped();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    Ok(Flow::Done)
}

/// Forward a finished named task's stdout sink to the parent stdout.
/// Used by end-poll reaping for tasks that completed without ever being
/// awaited, preserving the pre-capture behavior where their output was
/// already streamed to the parent writer.
fn forward_task_sink(sink: &Arc<SpillBuffer>, out: &Option<StreamHandle>, id: u64) -> Result<()> {
    let bytes = sink
        .drain_bytes()
        .map_err(|e| anyhow::anyhow!("named ASYNC task {id} output drain failed: {e}"))?;
    if !bytes.is_empty() {
        super::io::write_stdout(out.clone(), |writer| {
            writer
                .write_all(&bytes)
                .map_err(|e| anyhow::anyhow!("named ASYNC task {id} output forward failed: {e}"))?;
            Ok(())
        })?;
    }
    Ok(())
}

fn restore_scopes<P: ProcessManager>(state: &mut ExecState<P>, count: usize) -> Result<()> {
    for _ in 0..count {
        state.pop_scope()?;
    }
    Ok(())
}

/// Execute steps inside a fresh lexical scope (IF branches, TIMEOUT bodies).
/// Blocks scope everything (LET/ENV/WORKDIR/WORKSPACE); only pipes and
/// filesystem effects cross. Restores even when the body fails. Propagates
/// Flow signals (BREAK/CONTINUE/RETURN) to the caller after restoring.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_scoped_steps<P: ProcessManager>(
    state: &mut ExecState<P>,
    process: &mut P,
    steps: &[Step],
    stdin: CommandStdin,
    expose_stdin: bool,
    out: Option<StreamHandle>,
    err: Option<StreamHandle>,
    wait_at_end: bool,
) -> Result<Flow> {
    state.push_scope();
    let res = execute_steps(
        state,
        process,
        steps,
        stdin,
        expose_stdin,
        out,
        err,
        wait_at_end,
    );
    // Restore the scope even when the body failed, but never let an
    // unwinding failure mask the body's own error.
    let pop_res = state.pop_scope();
    match (res, pop_res) {
        (Ok(flow), Ok(())) => Ok(flow),
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e),
    }
}

/// Dispatch one compound step (loops, functions, scoped wrappers) through
/// the Flow path. Called with the caller's generation/idx so assertion
/// windows and error attribution match the leaf pipeline.
fn dispatch_flow_step<P: ProcessManager>(
    cmd: &StepKind,
    cx: &mut StepCtx<'_, P>,
    generation: usize,
    idx: usize,
) -> Result<Flow> {
    match cmd {
        StepKind::FuncDef { name, params, body } => {
            handlers::define_func(cx, name, params, body)?;
            Ok(Flow::Done)
        }
        StepKind::Call { name, args } => {
            let _ = handlers::call_func_value(cx, idx, name, args)?;
            Ok(Flow::Done)
        }
        StepKind::Return { expr } => handlers::handle_return(cx, idx, expr),
        StepKind::While { cond, body } => handlers::while_loop(cx, idx, cond, body),
        StepKind::Break => Ok(Flow::Break { idx }),
        StepKind::Continue => Ok(Flow::Continue { idx }),
        StepKind::For {
            key_var,
            key_type,
            var,
            var_type,
            in_expr,
            body,
        } => handlers::for_loop(
            cx,
            key_var.as_deref(),
            *key_type,
            var,
            *var_type,
            in_expr,
            body,
        ),
        StepKind::If {
            cond,
            then_body,
            else_ifs,
            else_body,
        } => handlers::if_then(cx, cond, then_body, else_ifs, else_body),
        StepKind::Timeout { duration, body } => {
            let duration = super::args::resolve_arg_as_duration(duration, cx)?;
            handlers::timeout(cx, idx, &duration, body)
        }
        StepKind::WithIo { bindings, cmd } => handlers::with_io(cx, generation, idx, bindings, cmd),
        StepKind::AssignCapture {
            var,
            decl_type,
            cmd,
        } => handlers::assign_capture(cx, generation, idx, var, *decl_type, cmd),
        _ => {
            unreachable!("dispatch_flow_step handles only compound steps")
        }
    }
}
