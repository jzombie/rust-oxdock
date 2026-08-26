use std::process::ExitStatus;
use std::sync::Arc;

use anyhow::{Result, bail};
use oxdock_fs::GuardedPath;
use oxdock_parser::{Step, StepKind, guard_option_allows};
use oxdock_process::{BackgroundHandle, ProcessManager, SharedInput};

use super::handlers;
use super::io::StreamHandle;
use super::state::{ExecState, ScopeSnapshot};

pub(super) struct StepCtx<'a, P: ProcessManager> {
    pub(super) state: &'a mut ExecState<P>,
    pub(super) process: &'a mut P,
    // Snapshot of the fs roots taken when `execute_steps` was entered. The
    // WORKSPACE step must restore these *entry* values even after a previous
    // WORKSPACE step mutated `state.fs` (behavior pinned by
    // `workspace_switches_between_snapshot_and_local`).
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
    let snapshot_root = state.fs.root().clone();
    let build_context = state.fs.build_context().clone();

    for (idx, step) in steps.iter().enumerate() {
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
                StepKind::InheritEnv { keys } => handlers::inherit_env(&mut cx, keys),
                StepKind::Workdir(path) => handlers::workdir(&mut cx, idx, path),
                StepKind::Workspace(target) => handlers::workspace(&mut cx, target),
                StepKind::Env { key, value } => handlers::env(&mut cx, key, value),
                StepKind::Run(cmd) => handlers::run(&mut cx, idx, cmd),
                StepKind::Echo(msg) => handlers::echo(&mut cx, msg),
                StepKind::RunBg(cmd) => handlers::run_bg(&mut cx, idx, cmd),
                StepKind::Copy {
                    from_current_workspace,
                    from,
                    to,
                } => handlers::copy(&mut cx, idx, *from_current_workspace, from, to),
                StepKind::CopyGit {
                    rev,
                    from,
                    to,
                    include_dirty,
                } => handlers::copy_git(&mut cx, idx, rev, from, to, *include_dirty),
                StepKind::HashSha256 { path } => handlers::hash_sha256(&mut cx, idx, path),
                StepKind::Symlink { from, to } => handlers::symlink(&mut cx, idx, from, to),
                StepKind::Mkdir(path) => handlers::mkdir(&mut cx, idx, path),
                StepKind::Ls(arg) => handlers::ls(&mut cx, idx, arg),
                StepKind::Cwd => handlers::cwd(&mut cx, idx),
                StepKind::Read(path_opt) => handlers::read(&mut cx, idx, path_opt),
                StepKind::Write { path, contents } => handlers::write(&mut cx, idx, path, contents),
                StepKind::AssertFile {
                    hash,
                    path,
                    contents,
                } => handlers::assert_file(&mut cx, idx, hash, path, contents),
                StepKind::AssertDir(path) => handlers::assert_dir(&mut cx, idx, path),
                StepKind::AssertAbsent(path) => handlers::assert_absent(&mut cx, idx, path),
                StepKind::AssertStdout(msg) => handlers::assert_stdout(&mut cx, idx, msg),
                StepKind::WithIo { bindings, cmd } => {
                    handlers::with_io(&mut cx, idx, bindings, cmd)
                }
                StepKind::WithIoBlock { .. } => {
                    bail!("WITH_IO block should have been expanded during parsing")
                }
                StepKind::Exit(code) => handlers::exit(&mut cx, *code),
            }
        };

        let restore_result = restore_scopes(state, step.scope_exit);
        step_result?;
        restore_result?;
        if let Some(status) = check_bg(&mut state.bg_children)? {
            if status.success() {
                return Ok(());
            } else {
                bail!("RUN_BG exited with status {}", status);
            }
        }
    }

    if wait_at_end && !state.bg_children.is_empty() {
        let mut first = state.bg_children.remove(0);
        let status = first.wait()?;
        // See `check_bg`: reap the remainder without joining pump threads.
        for child in state.bg_children.iter_mut() {
            if child.try_wait()?.is_none() {
                let _ = child.kill();
                let _ = child.try_wait();
            }
        }
        state.bg_children.clear();
        if status.success() {
            return Ok(());
        } else {
            bail!("RUN_BG exited with status {}", status);
        }
    }

    Ok(())
}

fn check_bg<H: BackgroundHandle>(bg: &mut Vec<H>) -> Result<Option<ExitStatus>> {
    let mut finished: Option<ExitStatus> = None;
    for child in bg.iter_mut() {
        if let Some(status) = child.try_wait()? {
            finished = Some(status);
            break;
        }
    }
    if let Some(status) = finished {
        // Tear down remaining background children. Reap without joining the
        // child's IO pump threads: a grandchild inheriting the stream can keep
        // them alive well beyond the kill, and nothing downstream needs their
        // residual output. `Drop` remains the bounded safety net.
        for child in bg.iter_mut() {
            if child.try_wait()?.is_none() {
                let _ = child.kill();
                let _ = child.try_wait();
            }
        }
        bg.clear();
        return Ok(Some(status));
    }
    Ok(None)
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
