use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use flate2::Compression;
use flate2::write::GzEncoder;
use oxdock_fs::CopySourceRoot;
use oxdock_fs::EXECUTABLE_MODE;
use oxdock_fs::EntryKind;
use oxdock_fs::GuardedPath;
use oxdock_fs::PathResolver;
use oxdock_fs::PolicyPath;
use oxdock_fs::command_path;
use oxdock_fs::to_forward_slashes;
use oxdock_parser::constants::KEYWORD_RUN;
use oxdock_parser::constants::{
    KEYWORD_APPEND, KEYWORD_BREAK, KEYWORD_CONST, KEYWORD_CONTINUE, KEYWORD_EXPAND, KEYWORD_LET,
    KEYWORD_READ, KEYWORD_READ_LINE, KEYWORD_RETURN, KEYWORD_WRITE,
};
use oxdock_parser::{
    Arg, Command, Expr, IoBinding, IoStream, PipeTarget, Step, StepKind, TypeTag, Value,
    WorkspaceTarget, render_structural, sigil_var,
};
use oxdock_process::{
    BackgroundHandle, CommandOptions, CommandResult, CommandStderr, CommandStdin, CommandStdout,
    INHERIT_STDOUT_ENV_VAR, PROCESS_DEBUG_ENV_VAR, ProcessManager, SharedInput, batch_shell,
    check_spawn_anchors, read_pump_chunk,
};
use oxdock_pty::{PtyInput, PtyOutcome, PtyRequest, WatchSize, run_pty_session};
use oxdock_utils::sha256_hex;
use oxdock_utils::tar::{StreamedEntry, sanitize_rel, unpack_auto_from};

use super::args::parse_bool_str;
use super::diagnostics as diag;
use super::fs_ops::copy_entry;
use super::io::{StreamHandle, TaskTee, TeeStream, write_stderr, write_stdout};
use super::native::FuncBody;
use super::state::{CallAliasFrame, ExecState, MAX_CALL_DEPTH, RawFuncDef, TaskPhase};
use super::steps::{Flow, StepCtx, TtyRequest};
use oxdock_pipe::{KeeperGuard, PipeHandle, PipeInner, SharedOutput};

/// Map a Flow reaching a context-free boundary (pipeline top, thread join)
/// into status. Only Done passes; anything else is a step-numbered error
/// naming the originating step in its own body.
fn top_level_flow(flow: Flow) -> Result<()> {
    match flow {
        Flow::Done => Ok(()),
        Flow::Break { idx } => Err(anyhow::Error::new(diag::check_flow_outside_loop(
            idx + 1,
            KEYWORD_BREAK,
        ))),
        Flow::Continue { idx } => Err(anyhow::Error::new(diag::check_flow_outside_loop(
            idx + 1,
            KEYWORD_CONTINUE,
        ))),
        Flow::Return { idx, .. } => Err(anyhow::Error::new(diag::check_return_outside(idx + 1))),
    }
}

pub(super) fn inherit_env<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    keys: &[String],
) -> Result<()> {
    // Entry prelude capture: top-level `INHERIT_ENV` names and values
    // belong to the entry file, re-read from the engine on every
    // entry call. Never caller env: this reads only `io` overrides
    // and `std::env` (the engine), ever.
    let track_entry = cx.state.is_entry_top_level();
    let mut removals: Vec<String> = Vec::new();
    let mut inserts: Vec<(String, String)> = Vec::new();
    for key in keys {
        if cx.state.io.inherit_env_is_removed(key) {
            removals.push(key.clone());
            continue;
        }
        if let Some(value) = cx.state.io.inherit_env_value(key).cloned() {
            inserts.push((key.clone(), value));
            continue;
        }
        if let Ok(value) = std::env::var(key) {
            inserts.push((key.clone(), value));
        }
    }
    let envs = Arc::make_mut(&mut cx.state.envs);
    for key in &removals {
        envs.remove(key);
    }
    for (key, value) in &inserts {
        envs.insert(key.clone(), value.clone());
    }
    if track_entry {
        // Entry prelude capture: the entry record's init store holds
        // the top-level names and values re-read from the engine on
        // every entry call, mirroring module init capture.
        if let Some(init) = cx
            .state
            .modules
            .get_mut(super::import::ENTRY_UNIT)
            .and_then(|record| record.init.as_mut())
        {
            for key in &removals {
                init.envs.remove(key);
            }
            for (key, value) in &inserts {
                init.envs.insert(key.clone(), value.clone());
            }
            for key in keys {
                if !init.inherit_names.contains(key) {
                    init.inherit_names.push(key.clone());
                }
            }
        }
    }
    // Module init capture: executed top-level `INHERIT_ENV` keys belong
    // to the live init body, recorded as they run under their guards.
    // The init record does not exist yet, so accumulate on the init
    // stack drained into `ModuleInit::inherit_names` after execution.
    // Gated on `call_depth == 0` so a nested callee re-inherit (a call
    // into another module during init) never leaks into this init.
    if cx.state.call_depth == 0
        && let Some(top) = cx.state.init_inherit_stack.last_mut()
    {
        for key in keys {
            if !top.contains(key) {
                top.push(key.clone());
            }
        }
    }
    Ok(())
}

pub(super) fn workdir<P: ProcessManager>(cx: &mut StepCtx<'_, P>, path: &str) -> Result<()> {
    cx.state.cwd = cx
        .state
        .fs
        .resolve_workdir(&cx.state.cwd, path)
        .map_err(|e| anyhow::Error::new(diag::eval_workdir_failed(path, &e)))?;
    Ok(())
}

pub(super) fn workspace<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    target: &WorkspaceTarget,
) -> Result<()> {
    // Selection only, no disk I/O. The snapshot side stays pending until the
    // first snapshot-targeted choke point materializes it (issue #131).
    // CACHE ensures its persistent directory on first cache-targeted use
    // and never deletes it; SYSTEM bypasses containment. SYSTEM keeps the
    // current working directory (rebased off the never-created snapshot
    // anchor when needed) so relative paths keep working from where the
    // script already was (issue #163).
    match target {
        WorkspaceTarget::Snapshot => {
            cx.state.fs.switch_to_snapshot();
            cx.state.cwd = cx.state.fs.root().clone();
        }
        WorkspaceTarget::Local => {
            cx.state.fs.switch_to_local();
            cx.state.cwd = cx.state.fs.root().clone();
        }
        WorkspaceTarget::Cache { local } => {
            cx.state.fs.switch_to_cache(*local);
            cx.state.cwd = cx.state.fs.root().clone();
        }
        WorkspaceTarget::System => {
            cx.state.fs.switch_to_system();
            let cwd = cx.state.cwd.clone();
            cx.state.cwd = cx.state.fs.system_entry_cwd(&cwd);
        }
    }
    Ok(())
}

pub(super) fn env<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    key: &str,
    value: &str,
) -> Result<()> {
    Arc::make_mut(&mut cx.state.envs).insert(key.to_owned(), value.to_owned());
    // Entry prelude capture: the entry record's init store holds the
    // entry file's own top-level `ENV` declarations seeding every
    // entry call. Tracked only at entry top level, never inside a
    // call or module, so caller mutations can never flow into the
    // callee prelude.
    if cx.state.is_entry_top_level()
        && let Some(init) = cx
            .state
            .modules
            .get_mut(super::import::ENTRY_UNIT)
            .and_then(|record| record.init.as_mut())
    {
        init.envs.insert(key.to_owned(), value.to_owned());
    }
    Ok(())
}

pub(super) fn run<P: ProcessManager>(cx: &mut StepCtx<'_, P>, cmd: &str) -> Result<()> {
    // Terminal steps spawn through the shared pty pump instead of the
    // process manager; everything else below stays hermetic pipes.
    if let Some(tty) = cx.tty {
        return run_pty(cx, &tty_shell_argv(cmd), tty, cmd, true);
    }
    let ctx = cx.state.command_ctx()?;
    let step_stdin = if cx.expose_stdin {
        cx.stdin.clone()
    } else {
        CommandStdin::Null
    };

    let inherit_override = cx
        .state
        .envs
        .get(INHERIT_STDOUT_ENV_VAR)
        .map(|v| parse_bool_str(v).unwrap_or(false))
        .unwrap_or(false);

    if std::env::var(PROCESS_DEBUG_ENV_VAR).is_ok() {
        eprintln!(
            "DEBUG: step RUN {} inherit_override={}",
            cmd, inherit_override
        );
    }

    let stdout_mode = if inherit_override {
        CommandStdout::Inherit
    } else {
        cx.out
            .clone()
            .map(|handle| handle.to_stdout())
            .unwrap_or(CommandStdout::Inherit)
    };
    let stderr_mode = if inherit_override {
        CommandStderr::Inherit
    } else {
        cx.err
            .clone()
            .map(|handle| handle.to_stderr())
            .unwrap_or(CommandStderr::Inherit)
    };

    let mut options = if cx.state.inside_async || cx.state.cancellable {
        // Inside an ASYNC block : use background mode so we can register
        // the handle for cancellation via active_process.
        CommandOptions::background()
    } else {
        CommandOptions::foreground()
    };
    options.stdin = step_stdin;
    options.stdout = stdout_mode;
    options.stderr = stderr_mode;

    // A teardown that won the race already set cancellation: spawning
    // now would create an unkillable-by-teardown child (the killer
    // found nothing registered and moved on to joining), so refuse
    // before the spawn instead of after the wait.
    {
        use std::sync::atomic::Ordering;
        if cx.state.cancel_token.load(Ordering::SeqCst) {
            return Err(anyhow::Error::new(diag::eval_run_cancelled(cmd)));
        }
    }

    // Spawn the command.
    let mut handle = match cx
        .process
        .spawn_command(&ctx, cmd, options)
        .map_err(|e| anyhow::Error::new(diag::eval_run_spawn_failed(cmd, &e)))?
    {
        CommandResult::Background(h) => h,
        CommandResult::Completed => return Ok(()),
        CommandResult::Captured(_) => {
            return Err(anyhow::Error::new(diag::eval_run_unexpected_capture(cmd)));
        }
    };

    // Register the handle for cancellation (only meaningful for background handles).
    {
        let mut guard = cx
            .state
            .active_process
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some(Box::new(handle.clone()));
    }

    // Cancellation landing between spawn and registration would miss
    // this handle the same way: re-check now that registration is
    // visible, killing our own handle (double-kill safe: reaped PIDs
    // zero out) instead of waiting out the child.
    {
        use std::sync::atomic::Ordering;
        if cx.state.cancel_token.load(Ordering::SeqCst) {
            let _ = handle.kill();
            return Err(anyhow::Error::new(diag::eval_run_cancelled(cmd)));
        }
    }

    // Wait for the process to complete.
    let status = handle.wait();

    // Clear the registration BEFORE dropping the handle clone in active_process.
    // The clone was never polled, so we must prevent Drop from logging it as killed.
    {
        let mut guard = cx
            .state
            .active_process
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }

    let status = status?;
    if !status.success() {
        return Err(anyhow::Error::new(diag::eval_run_exit_status(cmd, &status)));
    }
    Ok(())
}

/// Direct-spawn counterpart of [`run`]: executes an already-resolved `argv`
/// without any shell (`RUN ["exe", "arg", ...]`). `CommandContext` and
/// `CommandOptions` (cwd/env, `WITH_IO` pipes, `ASYNC`/cancellable
/// backgrounding, [`INHERIT_STDOUT_ENV_VAR`]) are built exactly like [`run`];
/// only the spawn call differs (`spawn_argv`, no `shell_cmd`).
pub(super) fn run_argv<P: ProcessManager>(cx: &mut StepCtx<'_, P>, argv: &[String]) -> Result<()> {
    // Terminal steps spawn through the shared pty pump instead of the
    // process manager; everything else below stays hermetic pipes.
    if let Some(tty) = cx.tty {
        let what = format!("{argv:?}");
        return run_pty(cx, argv, tty, &what, false);
    }
    let ctx = cx.state.command_ctx()?;
    let step_stdin = if cx.expose_stdin {
        cx.stdin.clone()
    } else {
        CommandStdin::Null
    };

    let inherit_override = cx
        .state
        .envs
        .get(INHERIT_STDOUT_ENV_VAR)
        .map(|v| parse_bool_str(v).unwrap_or(false))
        .unwrap_or(false);

    if std::env::var(PROCESS_DEBUG_ENV_VAR).is_ok() {
        eprintln!(
            "DEBUG: step RUN {:?} inherit_override={}",
            argv, inherit_override
        );
    }

    let stdout_mode = if inherit_override {
        CommandStdout::Inherit
    } else {
        cx.out
            .clone()
            .map(|handle| handle.to_stdout())
            .unwrap_or(CommandStdout::Inherit)
    };
    let stderr_mode = if inherit_override {
        CommandStderr::Inherit
    } else {
        cx.err
            .clone()
            .map(|handle| handle.to_stderr())
            .unwrap_or(CommandStderr::Inherit)
    };

    let mut options = if cx.state.inside_async || cx.state.cancellable {
        // Inside an ASYNC block : use background mode so we can register
        // the handle for cancellation via active_process.
        CommandOptions::background()
    } else {
        CommandOptions::foreground()
    };
    options.stdin = step_stdin;
    options.stdout = stdout_mode;
    options.stderr = stderr_mode;

    // Same pre-spawn cancellation refusal as [`run`]: a teardown that
    // already ran must not gain a child it can no longer see.
    {
        use std::sync::atomic::Ordering;
        if cx.state.cancel_token.load(Ordering::SeqCst) {
            return Err(anyhow::Error::new(diag::eval_run_argv_cancelled(argv)));
        }
    }

    // Spawn the executable directly (no shell).
    let mut handle = match cx
        .process
        .spawn_argv(&ctx, argv, options)
        .map_err(|e| anyhow::Error::new(diag::eval_run_argv_spawn_failed(argv, &e)))?
    {
        CommandResult::Background(h) => h,
        CommandResult::Completed => return Ok(()),
        CommandResult::Captured(_) => {
            return Err(anyhow::Error::new(diag::eval_run_argv_unexpected_capture(
                argv,
            )));
        }
    };

    // Register the handle for cancellation (only meaningful for background handles).
    {
        let mut guard = cx
            .state
            .active_process
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some(Box::new(handle.clone()));
    }

    // Same post-registration re-check as [`run`].
    {
        use std::sync::atomic::Ordering;
        if cx.state.cancel_token.load(Ordering::SeqCst) {
            let _ = handle.kill();
            return Err(anyhow::Error::new(diag::eval_run_argv_cancelled(argv)));
        }
    }

    // Wait for the process to complete.
    let status = handle.wait();

    // Clear the registration BEFORE dropping the handle clone in active_process.
    // The clone was never polled, so we must prevent Drop from logging it as killed.
    {
        let mut guard = cx
            .state
            .active_process
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }

    let status = status?;
    if !status.success() {
        return Err(anyhow::Error::new(diag::eval_run_argv_exit_status(
            argv, &status,
        )));
    }
    Ok(())
}

/// Pseudo-terminal counterpart of [`run`]/[`run_argv`]: spawns `argv`
/// under a pty through the shared pump instead of the process manager,
/// blocking the step like any `RUN` until the child exits and its
/// output drains.
///
/// The terminal request rides `cx.tty` (populated only by `with_io`
/// from `[tty]` bindings); every other path keeps hermetic pipes. The
/// master is one interleaved stream: it lands exclusively in the step
/// stdout sink, stdin flows from a `stdin = $p` script pipe or from
/// the host terminal itself when stdin is a real one (bare `[tty]`
/// headless stays closed), and cancellation kills the child through
/// the pump's own path before reporting like any interrupted `RUN`.
/// Environment, anchors, and working directory mirror [`run_argv`]
/// exactly: closed child env from the script map, the same anchor
/// pre-flight the native spawn runs (`shell` selects the shell-form
/// `PATH` requirement), the same snapshot-choked cwd. `what` renders
/// the step in errors exactly like the calling form.
#[allow(clippy::too_many_arguments)]
fn run_pty<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    argv: &[String],
    tty: TtyRequest,
    what: &str,
    shell: bool,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    if argv.is_empty() {
        return Err(anyhow::Error::new(diag::eval_run_needs_program(what)));
    }
    // Same pre-spawn cancellation refusal as [`run`].
    if cx.state.cancel_token.load(Ordering::SeqCst) {
        return Err(anyhow::Error::new(diag::eval_run_cancelled(what)));
    }
    if std::env::var(PROCESS_DEBUG_ENV_VAR).is_ok() {
        eprintln!("DEBUG: step RUN {what} under tty {tty:?}");
    }
    // Closed child env from the script map, staged verbatim after the
    // same anchor pre-flight the native spawn runs inside `apply_ctx`.
    let ctx = cx.state.command_ctx()?;
    let staged = ctx.envs().as_ref().clone();
    check_spawn_anchors(&argv[0], &staged, shell)?;
    let cwd = match ctx.cwd() {
        PolicyPath::Guarded(p) => command_path(p).to_string_lossy().into_owned(),
        PolicyPath::Unguarded(p) => p.as_path().to_string_lossy().into_owned(),
    };
    // Stdin feeds the master from a `stdin = $p` script pipe, or from
    // the host terminal itself when stdin is a real one: the backend
    // behind a pipe carries the wake hook that releases a worker
    // parked in a blocking read when the child dies, and the host
    // reader carries its own teardown flag plus a console unblock,
    // so neither strands the session. Anything else (bare `[tty]`
    // headless, ambient inheritance off-terminal, unwakeable
    // descriptors) runs closed instead of stranding the session on
    // a worker that can never observe exit.
    let input = match &cx.stdin {
        CommandStdin::Null | CommandStdin::Inherit
            if !cx.process.stdin_is_terminal() || cx.stdin.is_redirected() =>
        {
            None
        }
        CommandStdin::Null | CommandStdin::Inherit => oxdock_pty::acquire_host_stdin(),
        CommandStdin::Stream(reader) => match cx.stdin_pipe.clone() {
            Some(backend) => {
                let reader = reader.clone();
                Some(PtyInput {
                    read_chunk: Box::new(move |buf| read_pump_chunk(&reader, buf)),
                    cancel: Box::new(move || {
                        backend.cancel();
                    }),
                })
            }
            None => None,
        },
        #[cfg(not(miri))]
        CommandStdin::OsPipe(_) => {
            return Err(anyhow::Error::new(diag::eval_with_io_tty_live_pipe(
                "stdin",
            )));
        }
    };
    // Master output lands exclusively in the step stdout sink. `None`
    // inherits host stdout, matching `write_stdout`; a live OS handle
    // is unreachable (terminal stdio resolves bridged), so it stays a
    // defensive bail like the DSL writers.
    let output: SharedOutput = match &cx.out {
        Some(StreamHandle::Stream(writer)) => writer.clone(),
        None => Arc::new(Mutex::new(std::io::stdout())),
        #[cfg(not(miri))]
        Some(StreamHandle::Os(_)) => {
            return Err(anyhow::Error::new(diag::eval_with_io_tty_live_pipe(
                "stdout",
            )));
        }
    };
    let hub = cx.cancel_hub();
    let token = cx.cancel_token();
    // Geometry resolves here, at spawn: unnamed sides read the host
    // terminal behind our own stdio, so the pty matches the window
    // the author actually sees, not a stale parse-time guess. The
    // size cell keeps following the host while the step runs, so a
    // mid-run resize reaches the child through the normal watcher.
    let size = oxdock_pty::resolve_size(tty.rows, tty.cols);
    let outcome = run_pty_session(PtyRequest {
        argv: argv.to_vec(),
        env: staged.into_iter().collect(),
        cwd,
        initial_size: size,
        input,
        output,
        size_source: WatchSize::following_host(tty.rows, tty.cols),
        should_cancel: Box::new(move || hub.is_cancelled() || token.load(Ordering::SeqCst)),
        on_exit: None,
        on_output_drained: None,
        func: KEYWORD_RUN,
    })
    .map_err(|e| anyhow::Error::new(diag::eval_run_spawn_failed(what, &anyhow::Error::new(e))))?;
    // Pipe EOF rides the writer drops like any `RUN`: the supervisor
    // returns only after the output worker finished, so every byte is
    // already in the sink before the code below runs.
    match outcome {
        PtyOutcome::Completed(code) => {
            if code != 0 {
                return Err(anyhow::Error::new(diag::eval_run_pty_exit_status(
                    what, code,
                )));
            }
            Ok(())
        }
        PtyOutcome::Cancelled => Err(anyhow::Error::new(diag::eval_run_cancelled(what))),
    }
}

/// Shell-form argv for a terminal spawn: the same deterministic batch
/// shell the native spawn runs (`sh -c` / `cmd /C`), so shell steps
/// read identically under pipes and under a pty.
fn tty_shell_argv(cmd: &str) -> Vec<String> {
    #[allow(clippy::disallowed_macros)]
    let flag = if cfg!(windows) { "/C" } else { "-c" };
    vec![batch_shell(), flag.to_string(), cmd.to_string()]
}

/// Resolve exec-form (`RUN [...]`) argv elements with explicit coercion:
/// `Arg::String`/`Arg::Parts` resolve to exactly one entry each;
/// `Arg::Expr` evaluates and coerces by value : `String`/`Int`/`Bool` push
/// one entry, `List` flattens recursively (each scalar becomes its own
/// entry), `Map`/`TaskHandle` bail with a type error.
/// Template expansion applies strictly to script-literal source text
/// (`Arg::String`/`Arg::Parts` via `resolve_arg`, string literals inline
/// below). Evaluated runtime values are opaque data and are never
/// re-expanded: a variable holding `{{ ... }}` text passes through
/// verbatim instead of leaking a second expansion pass.
/// Never uses shell joining or `expand_dsl_vars`.
pub(super) fn resolve_run_exec_argv<P: ProcessManager>(
    argv: &[Arg],
    cx: &mut StepCtx<'_, P>,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for arg in argv {
        match arg {
            Arg::String(_, _) | Arg::Parts(_) => {
                out.push(super::args::resolve_arg(arg, cx)?);
            }
            Arg::Expr(Expr::Literal(v)) if v.as_str().is_some() => {
                let s = v.as_str().unwrap_or_default().to_string();
                out.push(super::args::expand_string(&s, &cx.state.envs, cx.state)?);
            }
            Arg::Expr(e) => {
                let val = super::args::evaluate_expr(e, cx)?;
                flatten_exec_value(&val, &mut out)?;
            }
        }
    }
    if out.is_empty() {
        return Err(anyhow::Error::new(diag::eval_run_exec_requires_arg()));
    }
    Ok(out)
}

fn flatten_exec_value(val: &Value, out: &mut Vec<String>) -> Result<()> {
    if let Some(s) = val.as_str() {
        out.push(s.to_string());
        return Ok(());
    }
    if let Some(i) = val.as_i64() {
        out.push(i.to_string());
        return Ok(());
    }
    if let Some(f) = val.as_f64() {
        out.push(f.to_string());
        return Ok(());
    }
    if let Some(b) = val.as_bool() {
        out.push(b.to_string());
        return Ok(());
    }
    if val.as_pipe_handle().is_some() {
        // A pipe in argv position is always a bug: a child process
        // cannot receive a handle as an argument string.
        return Err(anyhow::Error::new(diag::eval_run_exec_pipe_arg()));
    }
    if val.as_semaphore().is_some() {
        return Err(anyhow::Error::new(diag::eval_run_exec_semaphore_arg()));
    }
    if val.as_permit().is_some() {
        return Err(anyhow::Error::new(diag::eval_run_exec_permit_arg()));
    }
    if let Some(d) = val.as_duration() {
        out.push(oxdock_parser::command::format_duration(&d));
        return Ok(());
    }
    if let Some(p) = val.as_path() {
        out.push(p.to_string_lossy().to_string());
        return Ok(());
    }
    if let Some(items) = val.as_list() {
        for item in items {
            flatten_exec_value(item, out)?;
        }
        return Ok(());
    }
    if val.as_map().is_some() {
        return Err(anyhow::Error::new(
            diag::eval_run_exec_element_needs_string("map"),
        ));
    }
    if let Some(id) = val.as_handle() {
        return Err(anyhow::Error::new(
            diag::eval_run_exec_element_needs_string(&format!("task handle task#{id}")),
        ));
    }
    // Every other type stringifies through its descriptor rendering
    // (host values through their `Display`).
    out.push(format!("{val}"));
    Ok(())
}

