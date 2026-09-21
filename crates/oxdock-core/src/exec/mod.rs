mod args;
mod capture;
mod engine;
mod fs_ops;
mod handlers;
mod io;
mod native;
mod state;
mod steps;
#[cfg(test)]
mod tests;
mod typing;

pub use self::engine::{Engine, EngineOutput};
pub(crate) use self::handlers::{
    dispatch_append, dispatch_assert_contains, dispatch_assert_eq, dispatch_assign,
    dispatch_assign_async_step, dispatch_assign_capture_step, dispatch_async_block,
    dispatch_await_capture_step, dispatch_await_step, dispatch_break, dispatch_call,
    dispatch_cancel_step, dispatch_continue, dispatch_copy, dispatch_copy_git, dispatch_cwd,
    dispatch_echo, dispatch_env, dispatch_exit, dispatch_expand, dispatch_for_loop,
    dispatch_func_def, dispatch_hash_sha256, dispatch_if_then, dispatch_inherit_env, dispatch_ls,
    dispatch_mkdir, dispatch_push_into_step, dispatch_read, dispatch_read_line, dispatch_return,
    dispatch_run, dispatch_run_exec, dispatch_set, dispatch_sleep_step, dispatch_symlink,
    dispatch_timeout_step, dispatch_while_loop, dispatch_with_io, dispatch_with_io_block,
    dispatch_workdir, dispatch_workspace, dispatch_write,
};
pub use self::io::ExecIo;
pub use self::io::PipeStream;
pub use self::native::{
    FuncKind, FuncMeta, FuncParam, FunctionRegistry, HostModule, HostRegistration, NativeFn,
    OxDockFn, PureFn, builtin_function_metas, builtin_function_names, std_module_table,
};
pub use self::state::ExecState;
pub use self::steps::StepCtx;
pub use self::typing::{
    OxDockType, TypeDescriptor, Value, ValuePayload, clone_boxed, clone_copy, clone_shared,
    drop_boxed, drop_noop, drop_shared, eq_boxed, eq_inline, eq_shared, fmt_boxed, fmt_inline,
    fmt_shared, load_inline, startup_descriptors, store_inline, type_anchor, unshare_boxed,
    unshare_inline, unshare_shared,
};

use anyhow::Result;
use oxdock_fs::{
    GuardedPath, LazyGuardedTempDir, PathResolver, WorkspaceFs, reserve_cargo_scratch,
};
use oxdock_parser::Step;
use oxdock_process::{
    BuiltinEnv, DefaultProcessManager, ProcessManager, SharedInput, SharedOutput,
    default_process_manager,
};

use std::collections::BTreeMap;
use std::sync::Arc;

use self::fs_ops::describe_dir;
use self::io::{StreamHandle, assemble_default_io, teed_stderr, teed_stdout};
use self::steps::execute_steps;

/// Display text emitted by `CWD` while the snapshot root is selected but not
/// yet materialized (issue #131). Single source of truth: the `cwd` handler
/// prints this value, and the logic-test harness resolves the same value
/// from `@SNAPSHOT_PENDING@` fixture tokens.
pub const SNAPSHOT_PENDING_DISPLAY: &str = "<snapshot:pending>";

/// Fallback tree body used when a materialized snapshot cannot be described
/// while composing a run error (issue #131). Single source of truth for the
/// lazy error path.
pub const SNAPSHOT_TREE_UNAVAILABLE: &str = "<unavailable>";

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
    /// Top-level script variable bindings captured at `Flow::Done`, keyed by
    /// variable name with deterministic ordering. Read-only introspection for
    /// hosts that assert on in-memory evaluation without file round trips.
    /// Ephemeral block scopes are excluded; see `run_steps_with_manager`.
    pub bindings: BTreeMap<String, Value>,
}

