mod args;
mod capture;
mod fs_ops;
mod handlers;
mod io;
mod pipe;
mod state;
mod steps;
#[cfg(test)]
mod tests;

pub(crate) use self::handlers::{
    dispatch_append, dispatch_assert_absent, dispatch_assert_dir, dispatch_assert_file,
    dispatch_assert_stdout, dispatch_assign, dispatch_assign_async_step,
    dispatch_assign_capture_step, dispatch_async_block, dispatch_await_capture_step,
    dispatch_await_step, dispatch_break, dispatch_call, dispatch_cancel_step, dispatch_continue,
    dispatch_copy, dispatch_copy_git, dispatch_cwd, dispatch_echo, dispatch_env, dispatch_exit,
    dispatch_expand, dispatch_for_loop, dispatch_func_def, dispatch_hash_sha256, dispatch_if_then,
    dispatch_inherit_env, dispatch_ls, dispatch_mkdir, dispatch_read, dispatch_read_line,
    dispatch_return, dispatch_run, dispatch_run_exec, dispatch_set, dispatch_sleep_step,
    dispatch_symlink, dispatch_timeout_step, dispatch_while_loop, dispatch_with_io,
    dispatch_with_io_block, dispatch_workdir, dispatch_workspace, dispatch_write,
};
pub use self::io::ExecIo;
pub(crate) use self::steps::StepCtx;

use anyhow::Result;
use oxdock_fs::{
    GuardedPath, LazyGuardedTempDir, PathResolver, WorkspaceFs, reserve_cargo_scratch,
};
use oxdock_parser::Step;
use oxdock_process::{
    BuiltinEnv, ProcessManager, SharedInput, SharedOutput, default_process_manager,
};

use std::sync::Arc;

use self::fs_ops::describe_dir;
use self::io::{StreamHandle, assemble_default_io, teed_stdout};
use self::state::ExecState;
use self::steps::execute_steps;

pub fn run_steps(fs_root: &GuardedPath, steps: &[Step]) -> Result<()> {
    run_steps_with_context(fs_root, fs_root, steps)
}

pub fn run_steps_with_context(
    fs_root: &GuardedPath,
    build_context: &GuardedPath,
    steps: &[Step],
) -> Result<()> {
    run_steps_with_context_result(fs_root, build_context, steps, None, None).map(|_| ())
}

/// Execute the DSL and return the final working directory after all steps.
pub fn run_steps_with_context_result(
    fs_root: &GuardedPath,
    build_context: &GuardedPath,
    steps: &[Step],
    stdin: Option<SharedInput>,
    stdout: Option<SharedOutput>,
) -> Result<GuardedPath> {
    let io = assemble_default_io(stdin, stdout);
    run_steps_with_context_result_with_io(fs_root, build_context, steps, io)
}

pub fn run_steps_with_context_result_with_io(
    fs_root: &GuardedPath,
    build_context: &GuardedPath,
    steps: &[Step],
    io: ExecIo,
) -> Result<GuardedPath> {
    match run_steps_inner(fs_root, build_context, steps, io) {
        Ok(final_cwd) => Ok(final_cwd),
        Err(err) => {
            let fs = PathResolver::new(fs_root.as_path(), build_context.as_path())?;
            let tree = describe_dir(&fs, fs_root, 2, 24);
            let snapshot = format!(
                "filesystem snapshot (root {}):\n{}",
                fs_root.display(),
                tree
            );
            Err(compose_error_with_snapshot(err, snapshot))
        }
    }
}