pub(super) fn echo<P: ProcessManager>(cx: &mut StepCtx<'_, P>, msg: &str) -> Result<()> {
    // One formatted write, not `writeln!`: the formatting machinery
    // emits the text and the newline as separate writes, and each one
    // re-locks the shared parent behind task tees, so concurrent
    // tasks interleave mid-line ("worker 0worker 1"). A single
    // `write_all` keeps every backend's synchronous completion as one
    // atomic line.
    let line = format!("{msg}\n");
    write_stdout(cx.out.clone(), |writer| {
        writer
            .write_all(line.as_bytes())
            .map_err(|e| diag::eval_stream_write_failed(&e))?;
        Ok(())
    })?;
    Ok(())
}

/// Pipeline dispatch wrapper for `Sleep`
pub(crate) fn dispatch_sleep_step<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Sleep { duration } = step else {
        unreachable!()
    };
    let duration = super::args::resolve_arg_as_duration(duration, cx)?;
    sleep(cx, &duration)
}

/// Dispatch `SLEEP <duration>` : park the step without spawning a shell.
///
/// Cooperative: sleeps in bounded chunks and checks the cancellation token
/// between chunks, so an enclosing `TIMEOUT` deadline or parent task teardown
/// interrupts the sleep promptly. Never blocks on the full duration at once.
pub(crate) fn sleep<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    duration: &std::time::Duration,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    let start = Instant::now();
    loop {
        if cx.state.cancel_token.load(Ordering::SeqCst) {
            return Err(anyhow::Error::new(diag::eval_sleep_cancelled()));
        }
        let elapsed = start.elapsed();
        if elapsed >= *duration {
            return Ok(());
        }
        let remaining = duration.saturating_sub(elapsed);
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

/// Map a `COPY --from-workspace` target onto the filesystem layer's source
/// root (issue #163). The parser owns the DSL vocabulary; `oxdock-fs`
/// stays a leaf crate, so the translation lives here. Shared with the
/// remote fetch resolver, which honors the same flag on `--from-host`.
pub(super) fn copy_source_root(target: WorkspaceTarget) -> CopySourceRoot {
    match target {
        WorkspaceTarget::Snapshot => CopySourceRoot::Snapshot,
        WorkspaceTarget::Local => CopySourceRoot::Local,
        WorkspaceTarget::Cache { .. } => CopySourceRoot::Cache,
        WorkspaceTarget::System => CopySourceRoot::System,
    }
}

pub(super) fn copy<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    from_workspace: Option<WorkspaceTarget>,
    from: &str,
    to: &str,
) -> Result<()> {
    let from_abs = if let Some(target) = from_workspace {
        cx.state
            .fs
            .resolve_copy_source_from_target(copy_source_root(target), from)
            .map_err(|e| anyhow::Error::new(diag::eval_copy_failed(from, to, &e)))?
    } else {
        cx.state
            .fs
            .resolve_copy_source(from)
            .map_err(|e| anyhow::Error::new(diag::eval_copy_failed(from, to, &e)))?
    };
    let to_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, to)
        .map_err(|e| anyhow::Error::new(diag::eval_copy_failed(from, to, &e)))?;
    let to_abs = place_file_in_dir(cx.state.fs.as_ref(), &from_abs, to, &to_abs)
        .map_err(|e| anyhow::Error::new(diag::eval_copy_failed(from, to, &e)))?;
    copy_entry(cx.state.fs.as_ref(), &from_abs, &to_abs)
        .map_err(|e| anyhow::Error::new(diag::eval_copy_failed(from, to, &e)))?;
    Ok(())
}

/// Docker destination semantics for file sources: when the destination
/// names a directory (an existing one, or a trailing-slash spell), the
/// file lands inside it under its own basename (`COPY file dir/`).
/// Directory sources already copy their contents into the destination,
/// and plain file paths keep the rename behavior. Returns the
/// (possibly rebased) destination guard.
fn place_file_in_dir(
    fs: &dyn oxdock_fs::WorkspaceFs,
    from_abs: &GuardedPath,
    to: &str,
    to_abs: &GuardedPath,
) -> Result<GuardedPath> {
    if !matches!(fs.entry_kind(from_abs), Ok(EntryKind::File)) {
        return Ok(to_abs.clone());
    }
    let trailing_slash = to.ends_with('/') || to.ends_with('\\');
    let dst_is_dir = matches!(fs.entry_kind(to_abs), Ok(EntryKind::Dir));
    if !trailing_slash && !dst_is_dir {
        return Ok(to_abs.clone());
    }
    let display = from_abs.display().to_string();
    let base = from_abs
        .as_path()
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow::Error::new(diag::eval_copy_no_file_name(&display)))?;
    let to_display = to_abs.display().to_string();
    to_abs
        .join(&base)
        .map_err(|e| anyhow::Error::new(diag::eval_copy_destination_escapes(&to_display, &e)))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn copy_git<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    rev: &str,
    from: &str,
    to: &str,
    include_dirty: bool,
) -> Result<()> {
    let to_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, to)
        .map_err(|e| anyhow::Error::new(diag::eval_copy_git_failed(rev, from, to, &e)))?;
    cx.state
        .fs
        .copy_from_git(rev, from, &to_abs, include_dirty)
        .map_err(|e| anyhow::Error::new(diag::eval_copy_git_failed(rev, from, to, &e)))?;
    Ok(())
}

/// Resolve an archive source with a symlink refusal: canonical
/// resolution routes through the final link, so the resolved guard alone
/// cannot see it. The parent resolves normally (intermediate links stay
/// contained); the final name is matched against the parent's own
/// listing, whose file types never follow links. Matching is
/// case-insensitive so case-insensitive filesystems probe the entry the
/// resolve would actually open; any link among the matches refuses.
fn resolve_archive_src<P: ProcessManager>(
    cx: &StepCtx<'_, P>,
    src: &str,
    dst: &str,
) -> oxdock_diagnostics::DiagnosticResult<GuardedPath> {
    let trimmed = to_forward_slashes(src).trim_end_matches('/').to_string();
    // A leading slash roots the parent at `/` itself: without the
    // index-zero arm `/foo` would split into parent `""` and resolve
    // against the working directory instead of the root.
    let (parent, name) = match trimmed.rfind('/') {
        Some(0) => ("/".to_string(), trimmed[1..].to_string()),
        Some(idx) => (trimmed[..idx].to_string(), trimmed[idx + 1..].to_string()),
        None => (String::new(), trimmed),
    };
    if name.is_empty() || name == "." || name == ".." {
        // Root-ish spellings carry no final component to probe: a root
        // is never a link itself.
        return cx
            .state
            .fs
            .resolve_read(&cx.state.cwd, src)
            .map_err(|e| diag::eval_archive_failed(src, dst, &e));
    }
    let parent_abs = if parent.is_empty() {
        cx.state.cwd.clone()
    } else {
        cx.state
            .fs
            .resolve_read(&cx.state.cwd, &parent)
            .map_err(|e| diag::eval_archive_failed(src, dst, &e))?
    };
    let mut matched = false;
    for entry in cx
        .state
        .fs
        .read_dir_entries(&parent_abs)
        .map_err(|e| diag::eval_archive_failed(src, dst, &e))?
    {
        if entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(&name)
        {
            matched = true;
            let is_link = entry
                .file_type()
                .map(|kind| kind.is_symlink())
                .unwrap_or(true);
            if is_link {
                return Err(diag::eval_archive_refuses_symlink_source(src, dst));
            }
        }
    }
    if !matched {
        // Genuinely missing: let the resolver report it authoritatively.
        return cx
            .state
            .fs
            .resolve_read(&cx.state.cwd, src)
            .map_err(|e| diag::eval_archive_failed(src, dst, &e));
    }
    parent_abs
        .join(&name)
        .map_err(|e| diag::eval_archive_failed(src, dst, &e))
}

/// Collected packable tree: `(rel, guard)` files plus `(rel, guard)`
/// dirs, all forward-slash relative and sorted for deterministic
/// archives.
type ArchiveTree = (Vec<(String, GuardedPath)>, Vec<(String, GuardedPath)>);

/// Collect one packable tree (see [`ArchiveTree`]). Sources arrive
/// pre-probed ([`resolve_archive_src`]); every descendant is probed at
/// visit time through its own listing entry, whose file type never
/// follows links.
fn collect_archive_tree(
    fs: &dyn oxdock_fs::WorkspaceFs,
    src_abs: &GuardedPath,
    src: &str,
    dst: &str,
) -> oxdock_diagnostics::DiagnosticResult<ArchiveTree> {
    match fs
        .entry_kind(src_abs)
        .map_err(|e| diag::eval_archive_failed(src, dst, &e))?
    {
        EntryKind::File => {
            let base = src_abs
                .as_path()
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .ok_or_else(|| {
                    diag::eval_archive_no_file_name(src, dst, &src_abs.display().to_string())
                })?;
            let rel = sanitize_rel(&to_forward_slashes(&base))
                .map_err(|err| diag::eval_archive_failed(src, dst, &anyhow::Error::new(err)))?;
            Ok((vec![(rel, src_abs.clone())], Vec::new()))
        }
        EntryKind::Dir => {
            let mut files = Vec::new();
            let mut dirs = Vec::new();
            let mut stack = vec![(src_abs.clone(), String::new())];
            while let Some((dir_abs, dir_rel)) = stack.pop() {
                // A swapped intermediate link would redirect the whole
                // subtree walk: the rel prefixes must be real
                // directories before listing.
                PathResolver::refuse_link_ancestors(src_abs, &dir_rel)
                    .map_err(|e| diag::eval_archive_failed(src, dst, &e))?;
                let mut entries = fs
                    .read_dir_entries(&dir_abs)
                    .map_err(|e| diag::eval_archive_failed(src, dst, &e))?;
                entries.sort_by_key(|entry| entry.file_name());
                for entry in entries {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let joined = if dir_rel.is_empty() {
                        name.clone()
                    } else {
                        format!("{dir_rel}/{name}")
                    };
                    let child_rel = to_forward_slashes(&joined);
                    let is_link = entry
                        .file_type()
                        .map(|kind| kind.is_symlink())
                        .unwrap_or(true);
                    if is_link {
                        return Err(diag::eval_archive_refuses_symlink(src, dst, &child_rel));
                    }
                    let child = dir_abs
                        .join(&name)
                        .map_err(|e| diag::eval_archive_failed(src, dst, &e))?;
                    match fs
                        .entry_kind(&child)
                        .map_err(|e| diag::eval_archive_failed(src, dst, &e))?
                    {
                        EntryKind::Dir => {
                            dirs.push((child_rel.clone(), child.clone()));
                            stack.push((child, child_rel));
                        }
                        EntryKind::File => files.push((child_rel, child)),
                        EntryKind::Symlink => {
                            unreachable!("following entry_kind never reports symlinks")
                        }
                    }
                }
            }
            files.sort_by(|a, b| a.0.cmp(&b.0));
            dirs.sort_by(|a, b| a.0.cmp(&b.0));
            Ok((files, dirs))
        }
        EntryKind::Symlink => unreachable!("following entry_kind never reports symlinks"),
    }
}

/// Append one collected tree to an open tar builder, opening each file
/// only for its own append: file handles never accumulate, so archive
/// size stays O(chunk) memory with O(1) open files.
fn append_archive_tree<W: std::io::Write>(
    fs: &dyn oxdock_fs::WorkspaceFs,
    builder: &mut tar::Builder<W>,
    files: &[(String, GuardedPath)],
    dirs: &[(String, GuardedPath)],
    src_abs: &GuardedPath,
    src: &str,
    dst: &str,
) -> oxdock_diagnostics::DiagnosticResult<()> {
    for (rel, guard) in dirs {
        let rel = sanitize_rel(rel)
            .map_err(|err| diag::eval_archive_failed(src, dst, &anyhow::Error::new(err)))?;
        let mode = fs
            .archive_member_stat(guard)
            .map(|(_, mode)| mode.unwrap_or(0o755))
            .map_err(|e| diag::eval_archive_failed(src, dst, &e))?;
        let mut header = tar::Header::new_gnu();
        header.set_mode(mode);
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_data(&mut header, &rel, std::io::empty())
            .map_err(|e| diag::eval_archive_failed(src, dst, &anyhow::Error::new(e)))?;
    }
    for (rel, guard) in files {
        let rel = sanitize_rel(rel)
            .map_err(|err| diag::eval_archive_failed(src, dst, &anyhow::Error::new(err)))?;
        let (len, mode) = fs
            .archive_member_stat(guard)
            .map_err(|e| diag::eval_archive_failed(src, dst, &e))?;
        // Link-refusing open: a symlink planted after the listing
        // probe fails here instead of packing its target's bytes, and
        // the descriptor must stay under the packed tree.
        let mut reader = fs
            .open_read_nofollow(guard, src_abs)
            .map_err(|e| diag::eval_archive_failed(src, dst, &e))?;
        let mut header = tar::Header::new_gnu();
        header.set_mode(mode.unwrap_or(0o644));
        header.set_size(len);
        header.set_cksum();
        builder
            .append_data(&mut header, &rel, &mut reader)
            .map_err(|e| diag::eval_archive_failed(src, dst, &anyhow::Error::new(e)))?;
    }
    Ok(())
}

pub(super) fn archive<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    src: &str,
    dst: &str,
    raw: bool,
) -> oxdock_diagnostics::DiagnosticResult<()> {
    let src_abs = resolve_archive_src(cx, src, dst)?;
    let dst_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, dst)
        .map_err(|e| diag::eval_archive_failed(src, dst, &e))?;
    // Opening the destination truncates it: an archive landing on its
    // own source would destroy inputs before their bytes stream, so
    // equal paths fail closed. A pre-existing destination inside the
    // packed tree would likewise be truncated before its own append
    // reads it back; a fresh one is safe (collection already ran, so it
    // cannot be a member) which keeps whole-tree archives like
    // `ARCHIVE project project.tar.gz` working.
    if dst_abs == src_abs {
        return Err(diag::eval_archive_destination_is_source(src, dst));
    }
    if dst_abs.as_path().starts_with(src_abs.as_path()) && dst_abs.exists() {
        return Err(diag::eval_archive_destination_inside_tree(src, dst));
    }
    let (files, dirs) = collect_archive_tree(cx.state.fs.as_ref(), &src_abs, src, dst)?;
    cx.state
        .fs
        .ensure_parent_dir(&dst_abs)
        .map_err(|e| diag::eval_archive_failed(src, dst, &e))?;
    let out = cx
        .state
        .fs
        .open_write(&dst_abs)
        .map_err(|e| diag::eval_archive_failed(src, dst, &e))?;
    if raw {
        let mut builder = tar::Builder::new(out);
        append_archive_tree(
            cx.state.fs.as_ref(),
            &mut builder,
            &files,
            &dirs,
            &src_abs,
            src,
            dst,
        )?;
        let mut out = builder
            .into_inner()
            .map_err(|e| diag::eval_archive_failed(src, dst, &anyhow::Error::new(e)))?;
        std::io::Write::flush(&mut out)
            .map_err(|e| diag::eval_archive_failed(src, dst, &anyhow::Error::new(e)))?;
    } else {
        let encoder = GzEncoder::new(out, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        append_archive_tree(
            cx.state.fs.as_ref(),
            &mut builder,
            &files,
            &dirs,
            &src_abs,
            src,
            dst,
        )?;
        builder
            .into_inner()
            .map_err(|e| diag::eval_archive_failed(src, dst, &anyhow::Error::new(e)))?
            .try_finish()
            .map_err(|e| diag::eval_archive_failed(src, dst, &anyhow::Error::new(e)))?;
    }
    Ok(())
}

pub(super) fn unarchive<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    src: &str,
    dst: &str,
    max_bytes: Option<&str>,
) -> oxdock_diagnostics::DiagnosticResult<()> {
    let cap = match max_bytes {
        Some(raw) => oxdock_parser::command::parse_byte_count(raw)
            .map_err(|e| diag::eval_unarchive_bad_max_bytes(src, dst, raw, e))?,
        None => DEFAULT_UNARCHIVE_MAX_BYTES,
    };
    let src_abs = cx
        .state
        .fs
        .resolve_read(&cx.state.cwd, src)
        .map_err(|e| diag::eval_unarchive_failed(src, dst, &e))?;
    let dst_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, dst)
        .map_err(|e| diag::eval_unarchive_failed(src, dst, &e))?;
    cx.state
        .fs
        .create_dir_all(&dst_abs)
        .map_err(|e| diag::eval_unarchive_failed(src, dst, &e))?;
    let reader = cx
        .state
        .fs
        .open_read(&src_abs)
        .map_err(|e| diag::eval_unarchive_failed(src, dst, &e))?;
    let fs = cx.state.fs.as_ref();
    // Directory modes apply after every member lands: a read-only
    // header (e.g. 0555) would otherwise lock its own children out
    // before they extract. Deepest first, so restrictive parents settle
    // last with nothing left to write beneath them.
    let mut deferred_dir_modes: Vec<(GuardedPath, u32)> = Vec::new();
    // The expansion budget spans the whole archive, not one member: a
    // bomb split across many just-under-cap members must still trip it.
    let mut remaining = cap;
    unpack_auto_from(reader, &mut |entry| {
        match entry {
            StreamedEntry::Dir { path, mode } => {
                // Re-validate the decoder's rel at the write site: a
                // sanitized rel is pure descent, so the join below
                // cannot escape lexically in any path normalization
                // domain (verbatim, short names, slashes). Escapes via
                // planted links stay the filesystem layers' job, and
                // those run next, on every member, in every mode.
                let rel = sanitize_rel(&path).map_err(|err| {
                    anyhow::Error::new(diag::eval_unarchive_member_failed(
                        src,
                        dst,
                        &anyhow::Error::new(err),
                    ))
                })?;
                PathResolver::refuse_link_ancestors(&dst_abs, &rel).map_err(|e| {
                    anyhow::Error::new(diag::eval_unarchive_member_failed(src, dst, &e))
                })?;
                let dir = dst_abs.join(&rel).map_err(|e| {
                    anyhow::Error::new(diag::eval_unarchive_member_failed(src, dst, &e))
                })?;
                fs.create_dir_all(&dir).map_err(|e| {
                    anyhow::Error::new(diag::eval_unarchive_member_failed(src, dst, &e))
                })?;
                deferred_dir_modes.push((dir, mode));
            }
            StreamedEntry::File { path, mode, reader } => {
                let rel = sanitize_rel(&path).map_err(|err| {
                    anyhow::Error::new(diag::eval_unarchive_member_failed(
                        src,
                        dst,
                        &anyhow::Error::new(err),
                    ))
                })?;
                PathResolver::refuse_link_ancestors(&dst_abs, &rel).map_err(|e| {
                    anyhow::Error::new(diag::eval_unarchive_member_failed(src, dst, &e))
                })?;
                let file = dst_abs.join(&rel).map_err(|e| {
                    anyhow::Error::new(diag::eval_unarchive_member_failed(src, dst, &e))
                })?;
                fs.ensure_parent_dir(&file).map_err(|e| {
                    anyhow::Error::new(diag::eval_unarchive_member_failed(src, dst, &e))
                })?;
                // Link-refusing open: a symlink planted at the member
                // path fails here instead of truncating its target,
                // and the descriptor must stay under the destination.
                let mut out = fs.open_write_nofollow(&file, &dst_abs).map_err(|e| {
                    anyhow::Error::new(diag::eval_unarchive_member_failed(src, dst, &e))
                })?;
                // Metered copy: bombs read small and write huge, so the
                // cap counts member bytes out, not archive bytes in. The
                // tripped member stays truncated while the step fails.
                let mut capped = CappedReader {
                    inner: reader,
                    remaining,
                };
                std::io::copy(&mut capped, &mut out).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::QuotaExceeded {
                        anyhow::Error::new(diag::eval_unarchive_expansion_cap(src, dst, cap, &path))
                    } else {
                        anyhow::Error::new(diag::eval_unarchive_member_failed(
                            src,
                            dst,
                            &anyhow::Error::new(e),
                        ))
                    }
                })?;
                remaining = capped.remaining;
                std::io::Write::flush(&mut out).map_err(|e| {
                    anyhow::Error::new(diag::eval_unarchive_member_failed(
                        src,
                        dst,
                        &anyhow::Error::new(e),
                    ))
                })?;
                restore_archive_mode(fs, &file, mode).map_err(|e| {
                    anyhow::Error::new(diag::eval_unarchive_member_failed(src, dst, &e))
                })?;
            }
            StreamedEntry::Symlink { path, target } => {
                return Err(anyhow::Error::new(diag::eval_unarchive_refuses_symlink(
                    src, dst, &path, &target,
                )));
            }
        }
        Ok(())
    })
    .map_err(|e| diag::eval_unarchive_failed(src, dst, &e))?;
    for (dir, mode) in deferred_dir_modes.into_iter().rev() {
        restore_archive_mode(fs, &dir, mode)
            .map_err(|e| diag::eval_unarchive_restore_mode_failed(src, dst, &e))?;
    }
    Ok(())
}

/// Default `UNARCHIVE` expansion cap (1 GiB of member bytes): release
/// artifacts are megabytes, so this never gates legitimate use while a
/// decompression bomb trips it long before runner disks fill. Per-call
/// `--max-bytes` moves the ceiling either way.
const DEFAULT_UNARCHIVE_MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// Read adapter metering decompressed member bytes against the
/// expansion cap: bombs read small and write huge, so the meter sits on
/// the output side. Zero-length probes pass through so EOF detection
/// keeps working, and an exhausted budget probes the stream before
/// erroring: a payload ending exactly on the cap reads `Ok(0)` there
/// instead of false-tripping.
struct CappedReader<'a> {
    inner: &'a mut dyn std::io::Read,
    remaining: u64,
}

impl std::io::Read for CappedReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut probe = [0u8; 1];
            if self.inner.read(&mut probe)? == 0 {
                return Ok(0);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::QuotaExceeded,
                "archive member exceeds the expansion cap",
            ));
        }
        let allowed = (buf.len() as u64).min(self.remaining) as usize;
        let n = self.inner.read(&mut buf[..allowed])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}
/// Restore one header mode through the fs abstraction (Unix-effective,
/// no-op elsewhere). Zero modes skip: a degenerate header must never
/// brick its file unreadable.
fn restore_archive_mode(
    fs: &dyn oxdock_fs::WorkspaceFs,
    path: &GuardedPath,
    mode: u32,
) -> Result<()> {
    let mode = mode & 0o7777;
    if mode != 0 {
        fs.set_permissions_mode_unix(path, mode)?;
    }
    Ok(())
}

pub(super) fn symlink<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    from_workspace: Option<WorkspaceTarget>,
    from: &str,
    to: &str,
) -> Result<()> {
    let to_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, to)
        .map_err(|e| anyhow::Error::new(diag::eval_symlink_failed(from, to, &e)))?;
    let from_abs = if let Some(target) = from_workspace {
        cx.state
            .fs
            .resolve_copy_source_from_target(copy_source_root(target), from)
            .map_err(|e| anyhow::Error::new(diag::eval_symlink_failed(from, to, &e)))?
    } else {
        cx.state
            .fs
            .resolve_copy_source(from)
            .map_err(|e| anyhow::Error::new(diag::eval_symlink_failed(from, to, &e)))?
    };
    // `ln -s` destination semantics: a directory destination (existing,
    // or named with a trailing slash) receives the link under the
    // source basename instead of failing as "already exists".
    let to_abs = if to.ends_with('/') || to.ends_with('\\') {
        let base = link_base_name(&from_abs, from)?;
        to_abs
            .join(&base)
            .map_err(|e| anyhow::Error::new(diag::eval_symlink_failed(from, to, &e)))?
    } else if matches!(cx.state.fs.entry_kind(&to_abs), Ok(EntryKind::Dir)) {
        let base = link_base_name(&from_abs, from)?;
        to_abs
            .join(&base)
            .map_err(|e| anyhow::Error::new(diag::eval_symlink_failed(from, to, &e)))?
    } else {
        to_abs
    };
    cx.state
        .fs
        .symlink(&from_abs, &to_abs)
        .map_err(|e| anyhow::Error::new(diag::eval_symlink_failed(from, to, &e)))?;
    Ok(())
}