/// Execute the DSL against a lazily-created snapshot: no temporary directory
/// exists until the first snapshot-targeted resolution. The snapshot handle
/// is shared with the resolver, so all clones observe the same directory.
pub fn run_steps_with_lazy_snapshot(
    build_context: &GuardedPath,
    steps: &[Step],
    io: ExecIo,
) -> Result<LazyRunOutput> {
    run_steps_with_lazy_snapshot_and_modules(build_context, steps, io, Vec::new(), Vec::new())
}

/// Same as [`run_steps_with_lazy_snapshot`], plus host modules and types
/// (see [`run_steps_with_manager_with_modules`]). Lets binary hosts that
/// run on the lazy snapshot path expose plugin surface without changing
/// CLI semantics.
pub fn run_steps_with_lazy_snapshot_and_modules(
    build_context: &GuardedPath,
    steps: &[Step],
    io: ExecIo,
    modules: Vec<HostModule<DefaultProcessManager>>,
    types: Vec<&'static TypeDescriptor>,
) -> Result<LazyRunOutput> {
    let mut resolver = PathResolver::new_lazy(build_context.clone())?;
    resolver.set_workspace_root(build_context.clone());
    let snapshot = resolver.snapshot_handle();
    let fs: Box<dyn WorkspaceFs> = Box::new(resolver);
    match run_steps_with_manager_with_modules(
        fs,
        steps,
        default_process_manager(),
        io,
        modules,
        types,
    ) {
        Ok((final_cwd, fs, bindings)) => Ok(LazyRunOutput {
            final_cwd,
            snapshot,
            fs,
            bindings,
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
                Err(_) => String::from(SNAPSHOT_TREE_UNAVAILABLE),
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
    run_steps_with_manager(fs, steps, default_process_manager(), io).map(|(cwd, _, _)| cwd)
}

/// Base name of a qualified `MODULE::NAME` reference for human-facing
/// errors. Single source lives in `oxdock-parser`; listings (`FUNCTIONS()`,
/// `DESCRIBE`) keep the qualified form while step errors name the callable
/// as written.
pub(crate) use oxdock_parser::base_name;

/// Host introspection entry point: execute the DSL against a caller-provided
/// filesystem and return the final working directory, the filesystem handle,
/// and the top-level script variable bindings captured at `Flow::Done`.
/// Bindings are read from the root variable scope only, so ephemeral
/// variables from `FUNC` bodies, `FOR`/`WHILE` iterations, and `ASYNC` blocks
/// are excluded. On script failure the scope is discarded with the error and
/// no bindings are returned.
#[allow(clippy::type_complexity)]
pub fn run_steps_with_manager<P: ProcessManager>(
    fs: Box<dyn WorkspaceFs>,
    steps: &[Step],
    process: P,
    io: ExecIo,
) -> Result<(GuardedPath, Box<dyn WorkspaceFs>, BTreeMap<String, Value>)> {
    run_steps_with_manager_with_modules(fs, steps, process, io, Vec::new(), Vec::new())
}

/// Same as [`run_steps_with_manager`], plus host modules and types. Each
/// module's functions become callable as `MODULE::NAME` with full step
/// context; each type descriptor becomes visible to `TYPES()` and valid for
/// `LET $x: NAME` declarations carrying same-named opaque payloads.
/// Authors should derive entries with `#[oxdock_func]` / `#[oxdock_type]`
/// (see `oxdock-func-macro`) and group them into [`HostModule`] instead of
/// hand-writing metadata and glue.
#[allow(clippy::type_complexity)]
pub fn run_steps_with_manager_with_modules<P: ProcessManager>(
    fs: Box<dyn WorkspaceFs>,
    steps: &[Step],
    process: P,
    io: ExecIo,
    modules: Vec<HostModule<P>>,
    types: Vec<&'static self::typing::TypeDescriptor>,
) -> Result<(GuardedPath, Box<dyn WorkspaceFs>, BTreeMap<String, Value>)> {
    let mut state = new_state(fs, io)?;
    for module in modules {
        state.register_module(module);
    }
    for descriptor in types {
        state.register_type(descriptor);
    }
    finish_run(state, process, steps)
}

fn new_state<P: ProcessManager>(fs: Box<dyn WorkspaceFs>, io: ExecIo) -> Result<ExecState<P>> {
    let cwd = fs.root().clone();
    let build_context = fs.build_context().clone();
    let mut envs = BuiltinEnv::collect(&build_context).into_envs();
    for (key, value) in io.inherit_env_overrides() {
        envs.insert(key.clone(), value.clone());
    }
    let envs = Arc::new(envs);
    let assert_windows = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let assert_windows_stderr = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let exact_stdout = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut state = ExecState {
        fs,
        cargo_scratch: reserve_cargo_scratch()?,
        cwd,
        envs,
        bg_children: Vec::new(),
        scope_stack: Vec::new(),
        io,
        assert_windows: assert_windows.clone(),
        assert_windows_stderr: assert_windows_stderr.clone(),
        exact_stdout: exact_stdout.clone(),
        var_scopes: Vec::new(),
        cancel_token: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active_process: std::sync::Arc::new(std::sync::Mutex::new(None)),
        named_tasks: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        next_task_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
        inside_async: false,
        keeper_expiry: None,
        cancellable: false,
        functions: self::native::FunctionRegistry::with_builtins(),
        types: self::typing::startup_type_map(),
        call_depth: 0,
        // Root flow is task 0; worker ids start at 1 (see next_task_id).
        task_id: 0,
        _marker: std::marker::PhantomData,
    };

    // Push a global variable scope so top-level LET assignments are captured.
    state.push_var_scope();

    Ok(state)
}

#[allow(clippy::type_complexity)]
fn finish_run<P: ProcessManager>(
    mut state: ExecState<P>,
    process: P,
    steps: &[Step],
) -> Result<(GuardedPath, Box<dyn WorkspaceFs>, BTreeMap<String, Value>)> {
    let assert_windows = Arc::clone(&state.assert_windows);
    let assert_windows_stderr = Arc::clone(&state.assert_windows_stderr);
    let exact_stdout = Arc::clone(&state.exact_stdout);
    let _default_stdout = std::io::stdout();
    let stdin = state.io.stdin().into();
    // Every emitted byte flows through the tee so stream assertions see both
    // interpreter output and streamed child output, even when no capture
    // sink was configured (forwarding to real stdout in that case).
    let stdout = Some(StreamHandle::Stream(teed_stdout(
        state.io.stdout(),
        assert_windows,
        exact_stdout,
    )));
    let stderr = state
        .io
        .stderr()
        .map(|sink| StreamHandle::Stream(teed_stderr(Some(sink), assert_windows_stderr)));
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
        self::steps::Flow::Done => {
            // Root-scope isolation: at Done all blocks have popped, so the
            // first scope is the global one. Read it explicitly (rather than
            // a flattened all-scopes view) and strip declared types so hosts
            // see plain values.
            let bindings: BTreeMap<String, Value> = state
                .var_scopes
                .first()
                .map(|scope| {
                    scope
                        .iter()
                        .map(|(k, (_, v))| (k.clone(), v.clone()))
                        .collect()
                })
                .unwrap_or_default();
            Ok((state.fs.concretize_cwd(&state.cwd), state.fs, bindings))
        }
        self::steps::Flow::Break { idx } => {
            anyhow::bail!("step {}: BREAK outside loop", idx + 1)
        }
        self::steps::Flow::Continue { idx } => {
            anyhow::bail!("step {}: CONTINUE outside loop", idx + 1)
        }
        self::steps::Flow::Return { idx, .. } => {
            // Values only cross a boundary: a function call, an ASYNC
            // task, or an inline LET block. With none enclosing, the
            // pipeline top rejects it like any other escaped flow.
            anyhow::bail!(
                "step {}: RETURN outside function, ASYNC task, or LET block",
                idx + 1
            )
        }
    }
}
