use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::{Result, bail};
use oxdock_fs::GuardedPath;
use oxdock_parser::{Arg, Step, StepKind, guard_option_allows};
use oxdock_process::{BackgroundHandle, ProcessManager, SharedInput};

/// Create an ExitStatus from a raw exit code. Cross-platform.
fn exit_status_from_code(code: i32) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }
    #[cfg(windows)]
    {
        ExitStatus::from_raw(code as u32)
    }
}

use super::handlers;
use super::io::{SlidingWindow, StreamHandle};
use super::state::{ExecState, ScopeSnapshot};

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

pub struct StepCtx<'a, P: ProcessManager> {
    pub(super) state: &'a mut ExecState<P>,
    pub(super) process: &'a mut P,
    pub(super) snapshot_root: GuardedPath,
    pub(super) build_context: GuardedPath,
    pub(super) stdin: Option<SharedInput>,
    pub(super) expose_stdin: bool,
    pub(super) out: Option<StreamHandle>,
    pub(super) err: Option<StreamHandle>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn execute_steps<P: ProcessManager>(
    state: &mut ExecState<P>,
    process: &mut P,
    steps: &[Step],
    stdin: Option<SharedInput>,
    expose_stdin: bool,
    out: Option<StreamHandle>,
    err: Option<StreamHandle>,
    wait_at_end: bool,
) -> Result<()> {
    let generation = allocate_assert_generation();
    execute_steps_inner(
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
    Ok(())
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
    stdin: Option<SharedInput>,
    expose_stdin: bool,
    out: Option<StreamHandle>,
    err: Option<StreamHandle>,
) -> Result<()> {
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
    match cmd {
        StepKind::Run(arg) => {
            let cmd = super::args::resolve_arg(arg, &mut cx)?;
            let cmd = super::args::expand_dsl_vars(&cmd, cx.state);
            handlers::run(&mut cx, idx, &cmd)
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
        StepKind::WithIo { bindings, cmd } => {
            handlers::with_io(&mut cx, generation, idx, bindings, cmd)
        }
        StepKind::WithIoBlock { .. } => {
            bail!("WITH_IO block should have been expanded during parsing")
        }
        StepKind::Exit(code) => handlers::exit(&mut cx, *code),
        StepKind::For {
            key_var,
            var,
            in_expr,
            body,
        } => handlers::for_loop(&mut cx, key_var.as_deref(), var, in_expr, body),
        StepKind::If {
            cond,
            then_body,
            else_ifs,
            else_body,
        } => handlers::if_then(&mut cx, cond, then_body, else_ifs, else_body),
        StepKind::Assign { var, expr } => handlers::assign(&mut cx, var, expr),
        StepKind::AssignAsync { var, body } => handlers::dispatch_assign_async(var, body, &mut cx),
        StepKind::Await { var } => handlers::dispatch_await(var, &mut cx),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_steps_inner<P: ProcessManager>(
    state: &mut ExecState<P>,
    process: &mut P,
    generation: usize,
    steps: &[Step],
    stdin: Option<SharedInput>,
    expose_stdin: bool,
    out: Option<StreamHandle>,
    err: Option<StreamHandle>,
    wait_at_end: bool,
) -> Result<()> {
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
                state.scope_stack.push(ScopeSnapshot {
                    cwd: state.cwd.clone(),
                    root: state.fs.root().clone(),
                    envs: Arc::clone(&state.envs),
                });
            }
        }

        let should_run = guard_option_allows(step.guard.as_ref(), &state.envs);
        let step_result: Result<()> = if !should_run {
            Ok(())
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
                    handlers::run(&mut cx, idx, &cmd)
                }
                StepKind::Echo(arg) => {
                    let msg = super::args::resolve_arg(arg, &mut cx)?;
                    handlers::echo(&mut cx, &msg)
                }
                StepKind::AsyncBlock { .. } => handlers::dispatch_async_block(&step.kind, &mut cx),
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
                StepKind::WithIo { bindings, cmd } => {
                    handlers::with_io(&mut cx, generation, idx, bindings, cmd)
                }
                StepKind::WithIoBlock { .. } => {
                    bail!("WITH_IO block should have been expanded during parsing")
                }
                StepKind::Exit(code) => handlers::exit(&mut cx, *code),
                StepKind::For {
                    key_var,
                    var,
                    in_expr,
                    body,
                } => handlers::for_loop(&mut cx, key_var.as_deref(), var, in_expr, body),
                StepKind::If {
                    cond,
                    then_body,
                    else_ifs,
                    else_body,
                } => handlers::if_then(&mut cx, cond, then_body, else_ifs, else_body),
                StepKind::Assign { var, expr } => handlers::assign(&mut cx, var, expr),
                StepKind::AssignAsync { var, body } => handlers::dispatch_assign_async(var, body, &mut cx),
                StepKind::Await { var } => handlers::dispatch_await(var, &mut cx),
            }
        };

        let restore_result = restore_scopes(state, step.scope_exit);
        step_result?;
        restore_result?;
    }

    // Poll both bg_children and named_tasks at end-of-pipeline
    let has_bg = !state.bg_children.is_empty();
    let has_named = !state
        .named_tasks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty();
    if wait_at_end && (has_bg || has_named) {
        loop {
            let mut failed_status: Option<anyhow::Error> = None;

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

            // 2. Poll un-awaited named tasks
            if failed_status.is_none() {
                let mut named = state
                    .named_tasks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                named.retain(|id, task| match task.try_wait() {
                    Ok(Some(status)) => {
                        if !status.success() && failed_status.is_none() {
                            failed_status = Some(anyhow::anyhow!(
                                "named ASYNC task {id} exited with status {status}"
                            ));
                        }
                        false // remove completed task
                    }
                    Ok(None) => true, // retain running task
                    Err(e) => {
                        if failed_status.is_none() {
                            failed_status = Some(e);
                        }
                        false
                    }
                });
            }

            // 3. Fail-fast teardown
            if let Some(err) = failed_status {
                for survivor in state.bg_children.iter_mut() {
                    let _ = survivor.kill();
                }
                let mut named = state
                    .named_tasks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                for survivor in named.values_mut() {
                    let _ = survivor.kill();
                }
                state.bg_children.clear();
                named.clear();
                return Err(err);
            }

            let bg_empty = state.bg_children.is_empty();
            let named_empty = state
                .named_tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty();
            if bg_empty && named_empty {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    Ok(())
}

fn restore_scopes<P: ProcessManager>(state: &mut ExecState<P>, count: usize) -> Result<()> {
    for _ in 0..count {
        let snapshot = state
            .scope_stack
            .pop()
            .ok_or_else(|| anyhow::anyhow!("scope stack underflow during pop"))?;
        state.fs.set_root(&snapshot.root);
        state.cwd = snapshot.cwd;
        state.envs = snapshot.envs;
    }
    Ok(())
}