/// Basename of a link source for directory-destination placement. Lexical:
/// dangling sources have no entry kind to inspect.
fn link_base_name(from_abs: &GuardedPath, from: &str) -> Result<String> {
    from_abs
        .as_path()
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow::Error::new(diag::eval_symlink_no_file_name(from)))
}

pub(super) fn mkdir<P: ProcessManager>(cx: &mut StepCtx<'_, P>, path: &str) -> Result<()> {
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, path)
        .map_err(|e| anyhow::Error::new(diag::eval_mkdir_resolve_failed(path, &e)))?;
    cx.state.fs.create_dir_all(&target).map_err(|e| {
        anyhow::Error::new(diag::eval_mkdir_create_failed(
            &target.display().to_string(),
            &e,
        ))
    })?;
    Ok(())
}

pub(super) fn set_executable<P: ProcessManager>(cx: &mut StepCtx<'_, P>, path: &str) -> Result<()> {
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, path)
        .map_err(|e| anyhow::Error::new(diag::eval_set_executable_failed(path, &e)))?;
    cx.state
        .fs
        .set_executable(&target)
        .map_err(|e| anyhow::Error::new(diag::eval_set_executable_failed(path, &e)))?;
    Ok(())
}

pub(super) fn ls<P: ProcessManager>(cx: &mut StepCtx<'_, P>, arg: &Option<String>) -> Result<()> {
    let target_dir = if let Some(p) = arg {
        cx.state
            .fs
            .resolve_read(&cx.state.cwd, p)
            .map_err(|e| anyhow::Error::new(diag::eval_ls_resolve_failed(Some(p), &e)))?
    } else {
        // Bare `LS` lists the current directory: ride the choke point so a
        // pending snapshot materializes (a snapshot read), matching the
        // explicit-path branch above. Local roots resolve with zero I/O.
        cx.state
            .fs
            .resolve_read(&cx.state.cwd, ".")
            .map_err(|e| anyhow::Error::new(diag::eval_ls_resolve_failed(None, &e)))?
    };
    let mut entries = cx.state.fs.read_dir_entries(&target_dir).map_err(|e| {
        anyhow::Error::new(diag::eval_ls_read_failed(
            &target_dir.display().to_string(),
            &e,
        ))
    })?;
    entries.sort_by_key(|e| e.file_name());
    write_stdout(cx.out.clone(), |writer| {
        writeln!(writer, "{}:", target_dir.display())
            .map_err(|e| diag::eval_stream_write_failed(&e))?;
        for entry in &entries {
            writeln!(writer, "{}", entry.file_name().to_string_lossy())
                .map_err(|e| diag::eval_stream_write_failed(&e))?;
        }
        Ok(())
    })?;
    Ok(())
}

pub(super) fn read<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    path_opt: &Option<String>,
) -> oxdock_diagnostics::DiagnosticResult<()> {
    if let Some(path) = path_opt {
        let target = cx
            .state
            .fs
            .resolve_read(&cx.state.cwd, path)
            .map_err(|e| diag::eval_read_resolve_failed(path, &e))?;
        let mut reader = cx
            .state
            .fs
            .open_read(&target)
            .map_err(|e| diag::eval_read_open_failed(&target.display().to_string(), &e))?;
        write_stdout(cx.out.clone(), |writer| {
            let mut buf = [0u8; super::io::CHUNK_SIZE];
            loop {
                let n = reader
                    .read(&mut buf)
                    .map_err(|e| diag::eval_read_file_failed(&e))?;
                if n == 0 {
                    break;
                }
                writer
                    .write_all(&buf[..n])
                    .map_err(|e| diag::eval_read_write_failed(&e))?;
            }
            Ok(())
        })?;
    } else {
        let CommandStdin::Stream(input_stream) = cx.stdin.clone() else {
            return Err(diag::eval_read_needs_stdin());
        };
        let mut buf = [0u8; super::io::CHUNK_SIZE];
        loop {
            let n = {
                let mut guard = input_stream
                    .lock()
                    .map_err(|_| diag::eval_stdin_lock_failed(KEYWORD_READ))?;
                guard
                    .read(&mut buf)
                    .map_err(|e| diag::eval_stdin_read_failed(None, &e))?
            };
            if n == 0 {
                break;
            }
            write_stdout(cx.out.clone(), |writer| {
                writer
                    .write_all(&buf[..n])
                    .map_err(|e| diag::eval_read_write_failed(&e))
            })?;
        }
    }
    Ok(())
}

pub(super) fn read_line<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    var: &str,
) -> oxdock_diagnostics::DiagnosticResult<()> {
    let CommandStdin::Stream(input_stream) = cx.stdin.clone() else {
        return Err(diag::eval_read_line_needs_stdin());
    };
    let clean_var = var.trim_start_matches('$').to_string();
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = {
            let mut guard = input_stream
                .lock()
                .map_err(|_| diag::eval_stdin_lock_failed(KEYWORD_READ_LINE))?;
            guard
                .read(&mut byte)
                .map_err(|e| diag::eval_stdin_read_failed(None, &e))?
        };
        if n == 0 {
            break;
        }
        buf.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    let line = String::from_utf8(buf).map_err(|e| diag::eval_read_line_not_utf8(&e))?;
    let line = line
        .strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .unwrap_or(&line);
    let text = Value::string(line.to_string());
    if cx.state.get_var_typed(&clean_var).is_some() {
        cx.state.mutate_var(&clean_var, text)?;
    } else {
        cx.state.declare_var(clean_var, TypeTag::String, text)?;
    }
    Ok(())
}

/// Structured variable snapshot backing the `INSPECT()` expression form:
/// base keys (`type`, `variable`, `name`, `value`) plus live details :
/// pipe backend stats for `PIPE`, task phase for `HANDLE`. Undeclared
/// names are an error. (Expression evaluation carries no step index, so
/// unlike statement handlers this reports no step number.)
pub(super) fn inspect_var_map<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    var: &str,
) -> oxdock_diagnostics::DiagnosticResult<BTreeMap<String, Value>> {
    let clean_var = var.trim_start_matches('$').to_string();
    let Some((decl_type, value)) = cx.state.get_var_typed(&clean_var) else {
        return Err(diag::eval_undefined_var(&clean_var));
    };
    let mut map = BTreeMap::new();
    map.insert(
        "type".to_string(),
        Value::string(render_structural(&decl_type)),
    );
    map.insert("variable".to_string(), Value::string(clean_var.clone()));
    match (decl_type, value.as_pipe_handle()) {
        (TypeTag::Pipe, Some(handle)) => {
            let info = cx.state.io.inspect_pipe(&handle);
            map.insert("name".to_string(), Value::string(clean_var.clone()));
            map.insert("value".to_string(), Value::string(format!("{value}")));
            map.insert("is_os_pipe".to_string(), Value::bool(info.kind.is_os()));
            map.insert(
                "pipe_kind".to_string(),
                Value::string(info.kind.as_str().to_string()),
            );
            map.insert(
                "buffer_bytes".to_string(),
                Value::int(info.buffered.min(i64::MAX as u64) as i64),
            );
            map.insert("readers".to_string(), Value::int(info.readers as i64));
            map.insert("writers".to_string(), Value::int(info.writers as i64));
        }
        (TypeTag::Handle | TypeTag::HandleOf(_), _) => {
            if let Some(task_id) = value.as_handle() {
                map.insert("name".to_string(), Value::string(clean_var.clone()));
                map.insert(
                    "value".to_string(),
                    Value::string(format!("task {task_id}")),
                );
                let phase = cx
                    .state
                    .named_tasks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&task_id)
                    .map(|entry| {
                        let guard = entry.state.lock().unwrap_or_else(|e| e.into_inner());
                        match guard.phase {
                            TaskPhase::Running => "Running",
                            TaskPhase::Awaiting => "Awaiting",
                            TaskPhase::Cancelled => "Cancelled",
                            TaskPhase::Completed => "Completed",
                        }
                        .to_string()
                    })
                    .unwrap_or_else(|| "unknown (already awaited?)".to_string());
                map.insert("task_id".to_string(), Value::int(task_id as i64));
                map.insert("task_phase".to_string(), Value::string(phase));
            } else {
                map.insert("name".to_string(), Value::string(clean_var.clone()));
                map.insert("value".to_string(), Value::string(format!("{value}")));
            }
        }
        _ => {
            map.insert("name".to_string(), Value::string(clean_var.clone()));
            map.insert("value".to_string(), Value::string(format!("{value}")));
        }
    }
    Ok(map)
}

pub(super) fn write<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    path: &str,
    contents: Option<&str>,
) -> oxdock_diagnostics::DiagnosticResult<()> {
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, path)
        .map_err(|e| diag::eval_write_resolve_failed(path, &e))?;
    cx.state
        .fs
        .ensure_parent_dir(&target)
        .map_err(|e| diag::eval_ensure_parent_failed(&target.display().to_string(), &e))?;
    if let Some(body) = contents {
        cx.state
            .fs
            .write_file(&target, body.as_bytes())
            .map_err(|e| diag::eval_write_file_failed(&target.display().to_string(), &e))?;
    } else {
        let CommandStdin::Stream(input_stream) = cx.stdin.clone() else {
            return Err(diag::eval_write_needs_stdin(path));
        };
        let mut guard = input_stream
            .lock()
            .map_err(|_| diag::eval_stdin_lock_failed(KEYWORD_WRITE))?;
        let mut writer = cx
            .state
            .fs
            .open_write(&target)
            .map_err(|e| diag::eval_write_open_failed(&target.display().to_string(), &e))?;
        let mut buf = [0u8; super::io::CHUNK_SIZE];
        loop {
            let n = guard
                .read(&mut buf)
                .map_err(|e| diag::eval_stdin_read_failed(Some(KEYWORD_WRITE), &e))?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n]).map_err(|e| {
                diag::eval_write_stream_failed(
                    &target.display().to_string(),
                    &anyhow::Error::new(e),
                )
            })?;
            writer.flush().map_err(|e| {
                diag::eval_write_stream_failed(
                    &target.display().to_string(),
                    &anyhow::Error::new(e),
                )
            })?;
        }
    }
    Ok(())
}

pub(super) fn append<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    path: &str,
    contents: Option<&str>,
) -> oxdock_diagnostics::DiagnosticResult<()> {
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, path)
        .map_err(|e| diag::eval_append_resolve_failed(path, &e))?;
    cx.state
        .fs
        .ensure_parent_dir(&target)
        .map_err(|e| diag::eval_ensure_parent_failed(&target.display().to_string(), &e))?;
    if let Some(body) = contents {
        cx.state
            .fs
            .append_file(&target, body.as_bytes())
            .map_err(|e| diag::eval_append_failed(&target.display().to_string(), &e))?;
    } else {
        let CommandStdin::Stream(input_stream) = cx.stdin.clone() else {
            return Err(diag::eval_append_needs_stdin(path));
        };
        let mut guard = input_stream
            .lock()
            .map_err(|_| diag::eval_stdin_lock_failed(KEYWORD_APPEND))?;
        let mut writer = cx
            .state
            .fs
            .open_append(&target)
            .map_err(|e| diag::eval_append_open_failed(&target.display().to_string(), &e))?;
        let mut buf = [0u8; super::io::CHUNK_SIZE];
        loop {
            let n = guard
                .read(&mut buf)
                .map_err(|e| diag::eval_stdin_read_failed(Some(KEYWORD_APPEND), &e))?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n]).map_err(|e| {
                diag::eval_append_failed(&target.display().to_string(), &anyhow::Error::new(e))
            })?;
            writer.flush().map_err(|e| {
                diag::eval_append_failed(&target.display().to_string(), &anyhow::Error::new(e))
            })?;
        }
    }
    Ok(())
}

pub(super) fn replace<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    path_opt: &Option<String>,
    overrides: &[(String, String)],
) -> Result<()> {
    let ctx = cx.state.command_ctx()?;
    let vars = cx.state.all_vars();
    // Dispatch `{{ MODULE::FUNC(args) }}` through the shared pure table.
    // Known callees reuse the static-gate wording through an owned
    // snapshot: the `'static` closure cannot borrow the registry.
    let table = cx.state.pure_function_table();
    let snapshot = cx
        .state
        .functions
        .placeholder_snapshot(&super::import::active_unit_alias_targets(cx.state));
    let inner = super::native::placeholder_resolver(table);
    let calls: oxdock_process::PlaceholderCall =
        std::sync::Arc::new(move |module, func, args| match inner(module, func, args) {
            Ok(value) => Ok(value),
            Err(err) => {
                if let Some(text) = snapshot.describe(module, func) {
                    return Err(anyhow::Error::new(diag::eval_placeholder_miss_text(&text)));
                }
                Err(err)
            }
        });

    let mut expander = oxdock_process::StreamingExpand::new(overrides, ctx.envs())
        .with_vars(&vars)
        .with_call_resolver(calls);
    let mut out_buf = Vec::with_capacity(super::io::CHUNK_SIZE);

    write_stdout(cx.out.clone(), |w| {
        if let Some(path) = path_opt {
            let target = cx
                .state
                .fs
                .resolve_read(&cx.state.cwd, path)
                .map_err(|e| diag::eval_expand_resolve_failed(path, &e))?;
            let mut reader = cx
                .state
                .fs
                .open_read(&target)
                .map_err(|e| diag::eval_read_open_failed(&target.display().to_string(), &e))?;
            let mut buf = [0u8; super::io::CHUNK_SIZE];
            loop {
                let n = reader.read(&mut buf).map_err(|e| {
                    diag::eval_expand_read_failed(
                        &target.display().to_string(),
                        &anyhow::Error::new(e),
                    )
                })?;
                if n == 0 {
                    break;
                }
                expander.process_bytes(&buf[..n], &mut out_buf)?;
                w.write_all(&out_buf)
                    .map_err(|e| diag::eval_expand_write_failed(&e))?;
                out_buf.clear();
            }
        } else {
            let CommandStdin::Stream(input_stream) = cx.stdin.clone() else {
                return Err(diag::eval_expand_needs_stdin());
            };
            let mut guard = input_stream
                .lock()
                .map_err(|_| diag::eval_stdin_lock_failed(KEYWORD_EXPAND))?;
            let mut buf = [0u8; super::io::CHUNK_SIZE];
            loop {
                let n = guard
                    .read(&mut buf)
                    .map_err(|e| diag::eval_stdin_read_failed(None, &e))?;
                if n == 0 {
                    break;
                }
                expander.process_bytes(&buf[..n], &mut out_buf)?;
                w.write_all(&out_buf)
                    .map_err(|e| diag::eval_expand_write_failed(&e))?;
                out_buf.clear();
            }
        }

        expander.flush(&mut out_buf)?;
        if !out_buf.is_empty() {
            w.write_all(&out_buf)
                .map_err(|e| diag::eval_expand_write_failed(&e))?;
            out_buf.clear();
        }

        Ok(())
    })?;

    Ok(())
}

/// Strict equality/inequality assertion over evaluated values, stream
/// buffers, and pipe buffers. Typed `Value` comparison with no coercion;
/// files never appear here (read them into variables first). `--hash`
/// compares the SHA-256 of a string, pipe, or captured-stdout actual
/// instead of the raw bytes (`stderr` is unsupported).
///
/// `expect_equal` selects the polarity: true is `ASSERT_EQ`, false is
/// `ASSERT_NE`. All target resolution, Mode-1 draining, and digest logic
/// lives here once; the two verbs differ only in pass/fail sense and
/// error text.
#[allow(clippy::too_many_arguments)]
pub(super) fn assert_equality<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    generation: usize,
    verb: &str,
    expect_equal: bool,
    hash: &Option<String>,
    actual: &super::steps::ResolvedAssertTarget,
    expected: Option<&Value>,
) -> Result<()> {
    use super::steps::ResolvedAssertTarget;
    // Mode 1: exact stdout with piped stdin consumes the stream, mirroring
    // the legacy substring behavior, but compares exact bytes.
    if let ResolvedAssertTarget::Stdout = actual
        && let CommandStdin::Stream(input_stream) = cx.stdin.clone()
    {
        let drained = drain_stdin_bounded(input_stream, verb)?;
        super::io::write_stdout(cx.out.clone(), |w| {
            w.write_all(&drained)
                .map_err(|e| diag::eval_stream_write_failed(&e))?;
            Ok(())
        })?;
        if let Some(sha) = hash {
            // Hash form carries no positional expected: digest the
            // drained stream and compare with the selected polarity.
            let computed = sha256_hex(&drained);
            let is_equal = computed.eq_ignore_ascii_case(sha);
            if is_equal != expect_equal {
                if expect_equal {
                    return Err(anyhow::Error::new(diag::eval_assert_hash_mismatch(
                        verb, sha, &computed,
                    )));
                } else {
                    return Err(anyhow::Error::new(diag::eval_assert_hash_match(verb, sha)));
                }
            }
            return Ok(());
        }
        let Some(want) = expected.and_then(|v| v.as_str()) else {
            return Err(anyhow::Error::new(diag::eval_assert_stream_mismatch(
                verb,
                expected,
                &String::from_utf8_lossy(&drained),
            )));
        };
        let is_equal = drained == want.as_bytes();
        if is_equal != expect_equal {
            if expect_equal {
                return Err(anyhow::Error::new(diag::eval_assert_stream_mismatch_text(
                    verb,
                    want,
                    &String::from_utf8_lossy(&drained),
                )));
            } else {
                return Err(anyhow::Error::new(
                    diag::eval_assert_stream_unexpected_equal(verb, want),
                ));
            }
        }
        return Ok(());
    }
    if let Some(sha) = hash {
        let actual_bytes: Vec<u8> = match actual {
            ResolvedAssertTarget::Value(v) => match v.as_str() {
                Some(s) => s.as_bytes().to_vec(),
                None => {
                    return Err(anyhow::Error::new(diag::eval_assert_hash_needs_string(
                        verb, v,
                    )));
                }
            },
            ResolvedAssertTarget::Pipe(bytes) => bytes.clone(),
            ResolvedAssertTarget::Stdout => exact_stdout_bytes(cx, generation, verb)?,
            ResolvedAssertTarget::Stderr => {
                return Err(anyhow::Error::new(diag::eval_assert_stderr_unsupported(
                    verb,
                )));
            }
        };
        let computed = sha256_hex(&actual_bytes);
        let is_equal = computed.eq_ignore_ascii_case(sha);
        if is_equal != expect_equal {
            if expect_equal {
                return Err(anyhow::Error::new(diag::eval_assert_hash_mismatch(
                    verb, sha, &computed,
                )));
            } else {
                return Err(anyhow::Error::new(diag::eval_assert_hash_match(verb, sha)));
            }
        }
        return Ok(());
    }
    match actual {
        ResolvedAssertTarget::Value(got) => {
            let Some(want) = expected else {
                return Err(anyhow::Error::new(diag::eval_assert_requires_expected(
                    verb,
                )));
            };
            let is_equal = got == want;
            if is_equal != expect_equal {
                if expect_equal {
                    return Err(anyhow::Error::new(diag::eval_assert_value_mismatch(
                        verb, want, got,
                    )));
                } else {
                    return Err(anyhow::Error::new(
                        diag::eval_assert_value_unexpected_equal(verb, want, got),
                    ));
                }
            }
            Ok(())
        }
        ResolvedAssertTarget::Stdout => {
            let bytes = exact_stdout_bytes(cx, generation, verb)?;
            let Some(want) = expected.and_then(|v| v.as_str()) else {
                return Err(anyhow::Error::new(diag::eval_assert_stream_mismatch(
                    verb,
                    expected,
                    &String::from_utf8_lossy(&bytes),
                )));
            };
            let is_equal = bytes == want.as_bytes();
            if is_equal != expect_equal {
                if expect_equal {
                    return Err(anyhow::Error::new(diag::eval_assert_stream_mismatch_text(
                        verb,
                        want,
                        &String::from_utf8_lossy(&bytes),
                    )));
                } else {
                    return Err(anyhow::Error::new(
                        diag::eval_assert_stream_unexpected_equal(verb, want),
                    ));
                }
            }
            Ok(())
        }
        ResolvedAssertTarget::Stderr => Err(anyhow::Error::new(
            diag::eval_assert_stderr_unsupported(verb),
        )),
        ResolvedAssertTarget::Pipe(bytes) => {
            let actual_str = String::from_utf8(bytes.clone())
                .map_err(|e| anyhow::Error::new(diag::eval_pipe_not_utf8(verb, &e)))?;
            let Some(want) = expected.and_then(|v| v.as_str()) else {
                return Err(anyhow::Error::new(diag::eval_assert_pipe_mismatch(
                    verb,
                    expected,
                    &actual_str,
                )));
            };
            let is_equal = actual_str == want;
            if is_equal != expect_equal {
                if expect_equal {
                    return Err(anyhow::Error::new(diag::eval_assert_pipe_mismatch_text(
                        verb,
                        want,
                        &actual_str,
                    )));
                } else {
                    return Err(anyhow::Error::new(diag::eval_assert_pipe_unexpected_equal(
                        verb, want,
                    )));
                }
            }
            Ok(())
        }
    }
}