/// Compose a single error message with the top cause plus a caller-provided
/// filesystem-snapshot section. Shared by the eager and lazy runners so both
/// render identical chains and only differ in the snapshot section.
fn compose_error_with_snapshot(err: anyhow::Error, snapshot_section: String) -> anyhow::Error {
    // Compose a single error message with the top cause plus a compact fs snapshot.
    let chain = err.chain().map(|e| e.to_string()).collect::<Vec<_>>();
    let mut primary = chain
        .first()
        .cloned()
        .unwrap_or_else(|| "unknown error".into());
    let rest = if chain.len() > 1 {
        let first_cause = chain[1].clone();
        primary = format!("{primary} ({first_cause})");
        if chain.len() > 2 {
            let causes = chain
                .iter()
                .skip(2)
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n  ");
            format!("\ncauses:\n  {}", causes)
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    let msg = format!("{}{}\n{}", primary, rest, snapshot_section);
    anyhow::anyhow!(msg)
}

fn run_steps_inner(
    fs_root: &GuardedPath,
    build_context: &GuardedPath,
    steps: &[Step],
    io: ExecIo,
) -> Result<GuardedPath> {
    let mut resolver = PathResolver::new_guarded(fs_root.clone(), build_context.clone())?;
    resolver.set_workspace_root(build_context.clone());
    run_steps_with_fs_with_io(Box::new(resolver), steps, io)
}

/// Output of a lazily-executed run (issue #131): the final working directory
/// (concretized as of return), shared ownership of the snapshot backing dir,
/// and the filesystem handle for post-hoc convergence (e.g. concretizing the
/// cwd after shell-entry materialization).
pub struct LazyRunOutput {
    pub final_cwd: GuardedPath,
    pub snapshot: Arc<LazyGuardedTempDir>,
    pub fs: Box<dyn WorkspaceFs>,
}

/// Execute the DSL against a lazily-created snapshot: no temporary directory
/// exists until the first snapshot-targeted resolution. The snapshot handle
/// is shared with the resolver, so all clones observe the same directory.
pub fn run_steps_with_lazy_snapshot(
    build_context: &GuardedPath,
    steps: &[Step],
    io: ExecIo,
) -> Result<LazyRunOutput> {
    let mut resolver = PathResolver::new_lazy(build_context.clone())?;
    resolver.set_workspace_root(build_context.clone());
    let snapshot = resolver.snapshot_handle();
    let fs: Box<dyn WorkspaceFs> = Box::new(resolver);
    match run_steps_with_manager(fs, steps, default_process_manager(), io) {
        Ok((final_cwd, fs)) => Ok(LazyRunOutput {
            final_cwd,
            snapshot,
            fs,
        }),
        Err(err) => Err(enrich_lazy_error(&snapshot, build_context, err)),
    }
}

/// Error enrichment for lazy runs: a materialized snapshot gets the same
/// filesystem-snapshot treatment as eager runs; a pending one reports that
/// no snapshot directory was ever created instead of describing a tree.
/// Chain rendering is identical to the eager path (shared composer).
pub fn enrich_lazy_error(
    snapshot: &Arc<LazyGuardedTempDir>,
    build_context: &GuardedPath,
    err: anyhow::Error,
) -> anyhow::Error {
    match snapshot.get() {
        Some(concrete) => {
            let tree = match PathResolver::new(concrete.as_path(), build_context.as_path()) {
                Ok(describe_fs) => describe_dir(&describe_fs, concrete, 2, 24),
                Err(_) => String::from("<unavailable>"),
            };
            let snapshot_msg = format!(
                "filesystem snapshot (root {}):\n{}",
                concrete.display(),
                tree
            );
            compose_error_with_snapshot(err, snapshot_msg)
        }
        None => compose_error_with_snapshot(
            err,
            String::from(
                "filesystem snapshot: never materialized (no snapshot directory was created)",
            ),
        ),
    }
}

pub fn run_steps_with_fs(
    fs: Box<dyn WorkspaceFs>,
    steps: &[Step],
    stdin: Option<SharedInput>,
    stdout: Option<SharedOutput>,
) -> Result<GuardedPath> {
    let io = assemble_default_io(stdin, stdout);
    run_steps_with_fs_with_io(fs, steps, io)
}

pub fn run_steps_with_fs_with_io(
    fs: Box<dyn WorkspaceFs>,
    steps: &[Step],
    io: ExecIo,
) -> Result<GuardedPath> {
    run_steps_with_manager(fs, steps, default_process_manager(), io).map(|(cwd, _)| cwd)
}

fn run_steps_with_manager<P: ProcessManager>(
    fs: Box<dyn WorkspaceFs>,
    steps: &[Step],
    process: P,
    io: ExecIo,
) -> Result<(GuardedPath, Box<dyn WorkspaceFs>)> {
    let cwd = fs.root().clone();
    let build_context = fs.build_context().clone();
    let mut envs = BuiltinEnv::collect(&build_context).into_envs();
    for (key, value) in io.inherit_env_overrides() {
        envs.insert(key.clone(), value.clone());
    }
    let envs = Arc::new(envs);
    let assert_windows = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut state = ExecState {
        fs,
        cargo_scratch: reserve_cargo_scratch()?,
        cwd,
        envs,
        bg_children: Vec::new(),
        scope_stack: Vec::new(),
        io,
        assert_windows: assert_windows.clone(),
        var_scopes: Vec::new(),
        cancel_token: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active_process: std::sync::Arc::new(std::sync::Mutex::new(None)),
        named_tasks: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        next_task_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        inside_async: false,
        keeper_expiry: None,
        cancellable: false,
        funcs: std::sync::Arc::new(std::collections::HashMap::new()),
        host_funcs: std::sync::Arc::new(std::collections::HashMap::new()),
        call_depth: 0,
        _marker: std::marker::PhantomData,
    };

    // Push a global variable scope so top-level LET assignments are captured.
    state.push_var_scope();

    let _default_stdout = std::io::stdout();
    let stdin = state.io.stdin().into();
    // Every emitted byte flows through the tee so ASSERT_STDOUT sees both
    // interpreter output and streamed child output, even when no capture
    // sink was configured (forwarding to real stdout in that case).
    let stdout = Some(StreamHandle::Stream(teed_stdout(
        state.io.stdout(),
        assert_windows,
    )));
    let stderr = state.io.stderr().map(StreamHandle::Stream);
    let mut proc_mgr = process;
    let flow = execute_steps(
        &mut state,
        &mut proc_mgr,
        steps,
        stdin,
        false,
        stdout,
        stderr,
        true,
    )?;
    match flow {
        // Concretize on the way out so a bare pending anchor never escapes as
        // the reported final directory (shell entry / OUT_DIR sync need real
        // paths; a pending run reports the local root or concretizes after
        // shell-entry materialization through the returned fs handle).
        self::steps::Flow::Done => Ok((state.fs.concretize_cwd(&state.cwd), state.fs)),
        self::steps::Flow::Break { idx } => {
            anyhow::bail!("step {}: BREAK outside loop", idx + 1)
        }
        self::steps::Flow::Continue { idx } => {
            anyhow::bail!("step {}: CONTINUE outside loop", idx + 1)
        }
        self::steps::Flow::Return { idx, .. } => {
            anyhow::bail!("step {}: RETURN outside function", idx + 1)
        }
    }
}