/// Drain a piped stdin fully while enforcing the exact-match byte cap.
/// Shared by `ASSERT_EQ`/`ASSERT_NE stdout` Mode 1.
fn drain_stdin_bounded(input_stream: SharedInput, verb: &str) -> Result<Vec<u8>> {
    let mut guard = input_stream
        .lock()
        .map_err(|_| anyhow::Error::new(diag::eval_stdin_lock_failed(verb)))?;
    let mut out = Vec::new();
    let mut buf = [0u8; super::io::CHUNK_SIZE];
    loop {
        let n = guard
            .read(&mut buf)
            .map_err(|e| anyhow::Error::new(diag::eval_stdin_read_failed(Some(verb), &e)))?;
        if n == 0 {
            break;
        }
        if out.len() + n > super::io::EXACT_STDOUT_CAP {
            return Err(anyhow::Error::new(diag::eval_assert_stdin_overflow(verb)));
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

/// Read the trial-cumulative exact stdout bytes for this generation,
/// or bail with remediation guidance on overflow / missing capture.
fn exact_stdout_bytes<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    generation: usize,
    verb: &str,
) -> Result<Vec<u8>> {
    let captures = cx
        .state
        .exact_stdout
        .lock()
        .map_err(|_| anyhow::Error::new(diag::std_assert_lock_poisoned("exact_stdout")))?;
    match captures.get(&generation) {
        Some(entry) if entry.overflowed => {
            Err(anyhow::Error::new(diag::eval_assert_stdout_overflow(verb)))
        }
        Some(entry) => Ok(entry.bytes.clone()),
        None => Err(anyhow::Error::new(diag::eval_assert_no_exact_capture(verb))),
    }
}

/// Containment assertion over values, streams, and pipes: substring for
/// strings, exact-element match for lists, key presence for maps,
/// substring over stream and pipe buffers. Like `assert_eq`, files never
/// appear here; read them into variables first.
pub(super) fn assert_contains<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    generation: usize,
    idx_step: usize,
    haystack: &super::steps::ResolvedAssertTarget,
    needle: &Arg,
) -> Result<()> {
    use super::steps::ResolvedAssertTarget;
    let verb = Command::AssertContains.as_str();
    let needle_str = super::args::resolve_arg(needle, cx)?;
    match haystack {
        ResolvedAssertTarget::Value(v) => {
            if let Some(hay) = v.as_str() {
                if hay.contains(needle_str.as_str()) {
                    return Ok(());
                }
                return Err(anyhow::Error::new(diag::eval_assert_contains_str_mismatch(
                    verb,
                    &needle_str,
                    hay,
                )));
            }
            if let Some(items) = v.as_list() {
                if items
                    .iter()
                    .any(|e| e.as_str() == Some(needle_str.as_str()))
                {
                    return Ok(());
                }
                return Err(anyhow::Error::new(
                    diag::eval_assert_contains_value_mismatch(verb, &needle_str, v),
                ));
            }
            if let Some(map) = v.as_map() {
                if map.contains_key(needle_str.as_str()) {
                    return Ok(());
                }
                return Err(anyhow::Error::new(diag::eval_assert_contains_missing(
                    verb,
                    &needle_str,
                )));
            }
            Err(anyhow::Error::new(
                diag::eval_assert_contains_needs_haystack(verb, v),
            ))
        }
        ResolvedAssertTarget::Stdout => {
            // Mode 1: piped stdin is consumed through a local window and
            // forwarded, exactly like the legacy substring check.
            if let CommandStdin::Stream(input_stream) = cx.stdin.clone() {
                let mut guard = input_stream
                    .lock()
                    .map_err(|_| anyhow::Error::new(diag::eval_stdin_lock_failed(verb)))?;
                let mut window = super::io::SlidingWindow::new(needle_str.as_bytes().to_vec());
                let mut buf = [0u8; super::io::CHUNK_SIZE];
                let mut read_any = false;
                loop {
                    let n = guard.read(&mut buf).map_err(|e| {
                        anyhow::Error::new(diag::eval_stdin_read_failed(Some(verb), &e))
                    })?;
                    if n == 0 {
                        break;
                    }
                    read_any = true;
                    window.push_chunk(&buf[..n]);
                    super::io::write_stdout(cx.out.clone(), |w| {
                        w.write_all(&buf[..n])
                            .map_err(|e| diag::eval_stream_write_failed(&e))?;
                        Ok(())
                    })?;
                }
                if read_any {
                    if window.matched {
                        return Ok(());
                    }
                    let emitted = String::from_utf8_lossy(&window.ring_buffer()).into_owned();
                    return Err(anyhow::Error::new(
                        diag::eval_assert_contains_emitted_mismatch(verb, &needle_str, &emitted),
                    ));
                }
            }
            // Mode 2: pre-registered stdout window for this step.
            let windows = cx.state.assert_windows.lock().map_err(|_| {
                anyhow::Error::new(diag::std_assert_lock_poisoned("assert_windows"))
            })?;
            match windows.get(&(generation, idx_step)) {
                Some(w) if w.matched => Ok(()),
                Some(w) => {
                    let emitted = String::from_utf8_lossy(&w.ring_buffer()).into_owned();
                    Err(anyhow::Error::new(
                        diag::eval_assert_contains_emitted_mismatch(verb, &needle_str, &emitted),
                    ))
                }
                _ => Err(anyhow::Error::new(diag::eval_assert_contains_missing(
                    verb,
                    &needle_str,
                ))),
            }
        }
        ResolvedAssertTarget::Stderr => {
            // Stderr has no piped-stdin consumption mode: stdin bytes
            // belong to a different stream and are left alone.
            let windows = cx.state.assert_windows_stderr.lock().map_err(|_| {
                anyhow::Error::new(diag::std_assert_lock_poisoned("assert_windows_stderr"))
            })?;
            match windows.get(&(generation, idx_step)) {
                Some(w) if w.matched => Ok(()),
                Some(w) => {
                    let emitted = String::from_utf8_lossy(&w.ring_buffer()).into_owned();
                    Err(anyhow::Error::new(
                        diag::eval_assert_contains_emitted_mismatch(verb, &needle_str, &emitted),
                    ))
                }
                _ => Err(anyhow::Error::new(diag::eval_assert_contains_missing(
                    verb,
                    &needle_str,
                ))),
            }
        }
        ResolvedAssertTarget::Pipe(bytes) => {
            let hay = String::from_utf8(bytes.clone())
                .map_err(|e| anyhow::Error::new(diag::eval_pipe_not_utf8(verb, &e)))?;
            if hay.contains(needle_str.as_str()) {
                Ok(())
            } else {
                Err(anyhow::Error::new(
                    diag::eval_assert_contains_pipe_mismatch(verb, &needle_str, &hay),
                ))
            }
        }
    }
}

pub(crate) fn with_io_block<P: ProcessManager>(
    _cx: &mut StepCtx<'_, P>,
    _generation: usize,
    _idx: usize,
    _bindings: &[IoBinding],
) -> Result<()> {
    Err(anyhow::Error::new(diag::eval_with_io_unexpanded()))
}

/// True when the wrapped command is a lone `RUN`: the only consumer or
/// producer shape that can hold an OS handle directly.
fn is_single_run(cmd: &StepKind) -> bool {
    matches!(cmd, StepKind::Run(_) | StepKind::RunExec { .. })
}

/// True when the wrapped command is an `ASYNC` block or task whose body is
/// exactly one `RUN`, guarded or not.
fn async_single_run_body(cmd: &StepKind) -> bool {
    match cmd {
        StepKind::AsyncBlock { body } => {
            matches!(body.as_slice(), [step] if is_single_run(&step.kind))
        }
        StepKind::Bind {
            rhs: oxdock_parser::BindRhs::Async(body),
            ..
        } => matches!(body.as_slice(), [step] if is_single_run(&step.kind)),
        _ => false,
    }
}

/// Whether this binding resolves to a zero copy OS handle instead of a
/// bridged shared handle: the ultimate consumer or producer is a `RUN`.
/// Promotion itself is decided per handle by declaration scope
/// (`can_promote_to_os`); this only picks take vs bridge on OS-decided
/// handles.
fn run_terminated(cmd: &StepKind) -> bool {
    is_single_run(cmd) || async_single_run_body(cmd)
}

pub(crate) fn with_io<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    generation: usize,
    idx: usize,
    bindings: &[IoBinding],
    cmd: &StepKind,
) -> Result<Flow> {
    let (step_stdin, next_expose_stdin, step_stdout, step_stderr, out_pipe, stdin_pipe) =
        resolve_io_streams(cx, bindings, cmd)?;
    let tty = tty_request(bindings, cmd)?;

    super::steps::execute_single_step_with_generation(
        cx.state,
        cx.process,
        cmd,
        generation,
        idx,
        step_stdin,
        next_expose_stdin,
        step_stdout,
        step_stderr,
        out_pipe,
        stdin_pipe,
        tty,
    )
}

/// Extract the pty request from `WITH_IO` bindings, enforcing the
/// terminal rules every construction shares (direct steps and
/// block-expanded defaults alike reach this through `with_io`, so the
/// parser's span-carrying checks stay advisory while these decide).
/// Returns `None` for terminal-unaware steps, keeping hermetic pipes.
/// Geometry stays unresolved here: the spawn reads the host terminal
/// for unnamed sides, so a size change between steps is honored.
fn tty_request(bindings: &[IoBinding], cmd: &StepKind) -> Result<Option<TtyRequest>> {
    let mut tty = false;
    let mut rows = None;
    let mut cols = None;
    for binding in bindings {
        match binding.stream {
            IoStream::Tty => tty = true,
            IoStream::Rows(value) => rows = Some(value),
            IoStream::Cols(value) => cols = Some(value),
            IoStream::Stdin | IoStream::Stdout | IoStream::Stderr => {}
        }
    }
    if !tty {
        if rows.is_some() || cols.is_some() {
            return Err(anyhow::Error::new(diag::eval_with_io_geometry_needs_tty()));
        }
        return Ok(None);
    }
    if bindings
        .iter()
        .any(|b| matches!(b.stream, IoStream::Stderr))
    {
        return Err(anyhow::Error::new(diag::eval_with_io_tty_no_stderr()));
    }
    match cmd {
        StepKind::Run(_) | StepKind::RunExec { .. } => Ok(Some(TtyRequest { rows, cols })),
        _ => Err(anyhow::Error::new(diag::eval_with_io_tty_needs_run())),
    }
}

/// Resolve `WITH_IO` bindings against the active context handles, shared by
/// `with_io` and the `CALL` capture path (`LET`-capture) so a `CALL`
/// under `WITH_IO` layers observes identical stream wiring whether it
/// runs inline or for its return value.
///
/// Besides the runnable streams this returns the script backends behind
/// the stdin/stdout bindings, which enrich the step context for
/// timeout-bounded bridge reads and the socket-EOF force-close.
#[allow(clippy::type_complexity)]
fn resolve_io_streams<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    bindings: &[IoBinding],
    cmd: &StepKind,
) -> Result<(
    CommandStdin,
    bool,
    Option<StreamHandle>,
    Option<StreamHandle>,
    Option<Arc<PipeInner>>,
    Option<Arc<PipeInner>>,
)> {
    let mut step_stdin = CommandStdin::Null;
    let mut step_stdout = cx.out.clone();
    let mut step_stderr = cx.err.clone();
    let mut out_pipe = cx.out_pipe.clone();
    let mut stdin_pipe = cx.stdin_pipe.clone();
    let mut next_expose_stdin = false;
    let mut seen_stdin = false;
    let mut seen_stdout = false;
    let mut seen_stderr = false;
    let mut seen_tty = false;
    let mut seen_rows = false;
    let mut seen_cols = false;
    // `direct` (take vs bridged-shared) still judges by the ultimate
    // command: only RUN consumes raw descriptors. Promotion, in contrast,
    // is decided per handle below by declaration scope : and only on
    // worker threads. Main-flow bindings never promote: the main thread
    // is the universal sharer (it spawns every task), so anything it
    // decided could race a future share. Workers promote only their own
    // unshared pipes via the spawn-time pin walk.
    let direct = run_terminated(cmd);
    let allow_promote = cx.state.inside_async;
    // A pty master is one interleaved byte stream: its stdio stays on
    // script backends, never on zero copy OS descriptors. The pin walk
    // already skips terminal-bound handles; suppressing promotion here
    // as well keeps a handle an outer scope promoted usable through the
    // take-once bridge instead of a raw descriptor handoff.
    let tty_present = bindings.iter().any(|b| matches!(b.stream, IoStream::Tty));
    // Under a terminal the pin walk leaves stdio undecided, so force
    // script backends here (`direct` only judges take vs bridge on
    // already-OS handles, which then bridge take-once like DSL
    // consumers instead of handing the child a raw descriptor).
    let direct = direct && !tty_present;

    for binding in bindings {
        if let Some(target) = &binding.pipe {
            let handle = resolve_pipe_handle(cx, target)?;
            // Main-flow bindings materialize here; task-body bindings were
            // decided by the spawn-time pin walk, making this a no-op
            // there. Either way the scope check, not the command, rules.
            let promote = allow_promote && !tty_present && cx.state.can_promote_to_os(&handle);
            cx.state.io.ensure_handle(&handle, promote)?;
        }
        match binding.stream {
            IoStream::Stdin => {
                if seen_stdin {
                    return Err(anyhow::Error::new(diag::eval_with_io_duplicate_stream(
                        "stdin",
                    )));
                }
                seen_stdin = true;
                next_expose_stdin = true;
                (step_stdin, stdin_pipe) = if let Some(target) = &binding.pipe {
                    let handle = resolve_pipe_handle(cx, target)?;
                    let promote =
                        allow_promote && !tty_present && cx.state.can_promote_to_os(&handle);
                    let (stdin, backend) = cx.state.io.resolve_stdin(&handle, direct, promote)?;
                    (stdin, backend)
                } else {
                    (cx.stdin.clone(), None)
                };
            }
            IoStream::Stdout => {
                if seen_stdout {
                    return Err(anyhow::Error::new(diag::eval_with_io_duplicate_stream(
                        "stdout",
                    )));
                }
                seen_stdout = true;
                (step_stdout, out_pipe) = if let Some(target) = &binding.pipe {
                    let handle = resolve_pipe_handle(cx, target)?;
                    let promote =
                        allow_promote && !tty_present && cx.state.can_promote_to_os(&handle);
                    let (stdout, backend) = cx.state.io.resolve_stdout(&handle, direct, promote)?;
                    (Some(stdout), backend)
                } else {
                    (cx.out.clone(), cx.out_pipe.clone())
                };
            }
            IoStream::Stderr => {
                if seen_stderr {
                    return Err(anyhow::Error::new(diag::eval_with_io_duplicate_stream(
                        "stderr",
                    )));
                }
                seen_stderr = true;
                step_stderr = if let Some(target) = &binding.pipe {
                    let handle = resolve_pipe_handle(cx, target)?;
                    let promote =
                        allow_promote && !tty_present && cx.state.can_promote_to_os(&handle);
                    Some(cx.state.io.resolve_stderr(&handle, direct, promote)?)
                } else {
                    cx.err.clone()
                };
            }
            IoStream::Tty => {
                if seen_tty {
                    return Err(anyhow::Error::new(diag::eval_with_io_duplicate_stream(
                        "tty",
                    )));
                }
                seen_tty = true;
            }
            IoStream::Rows(_) => {
                if seen_rows {
                    return Err(anyhow::Error::new(diag::eval_with_io_duplicate_stream(
                        "rows",
                    )));
                }
                seen_rows = true;
            }
            IoStream::Cols(_) => {
                if seen_cols {
                    return Err(anyhow::Error::new(diag::eval_with_io_duplicate_stream(
                        "cols",
                    )));
                }
                seen_cols = true;
            }
        }
    }

    Ok((
        step_stdin,
        next_expose_stdin,
        step_stdout,
        step_stderr,
        out_pipe,
        stdin_pipe,
    ))
}

/// Resolve a `WITH_IO` pipe endpoint to the owned backend cell. The
/// endpoint is always a `$var` holding a `PIPE` value; declaration never
/// pre-registers anything, and materialization happens at binding sites.
fn resolve_pipe_handle<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    target: &PipeTarget,
) -> Result<PipeHandle> {
    let PipeTarget::Var { name: var } = target;
    let _ = super::args::load_var_binding(cx.state, var)?;
    match cx.state.get_var_typed(var) {
        Some((kind, value)) if matches!(kind, TypeTag::Pipe) => {
            let Some(handle) = value.as_pipe_handle() else {
                return Err(anyhow::Error::new(diag::eval_pipe_handle_mismatch(
                    kind.name(),
                    &value,
                )));
            };
            Ok(handle)
        }
        Some((kind, value)) => Err(anyhow::Error::new(diag::eval_pipe_handle_mismatch(
            kind.name(),
            &value,
        ))),
        None => Err(anyhow::Error::new(diag::eval_undeclared(var))),
    }
}

/// Peel a `LET $x: T = <command>` capture body to its value source,
/// mirroring the runtime binding rule: a `Call` (or single `IMPORT`)
/// binds its `RETURN` value; anything else binds captured stdout
/// text. `WITH_IO` layers merge outermost-first (inner wins per
/// stream); `TIMEOUT` layers peel through single-command bodies with
/// the tightest deadline winning, so a `TIMEOUT`-wrapped command
/// captures exactly what the bare command captures — the deadline
/// bounds the wait, never the value. Multi-step bodies stop the
/// peel and take the stdout-sink path with the `TIMEOUT` step
/// intact (deadline enforced there).
enum PeeledCapture<'a> {
    Call {
        bindings: Vec<IoBinding>,
        name: &'a str,
        args: &'a [Expr],
        deadline: Option<std::time::Duration>,
    },
    Import {
        bindings: Vec<IoBinding>,
        path: &'a str,
        alias: &'a str,
        deadline: Option<std::time::Duration>,
    },
    Sink,
}

fn merge_io_layers(layers: &[&Vec<IoBinding>]) -> Vec<IoBinding> {
    let mut merged: Vec<IoBinding> = Vec::new();
    for layer in layers {
        for binding in layer.iter() {
            match merged.iter_mut().find(|m| m.stream == binding.stream) {
                Some(slot) => *slot = binding.clone(),
                None => merged.push(binding.clone()),
            }
        }
    }
    merged
}

fn peel_capture_command<'a, P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    cmd: &'a StepKind,
) -> Result<PeeledCapture<'a>> {
    let mut layers: Vec<&Vec<IoBinding>> = Vec::new();
    let mut deadline: Option<std::time::Duration> = None;
    let mut current = cmd;
    loop {
        match current {
            StepKind::WithIo { bindings, cmd } => {
                layers.push(bindings);
                current = cmd;
            }
            StepKind::Timeout { duration, body } if body.len() == 1 => {
                let dur = super::args::resolve_arg_as_duration(duration, cx)?;
                deadline = Some(match deadline {
                    Some(d) => d.min(dur),
                    None => dur,
                });
                current = &body[0].kind;
            }
            StepKind::Call { name, args } => {
                return Ok(PeeledCapture::Call {
                    bindings: merge_io_layers(&layers),
                    name,
                    args,
                    deadline,
                });
            }
            StepKind::ImportFile { path, alias } => {
                return Ok(PeeledCapture::Import {
                    bindings: merge_io_layers(&layers),
                    path,
                    alias,
                    deadline,
                });
            }
            _ => return Ok(PeeledCapture::Sink),
        }
    }
}

pub(super) fn exit<P: ProcessManager>(cx: &mut StepCtx<'_, P>, code: i64) -> Result<()> {
    for child in cx.state.bg_children.iter_mut() {
        if let Ok(None) = child.handle.try_wait() {
            let _ = child.handle.kill();
            let _ = child.handle.try_wait();
        }
    }
    cx.state.bg_children.clear();
    Err(anyhow::Error::new(diag::eval_exit_requested(code)))
}

pub(crate) fn for_loop<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    key_var: Option<&str>,
    key_type: Option<String>,
    val_var: &str,
    val_type: String,
    in_expr: &Expr,
    body: &[Step],
) -> Result<Flow> {
    let iterable = super::args::evaluate_expr(in_expr, cx)?;
    let clean_val_var = val_var.trim_start_matches('$').to_string();
    let val_tag = cx.state.resolve_tag(&val_type)?;

    if let Some(items) = iterable.as_list() {
        for (i, item) in items.iter().enumerate() {
            // Each iteration is a scope (same rule as every other
            // block): loop vars live inside it, and ENV/WORKDIR/
            // WORKSPACE mutations revert on every iteration boundary.
            cx.state.push_scope();
            if let Some(idx_name) = key_var {
                let clean_idx = idx_name.trim_start_matches('$').to_string();
                let kt = match &key_type {
                    Some(name) => cx.state.resolve_tag(name)?,
                    None => TypeTag::Int,
                };
                cx.state.declare_var(
                    clean_idx,
                    kt.clone(),
                    super::args::coerce_value(Value::int(i as i64), &kt)?,
                )?;
            }
            cx.state.declare_var(
                clean_val_var.clone(),
                val_tag.clone(),
                super::args::coerce_value(item.clone(), &val_tag)?,
            )?;

            let res = super::steps::execute_steps(
                cx.state,
                cx.process,
                body,
                cx.stdin.clone(),
                false,
                cx.out.clone(),
                cx.err.clone(),
                cx.out_pipe.clone(),
                cx.stdin_pipe.clone(),
                false,
            );
            let pop_res = cx.state.pop_scope().map_err(anyhow::Error::from);
            let flow = match (res, pop_res) {
                (Ok(flow), Ok(())) => flow,
                (Err(e), _) => return Err(e),
                (Ok(_), Err(e)) => return Err(e),
            };
            match flow {
                Flow::Done | Flow::Continue { .. } => {}
                Flow::Break { .. } => return Ok(Flow::Done),
                Flow::Return { .. } => return Ok(flow),
            }
        }
        return Ok(Flow::Done);
    }
    if let Some(map) = iterable.as_map() {
        let key_name =
            key_var.ok_or_else(|| anyhow::Error::new(diag::eval_for_needs_key_value()))?;
        let clean_key_var = key_name.trim_start_matches('$').to_string();
        let mut keys: Vec<_> = map.keys().cloned().collect();
        keys.sort();

        // Map keys are strings: only a STRING key binding is valid here.
        let kt = match &key_type {
            Some(name) => cx.state.resolve_tag(name)?,
            None => TypeTag::String,
        };
        if !matches!(kt, TypeTag::String) {
            return Err(anyhow::Error::new(diag::eval_for_map_key_type(kt.name())));
        }
        for k in keys {
            let v = map[&k].clone();
            cx.state.push_scope();
            cx.state.declare_var(
                clean_key_var.clone(),
                TypeTag::String,
                Value::string(k.clone()),
            )?;
            cx.state.declare_var(
                clean_val_var.clone(),
                val_tag.clone(),
                super::args::coerce_value(v, &val_tag)?,
            )?;

            let res = super::steps::execute_steps(
                cx.state,
                cx.process,
                body,
                cx.stdin.clone(),
                false,
                cx.out.clone(),
                cx.err.clone(),
                cx.out_pipe.clone(),
                cx.stdin_pipe.clone(),
                false,
            );
            let pop_res = cx.state.pop_scope().map_err(anyhow::Error::from);
            let flow = match (res, pop_res) {
                (Ok(flow), Ok(())) => flow,
                (Err(e), _) => return Err(e),
                (Ok(_), Err(e)) => return Err(e),
            };
            match flow {
                Flow::Done | Flow::Continue { .. } => {}
                Flow::Break { .. } => return Ok(Flow::Done),
                Flow::Return { .. } => return Ok(flow),
            }
        }
        return Ok(Flow::Done);
    }
    Err(anyhow::Error::new(diag::eval_for_needs_list_or_map(
        &iterable,
    )))
}

/// Define a user function (`FUNC NAME($p: TYPE, ...) { ... }}).
/// Copy-on-write into a fresh registry Arc so `fork()` sharers keep the
/// old view, while `push_scope`/`pop_scope` snapshots revert nested
/// definitions on block exit.
pub(crate) fn define_func<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    name: &str,
    params: &[(String, String, bool)],
    return_type: &str,
    is_async: bool,
    body: &[Step],
) -> Result<()> {
    // Resolve param and return type names where the directory lives, so the
    // registry only ever holds tags. Unknown names fail here naming
    // the function and every known type and schema.
    let resolved = super::resolve_script_params(cx.state, name, params)?;
    let return_tag = cx.state.resolve_tag(return_type)?;
    cx.state
        .check_return_tag(&format!("function `{name}` returns"), &return_tag)?;
    // Runtime counterpart of the parse-time scope validation; the single
    // registry owns the reserved, duplicate, and shadowing rules. The
    // owner is the innermost module on the stack (lexically enclosing
    // file), or `None` for entry definitions, which stay params-only.
    let owner = cx.state.current_module();
    // Definition-site jail (#322): sealed here, at the definition,
    // never at the call, and only for file-top-level definitions.
    // File-top means the innermost call/init frame floor plus one
    // scope: entry top-level runs at depth one with no floor, module
    // init top-level runs one above its floor, and any nested block
    // or call body runs deeper. Block-scoped `FUNC`s never capture
    // unwound workspace state. Module init resets `call_depth` to
    // zero across its body, so a module first imported inside a
    // `FUNC` still seals its top-level jails.
    // Sealed into the registry entry for direct hits and into the
    // owning file frame for frame-walk hits, so both lookup paths
    // serve the same site. Harvest registration and call-time
    // materialization pass `None` at their own sites: importer and
    // caller context must never leak into the defining file.
    let at_file_top = cx.state.call_depth == 0
        && cx.state.functions.scope_depth()
            == cx.state.func_floors.last().copied().unwrap_or(0) + 1;
    let def_jail = at_file_top.then(|| (cx.state.fs.selection(), cx.state.cwd.clone()));
    cx.state.functions.define_script(
        name,
        &resolved,
        return_tag,
        return_type,
        is_async,
        body,
        owner.as_deref(),
        def_jail.clone(),
    )?;
    if let Some(jail) = def_jail {
        // One unit lookup: the entry record lives under the sentinel
        // like any module, so jail sealing shares the frame-walk view.
        let bucket = cx
            .state
            .modules
            .get_mut(super::import::unit_key(owner.as_deref()))
            .map(|record| &mut record.top_funcs);
        if let Some(defs) = bucket.and_then(|tops| tops.get_mut(name))
            && let Some(raw) = defs.iter_mut().find(|r| {
                r.jail.is_none()
                    && r.params == params
                    && oxdock_parser::guard_option_allows(r.guard.as_ref(), &cx.state.envs)
            })
        {
            raw.jail = Some(jail.clone());
        }
        // Canonical slots are registered at import time, before the
        // defining file runs: seal the first top-level definition
        // into them too, so qualified cross-module calls serve the
        // same site as frame-walk hits.
        if let Some(file) = owner.as_deref() {
            let canonical = super::native::file_func_key(file, name);
            cx.state.functions.seal_canonical_jail(&canonical, jail);
        }
    }
    Ok(())
}

/// Build a script [`FuncEntry`](super::native::FuncEntry) from a
/// file-frame definition (#322): the single construction site for
/// frame-walk hits, so arity, tags, and metadata can never disagree
/// across lookup paths.
fn build_script_entry<P: ProcessManager>(
    state: &mut ExecState<P>,
    name: &str,
    raw: &RawFuncDef,
    module: Option<String>,
) -> Result<super::native::FuncEntry<P>> {
    let resolved_params = super::native::resolve_script_params(state, name, &raw.params)?;
    let return_tag = state
        .resolve_tag(&raw.return_spelling)
        .map_err(|err| anyhow::Error::new(diag::eval_call_return_tag_failed(name, err)))?;
    state.check_return_tag(&format!("function `{name}` returns"), &return_tag)?;
    Ok(super::native::FuncEntry {
        meta: super::native::FuncMeta {
            name: name.to_string(),
            module: module.clone().unwrap_or_default(),
            kind: super::native::FuncKind::Script,
            params: Some(
                resolved_params
                    .iter()
                    .map(|(pname, param_type, optional)| super::native::FuncParam {
                        name: pname.clone(),
                        param_type: Some(param_type.clone()),
                        allowed: None,
                        docs: "",
                        options: None,
                        optional: *optional,
                    })
                    .collect(),
            ),
            returns: Some(return_tag.clone()),
            rpn: false,
            summary: "DSL-defined function.",
            docs: "Defined via FUNC in script.",
            examples: &[],
        },
        body: FuncBody::Script(super::native::FuncDefData {
            params: resolved_params,
            return_type: return_tag,
            return_spelling: raw.return_spelling.clone(),
            is_async: raw.is_async,
            body: raw.body.clone(),
            module,
            // Sealed at definition execution, carried here: the frame
            // walk serves the def as its file left it, never caller
            // ambient. `None` for never-executed defs (forward refs,
            // skipped harvests); the file fallback applies instead.
            def_jail: raw.jail.clone(),
        }),
    })
}

/// Invoke a function by UPPERCASE name and return its value. One lookup
/// against the unified registry, then gates, then effects: depth budget
/// and arity (mandatory metadata on every entry) validate before any
/// argument evaluates. Script bodies run in a fresh lexical scope
/// (LET/ENV/WORKDIR revert; pipes and files persist) with `declare_var`
/// coercion on parameter binding; `RETURN` inside yields the value and
/// fallthrough yields `""`. `BREAK`/`CONTINUE` escaping the body are
/// boundary errors (they must not reach a caller loop).
/// Resolve one call-site spelling to its registry entry: file-aliased
/// spellings through the shared alias-target matcher first, then the
/// registry as spelled for hosts and natives (script bodies never
/// serve there), then the frame walk down to the call/init floor, the
/// owning file frame, and the per-unit harvest map. Entry calls walk
/// from frame 0 (every live entry frame is entry-owned: inits pop,
/// harvests are explicit), while module calls stop at their boundary
/// so caller frames are unreachable by construction. No owner tags,
/// no step scans: the frames are the mechanism. Single owner for call
/// resolution; statement calls, expression calls, and RPN math route
/// here instead of restating the order.
pub(crate) fn resolve_call_entry<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    name: &str,
) -> Result<super::native::FuncEntry<P>> {
    let canonical = cx
        .state
        .functions
        .resolve_file_call_key(&super::import::active_unit_alias_targets(cx.state), name)?;
    let entry: super::native::FuncEntry<P> = match canonical {
        Some(key) => match cx.state.functions.get(&key) {
            Some(entry) => entry,
            None => {
                return Err(anyhow::Error::new(diag::eval_call_unknown(
                    name,
                    &super::import::missing_import_hint(cx.state, name),
                )));
            }
        },
        None => match cx.state.functions.get(name) {
            // Hosts and natives resolve as spelled (including
            // qualified host calls like `STD::X`, which never go
            // through the matcher). Script bodies never serve here:
            // nested frames, the file frame, and the harvest map
            // below own every script spelling, so a script hit here
            // is always foreign and falls through to fail below.
            Some(entry) if !matches!(entry.body, FuncBody::Script(_)) => entry,
            _ => match super::import::find_script_site(cx.state, name)? {
                Some(super::import::ScriptSite::Live(entry)) => entry,
                Some(super::import::ScriptSite::File { raw, unit }) => {
                    build_script_entry(cx.state, name, &raw, unit)?
                }
                None => {
                    return Err(anyhow::Error::new(diag::eval_call_unknown(
                        name,
                        &super::import::missing_import_hint(cx.state, name),
                    )));
                }
            },
        },
    };
    Ok(entry)
}

/// Depth budget and arity gate for one invocation: exact counts, or
/// the optional-MAP range. Single owner; every routed call gates here
/// before any argument evaluates.
pub(crate) fn gate_call<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    name: &str,
    entry: &super::native::FuncEntry<P>,
    nargs: usize,
) -> Result<()> {
    // Global recursion check.
    if cx.state.call_depth >= MAX_CALL_DEPTH {
        return Err(anyhow::Error::new(diag::eval_call_recursion_limit(
            super::base_name(name),
        )));
    }
    // Pre-evaluation arity gate from the entry metadata. Trailing
    // optional MAP params widen exact counts to ranges; everything
    // else stays exact.
    if let Some(params) = entry.meta.params.as_deref() {
        let required = super::required_arity(params);
        if nargs < required || nargs > params.len() {
            if required == params.len() {
                return Err(anyhow::Error::new(diag::eval_call_arity_exact(
                    super::base_name(name),
                    params.len(),
                    nargs,
                )));
            }
            return Err(anyhow::Error::new(diag::eval_call_arity_range(
                super::base_name(name),
                required,
                params.len(),
                nargs,
            )));
        }
    }
    Ok(())
}

pub(crate) fn call_func_value<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    name: &str,
    args: &[Expr],
) -> Result<Value> {
    // Resolve, gate, evaluate, execute: the phase owners below serve
    // statement calls, expression calls, and RPN math alike.
    let entry = resolve_call_entry(cx, name)?;
    gate_call(cx, name, &entry, args.len())?;
    // 4. Argument evaluation: only reachable for a valid invocation.
    // Omitted trailing optionals fill with zero values first, so
    // binding below always sees a full argument list. The arity gate
    // above guarantees every missing position is optional.
    let mut arg_vals = Vec::with_capacity(args.len());
    for arg in args {
        arg_vals.push(super::args::evaluate_expr(arg, cx)?);
    }
    execute_call_entry(cx, name, entry, arg_vals, false)
}

/// Execute a resolved entry with evaluated arguments. Single owner
/// for the call body; every routed call (statement, expression, RPN
/// math) ends here. `rpn_only` restricts stateful bodies to RPN-opted
/// entries: `Ctx` without `rpn` fails unknown (AST-only natives like
/// `PATH_TYPE` never run in math), and `Both` picks `ctx` only when
/// `rpn` else `pure`.
pub(crate) fn execute_call_entry<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    name: &str,
    entry: super::native::FuncEntry<P>,
    mut arg_vals: Vec<Value>,
    rpn_only: bool,
) -> Result<Value> {
    // 5. Execution by body kind. Script returns early (no host
    // return gate); host arms share one context wrap and one
    // return-type check below.
    let value = match entry.body {
        FuncBody::Script(def) => {
            // Hard isolation (#322): the only thing crossing a FUNC
            // boundary is explicit arguments. Every script call resets
            // workspace, workdir, env, and outer variables to the
            // callee defining file, restoring the caller on return.
            // A callee that forgets its jail fails safe into the
            // defining file default, never caller ambient.
            let module_key = def.module.clone();
            // One lookup for every file, entry included: the
            // definition-site jail when the definition sealed one,
            // else the defining file fallback. No per-kind
            // restatement below.
            let callee_ctx = cx
                .state
                .callee_file_ctx(module_key.as_deref(), def.def_jail.clone());
            cx.state.call_depth += 1;
            // All calls swap the whole variable stack for a fresh one
            // before the call frame pushes, and restore after it pops
            // (#115, #322). Module-owned bindings stay visible through
            // the module-store fallback in every accessor, so this
            // cuts importer frames and entry globals alike, never
            // module file state. Entry calls have no store fallback:
            // they observe params, locals, and (seeded below) the
            // file's top-level `CONST`s.
            let saved_vars = Some(std::mem::take(&mut cx.state.var_scopes));
            // `CONST` marks ride the same swap: every callee declares
            // into a fresh const frame and restores after.
            let saved_consts = Some(std::mem::take(&mut cx.state.const_scopes));
            // `ISOLATED` snapshots belong to the caller: the callee
            // env resets below, so inherited scrubs must not leak in.
            let saved_isolated = std::mem::take(&mut cx.state.isolated_stack);
            cx.state.push_scope();
            // Entry file `CONST`s are visible by fallback (#318):
            // `ExecState::entry_const_store` serves the entry init
            // filtered to consts when the caller is entry, so no
            // synthetic seed frame is needed. `LET`s never cross;
            // module callers observe nothing (fallback disallows).
            // Callee jail (#322): every call restores the workspace
            // selection and cwd sealed at the definition site, so a
            // file that establishes its jail before its `FUNC`s serves
            // them from there no matter which workspace the caller
            // sits in. A FUNC-level `WORKSPACE` or `WORKDIR` overrides
            // the sealed site for that call. The caller selection
            // returns via the scope pop. Reentrant init-time
            // self-calls (no stored context yet) keep the in-progress
            // init scope untouched, since that is the file own
            // context under construction. Entry and module share this
            // one path, never per-kind branches.
            let reentrant = module_key.is_some() && callee_ctx.is_none();
            if !reentrant && let Some((_, _, selection, cwd)) = &callee_ctx {
                cx.state.fs.restore_selection(*selection);
                cx.state.cwd = cwd.clone();
            }
            // Module-owned bodies resolve their own file's TYPE aliases
            // first, shadowing globals for the call: a file's bodies
            // validate against the same view (see phase 1), so imports
            // never leak bare names yet self-contained files keep
            // working. Harvested types stay global by explicit request.
            let owner_aliases: Vec<(String, String)> = module_key
                .as_deref()
                .and_then(|key| cx.state.modules.get(key))
                .map(|record| super::typing::collect_type_aliases(&record.steps, cx.state))
                .transpose()?
                .unwrap_or_default()
                .into_iter()
                .collect();
            let saved_type_aliases = cx.state.type_aliases.clone();
            for (name, target) in &owner_aliases {
                cx.state.register_alias(name.clone(), target.clone());
            }
            // Module-owned bodies execute with their module on the
            // context stack, so name resolution falls back to the
            // module's shared store; entry bodies push `None`, keeping
            // entry functions params-only even when called from module
            // code. Balanced below on every exit path.
            cx.state.module_stack.push(module_key);
            // A callee never inherits its caller namespace (#115,
            // #322): every body resets to the seed plus its defining
            // file own contributions. Declared `INHERIT_ENV` names
            // re-read from the engine here, never the caller (a
            // re-inherit of an already-present value leaves no init
            // delta, yet must still admit the name). A body needing
            // caller env takes it as an argument. Reentrant init-time
            // self-calls keep the in-progress init env, since that is
            // the file own context under construction. One env path
            // for entry and module alike.
            if reentrant {
                // Keep the in-progress init scope untouched.
            } else if let Some((envs, inherit_names, _, _)) = callee_ctx {
                cx.state.reset_module_env(envs);
                if !inherit_names.is_empty() {
                    inherit_env(cx, &inherit_names)?;
                }
            }
            // Omitted trailing optionals fill with zero values before
            // the body binds: the arity gate above guarantees every
            // missing position is optional, and registration rejects
            // unzeroable optionals, so the fill cannot fail here.
            for (pname, ptype, optional) in def.params.iter().skip(arg_vals.len()) {
                if !optional {
                    return Err(anyhow::Error::new(diag::eval_call_missing_param(
                        pname,
                        super::base_name(name),
                    )));
                }
                arg_vals.push(super::optional_zero_value(ptype).map_err(|err| {
                    anyhow::Error::new(diag::eval_call_zero_value_failed(
                        super::base_name(name),
                        pname,
                        err,
                    ))
                })?);
            }
            let outcome: Result<Value> = (|| {
                // Function-local imports scope to this invocation: push
                // a fresh frame for this unit so the body's own top-level
                // imports land here (never the shared unit-top map) and
                // the callee never sees the caller's locals. The frame
                // records the scope depth at entry (the call scope
                // pushed above): deeper block scopes record into scope
                // frames instead, so block imports unwind on block exit
                // like everywhere else. The pop below runs on every
                // path (the closure captures the outcome as a value
                // first), matching the `push_scope` / `pop_scope`
                // pattern with no unsafe.
                let unit_key = super::import::unit_key(def.module.as_deref()).to_string();
                let call_depth = cx.state.scope_stack.len();
                cx.state.call_alias_frames.push(CallAliasFrame {
                    unit: unit_key,
                    depth: call_depth,
                    targets: HashMap::new(),
                });
                // Frame floor for the nested walk (#322): scope frames
                // at or above this index belong to the current call;
                // anything below is caller context and unreachable.
                // Pushed and popped with the alias frame on every path.
                cx.state
                    .func_floors
                    .push(cx.state.functions.scope_depth().saturating_sub(1));
                let flow = (|| {
                    for ((pname, ptype, _), pval) in def.params.iter().zip(arg_vals) {
                        cx.state.declare_var(pname.clone(), ptype.clone(), pval)?;
                    }
                    super::steps::execute_steps(
                        cx.state,
                        cx.process,
                        &def.body,
                        cx.stdin.clone(),
                        false,
                        cx.out.clone(),
                        cx.err.clone(),
                        cx.out_pipe.clone(),
                        cx.stdin_pipe.clone(),
                        false,
                    )
                })();
                // Unconditional pops before `?` propagates: errors and
                // early boundary exits never leak frames, matching
                // `pop_scope` below. (Panics propagate without popping,
                // exactly like the scope stack: no `catch_unwind`
                // anywhere unwraps them.)
                let _ = cx.state.call_alias_frames.pop();
                let _ = cx.state.func_floors.pop();
                let flow = flow?;
                match flow {
                    Flow::Done => Ok(Value::string(String::new())),
                    Flow::Return { value, .. } => Ok(value),
                    Flow::Break { idx } => Err(anyhow::Error::new(
                        diag::eval_call_flow_crosses_func(idx + 1, KEYWORD_BREAK, name),
                    )),
                    Flow::Continue { idx } => Err(anyhow::Error::new(
                        diag::eval_call_flow_crosses_func(idx + 1, KEYWORD_CONTINUE, name),
                    )),
                }
            })();
            let pop_res = cx.state.pop_scope().map_err(anyhow::Error::from);
            if let Some(saved) = saved_vars {
                cx.state.var_scopes = saved;
            }
            if let Some(saved) = saved_consts {
                cx.state.const_scopes = saved;
            }
            cx.state.isolated_stack = saved_isolated;
            cx.state.call_depth -= 1;
            let _ = cx.state.module_stack.pop();
            cx.state.restore_aliases(saved_type_aliases);
            return match (outcome, pop_res) {
                (Ok(value), Ok(())) => {
                    let _ = (&def.return_type, &def.return_spelling, def.is_async);
                    Ok(value)
                }
                (Err(e), _) => Err(e),
                (Ok(_), Err(e)) => Err(e),
            };
        }
        FuncBody::Pure(func) => func(arg_vals),
        FuncBody::Ctx(func) => {
            if rpn_only && !entry.meta.rpn {
                return Err(anyhow::Error::new(diag::eval_call_unknown_rpn(
                    name,
                    &super::import::missing_import_hint(cx.state, name),
                )));
            }
            func(cx, arg_vals)
        }
        FuncBody::Both { pure, ctx } => {
            if !rpn_only || entry.meta.rpn {
                ctx(cx, arg_vals)
            } else {
                pure(arg_vals)
            }
        }
    }
    .map_err(|err| anyhow::Error::new(diag::eval_call_failed(name, &err)))?;
    super::args::check_host_return_value(
        &format!("function `{}` returned", super::base_name(name)),
        &entry.meta.returns,
        &value,
    )?;
    Ok(value)
}

/// Evaluate an inline block to a value (`LET $a: STRING = { ... }`): the
/// zero-arg sibling of [`call_func_value`]. The body runs inline on the
/// current thread with inherited stdio in a fresh variable scope; the
/// nearest `RETURN` is the block's value and fallthrough yields `""`
/// (both type-checked by the caller's `declare_var`/`mutate_var`).
/// `BREAK`/`CONTINUE` escaping the body are boundary errors, and inner
/// `CALL`s keep their own function boundary, so a `RETURN` inside one
/// never leaks into the block.
pub(crate) fn block_value<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    body: &[Step],
) -> Result<Value> {
    cx.state.push_scope();
    let outcome: Result<Value> =
        (|| {
            let flow = super::steps::execute_steps(
                cx.state,
                cx.process,
                body,
                cx.stdin.clone(),
                false,
                cx.out.clone(),
                cx.err.clone(),
                cx.out_pipe.clone(),
                cx.stdin_pipe.clone(),
                false,
            )?;
            match flow {
                Flow::Done => Ok(Value::string(String::new())),
                Flow::Return { value, .. } => Ok(value),
                Flow::Break { idx } => Err(anyhow::Error::new(
                    diag::eval_block_flow_crosses_block(idx + 1, KEYWORD_BREAK),
                )),
                Flow::Continue { idx } => Err(anyhow::Error::new(
                    diag::eval_block_flow_crosses_block(idx + 1, KEYWORD_CONTINUE),
                )),
            }
        })();
    let pop_res = cx.state.pop_scope().map_err(anyhow::Error::from);
    match (outcome, pop_res) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e),
    }
}

/// Evaluate `RETURN <expr>` to the enclosing boundary's value: a function
/// call, an `ASYNC` task (bound by `LET $out: STRING = AWAIT $t`), or an inline
/// `LET` block. A `RETURN` with no enclosing boundary is rejected where
/// the flow reaches the pipeline top. Uniform everywhere.
pub(crate) fn handle_return<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    expr: &Expr,
) -> Result<Flow> {
    let value = super::args::evaluate_expr(expr, cx)?;
    Ok(Flow::Return { idx, value })
}

/// Run `WHILE <bool-expr> { ... }`: re-evaluate the condition in the
/// current scope each iteration (Bool-only, same `is_truthy` rule as `IF`),
/// execute the body in a fresh per-iteration scope, and honor
/// `BREAK`/`CONTINUE`. A `RETURN` inside propagates to the enclosing
/// `call_func`; anything else yields Done.
pub(crate) fn while_loop<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    cond: &Expr,
    body: &[Step],
) -> Result<Flow> {
    use std::sync::atomic::Ordering;
    loop {
        if cx.state.cancel_token.load(Ordering::SeqCst) {
            return Err(anyhow::Error::new(diag::eval_async_cancelled()));
        }
        let val = super::args::evaluate_expr(cond, cx)?;
        if !super::args::is_truthy(&val)? {
            return Ok(Flow::Done);
        }
        cx.state.push_scope();
        let res = super::steps::execute_steps(
            cx.state,
            cx.process,
            body,
            cx.stdin.clone(),
            false,
            cx.out.clone(),
            cx.err.clone(),
            cx.out_pipe.clone(),
            cx.stdin_pipe.clone(),
            false,
        );
        let pop_res = cx.state.pop_scope().map_err(anyhow::Error::from);
        let flow = match (res, pop_res) {
            (Ok(flow), Ok(())) => flow,
            (Err(e), _) => return Err(e),
            (Ok(_), Err(e)) => return Err(e),
        };
        match flow {
            Flow::Done | Flow::Continue { .. } => {}
            Flow::Break { .. } => return Ok(Flow::Done),
            Flow::Return { .. } => return Ok(flow),
        }
    }
}

pub(crate) fn assign<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    var: &str,
    decl_type: String,
    expr: &Expr,
    is_const: bool,
) -> Result<()> {
    let value = super::args::evaluate_expr(expr, cx)?;
    let clean_var = var.trim_start_matches('$').to_string();
    let tag = cx.state.resolve_tag(&decl_type)?;
    cx.state.declare_binding(clean_var, tag, value, is_const)?;
    Ok(())
}

/// Single owner for pipeline `Bind` dispatch: expression, capture,
/// async task, and await-join all route through [`dispatch_bind_step`],
/// while the step-level loop routes `Bind` captures through the flow
/// path and every other rhs through the leaf path sharing these
/// helpers, so parsing, `Display`, static checking, and dispatch never
/// restate the `LET`/`CONST` split per form.
pub(crate) fn set_var_value<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    var: &str,
    expr: &Expr,
) -> Result<()> {
    let value = super::args::evaluate_expr(expr, cx)?;
    let clean_var = var.trim_start_matches('$').to_string();
    cx.state.mutate_var(&clean_var, value)?;
    Ok(())
}

/// Dispatch `LET $var: STRING = <sync command>` (or `CONST`, via
/// `is_const`) : run the command to completion with
/// a spillable capture sink as its stdout, then bind the exact bytes as a
/// string. Only stdout is captured (stderr keeps the parent wiring; stdin
/// passes through so `WITH_IO [stdin=$p]` still works). Captured bytes
/// never tee into the parent assertion windows. On command failure
/// nothing is bound.
///
/// When the captured command is `NAME(...)`, no sink is installed:
/// the callee's stdout keeps the active routing (observable via
/// `ASSERT_CONTAINS stdout`/pipes) and the bound value is the function's `RETURN`
/// payload (or `""` on fallthrough), coerced to the declared type.
/// A `TIMEOUT`-wrapped single command captures exactly what the bare
/// command captures, with the tightest deadline enforced around it.
pub(crate) fn assign_capture<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    generation: usize,
    idx: usize,
    var: &str,
    decl_type: String,
    cmd: &StepKind,
    is_const: bool,
) -> Result<Flow> {
    use std::sync::Arc;

    let kw = if is_const { KEYWORD_CONST } else { KEYWORD_LET };
    match peel_capture_command(cx, cmd)? {
        PeeledCapture::Import {
            bindings,
            path,
            alias,
            deadline,
        } => {
            // `IMPORT` (possibly under `WITH_IO`/`TIMEOUT` layers): no
            // capture sink. The imported pipeline's stdout keeps its
            // routed streams; the bound value is the `RETURN` payload,
            // exactly like `CALL`.
            if bindings
                .iter()
                .any(|b| b.stream == IoStream::Stdout && b.pipe.is_some())
            {
                return Err(anyhow::Error::new(diag::eval_capture_with_io_stdout(kw)));
            }
            let guard = arm_deadline(cx, deadline);
            let result = if bindings.is_empty() {
                super::import::import_file(cx, path, alias)
            } else {
                (|| {
                    let (step_stdin, expose_stdin, step_stdout, step_stderr, out_pipe, stdin_pipe) =
                        resolve_io_streams(cx, &bindings, cmd)?;
                    let state = &mut *cx.state;
                    let process = &mut *cx.process;
                    let mut sub_cx = super::steps::StepCtx {
                        state,
                        process,
                        stdin: step_stdin,
                        expose_stdin,
                        out: step_stdout,
                        err: step_stderr,
                        out_pipe,
                        stdin_pipe,
                        // `IMPORT` layers never carry a terminal: lowering rejects
                        // `tty` outside `RUN` before execution reaches them.
                        tty: None,
                    };
                    super::import::import_file(&mut sub_cx, path, alias)
                })()
            };
            let value = disarm_deadline(cx, guard, deadline, result)?;
            let clean_var = var.trim_start_matches('$').to_string();
            let tag = cx.state.resolve_tag(&decl_type)?;
            cx.state.declare_binding(clean_var, tag, value, is_const)?;
            return Ok(Flow::Done);
        }
        PeeledCapture::Call {
            bindings,
            name,
            args,
            deadline,
        } => {
            // `CALL` (possibly under `WITH_IO`/`TIMEOUT` layers): no
            // capture sink. The callee's stdout keeps its routed streams
            // (observable via `ASSERT_CONTAINS stdout`/pipes); the bound
            // value is the `RETURN` payload.
            if bindings
                .iter()
                .any(|b| b.stream == IoStream::Stdout && b.pipe.is_some())
            {
                return Err(anyhow::Error::new(diag::eval_capture_with_io_stdout(kw)));
            }
            let guard = arm_deadline(cx, deadline);
            let result = if bindings.is_empty() {
                call_func_value(cx, name, args)
            } else {
                (|| {
                    let (step_stdin, expose_stdin, step_stdout, step_stderr, out_pipe, stdin_pipe) =
                        resolve_io_streams(cx, &bindings, cmd)?;
                    // Reborrow state/process for the sub-context; `cx` is unused below.
                    let state = &mut *cx.state;
                    let process = &mut *cx.process;
                    let mut sub_cx = super::steps::StepCtx {
                        state,
                        process,
                        stdin: step_stdin,
                        expose_stdin,
                        out: step_stdout,
                        err: step_stderr,
                        out_pipe,
                        stdin_pipe,
                        // `CALL` layers never carry a terminal: lowering rejects
                        // `tty` outside `RUN` before execution reaches them.
                        tty: None,
                    };
                    call_func_value(&mut sub_cx, name, args)
                })()
            };
            let value = disarm_deadline(cx, guard, deadline, result)?;
            let clean_var = var.trim_start_matches('$').to_string();
            let tag = cx.state.resolve_tag(&decl_type)?;
            cx.state.declare_binding(clean_var, tag, value, is_const)?;
            return Ok(Flow::Done);
        }
        PeeledCapture::Sink => {}
    }
    let sink = Arc::new(super::capture::new_spill_buffer());
    let capture_out = Some(StreamHandle::Stream(sink.writer()));
    let flow = super::steps::execute_single_step_with_generation(
        cx.state,
        cx.process,
        cmd,
        generation,
        idx,
        cx.stdin.clone(),
        cx.expose_stdin,
        capture_out,
        cx.err.clone(),
        // Capture owns stdout (parse rejects stdout pipes here), so there
        // is no backend to carry.
        None,
        None,
        // A `WITH_IO [tty]` capture re-resolves its own request when the
        // inner step runs through `with_io`; ambient carriage stays empty.
        cx.tty,
    )?;
    match flow {
        Flow::Done => {}
        Flow::Break { idx } => {
            return Err(anyhow::Error::new(diag::eval_capture_flow_outside_loop(
                idx + 1,
                KEYWORD_BREAK,
            )));
        }
        Flow::Continue { idx } => {
            return Err(anyhow::Error::new(diag::eval_capture_flow_outside_loop(
                idx + 1,
                KEYWORD_CONTINUE,
            )));
        }
        Flow::Return { idx, .. } => {
            return Err(anyhow::Error::new(
                diag::eval_capture_return_outside_function(idx + 1),
            ));
        }
    }
    let text = sink
        .drain_string_strict()
        .map_err(|err| anyhow::Error::new(diag::eval_capture_not_utf8(kw, var, err)))?;
    let clean_var = var.trim_start_matches('$').to_string();
    let tag = cx.state.resolve_tag(&decl_type)?;
    cx.state
        .declare_binding(clean_var, tag, Value::string(text), is_const)?;
    Ok(Flow::Done)
}

pub(crate) fn if_then<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    cond: &Expr,
    then_body: &[Step],
    else_ifs: &[(Box<Expr>, Vec<Step>)],
    else_body: &Option<Vec<Step>>,
) -> Result<Flow> {
    let val = super::args::evaluate_expr(cond, cx)?;
    if super::args::is_truthy(&val)? {
        return super::steps::execute_scoped_steps(
            cx.state,
            cx.process,
            then_body,
            cx.stdin.clone(),
            false,
            cx.out.clone(),
            cx.err.clone(),
            cx.out_pipe.clone(),
            cx.stdin_pipe.clone(),
            false,
        );
    }
    for (else_cond, else_block) in else_ifs {
        let val = super::args::evaluate_expr(else_cond.as_ref(), cx)?;
        if super::args::is_truthy(&val)? {
            return super::steps::execute_scoped_steps(
                cx.state,
                cx.process,
                else_block,
                cx.stdin.clone(),
                false,
                cx.out.clone(),
                cx.err.clone(),
                cx.out_pipe.clone(),
                cx.stdin_pipe.clone(),
                false,
            );
        }
    }
    if let Some(body) = else_body {
        return super::steps::execute_scoped_steps(
            cx.state,
            cx.process,
            body,
            cx.stdin.clone(),
            false,
            cx.out.clone(),
            cx.err.clone(),
            cx.out_pipe.clone(),
            cx.stdin_pipe.clone(),
            false,
        );
    }
    Ok(Flow::Done)
}

/// Execute a `SWITCH`: with a discriminant, the first `CASE` value
/// strictly equal to it wins (values evaluate in order and stop at
/// the hit, so later calls never run); bare, the first truthy `CASE`
/// condition wins. Only the winning arm runs: no fallthrough. `BREAK`
/// exits the switch (consumed here, so an enclosing loop never sees
/// it); `CONTINUE` and `RETURN` propagate to the loop or boundary.
pub(crate) fn switch_run<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    discriminant: &Option<Box<Expr>>,
    cases: &[(Vec<Expr>, Vec<Step>)],
    default_body: &Option<Vec<Step>>,
) -> Result<Flow> {
    let mut chosen: Option<&Vec<Step>> = None;
    if let Some(disc) = discriminant {
        let target = super::args::evaluate_expr(disc, cx)?;
        'arms: for (values, body) in cases {
            for value in values {
                let candidate = super::args::evaluate_expr(value, cx)?;
                if super::args::values_equal(&target, &candidate) {
                    chosen = Some(body);
                    break 'arms;
                }
            }
        }
    } else {
        'arms: for (conds, body) in cases {
            for cond in conds {
                let val = super::args::evaluate_expr(cond, cx)?;
                if super::args::is_truthy(&val)? {
                    chosen = Some(body);
                    break 'arms;
                }
            }
        }
    }
    let body = match chosen.or(default_body.as_ref()) {
        Some(body) => body,
        None => return Ok(Flow::Done),
    };
    match super::steps::execute_scoped_steps(
        cx.state,
        cx.process,
        body,
        cx.stdin.clone(),
        false,
        cx.out.clone(),
        cx.err.clone(),
        cx.out_pipe.clone(),
        cx.stdin_pipe.clone(),
        false,
    )? {
        Flow::Break { .. } => Ok(Flow::Done),
        flow => Ok(flow),
    }
}

// ── Dispatch functions ──────────────────────────────────────────────────────
// These extract fields from `StepKind` variants, resolve arguments, and
// forward to the actual handler functions. Used by `define_pipeline!`.

/// Collect the pipes a step subtree produces to (`stdout`/`stderr`
/// bindings), same-thread only. Endpoints are always `$var`, so every
/// producer resolves against live state at pin time; no static walk exists
/// by design : with no literals left to name, there is nothing to collect
/// without state. Nested `ASYNC` bodies run on other threads with their
/// own pins and are excluded; `Timeout`/`For`/`If`/`WithIo` bodies run
/// inline and are included. Only producers pin: a task that only reads a
/// pipe relies on EOF-from-detach to complete, so pinning it would
/// deadlock. Each entry pairs the pipe name with whether OS promotion
/// applies (OR-merged across occurrences). Unresolvable names are skipped
/// here (execution-time resolution reports the real error); promotion never
/// applies to dynamic endpoints.
/// One pipe binding sighted in an async body: the owned handle, whether
/// its ultimate consumer/producer is RUN-terminated, and whether this
/// occurrence produces (`stdout`/`stderr`) rather than consumes (`stdin`).
struct BodyBinding {
    handle: PipeHandle,
    produces: bool,
}

/// Resolve one binding endpoint to its handle, skipping unresolvable
/// variables here: execution-time resolution reports the real error.
fn binding_handle<P: ProcessManager>(
    state: &ExecState<P>,
    target: &PipeTarget,
) -> Option<PipeHandle> {
    let PipeTarget::Var { name: var, .. } = target;
    match state.get_var_typed(var) {
        Some((TypeTag::Pipe, value)) => value.as_pipe_handle(),
        _ => None,
    }
}

/// Collect every pipe binding in a step subtree: producers and consumers
/// alike, same-thread inline bodies only. Nested `ASYNC` bodies run on
/// other threads with their own pins and are excluded, as are deferred
/// `FUNC` bodies and dynamic `Call` targets (they resolve where they run).
/// Unresolvable endpoints are skipped (execution reports the real error).
fn collect_body_bindings<P: ProcessManager>(
    kind: &StepKind,
    state: &ExecState<P>,
    out: &mut Vec<BodyBinding>,
) {
    match kind {
        StepKind::WithIo { bindings, cmd } => {
            // Terminal stdio never promotes: the pty master is one
            // interleaved byte stream pumped through script backends,
            // so its descriptors stay out of the OS-pipe walk even
            // inside `ASYNC` single-`RUN` bodies that would otherwise
            // promote them. Geometry bindings carry no pipes.
            let tty = bindings.iter().any(|b| matches!(b.stream, IoStream::Tty));
            for binding in bindings {
                if tty
                    && matches!(
                        binding.stream,
                        IoStream::Stdin | IoStream::Stdout | IoStream::Stderr
                    )
                {
                    continue;
                }
                let Some(target) = &binding.pipe else {
                    continue;
                };
                let Some(handle) = binding_handle(state, target) else {
                    continue;
                };
                out.push(BodyBinding {
                    handle,
                    produces: !matches!(binding.stream, IoStream::Stdin),
                });
            }
            collect_body_bindings(cmd, state, out);
        }
        StepKind::Timeout { body, .. }
        | StepKind::Fail { body, .. }
        | StepKind::Isolated { body }
        | StepKind::For { body, .. }
        | StepKind::While { body, .. } => {
            for step in body {
                collect_body_bindings(&step.kind, state, out);
            }
        }
        StepKind::Switch {
            cases,
            default_body,
            ..
        } => {
            for (_, branch) in cases {
                for step in branch {
                    collect_body_bindings(&step.kind, state, out);
                }
            }
            if let Some(body) = default_body {
                for step in body {
                    collect_body_bindings(&step.kind, state, out);
                }
            }
        }
        StepKind::If {
            then_body,
            else_ifs,
            else_body,
            ..
        } => {
            for step in then_body {
                collect_body_bindings(&step.kind, state, out);
            }
            for (_, branch) in else_ifs {
                for step in branch {
                    collect_body_bindings(&step.kind, state, out);
                }
            }
            if let Some(body) = else_body {
                for step in body {
                    collect_body_bindings(&step.kind, state, out);
                }
            }
        }
        StepKind::FuncDef { .. }
        | StepKind::Call { .. }
        | StepKind::AsyncBlock { .. }
        | StepKind::Bind { .. } => {}
        _ => {}
    }
}

/// Ensure every pipe an async `body` binds (producers and consumers) is
/// materialized, and pin a keeper slot on each produced pipe : all
/// synchronously on the spawning thread, so backend decisions never
/// depend on thread scheduling.
///
/// Promotion is per-handle by declaration scope: only pipes this task
/// declared and never shared become OS pairs. Anything else stays
/// script-backed, so concurrent tasks can never disagree about EOF
/// propagation. Pins group by the top-level index of the final producer
/// step for each handle: the worker drops a handle's guard once that step
/// completes, so transient gaps between producers never signal EOF while
/// later consumer steps in the same task still observe it. Returns `None`
/// when the body produces to no pipe. A task that only reads a
/// pipe gets existence without a pin (consumers rely on EOF-from-detach,
/// so pinning them would deadlock).
fn pin_async_keepers<P: ProcessManager>(
    cx: &StepCtx<'_, P>,
    body: &[Step],
    worker_task_id: u64,
) -> Result<Option<super::state::KeeperExpiry>> {
    struct Acc {
        handle: PipeHandle,
        last_producer: Option<usize>,
    }
    let mut accs: Vec<Acc> = Vec::new();
    for (idx, step) in body.iter().enumerate() {
        let mut found = Vec::new();
        collect_body_bindings(&step.kind, cx.state, &mut found);
        for b in found {
            match accs.iter_mut().find(|a| a.handle.ptr_eq(&b.handle)) {
                Some(a) => {
                    if b.produces {
                        a.last_producer = Some(idx);
                    }
                }
                None => accs.push(Acc {
                    handle: b.handle,
                    last_producer: b.produces.then_some(idx),
                }),
            }
        }
    }
    // Decide every sighted handle first (consumers included: a consumer
    // arriving before any producer still finds the decided backend
    // instead of racing the producer's setup). Promotion is judged from
    // the worker's perspective: only pipes the worker itself declared and
    // never shared become OS pairs.
    for a in &accs {
        let promote = ExecState::<P>::can_promote_in_task(&a.handle, worker_task_id);
        cx.state.io.ensure_handle(&a.handle, promote)?;
    }
    let mut map: HashMap<usize, Vec<KeeperGuard>> = HashMap::new();
    for a in &accs {
        if let Some(idx) = a.last_producer
            && let Some(guard) = cx.state.io.pin_keeper(&a.handle)?
        {
            map.entry(idx).or_default().push(guard);
        }
    }
    if map.is_empty() {
        Ok(None)
    } else {
        Ok(Some(super::state::KeeperExpiry::new(body, map)))
    }
}

pub(crate) fn dispatch_run<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Run(arg) = step else {
        unreachable!()
    };
    let cmd = super::args::resolve_arg(arg, cx)?;
    let cmd = super::args::expand_dsl_vars(&cmd, cx.state)?;
    run(cx, &cmd)
}

pub(crate) fn dispatch_run_exec<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::RunExec { argv } = step else {
        unreachable!()
    };
    let resolved = resolve_run_exec_argv(argv, cx)?;
    run_argv(cx, &resolved)
}

pub(crate) fn dispatch_async_block<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::AsyncBlock { body } = step else {
        unreachable!()
    };

    // Pre-allocate keeper handles synchronously on this thread, before the
    // worker exists, so pipes the block produces to can never observe a
    // transient-only zero-writer window. Guards expire by step index as
    // the worker completes its final producer steps, then ride out the
    // thread in forked state.
    let body = body.clone();
    // Anonymous workers get an identity up front: the pin walk judges
    // promotion from the worker's perspective, and pipes the worker mints
    // carry this id as their declaration origin. Allocation runs the
    // retained-finished sweep so anonymous spawns share the single
    // capacity gate.
    let worker_task_id = cx.state.alloc_task_id()?;
    let expiry = pin_async_keepers(cx, &body, worker_task_id)?;

    // Fork the execution state for the child thread.
    // This clones the fs (via clone_box), envs, cwd, var_scopes, etc.
    // The child gets fresh bg_children and scope_stack.
    let mut forked_state = cx.state.fork();
    forked_state.keeper_expiry = expiry;
    forked_state.task_id = worker_task_id;
    cx.state.mark_captured_pipes_escaped();
    let forked_process = cx.process.clone();
    let stdin = cx.stdin.clone();
    let expose_stdin = cx.expose_stdin;
    let out = cx.out.clone();
    let err = cx.err.clone();
    // Live pipe backends travel with the worker: without them a REMOTE
    // step inside the task loses its streaming backend (session
    // accumulates, core flushes at completion) instead of delivering
    // bytes as they arrive.
    let out_pipe = cx.out_pipe.clone();
    let stdin_pipe = cx.stdin_pipe.clone();
    // Hold a keeper on the inherited stdout backend for the worker's
    // lifetime: transient writer churn would otherwise toggle the close
    // latch mid-flight, letting a concurrent drain observe EOF (and exit)
    // during a producer lull. Keepers never resurrect a closed pipe and
    // explicit close still wins, so drain-after-join is unaffected.
    let out_keepalive = out_pipe
        .as_ref()
        .map(|backend| KeeperGuard::new(std::sync::Arc::clone(backend)));
    let cancel_token = std::sync::Arc::clone(&forked_state.cancel_token);
    let cancel_hub = std::sync::Arc::clone(&forked_state.cancel_hub);
    let active_process = std::sync::Arc::clone(&forked_state.active_process);
    let worker: std::sync::Arc<std::sync::Mutex<Option<std::thread::ThreadId>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let worker_child = std::sync::Arc::clone(&worker);

    // Spawn a thread that executes the block's steps with subshell isolation.
    // ENV/WORKDIR/etc mutations in the block do not leak to the parent.
    // Control flow never crosses the thread boundary: a stray BREAK,
    // CONTINUE, or RETURN becomes a step-numbered error here.
    let join = std::thread::spawn(move || {
        // Slot guard first: dropping on return or unwind releases the
        // global live-task count, so the cap tracks threads actually
        // alive rather than table entries.
        let _slot =
            super::state::TaskSlotGuard::new(std::sync::Arc::clone(&forked_state.live_tasks));
        *worker_child.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::thread::current().id());
        let _out_keepalive = out_keepalive;
        let mut child_state = forked_state;
        let mut child_process = forked_process;
        let flow = super::steps::execute_steps(
            &mut child_state,
            &mut child_process,
            &body,
            stdin,
            expose_stdin,
            out,
            err,
            out_pipe,
            stdin_pipe,
            true, // wait_at_end: child waits for its own bg_children
        )?;
        match flow {
            Flow::Done => Ok(()),
            Flow::Break { idx } => Err(anyhow::Error::new(diag::eval_async_flow_crosses_async(
                idx + 1,
                KEYWORD_BREAK,
            ))),
            Flow::Continue { idx } => Err(anyhow::Error::new(diag::eval_async_flow_crosses_async(
                idx + 1,
                KEYWORD_CONTINUE,
            ))),
            Flow::Return { idx, .. } => Err(anyhow::Error::new(
                diag::eval_async_flow_crosses_async(idx + 1, KEYWORD_RETURN),
            )),
        }
    });

    // Store the thread handle as a background handle in the parent's state.
    cx.state.bg_children.push(super::state::BgChild {
        handle: Box::new(super::steps::ThreadJoinHandle::new(
            join,
            cancel_token,
            cancel_hub,
            active_process,
            worker,
        )),
        task_id: worker_task_id,
    });
    Ok(())
}

pub(crate) fn dispatch_echo<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Echo(arg) = step else {
        unreachable!()
    };
    let msg = super::args::resolve_arg(arg, cx)?;
    echo(cx, &msg)
}

pub(crate) fn dispatch_workdir<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Workdir(arg) = step else {
        unreachable!()
    };
    let path = super::args::resolve_arg(arg, cx)?;
    workdir(cx, &path)
}

pub(crate) fn dispatch_workspace<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Workspace(target) = step else {
        unreachable!()
    };
    workspace(cx, target)
}

pub(crate) fn dispatch_remote<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::RemoteBlock {
        target,
        vars,
        env,
        body,
    } = step
    else {
        unreachable!()
    };
    super::remote::run_remote_block(cx, target, vars, env, body)
}

pub(crate) fn dispatch_env<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Env { key, value } = step else {
        unreachable!()
    };
    let resolved = super::args::resolve_arg(value, cx)?;
    env(cx, key, &resolved)
}

pub(crate) fn dispatch_copy<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Copy {
        from_workspace,
        from_host,
        to_host,
        from,
        to,
    } = step
    else {
        unreachable!()
    };
    if *from_host || *to_host {
        return super::remote::copy_transfer(cx, *from_host, *to_host, from, to);
    }
    let from_resolved = super::args::resolve_arg(from, cx)?;
    let to_resolved = super::args::resolve_arg(to, cx)?;
    copy(cx, from_workspace.clone(), &from_resolved, &to_resolved)
}

pub(crate) fn dispatch_archive<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Archive { src, dst, raw } = step else {
        unreachable!()
    };
    let src_resolved = super::args::resolve_arg(src, cx)?;
    let dst_resolved = super::args::resolve_arg(dst, cx)?;
    Ok(archive(cx, &src_resolved, &dst_resolved, *raw)?)
}

pub(crate) fn dispatch_unarchive<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Unarchive {
        src,
        dst,
        max_bytes,
    } = step
    else {
        unreachable!()
    };
    let src_resolved = super::args::resolve_arg(src, cx)?;
    let dst_resolved = super::args::resolve_arg(dst, cx)?;
    let cap_resolved = super::args::resolve_arg_opt(max_bytes, cx)?;
    Ok(unarchive(
        cx,
        &src_resolved,
        &dst_resolved,
        cap_resolved.as_deref(),
    )?)
}

pub(crate) fn dispatch_copy_git<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::CopyGit {
        rev,
        from,
        to,
        include_dirty,
    } = step
    else {
        unreachable!()
    };
    let rev_resolved = super::args::resolve_arg(rev, cx)?;
    let from_resolved = super::args::resolve_arg(from, cx)?;
    let to_resolved = super::args::resolve_arg(to, cx)?;
    copy_git(
        cx,
        &rev_resolved,
        &from_resolved,
        &to_resolved,
        *include_dirty,
    )
}

pub(crate) fn dispatch_symlink<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Symlink {
        from_workspace,
        from,
        to,
    } = step
    else {
        unreachable!()
    };
    let from_resolved = super::args::resolve_arg(from, cx)?;
    let to_resolved = super::args::resolve_arg(to, cx)?;
    symlink(cx, from_workspace.clone(), &from_resolved, &to_resolved)
}

pub(crate) fn dispatch_mkdir<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Mkdir(arg) = step else {
        unreachable!()
    };
    let path = super::args::resolve_arg(arg, cx)?;
    mkdir(cx, &path)
}

pub(super) fn install<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    from: &str,
    to: &str,
) -> Result<()> {
    let from_abs = cx
        .state
        .fs
        .resolve_read(&cx.state.cwd, from)
        .map_err(|e| anyhow::Error::new(diag::eval_install_failed(from, to, &e)))?;
    let to_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, to)
        .map_err(|e| anyhow::Error::new(diag::eval_install_failed(from, to, &e)))?;
    cx.state
        .fs
        .install_file_atomic(&from_abs, &to_abs, EXECUTABLE_MODE)
        .map_err(|e| anyhow::Error::new(diag::eval_install_failed(from, to, &e)))?;
    Ok(())
}

pub(crate) fn dispatch_install<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Install { from, to } = step else {
        unreachable!()
    };
    let from_resolved = super::args::resolve_arg(from, cx)?;
    let to_resolved = super::args::resolve_arg(to, cx)?;
    install(cx, &from_resolved, &to_resolved)
}

pub(crate) fn dispatch_set_executable<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::SetExecutable { path } = step else {
        unreachable!()
    };
    let resolved = super::args::resolve_arg(path, cx)?;
    set_executable(cx, &resolved)
}

pub(crate) fn dispatch_ls<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Ls(arg) = step else {
        unreachable!()
    };
    let resolved = super::args::resolve_arg_opt(arg, cx)?;
    ls(cx, &resolved)
}

pub(crate) fn dispatch_read<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Read(arg) = step else {
        unreachable!()
    };
    let resolved = super::args::resolve_arg_opt(arg, cx)?;
    Ok(read(cx, &resolved)?)
}

pub(crate) fn dispatch_read_line<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::ReadLine { var } = step else {
        unreachable!()
    };
    Ok(read_line(cx, var)?)
}

/// Render the terminal prompt: `{message} [{default}]: ` with a default,
/// `{message}: ` without one. Pure so unit tests pin it without a TTY.
#[cfg_attr(miri, allow(dead_code))]
pub(super) fn format_prompt(message: &str, default: Option<&str>) -> String {
    match default {
        Some(d) => format!("{message} [{d}]: "),
        None => format!("{message}: "),
    }
}

/// Map a trimmed answer line onto its binding: empty input yields the
/// default when one exists, else an empty string. Pure so unit tests pin
/// it without a TTY.
#[cfg_attr(miri, allow(dead_code))]
pub(super) fn apply_prompt_answer(line: &str, default: Option<&str>) -> String {
    if line.is_empty() {
        default.unwrap_or("").to_string()
    } else {
        line.to_string()
    }
}

pub(super) fn prompt<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    var: &str,
    message: &str,
    default: Option<&str>,
) -> Result<()> {
    // No Miri special-case: `SyntheticProcessManager::stdin_is_terminal`
    // reports false without touching a fd, so under Miri this always
    // takes the non-interactive branch below (default binds, else
    // bail). The terminal-reading tail is unreachable there.
    let clean_var = var.trim_start_matches('$').to_string();
    // Interactivity needs both a host TTY and an unredirected
    // stdin: a `WITH_IO [stdin=...]` pipe can never answer a
    // prompt, so PROMPT short-circuits instead of blocking on it.
    if !cx.process.stdin_is_terminal() || cx.stdin.is_redirected() {
        match default {
            Some(d) => {
                eprintln!("non-interactive: using default for {message}");
                let text = Value::string(d.to_string());
                bind_prompt_var(cx, &clean_var, text)?;
                return Ok(());
            }
            None => {
                return Err(anyhow::Error::new(diag::eval_prompt_no_answer(message)));
            }
        }
    }
    let rendered = format_prompt(message, default);
    {
        use std::io::Write;
        let mut err = std::io::stderr().lock();
        err.write_all(rendered.as_bytes())
            .map_err(|e| anyhow::Error::new(diag::eval_prompt_write_failed(message, &e)))?;
        err.flush()
            .map_err(|e| anyhow::Error::new(diag::eval_prompt_flush_failed(message, &e)))?;
    }
    let mut line = String::new();
    let n = std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| anyhow::Error::new(diag::eval_prompt_read_failed(message, &e)))?;
    if n == 0 && line.is_empty() {
        return Err(anyhow::Error::new(diag::eval_prompt_eof(message)));
    }
    let trimmed = line
        .strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .unwrap_or(&line);
    let text = Value::string(apply_prompt_answer(trimmed, default));
    bind_prompt_var(cx, &clean_var, text)?;
    Ok(())
}

#[cfg_attr(miri, allow(dead_code))]
fn bind_prompt_var<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    clean_var: &str,
    text: Value,
) -> Result<()> {
    if cx.state.get_var_typed(clean_var).is_some() {
        cx.state
            .mutate_var(clean_var, text)
            .map_err(|e| anyhow::Error::new(diag::eval_prompt_bind_failed(clean_var, e)))?;
    } else {
        cx.state
            .declare_var(clean_var.to_string(), TypeTag::String, text)
            .map_err(|e| anyhow::Error::new(diag::eval_prompt_bind_failed(clean_var, e)))?;
    }
    Ok(())
}

pub(crate) fn dispatch_prompt<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Prompt {
        var,
        message,
        default,
    } = step
    else {
        unreachable!()
    };
    let message_resolved = super::args::resolve_arg(message, cx)?;
    let default_resolved = match default {
        Some(d) => Some(super::args::resolve_arg(d, cx)?),
        None => None,
    };
    prompt(cx, var, &message_resolved, default_resolved.as_deref())
}

/// Dispatch `LIST_APPEND $list <item>` : append to a LIST binding in
/// place (copy-on-write: sole owners mutate with no copy, aliases
/// detach). Touches no stream: like SLEEP it is a step effect, not an
/// IO filter, so pipeline bindings resolve but carry no bytes.
pub(super) fn push_into<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    var: &str,
    item: Value,
) -> Result<()> {
    cx.state
        .push_into_list(var, item)
        .map_err(|e| anyhow::Error::new(diag::eval_list_append_failed(var, e)))
}

pub(crate) fn dispatch_push_into_step<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::ListAppend { list, item } = step else {
        unreachable!()
    };
    let value = super::args::evaluate_assert_operand(item, cx)?;
    push_into(cx, list, value)
}

pub(crate) fn dispatch_write<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Write { path, contents } = step else {
        unreachable!()
    };
    let path_resolved = super::args::resolve_arg(path, cx)?;
    let contents_resolved = super::args::resolve_arg_opt(contents, cx)?;
    Ok(write(cx, &path_resolved, contents_resolved.as_deref())?)
}

pub(crate) fn dispatch_append<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Append { path, contents } = step else {
        unreachable!()
    };
    let path_resolved = super::args::resolve_arg(path, cx)?;
    let contents_resolved = super::args::resolve_arg_opt(contents, cx)?;
    Ok(append(cx, &path_resolved, contents_resolved.as_deref())?)
}

pub(crate) fn dispatch_append_line<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::AppendLine { path, contents } = step else {
        unreachable!()
    };
    let path_resolved = super::args::resolve_arg(path, cx)?;
    let contents_resolved = super::args::resolve_arg(contents, cx)?;
    append_line(cx, &path_resolved, &contents_resolved)
}

pub(super) fn append_line<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    path: &str,
    contents: &str,
) -> Result<()> {
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, path)
        .map_err(|e| anyhow::Error::new(diag::eval_append_line_resolve_failed(path, &e)))?;
    cx.state.fs.ensure_parent_dir(&target).map_err(|e| {
        anyhow::Error::new(diag::eval_ensure_parent_failed(
            &target.display().to_string(),
            &e,
        ))
    })?;
    let mut line = contents.to_string();
    line.push('\n');
    cx.state
        .fs
        .append_file(&target, line.as_bytes())
        .map_err(|e| {
            anyhow::Error::new(diag::eval_append_failed(&target.display().to_string(), &e))
        })?;
    Ok(())
}

pub(super) fn remove<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    path: &str,
    recursive: bool,
) -> Result<()> {
    // Removal resolves against the ambient cwd. The deletion guard
    // below compares against the real run cwd and roots, so a module
    // can never remove the caller's working directory out from
    // under it.
    let cwd = cx.state.cwd.clone();
    let target = cx
        .state
        .fs
        .resolve_write(&cwd, path)
        .map_err(|e| anyhow::Error::new(diag::eval_remove_failed(path, &e)))?;
    // Root and working-directory deletion guard: removing the cwd (or any
    // of its ancestors), the workspace root, or the effective root (`/`
    // under `WORKSPACE SYSTEM`) would destroy the execution context or
    // the host root, so fail closed before any filesystem driver runs.
    // Both sides compare canonicalized: symlinked prefixes would
    // otherwise defeat a lexical equality check. Guarded against the
    // ambient cwd, which is the same in every file.
    let guard_cwd = cx.state.cwd.clone();
    let cwd_canon = cx
        .state
        .fs
        .canonicalize(&guard_cwd)
        .unwrap_or_else(|_| guard_cwd.clone());
    if target == cwd_canon
        || cwd_canon.as_path().starts_with(target.as_path())
        || Some(&target) == cx.state.fs.workspace_root()
        || target == *cx.state.fs.root()
    {
        return Err(anyhow::Error::new(diag::eval_remove_protected(path)));
    }
    // Lexical link probe: canonical resolution routes through the
    // final link, so a link would otherwise read as its target and the
    // target would be deleted instead of the link.
    if cx
        .state
        .fs
        .remove_link_if_symlink(&cwd, path)
        .map_err(|e| anyhow::Error::new(diag::eval_remove_failed(path, &e)))?
    {
        return Ok(());
    }
    match cx.state.fs.entry_kind_no_follow(&target) {
        Err(_) => Err(anyhow::Error::new(diag::eval_remove_missing(path))),
        Ok(EntryKind::File) => {
            cx.state
                .fs
                .remove_file(&target)
                .map_err(|e| anyhow::Error::new(diag::eval_remove_failed(path, &e)))?;
            Ok(())
        }
        // A link reaching here appeared between the probe and the
        // resolve: fail closed rather than touch its target.
        Ok(EntryKind::Symlink) => Err(anyhow::Error::new(diag::eval_remove_link_changed(path))),
        Ok(EntryKind::Dir) => {
            if recursive {
                cx.state
                    .fs
                    .remove_dir_all(&target)
                    .map_err(|e| anyhow::Error::new(diag::eval_remove_failed(path, &e)))?;
                Ok(())
            } else {
                cx.state.fs.remove_empty_dir(&target).map_err(|e| {
                    anyhow::Error::new(diag::eval_remove_not_empty_failed(path, &e))
                })?;
                Ok(())
            }
        }
    }
}

pub(crate) fn dispatch_remove<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Remove { path, recursive } = step else {
        unreachable!()
    };
    let path_resolved = super::args::resolve_arg(path, cx)?;
    remove(cx, &path_resolved, *recursive)
}

pub(crate) fn dispatch_expand<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Expand { path, overrides } = step else {
        unreachable!()
    };
    let path_resolved = super::args::resolve_arg_opt(path, cx)?;
    let overrides_resolved = super::args::resolve_overrides(overrides, cx)?;
    replace(cx, &path_resolved, &overrides_resolved)
}

fn dispatch_assert_equality<P: ProcessManager>(
    hash: &Option<String>,
    actual: &oxdock_parser::AssertTarget,
    expected: &Option<Arg>,
    cx: &mut StepCtx<'_, P>,
    verb: &str,
    expect_equal: bool,
) -> Result<()> {
    let target = super::steps::resolve_assert_target(actual, cx)?;
    let expected_val = match expected {
        Some(e) => Some(super::args::evaluate_assert_operand(e, cx)?),
        None => None,
    };
    assert_equality(
        cx,
        0,
        verb,
        expect_equal,
        hash,
        &target,
        expected_val.as_ref(),
    )
}

pub(crate) fn dispatch_assert_eq<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::AssertEq {
        hash,
        actual,
        expected,
    } = step
    else {
        unreachable!()
    };
    dispatch_assert_equality(hash, actual, expected, cx, Command::AssertEq.as_str(), true)
}

pub(crate) fn dispatch_assert_ne<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::AssertNe {
        hash,
        actual,
        expected,
    } = step
    else {
        unreachable!()
    };
    dispatch_assert_equality(
        hash,
        actual,
        expected,
        cx,
        Command::AssertNe.as_str(),
        false,
    )
}

pub(crate) fn dispatch_assert_contains<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::AssertContains { haystack, needle } = step else {
        unreachable!()
    };
    let target = super::steps::resolve_assert_target(haystack, cx)?;
    assert_contains(cx, 0, 0, &target, needle)
}

pub(crate) fn dispatch_exit<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Exit(code) = step else {
        unreachable!()
    };
    let code = super::args::resolve_arg_as_int(code, cx)?;
    exit(cx, code)
}

/// Write a `FAIL` message to stderr: same single-`write_all` atomicity
/// as [`echo`], but on the error stream so failures never pollute
/// captured stdout.
pub(super) fn fail_message<P: ProcessManager>(cx: &mut StepCtx<'_, P>, msg: &str) -> Result<()> {
    let line = format!("{msg}\n");
    write_stderr(cx.err.clone(), |writer| {
        writer
            .write_all(line.as_bytes())
            .map_err(|e| diag::eval_stream_write_failed(&e))?;
        Ok(())
    })?;
    Ok(())
}

/// Run a `FAIL` step: the optional block executes first with its stdout
/// rerouted to the error stream (every nested `ECHO`, `RUN`, or callee
/// inherits the swapped handle unless an inner `WITH_IO` overrides it),
/// then the optional message prints to stderr and the pipeline exits
/// through the shared [`exit`] path with `code` (default 1).
pub(crate) fn fail<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    code: &Option<Arg>,
    message: &Option<Arg>,
    body: &[Step],
) -> Result<Flow> {
    if !body.is_empty() {
        // Rerouted body stdout must not leak into the caller's
        // stdout pipe: `None` carries no backend, so a nested
        // `REMOTE` (or any future `out_pipe` consumer) has no
        // caller pipe to stream into, and its output falls back
        // to the rerouted error stream. `stdin_pipe` still rides
        // along: input routing is unaffected by the swap.
        // `None` also inherits host stdout, so a bare swap would be
        // a no-op there: pin an explicit host-stderr sink in that
        // case, while captured setups reuse the live `err` handle.
        // Every nested `ECHO`, `RUN`, or callee inherits the swap
        // unless an inner `WITH_IO` overrides it for that step.
        let body_out = match cx.err.clone() {
            Some(handle) => Some(handle),
            None => Some(StreamHandle::Stream(Arc::new(
                Mutex::new(std::io::stderr()),
            ))),
        };
        let flow = super::steps::execute_scoped_steps(
            cx.state,
            cx.process,
            body,
            cx.stdin.clone(),
            cx.expose_stdin,
            body_out,
            cx.err.clone(),
            None,
            cx.stdin_pipe.clone(),
            false,
        )?;
        if !matches!(flow, Flow::Done) {
            return Ok(flow);
        }
    }
    // Argument resolution runs after the block: the block executes
    // first, so a body mutation of the code variable takes effect
    // and an invalid code cannot skip cleanup.
    let code = match code {
        Some(arg) => super::args::resolve_arg_as_int(arg, cx)?,
        None => 1,
    };
    if let Some(msg) = message {
        let msg = super::args::resolve_arg(msg, cx)?;
        fail_message(cx, &msg)?;
    }
    exit(cx, code).map(|()| Flow::Done)
}

pub(crate) fn dispatch_fail<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Fail {
        code,
        message,
        body,
    } = step
    else {
        unreachable!()
    };
    top_level_flow(fail(cx, code, message, body)?)
}

pub(crate) fn dispatch_inherit_env<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::InheritEnv { keys } = step else {
        unreachable!()
    };
    inherit_env(cx, keys)
}

pub(crate) fn dispatch_isolated_inherit_env<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::IsolatedInheritEnv { keys } = step else {
        unreachable!()
    };
    isolated_inherit_env(cx, keys)
}

// ── Structural dispatch wrappers ────────────────────────────────────────────

pub(crate) fn dispatch_for_loop<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::For {
        key_var,
        key_type,
        var,
        var_type,
        in_expr,
        body,
    } = step
    else {
        unreachable!()
    };
    top_level_flow(for_loop(
        cx,
        key_var.as_deref(),
        key_type.clone(),
        var,
        var_type.clone(),
        in_expr,
        body,
    )?)
}

pub(crate) fn dispatch_if_then<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::If {
        cond,
        then_body,
        else_ifs,
        else_body,
    } = step
    else {
        unreachable!()
    };
    top_level_flow(if_then(cx, cond, then_body, else_ifs, else_body)?)
}

pub(crate) fn dispatch_switch<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Switch {
        discriminant,
        cases,
        default_body,
    } = step
    else {
        unreachable!()
    };
    top_level_flow(switch_run(cx, discriminant, cases, default_body)?)
}

pub(crate) fn dispatch_set<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Set { var, expr } = step else {
        unreachable!()
    };
    set_var_value(cx, var, expr)
}

pub(crate) fn dispatch_with_io<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::WithIo { bindings, cmd } = step else {
        unreachable!()
    };
    top_level_flow(with_io(cx, 0, 0, bindings, cmd)?)
}

pub(crate) fn dispatch_with_io_block<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::WithIoBlock { bindings } = step else {
        unreachable!()
    };
    with_io_block(cx, 0, 0, bindings)
}

pub(crate) fn dispatch_func_def<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::FuncDef {
        name,
        params,
        return_type,
        is_async,
        body,
        ..
    } = step
    else {
        unreachable!()
    };
    define_func(cx, name, params, return_type, *is_async, body)
}

pub(crate) fn dispatch_import_file<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::ImportFile { path, alias } = step else {
        unreachable!()
    };
    super::import::import_file(cx, path, alias).map(|_| ())
}

pub(crate) fn dispatch_import_funcs<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::ImportFuncs {
        path,
        alias,
        func_names,
        type_names,
    } = step
    else {
        unreachable!()
    };
    super::import::import_funcs(cx, path, alias.as_deref(), func_names, type_names)
}

pub(crate) fn dispatch_call<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Call { name, args } = step else {
        unreachable!()
    };
    call_func_value(cx, name, args).map(|_| ())
}

pub(crate) fn dispatch_return<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Return { expr, .. } = step else {
        unreachable!()
    };
    top_level_flow(handle_return(cx, 0, expr)?)
}

pub(crate) fn dispatch_while_loop<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::While { cond, body } = step else {
        unreachable!()
    };
    top_level_flow(while_loop(cx, cond, body)?)
}

pub(crate) fn dispatch_break<P: ProcessManager>(
    step: &StepKind,
    _cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Break = step else {
        unreachable!()
    };
    Err(anyhow::Error::new(diag::eval_break_outside_loop()))
}

pub(crate) fn dispatch_continue<P: ProcessManager>(
    step: &StepKind,
    _cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Continue = step else {
        unreachable!()
    };
    Err(anyhow::Error::new(diag::eval_continue_outside_loop()))
}

// ── AWAIT / AssignAsync handlers ─────────────────────────────────────────

/// Whether an ASYNC body can publish a value: true when any step in its
/// own scope is a `RETURN`. Nested `FUNC` definitions and nested `ASYNC`
/// bodies belong to inner scopes and never count, and bare `Call` and
/// bare `IMPORT` statements discard their values. A body that can return
/// but falls off the end fails its capture loudly, so a missing fallback
/// branch can never silently yield the void-task zero.
fn body_returns_value(body: &[Step]) -> bool {
    body.iter().any(|step| step_returns_value(&step.kind))
}

fn step_returns_value(kind: &StepKind) -> bool {
    match kind {
        StepKind::Return { .. } => true,
        // Bare `Call` and bare `IMPORT` statements discard their values
        // in every position, task bodies included: only `RETURN`
        // publishes.
        StepKind::WithIo { cmd, .. } => step_returns_value(cmd),
        StepKind::For { body, .. }
        | StepKind::While { body, .. }
        | StepKind::Timeout { body, .. }
        | StepKind::Isolated { body } => body_returns_value(body),
        StepKind::If {
            then_body,
            else_ifs,
            else_body,
            ..
        } => {
            body_returns_value(then_body)
                || else_ifs
                    .iter()
                    .any(|(_, branch)| body_returns_value(branch))
                || else_body
                    .as_ref()
                    .is_some_and(|body| body_returns_value(body))
        }
        StepKind::Switch {
            cases,
            default_body,
            ..
        } => {
            cases.iter().any(|(_, branch)| body_returns_value(branch))
                || default_body
                    .as_ref()
                    .is_some_and(|body| body_returns_value(body))
        }
        StepKind::FuncDef { .. } | StepKind::AsyncBlock { .. } | StepKind::Bind { .. } => false,
        _ => false,
    }
}

/// Dispatch `LET $var: TYPE = ASYNC { ... }` : spawn a background task and store
/// the handle in the variable scope.
pub(crate) fn dispatch_assign_async<P: ProcessManager>(
    var: &str,
    decl_type: String,
    body: &[Step],
    cx: &mut StepCtx<'_, P>,
    is_const: bool,
) -> Result<()> {
    // Generate a unique task ID. Allocation refuses past the live-task
    // cap, so a spawn storm fails the step loudly instead of wedging
    // the host.
    let task_id = cx.state.alloc_task_id()?;

    // Pre-allocate keeper handles synchronously on this thread, before the
    // worker exists, so pipes the task produces to (e.g. keeper
    // `WITH_IO [stdout=$tx] ASYNC ...` bindings) can never observe a
    // transient-only zero-writer window. Guards expire by step index as
    // the worker completes its final producer steps. The pin walk judges
    // promotion from the worker's perspective (task_id is already drawn).
    let body = body.to_vec();
    let expiry = pin_async_keepers(cx, &body, task_id)?;

    // Fork the execution state for the child thread
    let mut forked_state = cx.state.fork();
    forked_state.keeper_expiry = expiry;
    // Stamp the worker identity before the thread starts, and mark every
    // pipe visible here as shared: forking copies the whole environment,
    // so each of them is capturable whether or not the body names it.
    // Either step keeps later promotion checks honest on every thread.
    forked_state.task_id = task_id;
    cx.state.mark_captured_pipes_escaped();
    let forked_process = cx.process.clone();
    let stdin = cx.stdin.clone();
    let expose_stdin = cx.expose_stdin;
    // Tasks share the parent writer: output streams live, exactly like the
    // main flow. A task's *value* (for `LET $out: STRING = AWAIT $t`) travels only
    // through an explicit `RETURN`, like a function call. A copy of every
    // default-routed byte also lands in the task's tail ring for
    // `TASK_POLL`. Stderr keeps parent wiring. Live pipe backends ride
    // along the same way, so a REMOTE step in the task keeps streaming.
    let tails = std::sync::Arc::new(std::sync::Mutex::new(super::state::TaskTails::new()));
    let out = TaskTee::wrap(cx.out.clone(), &tails, TeeStream::Stdout);
    let err = TaskTee::wrap(cx.err.clone(), &tails, TeeStream::Stderr);
    let out_pipe = cx.out_pipe.clone();
    let stdin_pipe = cx.stdin_pipe.clone();
    // Same lifetime hold as the block-task worker: a concurrent drain
    // must block through producer lulls, not observe a flickered EOF.
    let out_keepalive = out_pipe
        .as_ref()
        .map(|backend| KeeperGuard::new(std::sync::Arc::clone(backend)));
    let cancel_token = std::sync::Arc::clone(&forked_state.cancel_token);
    let cancel_hub = std::sync::Arc::clone(&forked_state.cancel_hub);
    let active_process = std::sync::Arc::clone(&forked_state.active_process);
    let worker: std::sync::Arc<std::sync::Mutex<Option<std::thread::ThreadId>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let worker_child = std::sync::Arc::clone(&worker);

    // Spawn the task thread. Leftover guards unpin at thread termination.
    // Every body runs the block path below: a task publishes a value
    // only through an explicit `RETURN`, which the entry records for
    // `LET $out: STRING = AWAIT $t`. A body with no `RETURN` yields `INT` 0 on
    // success; a body that can return but fell off the end fails its
    // capture loudly. Control flow never crosses the thread boundary:
    // stray BREAK/CONTINUE become errors here.
    // A task publishes a value only through an explicit `RETURN`.
    // Recorded on the entry so a later capture can tell "void task"
    // (binds 0) from "fell off the end" (loud error).
    let returns_value = body_returns_value(&body);
    let (entry_tx, entry_rx) = std::sync::mpsc::channel::<Arc<super::state::TaskEntry>>();
    let join = std::thread::spawn(move || {
        // Slot guard first: dropping on return or unwind releases the
        // global live-task count, so the cap tracks threads actually
        // alive rather than table entries.
        let _slot =
            super::state::TaskSlotGuard::new(std::sync::Arc::clone(&forked_state.live_tasks));
        *worker_child.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::thread::current().id());
        let _out_keepalive = out_keepalive;
        let mut child_state = forked_state;
        let mut child_process = forked_process;
        let entry = entry_rx
            .recv()
            .map_err(|_| anyhow::Error::new(diag::eval_async_entry_unavailable()))?;
        let flow = super::steps::execute_steps(
            &mut child_state,
            &mut child_process,
            &body,
            stdin,
            expose_stdin,
            out,
            err,
            out_pipe,
            stdin_pipe,
            true,
        )?;
        match flow {
            Flow::Done => Ok(()),
            Flow::Break { idx } => Err(anyhow::Error::new(diag::eval_async_flow_crosses_async(
                idx + 1,
                KEYWORD_BREAK,
            ))),
            Flow::Continue { idx } => Err(anyhow::Error::new(diag::eval_async_flow_crosses_async(
                idx + 1,
                KEYWORD_CONTINUE,
            ))),
            Flow::Return { value, .. } => {
                // Only an explicit `RETURN` publishes a task value;
                // there is no stdout sniffing. `LET $out: STRING = AWAIT $t`
                // binds this value.
                entry
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .return_value = Some(value);
                Ok(())
            }
        }
    });

    // Create the thread handle
    let handle =
        super::steps::ThreadJoinHandle::new(join, cancel_token, cancel_hub, active_process, worker);

    // Store in named_tasks as a synchronized entry. The handle lives inside
    // the entry so CANCEL can tear it down even under concurrent AWAIT.
    // Published to the child above so tasks store their RETURN value
    // under the entry lock. The entry also owns the tail ring fed by
    // the worker tee, so TASK_POLL can snapshot output mid-flight.
    {
        let mut named = cx
            .state
            .named_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = Arc::new(super::state::TaskEntry::new(
            Box::new(handle),
            returns_value,
            Arc::clone(&tails),
        ));
        named.insert(task_id, Arc::clone(&entry));
        let _ = entry_tx.send(entry);
    }

    // Store the task handle in the variable scope
    let tag = cx.state.resolve_tag(&decl_type)?;
    cx.state
        .declare_binding(var.to_string(), tag, Value::handle(task_id), is_const)?;
    Ok(())
}

/// Dispatch `AWAIT $var` : block until the named task completes, propagate
/// error if it failed.
///
/// State machine (`TaskEntry`): the first `AWAIT` transitions the entry
/// `Running -> Awaiting` and owns the bounded poll loop below. A concurrent
/// `CANCEL` (or a `TIMEOUT` deadline) transitions the entry to `Cancelled`;
/// this loop observes that within ~10ms and rendezvouses on teardown
/// completion before reporting cancellation.
/// Resolve a task-handle variable to its shared registry entry.
fn resolve_task_entry<P: ProcessManager>(
    var: &str,
    cx: &StepCtx<'_, P>,
) -> Result<Arc<super::state::TaskEntry>> {
    // Resolve the variable to a TaskHandle
    let val = cx
        .state
        .get_var(var)
        .ok_or_else(|| anyhow::Error::new(diag::eval_task_var_undefined(var)))?;
    let Some(task_id) = val.as_handle() else {
        return Err(anyhow::Error::new(diag::eval_task_not_handle(var)));
    };
    resolve_task_entry_by_id(task_id, &sigil_var(var), cx)
}

/// Resolve a task id to its shared registry entry. Backs single-handle
/// AWAIT plus the per-member path of LIST AWAIT so double-await and
/// already-reaped reporting never diverge. `TASK_POLL` reuses the same
/// lookup so deserialized ids resolve without a scope variable.
pub(crate) fn resolve_task_entry_by_id<P: ProcessManager>(
    task_id: u64,
    label: &str,
    cx: &StepCtx<'_, P>,
) -> Result<Arc<super::state::TaskEntry>> {
    // Clone the shared entry under a short map lock.
    let entry = {
        let named = cx
            .state
            .named_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        named.get(&task_id).cloned()
    };
    let Some(entry) = entry else {
        return Err(anyhow::Error::new(diag::eval_task_not_found(label)));
    };
    Ok(entry)
}

/// Claim a task entry and run the bounded await poll loop to completion.
/// Shared by bare `AWAIT` and `LET $o: TYPE = AWAIT $t` so cancellation, timeout,
/// double-await, and failure semantics never diverge. Returns the child's
/// exit status. Task output already streamed live during the run; there is
/// nothing to forward or bind here.
fn await_task_entry(
    entry: &Arc<super::state::TaskEntry>,
    cancel_token: &Arc<std::sync::atomic::AtomicBool>,
    var: &str,
) -> Result<std::process::ExitStatus> {
    use super::state::TaskPhase;

    // Claim the entry for awaiting.
    {
        let mut guard = entry.state.lock().unwrap_or_else(|e| e.into_inner());
        match guard.phase {
            TaskPhase::Cancelled => {
                drop(guard);
                // Teardown barrier: never outrun the killer's join.
                entry.wait_reaped();
                return Err(anyhow::Error::new(diag::eval_await_cancelled(var)));
            }
            TaskPhase::Completed | TaskPhase::Awaiting => {
                return Err(anyhow::Error::new(diag::eval_task_not_found(&sigil_var(
                    var,
                ))));
            }
            TaskPhase::Running => {
                guard.phase = TaskPhase::Awaiting;
            }
        }
    }

    // Bounded poll loop. Every iteration runs under a short entry lock and
    // then sleeps ~10ms, so concurrent CANCEL and TIMEOUT preemption land
    // within one tick without holding any mutex across blocking calls.
    loop {
        enum Decision {
            Pending,
            /// Entry was cancelled externally; rendezvous then report.
            Cancelled,
            /// Deadline fired while awaiting: this thread owns the kill.
            KillOnTimeout,
            /// Natural completion with the child's exit status.
            Done(std::process::ExitStatus),
        }
        // Stage the blocking work (if any) under a short lock, then act
        // outside the lock.
        let mut timeout_kill: Option<Box<dyn BackgroundHandle>> = None;
        let decision = {
            let mut guard = entry.state.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(guard.phase, TaskPhase::Cancelled) {
                Decision::Cancelled
            } else if cancel_token.load(std::sync::atomic::Ordering::SeqCst) {
                // Parent TIMEOUT watcher fired while awaiting: the named
                // child's OS process lives on the child's own
                // `active_process`, unreachable from the watcher, so this
                // thread performs the kill via the shared entry.
                match guard.handle.take() {
                    Some(handle) => {
                        guard.phase = TaskPhase::Cancelled;
                        timeout_kill = Some(handle);
                        Decision::KillOnTimeout
                    }
                    // Lost the race with CANCEL's take: fall through to the
                    // teardown barrier below.
                    None => Decision::Cancelled,
                }
            } else {
                match guard.handle.as_mut() {
                    Some(handle) => match handle.try_wait() {
                        Ok(Some(status)) => {
                            let _ = guard.handle.take();
                            guard.phase = TaskPhase::Completed;
                            Decision::Done(status)
                        }
                        Ok(None) => Decision::Pending,
                        Err(err) => {
                            let _ = guard.handle.take();
                            guard.phase = TaskPhase::Completed;
                            // Surface the child failure after marking the
                            // terminal state below.
                            drop(guard);
                            entry.finish_teardown();
                            return Err(err.into());
                        }
                    },
                    // No handle while still active: another thread took it
                    // and flipped the phase; re-observe on the next tick.
                    None => Decision::Cancelled,
                }
            }
        };
        match decision {
            Decision::Pending => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Decision::Cancelled => {
                entry.wait_reaped();
                return Err(anyhow::Error::new(diag::eval_await_cancelled(var)));
            }
            Decision::KillOnTimeout => {
                if let Some(mut handle) = timeout_kill {
                    let _ = handle.kill();
                }
                entry.finish_teardown();
                return Err(anyhow::Error::new(diag::eval_await_timeout(var)));
            }
            Decision::Done(status) => {
                entry.finish_teardown();
                if !status.success() {
                    return Err(anyhow::Error::new(diag::eval_await_exit_status(
                        var, &status,
                    )));
                }
                return Ok(status);
            }
        }
    }
}

/// Dispatch `AWAIT $var` : block until the named task completes, propagate
/// error if it failed, and forward the task's captured stdout to the parent
/// stdout.
///
/// State machine (`TaskEntry`): the first `AWAIT` transitions the entry
/// `Running -> Awaiting` and owns the bounded poll loop below. A concurrent
/// `CANCEL` (or a `TIMEOUT` deadline) transitions the entry to `Cancelled`;
/// this loop observes that within ~10ms and rendezvouses on teardown
/// completion before reporting cancellation.
pub(crate) fn dispatch_await<P: ProcessManager>(var: &str, cx: &mut StepCtx<'_, P>) -> Result<()> {
    let val = cx
        .state
        .get_var(var)
        .ok_or_else(|| anyhow::Error::new(diag::eval_task_var_undefined(var)))?;
    // A LIST of HANDLEs awaits every member in order, so worker pools
    // collected with PUSH join as a group; an empty LIST is a no-op. The
    // first failing member bails like sequential single AWAITs would.
    // `LET $out: STRING = AWAIT $t` stays single-handle: capture needs one output.
    // Bare AWAIT is join-only: task output already streamed live during
    // the run, so there is nothing to forward.
    if let Some(items) = val.as_list().cloned() {
        for (idx, item) in items.iter().enumerate() {
            // `member` feeds callees whose templates add the `$`
            // themselves (`await_task_entry`); `label` feeds this
            // function's own `'{label}'` messages. Splitting the two
            // keeps every rendering at exactly one `$`.
            let member = format!("{var}[{idx}]");
            let label = sigil_var(&member);
            let Some(task_id) = item.as_handle() else {
                return Err(anyhow::Error::new(diag::eval_await_member_not_handle(
                    &label,
                    item.type_name(),
                )));
            };
            let entry = resolve_task_entry_by_id(task_id, &label, cx)?;
            await_task_entry(&entry, &cx.state.cancel_token, &member)?;
        }
        return Ok(());
    }
    let entry = resolve_task_entry(var, cx)?;
    await_task_entry(&entry, &cx.state.cancel_token, var)?;
    Ok(())
}

/// Dispatch `LET $out: TYPE = AWAIT $task` (or `CONST`, via
/// `is_const`) : join like bare `AWAIT` (identical
/// cancellation/timeout/double-await semantics via [`await_task_entry`]),
/// then bind the task's explicit `RETURN` value (coerced to the declared
/// type). A task whose body cannot return yields `INT` 0 on success, like
/// a process exit status; a task that can return but fell off the end
/// without one fails loudly, so a missing fallback branch can never
/// silently bind zero. Failed tasks bail above, so a bound value always
/// means success. Task stdout streams live and is never sniffed.
pub(crate) fn dispatch_await_capture<P: ProcessManager>(
    out_var: &str,
    out_type: String,
    task_var: &str,
    cx: &mut StepCtx<'_, P>,
    is_const: bool,
) -> Result<()> {
    let kw = if is_const { KEYWORD_CONST } else { KEYWORD_LET };
    let entry = resolve_task_entry(task_var, cx)?;
    await_task_entry(&entry, &cx.state.cancel_token, task_var)?;
    let (return_value, returns_value) = {
        let guard = entry.state.lock().unwrap_or_else(|e| e.into_inner());
        (guard.return_value.clone(), guard.returns_value)
    };
    if let Some(value) = return_value {
        let tag = cx.state.resolve_tag(&out_type)?;
        cx.state.declare_binding(
            out_var.trim_start_matches('$').to_string(),
            tag,
            value,
            is_const,
        )?;
        return Ok(());
    }
    if returns_value {
        return Err(anyhow::Error::new(diag::eval_await_fell_off(
            kw, out_var, task_var,
        )));
    }
    let tag = cx.state.resolve_tag(&out_type)?;
    cx.state.declare_binding(
        out_var.trim_start_matches('$').to_string(),
        tag,
        Value::int(0),
        is_const,
    )?;
    Ok(())
}

/// Dispatch `CANCEL $var` : synchronously kill a named background task.
///
/// Blocking and deterministic: this function itself takes the handle from
/// the shared entry and joins the task thread, so return implies the OS
/// process is dead and no residual filesystem/stream mutation can follow.
/// A concurrent `AWAIT` rendezvouses on teardown completion and reports
/// cancellation.
pub(crate) fn dispatch_cancel<P: ProcessManager>(var: &str, cx: &mut StepCtx<'_, P>) -> Result<()> {
    use super::state::TaskPhase;

    let val = cx
        .state
        .get_var(var)
        .ok_or_else(|| anyhow::Error::new(diag::eval_task_var_undefined(var)))?;
    let Some(task_id) = val.as_handle() else {
        return Err(anyhow::Error::new(diag::eval_task_not_handle(var)));
    };

    let entry = {
        let named = cx
            .state
            .named_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        named.get(&task_id).cloned()
    };
    let Some(entry) = entry else {
        return Err(anyhow::Error::new(diag::eval_cancel_gone(var)));
    };

    // Transition to Cancelled and take the handle under a short lock.
    let handle = {
        let mut guard = entry.state.lock().unwrap_or_else(|e| e.into_inner());
        match guard.phase {
            TaskPhase::Cancelled => {
                return Err(anyhow::Error::new(diag::eval_cancel_already(var)));
            }
            TaskPhase::Completed => {
                return Err(anyhow::Error::new(diag::eval_cancel_gone(var)));
            }
            TaskPhase::Running | TaskPhase::Awaiting => {
                guard.phase = TaskPhase::Cancelled;
                guard.handle.take()
            }
        }
    };

    // Synchronous teardown outside every lock: signal the child token,
    // SIGKILL the active process, join the thread.
    if let Some(mut handle) = handle {
        handle.kill()?;
    }
    entry.finish_teardown();
    Ok(())
}

/// Pipeline dispatch wrapper for every `Bind` right-hand side.
/// Single owner, shared with the step-level `dispatch_bind`.
pub(crate) fn dispatch_bind_step<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Bind {
        var,
        decl_type,
        is_const,
        rhs,
    } = step
    else {
        unreachable!()
    };
    use oxdock_parser::BindRhs;
    match rhs {
        BindRhs::Expr(expr) => assign(cx, var, decl_type.clone(), expr, *is_const),
        BindRhs::Capture(cmd) => top_level_flow(assign_capture(
            cx,
            super::steps::allocate_assert_generation(),
            0,
            var,
            decl_type.clone(),
            cmd,
            *is_const,
        )?),
        BindRhs::Async(body) => dispatch_assign_async(var, decl_type.clone(), body, cx, *is_const),
        BindRhs::Await { task_var, .. } => {
            dispatch_await_capture(var, decl_type.clone(), task_var, cx, *is_const)
        }
    }
}

/// Pipeline dispatch wrapper for `Await`
pub(crate) fn dispatch_await_step<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Await { var, .. } = step else {
        unreachable!()
    };
    dispatch_await(var, cx)
}

/// Pipeline dispatch wrapper for `Cancel`
pub(crate) fn dispatch_cancel_step<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Cancel { var, .. } = step else {
        unreachable!()
    };
    dispatch_cancel(var, cx)
}

/// Pipeline dispatch wrapper for `Timeout`
pub(crate) fn dispatch_timeout_step<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Timeout { duration, body } = step else {
        unreachable!()
    };
    let duration = super::args::resolve_arg_as_duration(duration, cx)?;
    top_level_flow(timeout(cx, &duration, body)?)
}

/// Pipeline dispatch wrapper for `Isolated`
pub(crate) fn dispatch_isolated_step<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Isolated { body } = step else {
        unreachable!()
    };
    top_level_flow(isolated(cx, body)?)
}

/// Owned deadline watch shared by `TIMEOUT` steps and deadline-bound
/// captures: arming spawns the watchdog, marks the state cancellable,
/// and floors the task-id counter so the scope-end reaper joins only
/// workers this scope spawns; disarming joins the watchdog and
/// restores everything, turning a fired deadline into the standard
/// deadline error even when the guarded operation succeeded.
struct DeadlineGuard {
    done: Arc<std::sync::atomic::AtomicBool>,
    fired: Arc<std::sync::atomic::AtomicBool>,
    watcher: Option<std::thread::JoinHandle<()>>,
    prev_cancellable: bool,
    prev_floor: Option<u64>,
    initial_cancel: bool,
}

/// Arm a deadline watch around the work that follows. `None` is a
/// no-op returning `None`, so call sites with an optional deadline
/// need no branching.
fn arm_deadline<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    duration: Option<std::time::Duration>,
) -> Option<DeadlineGuard> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    let duration = duration?;
    let initial_cancel = cx.state.cancel_token.load(Ordering::SeqCst);
    let deadline = Instant::now() + duration;
    let done = Arc::new(AtomicBool::new(false));
    let fired = Arc::new(AtomicBool::new(false));
    let cancel_token = Arc::clone(&cx.state.cancel_token);
    let cancel_hub = Arc::clone(&cx.state.cancel_hub);
    let active_process = Arc::clone(&cx.state.active_process);

    // Deadline watcher: enforces the timeout while the guarded work
    // runs on this thread. Exits as soon as `done` is set; joined on
    // disarm, so no thread outlives the guard.
    let watcher_done = Arc::clone(&done);
    let watcher_fired = Arc::clone(&fired);
    let watcher = std::thread::spawn(move || {
        loop {
            if watcher_done.load(Ordering::SeqCst) {
                return;
            }
            if Instant::now() >= deadline {
                watcher_fired.store(true, Ordering::SeqCst);
                cancel_token.store(true, Ordering::SeqCst);
                cancel_hub.mark_cancelled();
                if let Ok(mut guard) = active_process.lock()
                    && let Some(proc) = guard.as_mut()
                {
                    let _ = proc.kill();
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });

    // Deadline enforcement for nested RUNs: a blocking foreground
    // process registers in active_process, where the watcher can
    // reach it. Restored afterwards (nesting-safe: an outer TIMEOUT
    // already sets this, and the restore preserves it).
    //
    // Scope-bounded join: snapshot the task-id counter so the scope-end
    // reaper joins only workers this scope spawns. A worker the scope
    // leaves running gets up to the deadline, then the fail-fast
    // teardown stops it. Pre-existing tasks keep running past the scope
    // instead of holding it open: without the floor, a long-lived
    // server task turns every later TIMEOUT into a wait for its
    // deadline.
    let prev_cancellable = cx.state.cancellable;
    cx.state.cancellable = true;
    let floor = cx.state.next_task_id.load(Ordering::SeqCst);
    let prev_floor = cx.state.reap_floor.replace(floor);
    Some(DeadlineGuard {
        done,
        fired,
        watcher: Some(watcher),
        prev_cancellable,
        prev_floor,
        initial_cancel,
    })
}

/// Disarm a deadline watch, joining the watchdog and restoring state.
/// A fired deadline becomes the standard deadline error, wrapping the
/// guarded result when it erred. `None` passes the result through, so
/// deadline-free call sites share the same code path.
fn disarm_deadline<T, P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    mut guard: Option<DeadlineGuard>,
    duration: Option<std::time::Duration>,
    result: Result<T>,
) -> Result<T> {
    use std::sync::atomic::Ordering;

    let Some(guard) = guard.as_mut() else {
        return result;
    };
    cx.state.cancellable = guard.prev_cancellable;
    cx.state.reap_floor = guard.prev_floor.take();

    guard.done.store(true, Ordering::SeqCst);
    if let Some(watcher) = guard.watcher.take() {
        let _ = watcher.join();
    }

    // Restore pre-existing cancellation state if deadline watcher did not fire
    if !guard.fired.load(Ordering::SeqCst) {
        cx.state
            .cancel_token
            .store(guard.initial_cancel, Ordering::SeqCst);
    }

    let budget = oxdock_parser::command::format_duration(
        &duration.expect("disarm with a guard always carries a duration"),
    );
    match result {
        Ok(value) => {
            if guard.fired.load(Ordering::SeqCst) {
                return Err(anyhow::Error::new(diag::eval_timeout_deadline(&budget)));
            }
            Ok(value)
        }
        Err(err) => {
            if guard.fired.load(Ordering::SeqCst) {
                Err(err).with_context(|| diag::eval_timeout_deadline(&budget))
            } else {
                Err(err)
            }
        }
    }
}

/// Dispatch `TIMEOUT <duration> <body>`: run `body` on the current thread
/// with a deadline. If the deadline elapses first, cancel the state,
/// `kill()` the active foreground process (when one is registered), and
/// return a deadline error wrapping the body result; a body error with
/// time still left returns unwrapped.
///
/// `idx` is the 0-based step index used for error attribution.
pub(crate) fn timeout<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    duration: &std::time::Duration,
    body: &[Step],
) -> Result<Flow> {
    let guard = arm_deadline(cx, Some(*duration));
    let result = super::steps::execute_scoped_steps(
        cx.state,
        cx.process,
        body,
        cx.stdin.clone(),
        cx.expose_stdin,
        cx.out.clone(),
        cx.err.clone(),
        cx.out_pipe.clone(),
        cx.stdin_pipe.clone(),
        true,
    );
    disarm_deadline(cx, guard, Some(*duration), result)
}

/// RAII scope for `ISOLATED` bodies: snapshots the script env, clears it,
/// and restores both on drop across every exit path (`?`, `bail!`, early
/// return). Wraps `&mut StepCtx` (not `&mut ExecState`): holding the state
/// borrow directly would collide with the mutable borrow
/// `execute_scoped_steps` needs (E0499), so the body executes through the
/// `DerefMut` reborrow instead.
struct IsolatedScopeGuard<'a, 'b, P: ProcessManager> {
    cx: &'a mut StepCtx<'b, P>,
}

impl<'a, 'b, P: ProcessManager> IsolatedScopeGuard<'a, 'b, P> {
    fn enter(cx: &'a mut StepCtx<'b, P>) -> Self {
        cx.state.isolated_stack.push(Arc::clone(&cx.state.envs));
        cx.state.envs = Arc::clone(&cx.state.seed);
        Self { cx }
    }
}

impl<'a, 'b, P: ProcessManager> std::ops::Deref for IsolatedScopeGuard<'a, 'b, P> {
    type Target = StepCtx<'b, P>;

    fn deref(&self) -> &Self::Target {
        self.cx
    }
}

impl<'a, 'b, P: ProcessManager> std::ops::DerefMut for IsolatedScopeGuard<'a, 'b, P> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.cx
    }
}

impl<P: ProcessManager> Drop for IsolatedScopeGuard<'_, '_, P> {
    fn drop(&mut self) {
        // Tolerant pop: imbalance is unreachable by construction (entry
        // always pushes before body code runs), and a panicking `Drop`
        // during unwind would abort the process. Worst case the live
        // envs stay as the body left them, exactly like a normal scope
        // whose pop failed.
        if let Some(snapshot) = self.cx.state.isolated_stack.pop() {
            self.cx.state.envs = snapshot;
        }
    }
}

/// Execute an `ISOLATED` body with the seeded script environment:
/// the map resets to the session seed, and only names the block
/// re-adds through its leading allowlist reach spawned children on
/// top of that seed. Variables, pipes, and files flow through
/// untouched; scope exit restores the prior environment via the
/// guard, on every path.
pub(crate) fn isolated<P: ProcessManager>(cx: &mut StepCtx<'_, P>, body: &[Step]) -> Result<Flow> {
    let mut guard = IsolatedScopeGuard::enter(cx);
    // Reborrow through the guard once: field borrows off a direct `&mut`
    // place stay disjoint (unlike repeated `DerefMut` autorefs), so the
    // ten borrows below coexist. The guard outlives the call and its
    // `Drop` restores on every exit path, including `Err` returns.
    let scoped: &mut StepCtx<'_, P> = &mut guard;
    let result = super::steps::execute_scoped_steps(
        scoped.state,
        scoped.process,
        body,
        scoped.stdin.clone(),
        scoped.expose_stdin,
        scoped.out.clone(),
        scoped.err.clone(),
        scoped.out_pipe.clone(),
        scoped.stdin_pipe.clone(),
        false,
    );
    drop(guard);
    // Inner steps number from the block, not the script: name the
    // boundary so `step 1: RUN ...` inside ISOLATED never reads as the
    // script's first step.
    result.with_context(diag::eval_inside_isolated)
}

/// Resolve a block allowlist against the enclosing `ISOLATED` snapshot
/// only: never host env, never host-provided overrides. Absent names stay
/// absent silently (matching top-level `INHERIT_ENV`), so optional tokens
/// stay expressible. Outside any block this is unreachable by
/// construction (the parser only produces the node inside `ISOLATED`,
/// and the static pass rejects strays); the bail is defense in depth.
pub(crate) fn isolated_inherit_env<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    keys: &[String],
) -> Result<()> {
    let Some(snapshot) = cx.state.isolated_stack.last().cloned() else {
        return Err(anyhow::Error::new(diag::eval_isolated_outside_allowlist()));
    };
    let envs = Arc::make_mut(&mut cx.state.envs);
    for key in keys {
        let matched = snapshot
            .iter()
            .find(|(k, _)| oxdock_process::names_eq(k, key));
        if let Some((_, value)) = matched {
            // De-duplicate casing variants before inserting, so the map
            // never carries two spellings of one name into dispatch.
            envs.retain(|k, _| !oxdock_process::names_eq(k, key));
            envs.insert(key.clone(), value.clone());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{apply_prompt_answer, format_prompt};

    #[test]
    fn prompt_format_shows_default_in_brackets() {
        assert_eq!(
            format_prompt("Install destination?", Some("/tmp/bin")),
            "Install destination? [/tmp/bin]: "
        );
        assert_eq!(
            format_prompt("Install destination?", None),
            "Install destination?: "
        );
    }

    #[test]
    fn prompt_answer_prefers_input_over_default() {
        assert_eq!(
            apply_prompt_answer("/custom/bin", Some("/tmp/bin")),
            "/custom/bin"
        );
        assert_eq!(apply_prompt_answer("", Some("/tmp/bin")), "/tmp/bin");
        assert_eq!(apply_prompt_answer("", None), "");
    }
}
