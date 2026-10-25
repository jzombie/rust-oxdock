use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use flate2::Compression;
use flate2::write::GzEncoder;
use oxdock_fs::CopySourceRoot;
use oxdock_fs::EntryKind;
use oxdock_fs::GuardedPath;
use oxdock_fs::PathResolver;
use oxdock_fs::PolicyPath;
use oxdock_fs::command_path;
use oxdock_fs::to_forward_slashes;
use oxdock_parser::{
    Arg, Expr, IoBinding, IoStream, PipeTarget, Step, StepKind, TypeTag, Value, WorkspaceTarget,
    render_structural,
};
use oxdock_process::{
    BackgroundHandle, CommandOptions, CommandResult, CommandStderr, CommandStdin, CommandStdout,
    INHERIT_STDOUT_ENV_VAR, PROCESS_DEBUG_ENV_VAR, ProcessManager, SharedInput, batch_shell,
    check_spawn_anchors, read_pump_chunk,
};
use oxdock_pty::{PtyInput, PtyOutcome, PtyRequest, WatchSize, run_pty_session};
use oxdock_remote_proto::tar::{StreamedEntry, sanitize_rel, unpack_auto_from};
use sha2::{Digest, Sha256};

use super::SNAPSHOT_PENDING_DISPLAY;
use super::args::parse_bool_str;
use super::fs_ops::{canonical_cwd, copy_entry, hash_path};
use super::io::{StreamHandle, TaskTee, TeeStream, write_stdout};
use super::native::FuncBody;
use super::state::{ExecState, MAX_CALL_DEPTH, TaskPhase};
use super::steps::{Flow, StepCtx, TtyRequest};
use oxdock_pipe::{KeeperGuard, PipeHandle, PipeInner, SharedOutput};

/// Map a Flow reaching a context-free boundary (pipeline top, thread join)
/// into status. Only Done passes; anything else is a step-numbered error
/// naming the originating step in its own body.
fn top_level_flow(flow: Flow) -> Result<()> {
    match flow {
        Flow::Done => Ok(()),
        Flow::Break { idx } => {
            bail!("step {}: BREAK outside loop", idx + 1);
        }
        Flow::Continue { idx } => {
            bail!("step {}: CONTINUE outside loop", idx + 1);
        }
        Flow::Return { idx, .. } => {
            bail!(
                "step {}: RETURN outside function, ASYNC task, or LET block",
                idx + 1
            );
        }
    }
}

pub(super) fn inherit_env<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    keys: &[String],
) -> Result<()> {
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
    for key in removals {
        envs.remove(&key);
    }
    for (key, value) in inserts {
        envs.insert(key, value);
    }
    Ok(())
}

pub(super) fn workdir<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    path: &str,
) -> Result<()> {
    cx.state.cwd = cx
        .state
        .fs
        .resolve_workdir(&cx.state.cwd, path)
        .with_context(|| format!("step {}: WORKDIR {}", idx + 1, path))?;
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
    Ok(())
}

pub(super) fn run<P: ProcessManager>(cx: &mut StepCtx<'_, P>, idx: usize, cmd: &str) -> Result<()> {
    // Terminal steps spawn through the shared pty pump instead of the
    // process manager; everything else below stays hermetic pipes.
    if let Some(tty) = cx.tty {
        return run_pty(cx, idx, &tty_shell_argv(cmd), tty, cmd, true);
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
            bail!("step {}: RUN {} interrupted by cancellation", idx + 1, cmd);
        }
    }

    // Spawn the command.
    let mut handle = match cx
        .process
        .spawn_command(&ctx, cmd, options)
        .with_context(|| format!("step {}: RUN {}", idx + 1, cmd))?
    {
        CommandResult::Background(h) => h,
        CommandResult::Completed => return Ok(()),
        CommandResult::Captured(_) => {
            bail!("step {}: RUN {} unexpectedly captured output", idx + 1, cmd)
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
            bail!("step {}: RUN {} interrupted by cancellation", idx + 1, cmd);
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
        bail!(
            "step {}: RUN {} exited with status {}",
            idx + 1,
            cmd,
            status
        );
    }
    Ok(())
}

/// Direct-spawn counterpart of [`run`]: executes an already-resolved `argv`
/// without any shell (`RUN ["exe", "arg", ...]`). `CommandContext` and
/// `CommandOptions` (cwd/env, `WITH_IO` pipes, `ASYNC`/cancellable
/// backgrounding, [`INHERIT_STDOUT_ENV_VAR`]) are built exactly like [`run`];
/// only the spawn call differs (`spawn_argv`, no `shell_cmd`).
pub(super) fn run_argv<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    argv: &[String],
) -> Result<()> {
    // Terminal steps spawn through the shared pty pump instead of the
    // process manager; everything else below stays hermetic pipes.
    if let Some(tty) = cx.tty {
        let what = format!("{argv:?}");
        return run_pty(cx, idx, argv, tty, &what, false);
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
            bail!("step {}: RUN {argv:?} interrupted by cancellation", idx + 1);
        }
    }

    // Spawn the executable directly (no shell).
    let mut handle = match cx
        .process
        .spawn_argv(&ctx, argv, options)
        .with_context(|| format!("step {}: RUN {argv:?}", idx + 1))?
    {
        CommandResult::Background(h) => h,
        CommandResult::Completed => return Ok(()),
        CommandResult::Captured(_) => {
            bail!(
                "step {}: RUN {argv:?} unexpectedly captured output",
                idx + 1
            )
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
            bail!("step {}: RUN {argv:?} interrupted by cancellation", idx + 1);
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
        bail!(
            "step {}: RUN {argv:?} exited with status {}",
            idx + 1,
            status
        );
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
/// stdout sink, stdin flows only from a `stdin = $p` script pipe (bare
/// `[tty]` runs closed), and cancellation kills the child through the
/// pump's own path before reporting like any interrupted `RUN`.
/// Environment, anchors, and working directory mirror [`run_argv`]
/// exactly: closed child env from the script map, the same anchor
/// pre-flight the native spawn runs (`shell` selects the shell-form
/// `PATH` requirement), the same snapshot-choked cwd. `what` renders
/// the step in errors exactly like the calling form.
#[allow(clippy::too_many_arguments)]
fn run_pty<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    argv: &[String],
    tty: TtyRequest,
    what: &str,
    shell: bool,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    if argv.is_empty() {
        bail!("step {}: RUN {what} needs at least a program", idx + 1);
    }
    // Same pre-spawn cancellation refusal as [`run`].
    if cx.state.cancel_token.load(Ordering::SeqCst) {
        bail!("step {}: RUN {what} interrupted by cancellation", idx + 1);
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
    // Stdin feeds the master only from a `stdin = $p` script pipe: the
    // backend behind it carries the wake hook that releases a worker
    // parked in a blocking read when the child dies. Anything else
    // (bare `[tty]`, ambient inheritance, unwakeable descriptors) runs
    // closed instead of stranding the session on a worker that can
    // never observe exit.
    let input = match &cx.stdin {
        CommandStdin::Null | CommandStdin::Inherit => None,
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
            bail!(
                "step {}: WITH_IO [tty] stdin cannot arrive on a live OS pipe; bind a script pipe with stdin=$var",
                idx + 1
            )
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
            bail!(
                "step {}: WITH_IO [tty] stdout cannot arrive on a live OS pipe; bind a script pipe with stdout=$var",
                idx + 1
            )
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
        func: "RUN",
    })
    .with_context(|| format!("step {}: RUN {what}", idx + 1))?;
    // Pipe EOF rides the writer drops like any `RUN`: the supervisor
    // returns only after the output worker finished, so every byte is
    // already in the sink before the code below runs.
    match outcome {
        PtyOutcome::Completed(code) => {
            if code != 0 {
                bail!("step {}: RUN {what} exited with status {code}", idx + 1);
            }
            Ok(())
        }
        PtyOutcome::Cancelled => {
            bail!("step {}: RUN {what} interrupted by cancellation", idx + 1);
        }
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
        bail!("RUN exec form requires at least one argument");
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
        anyhow::bail!(
            "cannot pass PIPE as a command argument: pipes carry streams between steps, not text. Capture the producing step's output with LET $var: STRING = ... instead."
        );
    }
    if val.as_semaphore().is_some() {
        anyhow::bail!(
            "cannot pass SEMAPHORE as a command argument: semaphores coordinate task access between steps, not text."
        );
    }
    if val.as_permit().is_some() {
        anyhow::bail!(
            "cannot pass PERMIT as a command argument: permits grant scoped task access, not text."
        );
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
        bail!("RUN exec form element must be a string, got map");
    }
    if let Some(id) = val.as_handle() {
        bail!("RUN exec form element must be a string, got task handle task#{id}");
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
        writer.write_all(line.as_bytes())?;
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
    sleep(cx, 0, &duration)
}

/// Dispatch `SLEEP <duration>` : park the step without spawning a shell.
///
/// Cooperative: sleeps in bounded chunks and checks the cancellation token
/// between chunks, so an enclosing `TIMEOUT` deadline or parent task teardown
/// interrupts the sleep promptly. Never blocks on the full duration at once.
pub(crate) fn sleep<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    duration: &std::time::Duration,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    let start = Instant::now();
    loop {
        if cx.state.cancel_token.load(Ordering::SeqCst) {
            bail!("step {}: SLEEP interrupted by cancellation", idx + 1);
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
    idx: usize,
    from_workspace: Option<WorkspaceTarget>,
    from: &str,
    to: &str,
) -> Result<()> {
    let from_abs = if let Some(target) = from_workspace {
        cx.state
            .fs
            .resolve_copy_source_from_target(copy_source_root(target), from)
            .with_context(|| format!("step {}: COPY {} {}", idx + 1, from, to))?
    } else {
        cx.state
            .fs
            .resolve_copy_source(from)
            .with_context(|| format!("step {}: COPY {} {}", idx + 1, from, to))?
    };
    let to_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, to)
        .with_context(|| format!("step {}: COPY {} {}", idx + 1, from, to))?;
    let to_abs = place_file_in_dir(cx.state.fs.as_ref(), &from_abs, to, &to_abs)
        .with_context(|| format!("step {}: COPY {} {}", idx + 1, from, to))?;
    copy_entry(cx.state.fs.as_ref(), &from_abs, &to_abs)
        .with_context(|| format!("step {}: COPY {} {}", idx + 1, from, to))?;
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
    let base = from_abs
        .as_path()
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .with_context(|| format!("COPY source has no file name: {}", from_abs.display()))?;
    to_abs
        .join(&base)
        .with_context(|| format!("COPY destination escapes root: {}", to_abs.display()))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn copy_git<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    rev: &str,
    from: &str,
    to: &str,
    include_dirty: bool,
) -> Result<()> {
    let to_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, to)
        .with_context(|| format!("step {}: COPY_GIT {} {} {}", idx + 1, rev, from, to))?;
    cx.state
        .fs
        .copy_from_git(rev, from, &to_abs, include_dirty)
        .with_context(|| format!("step {}: COPY_GIT {} {} {}", idx + 1, rev, from, to))?;
    Ok(())
}

/// Source mode for one archive header: Unix records the real mode so
/// unpackers (system `tar` or `UNARCHIVE`) restore it; other platforms
/// land the conventional default.
fn archive_entry_mode(meta: &std::fs::Metadata, default: u32) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = default;
        meta.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        default
    }
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
    tag: &str,
) -> Result<GuardedPath> {
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
            .with_context(|| tag.to_string());
    }
    let parent_abs = if parent.is_empty() {
        cx.state.cwd.clone()
    } else {
        cx.state
            .fs
            .resolve_read(&cx.state.cwd, &parent)
            .with_context(|| tag.to_string())?
    };
    let mut matched = false;
    for entry in cx
        .state
        .fs
        .read_dir_entries(&parent_abs)
        .with_context(|| tag.to_string())?
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
                bail!("{tag}: refuses to pack symlink source {src:?}");
            }
        }
    }
    if !matched {
        // Genuinely missing: let the resolver report it authoritatively.
        return cx
            .state
            .fs
            .resolve_read(&cx.state.cwd, src)
            .with_context(|| tag.to_string());
    }
    parent_abs.join(&name).with_context(|| tag.to_string())
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
    idx: usize,
) -> Result<ArchiveTree> {
    let tag = format!("step {}: ARCHIVE {src} {dst}", idx + 1);
    match fs.entry_kind(src_abs).with_context(|| tag.clone())? {
        EntryKind::File => {
            let base = src_abs
                .as_path()
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .with_context(|| {
                    format!("{tag}: source has no file name: {}", src_abs.display())
                })?;
            let rel = sanitize_rel(&to_forward_slashes(&base)).with_context(|| tag.clone())?;
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
                    .with_context(|| tag.clone())?;
                let mut entries = fs.read_dir_entries(&dir_abs).with_context(|| tag.clone())?;
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
                        bail!("{tag}: refuses to pack symlink {child_rel:?}");
                    }
                    let child = dir_abs.join(&name).with_context(|| tag.clone())?;
                    match fs.entry_kind(&child).with_context(|| tag.clone())? {
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
    tag: &str,
) -> Result<()> {
    for (rel, guard) in dirs {
        let rel = sanitize_rel(rel).with_context(|| tag.to_string())?;
        let mode = fs
            .metadata(guard)
            .map(|meta| archive_entry_mode(&meta, 0o755))
            .with_context(|| tag.to_string())?;
        let mut header = tar::Header::new_gnu();
        header.set_mode(mode);
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_data(&mut header, &rel, std::io::empty())
            .with_context(|| tag.to_string())?;
    }
    for (rel, guard) in files {
        let rel = sanitize_rel(rel).with_context(|| tag.to_string())?;
        let meta = fs.metadata(guard).with_context(|| tag.to_string())?;
        // Link-refusing open: a symlink planted after the listing
        // probe fails here instead of packing its target's bytes, and
        // the descriptor must stay under the packed tree.
        let mut reader = fs
            .open_read_nofollow(guard, src_abs)
            .with_context(|| tag.to_string())?;
        let mut header = tar::Header::new_gnu();
        header.set_mode(archive_entry_mode(&meta, 0o644));
        header.set_size(meta.len());
        header.set_cksum();
        builder
            .append_data(&mut header, &rel, &mut reader)
            .with_context(|| tag.to_string())?;
    }
    Ok(())
}

pub(super) fn archive<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    src: &str,
    dst: &str,
    raw: bool,
) -> Result<()> {
    let tag = format!("step {}: ARCHIVE {src} {dst}", idx + 1);
    let src_abs = resolve_archive_src(cx, src, &tag)?;
    let dst_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, dst)
        .with_context(|| tag.clone())?;
    // Opening the destination truncates it: an archive landing on its
    // own source would destroy inputs before their bytes stream, so
    // equal paths fail closed. A pre-existing destination inside the
    // packed tree would likewise be truncated before its own append
    // reads it back; a fresh one is safe (collection already ran, so it
    // cannot be a member) which keeps whole-tree archives like
    // `ARCHIVE project project.tar.gz` working.
    if dst_abs == src_abs {
        bail!("{tag}: destination {dst:?} cannot be the source itself");
    }
    if dst_abs.as_path().starts_with(src_abs.as_path()) && dst_abs.exists() {
        bail!("{tag}: destination {dst:?} cannot overwrite inside the packed tree");
    }
    let (files, dirs) = collect_archive_tree(cx.state.fs.as_ref(), &src_abs, src, dst, idx)?;
    cx.state
        .fs
        .ensure_parent_dir(&dst_abs)
        .with_context(|| tag.clone())?;
    let out = cx
        .state
        .fs
        .open_write(&dst_abs)
        .with_context(|| tag.clone())?;
    if raw {
        let mut builder = tar::Builder::new(out);
        append_archive_tree(
            cx.state.fs.as_ref(),
            &mut builder,
            &files,
            &dirs,
            &src_abs,
            &tag,
        )?;
        let mut out = builder.into_inner().with_context(|| tag.clone())?;
        std::io::Write::flush(&mut out).with_context(|| tag.clone())?;
    } else {
        let encoder = GzEncoder::new(out, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        append_archive_tree(
            cx.state.fs.as_ref(),
            &mut builder,
            &files,
            &dirs,
            &src_abs,
            &tag,
        )?;
        builder
            .into_inner()
            .with_context(|| tag.clone())?
            .try_finish()
            .with_context(|| tag.clone())?;
    }
    Ok(())
}

pub(super) fn unarchive<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    src: &str,
    dst: &str,
    max_bytes: Option<&str>,
) -> Result<()> {
    let tag = format!("step {}: UNARCHIVE {src} {dst}", idx + 1);
    let cap = match max_bytes {
        Some(raw) => oxdock_parser::command::parse_byte_count(raw)
            .with_context(|| format!("{tag}: --max-bytes {raw:?}"))?,
        None => DEFAULT_UNARCHIVE_MAX_BYTES,
    };
    let src_abs = cx
        .state
        .fs
        .resolve_read(&cx.state.cwd, src)
        .with_context(|| tag.clone())?;
    let dst_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, dst)
        .with_context(|| tag.clone())?;
    cx.state
        .fs
        .create_dir_all(&dst_abs)
        .with_context(|| tag.clone())?;
    let reader = cx
        .state
        .fs
        .open_read(&src_abs)
        .with_context(|| tag.clone())?;
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
        let entry_tag = format!("{tag}: member");
        match entry {
            StreamedEntry::Dir { path, mode } => {
                // Re-validate the decoder's rel at the write site: a
                // sanitized rel is pure descent, so the join below
                // cannot escape lexically in any path normalization
                // domain (verbatim, short names, slashes). Escapes via
                // planted links stay the filesystem layers' job, and
                // those run next, on every member, in every mode.
                let rel = sanitize_rel(&path).with_context(|| entry_tag.clone())?;
                PathResolver::refuse_link_ancestors(&dst_abs, &rel)
                    .with_context(|| entry_tag.clone())?;
                let dir = dst_abs.join(&rel).with_context(|| entry_tag.clone())?;
                fs.create_dir_all(&dir).with_context(|| entry_tag.clone())?;
                deferred_dir_modes.push((dir, mode));
            }
            StreamedEntry::File { path, mode, reader } => {
                let rel = sanitize_rel(&path).with_context(|| entry_tag.clone())?;
                PathResolver::refuse_link_ancestors(&dst_abs, &rel)
                    .with_context(|| entry_tag.clone())?;
                let file = dst_abs.join(&rel).with_context(|| entry_tag.clone())?;
                fs.ensure_parent_dir(&file)
                    .with_context(|| entry_tag.clone())?;
                // Link-refusing open: a symlink planted at the member
                // path fails here instead of truncating its target,
                // and the descriptor must stay under the destination.
                let mut out = fs
                    .open_write_nofollow(&file, &dst_abs)
                    .with_context(|| entry_tag.clone())?;
                // Metered copy: bombs read small and write huge, so the
                // cap counts member bytes out, not archive bytes in. The
                // tripped member stays truncated while the step fails.
                let mut capped = CappedReader {
                    inner: reader,
                    remaining,
                };
                std::io::copy(&mut capped, &mut out).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::QuotaExceeded {
                        anyhow!(
                            "{entry_tag}: archive exceeds the {cap}-byte expansion cap at {path:?}"
                        )
                    } else {
                        anyhow::Error::new(e).context(entry_tag.clone())
                    }
                })?;
                remaining = capped.remaining;
                std::io::Write::flush(&mut out).with_context(|| entry_tag.clone())?;
                restore_archive_mode(fs, &file, mode).with_context(|| entry_tag.clone())?;
            }
            StreamedEntry::Symlink { path, target } => {
                bail!("{entry_tag}: refuses symlink member {path:?} -> {target:?}");
            }
        }
        Ok(())
    })
    .with_context(|| tag.clone())?;
    for (dir, mode) in deferred_dir_modes.into_iter().rev() {
        restore_archive_mode(fs, &dir, mode).with_context(|| format!("{tag}: restoring mode"))?;
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

pub(super) fn hash_sha256<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    path: &str,
) -> Result<()> {
    let target = cx
        .state
        .fs
        .resolve_read(&cx.state.cwd, path)
        .with_context(|| format!("step {}: HASH_SHA256 {}", idx + 1, path))?;
    let mut hasher = Sha256::new();
    hash_path(cx.state.fs.as_ref(), &target, "", &mut hasher)?;
    let digest = hasher.finalize();
    let bytes: &[u8] = digest.as_ref();
    write_stdout(cx.out.clone(), |writer| {
        for b in bytes {
            write!(writer, "{b:02x}")?;
        }
        writeln!(writer)?;
        Ok(())
    })?;
    Ok(())
}

pub(super) fn symlink<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    from_workspace: Option<WorkspaceTarget>,
    from: &str,
    to: &str,
) -> Result<()> {
    let to_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, to)
        .with_context(|| format!("step {}: SYMLINK {} {}", idx + 1, from, to))?;
    let from_abs = if let Some(target) = from_workspace {
        cx.state
            .fs
            .resolve_copy_source_from_target(copy_source_root(target), from)
            .with_context(|| format!("step {}: SYMLINK {} {}", idx + 1, from, to))?
    } else {
        cx.state
            .fs
            .resolve_copy_source(from)
            .with_context(|| format!("step {}: SYMLINK {} {}", idx + 1, from, to))?
    };
    // `ln -s` destination semantics: a directory destination (existing,
    // or named with a trailing slash) receives the link under the
    // source basename instead of failing as "already exists".
    let to_abs = if to.ends_with('/') || to.ends_with('\\') {
        let base = link_base_name(&from_abs, from)?;
        to_abs
            .join(&base)
            .with_context(|| format!("step {}: SYMLINK {} {}", idx + 1, from, to))?
    } else if matches!(cx.state.fs.entry_kind(&to_abs), Ok(EntryKind::Dir)) {
        let base = link_base_name(&from_abs, from)?;
        to_abs
            .join(&base)
            .with_context(|| format!("step {}: SYMLINK {} {}", idx + 1, from, to))?
    } else {
        to_abs
    };
    cx.state
        .fs
        .symlink(&from_abs, &to_abs)
        .with_context(|| format!("step {}: SYMLINK {} {}", idx + 1, from, to))?;
    Ok(())
}

/// Basename of a link source for directory-destination placement. Lexical:
/// dangling sources have no entry kind to inspect.
fn link_base_name(from_abs: &GuardedPath, from: &str) -> Result<String> {
    from_abs
        .as_path()
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .with_context(|| format!("SYMLINK source has no file name: {from}"))
}

pub(super) fn mkdir<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    path: &str,
) -> Result<()> {
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, path)
        .with_context(|| format!("step {}: MKDIR {}", idx + 1, path))?;
    cx.state
        .fs
        .create_dir_all(&target)
        .with_context(|| format!("failed to create dir {}", target.display()))?;
    Ok(())
}

pub(super) fn ls<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    arg: &Option<String>,
) -> Result<()> {
    let target_dir = if let Some(p) = arg {
        cx.state
            .fs
            .resolve_read(&cx.state.cwd, p)
            .with_context(|| format!("step {}: LS {}", idx + 1, p))?
    } else {
        // Bare `LS` lists the current directory: ride the choke point so a
        // pending snapshot materializes (a snapshot read), matching the
        // explicit-path branch above. Local roots resolve with zero I/O.
        cx.state
            .fs
            .resolve_read(&cx.state.cwd, ".")
            .with_context(|| format!("step {}: LS", idx + 1))?
    };
    let mut entries = cx
        .state
        .fs
        .read_dir_entries(&target_dir)
        .with_context(|| format!("step {}: LS {}", idx + 1, target_dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    write_stdout(cx.out.clone(), |writer| {
        writeln!(writer, "{}:", target_dir.display())?;
        for entry in &entries {
            writeln!(writer, "{}", entry.file_name().to_string_lossy())?;
        }
        Ok(())
    })?;
    Ok(())
}

pub(super) fn cwd<P: ProcessManager>(cx: &mut StepCtx<'_, P>, idx: usize) -> Result<()> {
    // A pending snapshot has no concrete directory yet: report the stable
    // sentinel instead of a local path or a fabricated location (issue #131).
    if cx.state.fs.is_snapshot_pending() {
        return write_stdout(cx.out.clone(), |writer| {
            writeln!(writer, "{SNAPSHOT_PENDING_DISPLAY}")?;
            Ok(())
        });
    }
    let concrete = cx.state.fs.concretize_cwd(&cx.state.cwd);
    let real = canonical_cwd(cx.state.fs.as_ref(), &concrete).with_context(|| {
        format!(
            "step {}: CWD failed to canonicalize {}",
            idx + 1,
            concrete.display()
        )
    })?;
    write_stdout(cx.out.clone(), |writer| {
        writeln!(writer, "{}", real)?;
        Ok(())
    })?;
    Ok(())
}

pub(super) fn read<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    path_opt: &Option<String>,
) -> Result<()> {
    if let Some(path) = path_opt {
        let target = cx
            .state
            .fs
            .resolve_read(&cx.state.cwd, path)
            .with_context(|| format!("step {}: READ {}", idx + 1, path))?;
        let mut reader = cx
            .state
            .fs
            .open_read(&target)
            .with_context(|| format!("failed to open {}", target.display()))?;
        write_stdout(cx.out.clone(), |writer| {
            let mut buf = [0u8; super::io::CHUNK_SIZE];
            loop {
                let n = reader.read(&mut buf).context("failed to read from file")?;
                if n == 0 {
                    break;
                }
                writer
                    .write_all(&buf[..n])
                    .context("failed to write to output")?;
            }
            Ok(())
        })?;
    } else {
        let CommandStdin::Stream(input_stream) = cx.stdin.clone() else {
            bail!(
                "step {}: READ requires stdin (use WITH_IO [stdin=...] READ)",
                idx + 1
            );
        };
        let mut buf = [0u8; super::io::CHUNK_SIZE];
        loop {
            let n = {
                let mut guard = input_stream
                    .lock()
                    .map_err(|_| anyhow!("failed to lock stdin for READ"))?;
                guard.read(&mut buf).context("failed to read from stdin")?
            };
            if n == 0 {
                break;
            }
            write_stdout(cx.out.clone(), |writer| {
                writer
                    .write_all(&buf[..n])
                    .context("failed to write to output")
            })?;
        }
    }
    Ok(())
}

pub(super) fn read_line<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    var: &str,
) -> Result<()> {
    let CommandStdin::Stream(input_stream) = cx.stdin.clone() else {
        bail!(
            "step {}: READ_LINE requires stdin (use WITH_IO [stdin=...] READ_LINE $var)",
            idx + 1
        );
    };
    let clean_var = var.trim_start_matches('$').to_string();
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = {
            let mut guard = input_stream
                .lock()
                .map_err(|_| anyhow!("failed to lock stdin for READ_LINE"))?;
            guard.read(&mut byte).context("failed to read from stdin")?
        };
        if n == 0 {
            break;
        }
        buf.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    let line = String::from_utf8(buf).context("READ_LINE received non-UTF8 bytes")?;
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
) -> Result<BTreeMap<String, Value>> {
    let clean_var = var.trim_start_matches('$').to_string();
    let Some((decl_type, value)) = cx.state.get_var_typed(&clean_var) else {
        bail!("variable '${clean_var}' is not defined");
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
        (TypeTag::Handle, _) => {
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
    idx: usize,
    path: &str,
    contents: Option<&str>,
) -> Result<()> {
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, path)
        .with_context(|| format!("step {}: WRITE {}", idx + 1, path))?;
    cx.state
        .fs
        .ensure_parent_dir(&target)
        .with_context(|| format!("failed to create parent for {}", target.display()))?;
    if let Some(body) = contents {
        cx.state
            .fs
            .write_file(&target, body.as_bytes())
            .with_context(|| format!("failed to write {}", target.display()))?;
    } else {
        let CommandStdin::Stream(input_stream) = cx.stdin.clone() else {
            bail!(
                "step {}: WRITE {} requires stdin (use WITH_IO [stdin=...] WRITE)",
                idx + 1,
                path
            );
        };
        let mut guard = input_stream
            .lock()
            .map_err(|_| anyhow!("failed to lock stdin for WRITE"))?;
        let mut writer = cx
            .state
            .fs
            .open_write(&target)
            .with_context(|| format!("failed to open {} for writing", target.display()))?;
        let mut buf = [0u8; super::io::CHUNK_SIZE];
        loop {
            let n = guard
                .read(&mut buf)
                .context("failed to read from stdin for WRITE")?;
            if n == 0 {
                break;
            }
            writer
                .write_all(&buf[..n])
                .with_context(|| format!("failed to write to {}", target.display()))?;
            writer.flush()?;
        }
    }
    Ok(())
}

pub(super) fn append<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    path: &str,
    contents: Option<&str>,
) -> Result<()> {
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, path)
        .with_context(|| format!("step {}: APPEND {}", idx + 1, path))?;
    cx.state
        .fs
        .ensure_parent_dir(&target)
        .with_context(|| format!("failed to create parent for {}", target.display()))?;
    if let Some(body) = contents {
        cx.state
            .fs
            .append_file(&target, body.as_bytes())
            .with_context(|| format!("failed to append to {}", target.display()))?;
    } else {
        let CommandStdin::Stream(input_stream) = cx.stdin.clone() else {
            bail!(
                "step {}: APPEND {} requires stdin (use WITH_IO [stdin=...] APPEND)",
                idx + 1,
                path
            );
        };
        let mut guard = input_stream
            .lock()
            .map_err(|_| anyhow!("failed to lock stdin for APPEND"))?;
        let mut writer = cx
            .state
            .fs
            .open_append(&target)
            .with_context(|| format!("failed to open {} for appending", target.display()))?;
        let mut buf = [0u8; super::io::CHUNK_SIZE];
        loop {
            let n = guard
                .read(&mut buf)
                .context("failed to read from stdin for APPEND")?;
            if n == 0 {
                break;
            }
            writer
                .write_all(&buf[..n])
                .with_context(|| format!("failed to append to {}", target.display()))?;
            writer.flush()?;
        }
    }
    Ok(())
}

pub(super) fn replace<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    path_opt: &Option<String>,
    overrides: &[(String, String)],
) -> Result<()> {
    let ctx = cx.state.command_ctx()?;
    let vars = cx.state.all_vars();
    // Dispatch `{{ MODULE::FUNC(args) }}` through the shared pure table:
    // one `Arc` clone per EXPAND, nested lookup per call with zero
    // allocation, known-names listing built only on failure. Stateful,
    // script, and pipe backed entries never appear here: placeholders
    // evaluate without a step context and must stay side effect free.
    let table = cx.state.pure_function_table();
    let calls: oxdock_process::PlaceholderCall = Arc::new(move |module, func, args| {
        let callee = table
            .get(module)
            .and_then(|entries| entries.get(func))
            .ok_or_else(|| {
                let mut known: Vec<String> = table
                    .iter()
                    .flat_map(|(m, entries)| entries.keys().map(move |f| format!("{m}::{f}")))
                    .collect();
                known.sort();
                anyhow!(
                    "unknown placeholder function '{module}::{func}'; known functions: {}",
                    known.join(", ")
                )
            })?;
        callee(args)
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
                .with_context(|| format!("step {}: EXPAND {}", idx + 1, path))?;
            let mut reader = cx
                .state
                .fs
                .open_read(&target)
                .with_context(|| format!("failed to open {}", target.display()))?;
            let mut buf = [0u8; super::io::CHUNK_SIZE];
            loop {
                let n = reader
                    .read(&mut buf)
                    .with_context(|| format!("failed to read {}", target.display()))?;
                if n == 0 {
                    break;
                }
                expander.process_bytes(&buf[..n], &mut out_buf)?;
                w.write_all(&out_buf).context("failed to write output")?;
                out_buf.clear();
            }
        } else {
            let CommandStdin::Stream(input_stream) = cx.stdin.clone() else {
                bail!(
                    "step {}: EXPAND requires stdin when no file path is given \
                     (use WITH_IO [stdin=...] EXPAND)",
                    idx + 1
                );
            };
            let mut guard = input_stream
                .lock()
                .map_err(|_| anyhow!("failed to lock stdin for EXPAND"))?;
            let mut buf = [0u8; super::io::CHUNK_SIZE];
            loop {
                let n = guard.read(&mut buf).context("failed to read from stdin")?;
                if n == 0 {
                    break;
                }
                expander.process_bytes(&buf[..n], &mut out_buf)?;
                w.write_all(&out_buf).context("failed to write output")?;
                out_buf.clear();
            }
        }

        expander.flush(&mut out_buf)?;
        if !out_buf.is_empty() {
            w.write_all(&out_buf).context("failed to write output")?;
            out_buf.clear();
        }

        Ok(())
    })?;

    Ok(())
}

/// Strict equality assertion over evaluated values, stream buffers, and
/// pipe buffers. Typed `Value` comparison with no coercion; files never
/// appear here (read them into variables first). `--hash` compares the
/// SHA-256 of a string, pipe, or captured-stdout actual instead of the
/// raw bytes (`stderr` is unsupported).
pub(super) fn assert_eq<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    generation: usize,
    idx_step: usize,
    hash: &Option<String>,
    actual: &super::steps::ResolvedAssertTarget,
    expected: Option<&Value>,
) -> Result<()> {
    use super::steps::ResolvedAssertTarget;
    // Exact stdout accumulators are generation-scoped (trial-cumulative),
    // unlike per-step substring windows, so the step index is unused here.
    let _ = idx_step;
    // Mode 1: exact stdout with piped stdin consumes the stream, mirroring
    // the legacy substring behavior, but compares exact bytes.
    if let ResolvedAssertTarget::Stdout = actual
        && let CommandStdin::Stream(input_stream) = cx.stdin.clone()
    {
        let drained = drain_stdin_bounded(idx, input_stream, "ASSERT_EQ")?;
        super::io::write_stdout(cx.out.clone(), |w| {
            w.write_all(&drained)?;
            Ok(())
        })?;
        let Some(want) = expected.and_then(|v| v.as_str()) else {
            bail!(
                "step {}: ASSERT_EQ stream mismatch\nexpected: {:?}\nactual:   {:?}",
                idx + 1,
                expected,
                String::from_utf8_lossy(&drained)
            );
        };
        if drained != want.as_bytes() {
            bail!(
                "step {}: ASSERT_EQ stream mismatch\nexpected: {:?}\nactual:   {:?}",
                idx + 1,
                want,
                String::from_utf8_lossy(&drained)
            );
        }
        return Ok(());
    }
    if let Some(sha) = hash {
        let actual_bytes: Vec<u8> = match actual {
            ResolvedAssertTarget::Value(v) => match v.as_str() {
                Some(s) => s.as_bytes().to_vec(),
                None => {
                    bail!(
                        "step {}: ASSERT_EQ --hash needs a string actual, found {:?}",
                        idx + 1,
                        v
                    );
                }
            },
            ResolvedAssertTarget::Pipe(bytes) => bytes.clone(),
            ResolvedAssertTarget::Stdout => exact_stdout_bytes(cx, idx, generation)?,
            ResolvedAssertTarget::Stderr => {
                bail!(
                    "step {}: ASSERT_EQ over stderr is not supported; use ASSERT_CONTAINS stderr ...",
                    idx + 1
                );
            }
        };
        let mut hasher = Sha256::new();
        hasher.update(&actual_bytes);
        let digest = hasher.finalize();
        let bytes: &[u8] = digest.as_ref();
        let computed: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        if !computed.eq_ignore_ascii_case(sha) {
            bail!(
                "step {}: ASSERT_EQ --hash mismatch: expected {}, computed {}",
                idx + 1,
                sha,
                computed
            );
        }
        return Ok(());
    }
    match actual {
        ResolvedAssertTarget::Value(got) => {
            let Some(want) = expected else {
                bail!("step {}: ASSERT_EQ requires an expected value", idx + 1);
            };
            if got == want {
                Ok(())
            } else {
                bail!(
                    "step {}: ASSERT_EQ mismatch\nexpected: {:?}\nactual:   {:?}",
                    idx + 1,
                    want,
                    got
                );
            }
        }
        ResolvedAssertTarget::Stdout => {
            let bytes = exact_stdout_bytes(cx, idx, generation)?;
            let Some(want) = expected.and_then(|v| v.as_str()) else {
                bail!(
                    "step {}: ASSERT_EQ stream mismatch\nexpected: {:?}\nactual:   {:?}",
                    idx + 1,
                    expected,
                    String::from_utf8_lossy(&bytes)
                );
            };
            if bytes != want.as_bytes() {
                bail!(
                    "step {}: ASSERT_EQ stream mismatch\nexpected: {:?}\nactual:   {:?}",
                    idx + 1,
                    want,
                    String::from_utf8_lossy(&bytes)
                );
            }
            Ok(())
        }
        ResolvedAssertTarget::Stderr => {
            bail!(
                "step {}: ASSERT_EQ over stderr is not supported; use ASSERT_CONTAINS stderr ...",
                idx + 1
            );
        }
        ResolvedAssertTarget::Pipe(bytes) => {
            let actual_str = String::from_utf8(bytes.clone()).with_context(|| {
                format!("step {}: ASSERT_EQ pipe content is not UTF-8", idx + 1)
            })?;
            let Some(want) = expected.and_then(|v| v.as_str()) else {
                bail!(
                    "step {}: ASSERT_EQ pipe mismatch\nexpected: {:?}\nactual:   {:?}",
                    idx + 1,
                    expected,
                    actual_str
                );
            };
            if actual_str != want {
                bail!(
                    "step {}: ASSERT_EQ pipe mismatch\nexpected: {:?}\nactual:   {:?}",
                    idx + 1,
                    want,
                    actual_str
                );
            }
            Ok(())
        }
    }
}

/// Drain a piped stdin fully while enforcing the exact-match byte cap.
/// Shared by `ASSERT_EQ stdout` Mode 1.
fn drain_stdin_bounded(idx: usize, input_stream: SharedInput, verb: &str) -> Result<Vec<u8>> {
    let mut guard = input_stream
        .lock()
        .map_err(|_| anyhow!("failed to lock stdin for {verb}"))?;
    let mut out = Vec::new();
    let mut buf = [0u8; super::io::CHUNK_SIZE];
    loop {
        let n = guard
            .read(&mut buf)
            .with_context(|| format!("failed to read from stdin for {verb}"))?;
        if n == 0 {
            break;
        }
        if out.len() + n > super::io::EXACT_STDOUT_CAP {
            bail!(
                "step {}: {verb} stdin exceeded the 8 MiB exact buffer; assert via pipe targets or harness expect.stdout",
                idx + 1
            );
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

/// Read the trial-cumulative exact stdout bytes for this generation,
/// or bail with remediation guidance on overflow / missing capture.
fn exact_stdout_bytes<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    generation: usize,
) -> Result<Vec<u8>> {
    let captures = cx
        .state
        .exact_stdout
        .lock()
        .map_err(|_| anyhow!("exact_stdout poisoned"))?;
    match captures.get(&generation) {
        Some(entry) if entry.overflowed => bail!(
            "step {}: ASSERT_EQ stdout overflowed the 8 MiB exact buffer; assert via pipe targets or harness expect.stdout",
            idx + 1
        ),
        Some(entry) => Ok(entry.bytes.clone()),
        None => bail!(
            "step {}: ASSERT_EQ stdout has no exact capture registered",
            idx + 1
        ),
    }
}

/// Containment assertion over values, streams, and pipes: substring for
/// strings, exact-element match for lists, key presence for maps,
/// substring over stream and pipe buffers. Like `assert_eq`, files never
/// appear here; read them into variables first.
pub(super) fn assert_contains<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    generation: usize,
    idx_step: usize,
    haystack: &super::steps::ResolvedAssertTarget,
    needle: &Arg,
) -> Result<()> {
    use super::steps::ResolvedAssertTarget;
    let needle_str = super::args::resolve_arg(needle, cx)?;
    match haystack {
        ResolvedAssertTarget::Value(v) => {
            if let Some(hay) = v.as_str() {
                if hay.contains(needle_str.as_str()) {
                    return Ok(());
                }
                bail!(
                    "step {}: ASSERT_CONTAINS did not contain '{}'; actual: {:?}",
                    idx + 1,
                    needle_str,
                    hay
                );
            }
            if let Some(items) = v.as_list() {
                if items
                    .iter()
                    .any(|e| e.as_str() == Some(needle_str.as_str()))
                {
                    return Ok(());
                }
                bail!(
                    "step {}: ASSERT_CONTAINS did not contain '{}'; actual: {:?}",
                    idx + 1,
                    needle_str,
                    v
                );
            }
            if let Some(map) = v.as_map() {
                if map.contains_key(needle_str.as_str()) {
                    return Ok(());
                }
                bail!(
                    "step {}: ASSERT_CONTAINS did not contain '{}'",
                    idx + 1,
                    needle_str
                );
            }
            bail!(
                "step {}: ASSERT_CONTAINS needs a string, list, or map haystack, found {:?}",
                idx + 1,
                v
            );
        }
        ResolvedAssertTarget::Stdout => {
            // Mode 1: piped stdin is consumed through a local window and
            // forwarded, exactly like the legacy substring check.
            if let CommandStdin::Stream(input_stream) = cx.stdin.clone() {
                let mut guard = input_stream
                    .lock()
                    .map_err(|_| anyhow!("failed to lock stdin for ASSERT_CONTAINS"))?;
                let mut window = super::io::SlidingWindow::new(needle_str.as_bytes().to_vec());
                let mut buf = [0u8; super::io::CHUNK_SIZE];
                let mut read_any = false;
                loop {
                    let n = guard
                        .read(&mut buf)
                        .context("failed to read from stdin for ASSERT_CONTAINS")?;
                    if n == 0 {
                        break;
                    }
                    read_any = true;
                    window.push_chunk(&buf[..n]);
                    super::io::write_stdout(cx.out.clone(), |w| {
                        w.write_all(&buf[..n])?;
                        Ok(())
                    })?;
                }
                if read_any {
                    if window.matched {
                        return Ok(());
                    }
                    let emitted = String::from_utf8_lossy(&window.ring_buffer()).into_owned();
                    bail!(
                        "step {}: ASSERT_CONTAINS did not contain '{}'; emitted:\n{}",
                        idx + 1,
                        needle_str,
                        emitted.trim_end()
                    );
                }
            }
            // Mode 2: pre-registered stdout window for this step.
            let windows = cx
                .state
                .assert_windows
                .lock()
                .map_err(|_| anyhow!("assert_windows poisoned"))?;
            match windows.get(&(generation, idx_step)) {
                Some(w) if w.matched => Ok(()),
                Some(w) => {
                    let emitted = String::from_utf8_lossy(&w.ring_buffer()).into_owned();
                    bail!(
                        "step {}: ASSERT_CONTAINS did not contain '{}'; emitted:\n{}",
                        idx + 1,
                        needle_str,
                        emitted.trim_end()
                    )
                }
                _ => bail!(
                    "step {}: ASSERT_CONTAINS did not contain '{}'",
                    idx + 1,
                    needle_str
                ),
            }
        }
        ResolvedAssertTarget::Stderr => {
            // Stderr has no piped-stdin consumption mode: stdin bytes
            // belong to a different stream and are left alone.
            let windows = cx
                .state
                .assert_windows_stderr
                .lock()
                .map_err(|_| anyhow!("assert_windows_stderr poisoned"))?;
            match windows.get(&(generation, idx_step)) {
                Some(w) if w.matched => Ok(()),
                Some(w) => {
                    let emitted = String::from_utf8_lossy(&w.ring_buffer()).into_owned();
                    bail!(
                        "step {}: ASSERT_CONTAINS did not contain '{}'; emitted:\n{}",
                        idx + 1,
                        needle_str,
                        emitted.trim_end()
                    )
                }
                _ => bail!(
                    "step {}: ASSERT_CONTAINS did not contain '{}'",
                    idx + 1,
                    needle_str
                ),
            }
        }
        ResolvedAssertTarget::Pipe(bytes) => {
            let hay = String::from_utf8(bytes.clone()).with_context(|| {
                format!(
                    "step {}: ASSERT_CONTAINS pipe content is not UTF-8",
                    idx + 1
                )
            })?;
            if hay.contains(needle_str.as_str()) {
                Ok(())
            } else {
                bail!(
                    "step {}: ASSERT_CONTAINS did not contain '{}'; pipe held {:?}",
                    idx + 1,
                    needle_str,
                    hay
                );
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
    bail!("WITH_IO block should have been expanded during parsing")
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
        StepKind::AsyncBlock { body } | StepKind::AssignAsync { body, .. } => {
            matches!(body.as_slice(), [step] if is_single_run(&step.kind))
        }
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
        resolve_io_streams(cx, idx, bindings, cmd)?;
    let tty = tty_request(idx, bindings, cmd)?;

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
fn tty_request(idx: usize, bindings: &[IoBinding], cmd: &StepKind) -> Result<Option<TtyRequest>> {
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
            bail!(
                "step {}: WITH_IO [rows]/[cols] need [tty] on the same step: geometry without a terminal has nothing to size",
                idx + 1
            );
        }
        return Ok(None);
    }
    if bindings
        .iter()
        .any(|b| matches!(b.stream, IoStream::Stderr))
    {
        bail!(
            "step {}: WITH_IO [tty] cannot bind stderr: the pty master carries stdout and stderr interleaved, so the two streams cannot demultiplex",
            idx + 1
        );
    }
    match cmd {
        StepKind::Run(_) | StepKind::RunExec { .. } => Ok(Some(TtyRequest { rows, cols })),
        _ => bail!(
            "step {}: WITH_IO [tty] only wraps a RUN step: the pty master is one interleaved child stream, so terminal-unaware commands cannot run under it",
            idx + 1
        ),
    }
}

/// Resolve `WITH_IO` bindings against the active context handles, shared by
/// `with_io` and the `CALL` fast paths (`LET`-capture / `ASYNC` tasks) so a
/// `CALL` under `WITH_IO` layers observes identical stream wiring whether
/// it runs inline or for its return value.
///
/// Besides the runnable streams this returns the script backends behind
/// the stdin/stdout bindings, which enrich the step context for
/// timeout-bounded bridge reads and the socket-EOF force-close.
#[allow(clippy::type_complexity)]
fn resolve_io_streams<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
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
            let handle = resolve_pipe_handle(cx, idx, target)?;
            // Main-flow bindings materialize here; task-body bindings were
            // decided by the spawn-time pin walk, making this a no-op
            // there. Either way the scope check, not the command, rules.
            let promote = allow_promote && !tty_present && cx.state.can_promote_to_os(&handle);
            cx.state.io.ensure_handle(&handle, promote)?;
        }
        match binding.stream {
            IoStream::Stdin => {
                if seen_stdin {
                    bail!("step {}: WITH_IO declared stdin more than once", idx + 1);
                }
                seen_stdin = true;
                next_expose_stdin = true;
                (step_stdin, stdin_pipe) = if let Some(target) = &binding.pipe {
                    let handle = resolve_pipe_handle(cx, idx, target)?;
                    let promote =
                        allow_promote && !tty_present && cx.state.can_promote_to_os(&handle);
                    let (stdin, backend) =
                        cx.state.io.resolve_stdin(idx, &handle, direct, promote)?;
                    (stdin, backend)
                } else {
                    (cx.stdin.clone(), None)
                };
            }
            IoStream::Stdout => {
                if seen_stdout {
                    bail!("step {}: WITH_IO declared stdout more than once", idx + 1);
                }
                seen_stdout = true;
                (step_stdout, out_pipe) = if let Some(target) = &binding.pipe {
                    let handle = resolve_pipe_handle(cx, idx, target)?;
                    let promote =
                        allow_promote && !tty_present && cx.state.can_promote_to_os(&handle);
                    let (stdout, backend) =
                        cx.state.io.resolve_stdout(idx, &handle, direct, promote)?;
                    (Some(stdout), backend)
                } else {
                    (cx.out.clone(), cx.out_pipe.clone())
                };
            }
            IoStream::Stderr => {
                if seen_stderr {
                    bail!("step {}: WITH_IO declared stderr more than once", idx + 1);
                }
                seen_stderr = true;
                step_stderr = if let Some(target) = &binding.pipe {
                    let handle = resolve_pipe_handle(cx, idx, target)?;
                    let promote =
                        allow_promote && !tty_present && cx.state.can_promote_to_os(&handle);
                    Some(cx.state.io.resolve_stderr(idx, &handle, direct, promote)?)
                } else {
                    cx.err.clone()
                };
            }
            IoStream::Tty => {
                if seen_tty {
                    bail!("step {}: WITH_IO declared tty more than once", idx + 1);
                }
                seen_tty = true;
            }
            IoStream::Rows(_) => {
                if seen_rows {
                    bail!("step {}: WITH_IO declared rows more than once", idx + 1);
                }
                seen_rows = true;
            }
            IoStream::Cols(_) => {
                if seen_cols {
                    bail!("step {}: WITH_IO declared cols more than once", idx + 1);
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
    idx: usize,
    target: &PipeTarget,
) -> Result<PipeHandle> {
    let PipeTarget::Var(var) = target;
    match cx.state.get_var_typed(var) {
        Some((kind, value)) if matches!(kind, TypeTag::Pipe) => {
            let Some(handle) = value.as_pipe_handle() else {
                bail!(
                    "step {}: TypeMismatch: expected PIPE, got {} ({:?})",
                    idx + 1,
                    kind.name(),
                    value
                );
            };
            Ok(handle)
        }
        Some((kind, value)) => {
            bail!(
                "step {}: TypeMismatch: expected PIPE, got {} ({:?})",
                idx + 1,
                kind.name(),
                value
            );
        }
        None => {
            bail!("step {}: undeclared variable ${var}", idx + 1);
        }
    }
}

/// If `cmd` is a bare `NAME(...)` call possibly nested under `WITH_IO` layers, return the
/// merged bindings (outermost first, inner wins per stream) plus the call
/// name and args. Used by `LET`-capture and `ASYNC` fast paths so
/// `WITH_IO [stdin=$tx] FOO()` binds the `RETURN` value instead
/// of swallowing stdout into a capture sink.
fn extract_call(cmd: &StepKind) -> Option<(Vec<IoBinding>, &str, &[Expr])> {
    let mut layers: Vec<&Vec<IoBinding>> = Vec::new();
    let mut current = cmd;
    loop {
        match current {
            StepKind::Call { name, args } => {
                let mut merged: Vec<IoBinding> = Vec::new();
                for layer in &layers {
                    for binding in layer.iter() {
                        match merged.iter_mut().find(|m| m.stream == binding.stream) {
                            Some(slot) => *slot = binding.clone(),
                            None => merged.push(binding.clone()),
                        }
                    }
                }
                return Some((merged, name, args));
            }
            StepKind::WithIo { bindings, cmd } => {
                layers.push(bindings);
                current = cmd;
            }
            _ => return None,
        }
    }
}

pub(super) fn exit<P: ProcessManager>(cx: &mut StepCtx<'_, P>, code: i64) -> Result<()> {
    for child in cx.state.bg_children.iter_mut() {
        if let Ok(None) = child.try_wait() {
            let _ = child.kill();
            let _ = child.try_wait();
        }
    }
    cx.state.bg_children.clear();
    Err(anyhow::Error::new(super::exit::ExitRequest(code)))
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
            let pop_res = cx.state.pop_scope();
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
        let key_name = key_var.ok_or_else(|| {
            anyhow!("FOR loop over Map requires key and value bindings: FOR $k: STRING, $v: TYPE IN $map")
        })?;
        let clean_key_var = key_name.trim_start_matches('$').to_string();
        let mut keys: Vec<_> = map.keys().cloned().collect();
        keys.sort();

        // Map keys are strings: only a STRING key binding is valid here.
        let kt = match &key_type {
            Some(name) => cx.state.resolve_tag(name)?,
            None => TypeTag::String,
        };
        if !matches!(kt, TypeTag::String) {
            anyhow::bail!(
                "FOR loop over MAP requires a STRING key variable, got {}",
                kt.name(),
            );
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
            let pop_res = cx.state.pop_scope();
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
    bail!(
        "FOR loop requires a List or Map iterable, found {:?}",
        iterable
    )
}

/// Define a user function (`FUNC NAME($p: TYPE, ...) { ... }}).
/// Copy-on-write into a fresh registry Arc so `fork()` sharers keep the
/// old view, while `push_scope`/`pop_scope` snapshots revert nested
/// definitions on block exit.
pub(crate) fn define_func<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    name: &str,
    params: &[(String, String)],
    body: &[Step],
) -> Result<()> {
    // Resolve param type names where the directory lives, so the
    // registry only ever holds tags. Unknown names fail here naming
    // the function and every known type and schema.
    let mut resolved = Vec::with_capacity(params.len());
    for (pname, ptype) in params {
        resolved.push((pname.clone(), cx.state.resolve_tag(ptype)?));
    }
    // Runtime counterpart of the parse-time scope validation; the single
    // registry owns the reserved, duplicate, and shadowing rules.
    cx.state.functions.define_script(name, &resolved, body)
}

/// Invoke a function by UPPERCASE name and return its value. One lookup
/// against the unified registry, then gates, then effects: depth budget
/// and arity (mandatory metadata on every entry) validate before any
/// argument evaluates. Script bodies run in a fresh lexical scope
/// (LET/ENV/WORKDIR revert; pipes and files persist) with `declare_var`
/// coercion on parameter binding; `RETURN` inside yields the value and
/// fallthrough yields `""`. `BREAK`/`CONTINUE` escaping the body are
/// boundary errors (they must not reach a caller loop).
pub(crate) fn call_func_value<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    name: &str,
    args: &[Expr],
) -> Result<Value> {
    // 1. Registry lookup: the single source for every callable.
    let Some(entry) = cx.state.functions.get(name) else {
        bail!("step {}: unknown function `{name}`", idx + 1);
    };
    // 2. Global recursion check.
    if cx.state.call_depth >= MAX_CALL_DEPTH {
        bail!(
            "step {}: recursion depth limit exceeded in FUNC {}",
            idx + 1,
            super::base_name(name)
        );
    }
    // 3. Pre-evaluation arity gate from the entry metadata. Trailing
    // optional MAP params widen exact counts to ranges; everything
    // else stays exact.
    if let Some(params) = entry.meta.params.as_deref() {
        let required = super::required_arity(params);
        if args.len() < required || args.len() > params.len() {
            if required == params.len() {
                bail!(
                    "step {}: {}() expects {} argument(s), got {}",
                    idx + 1,
                    super::base_name(name),
                    params.len(),
                    args.len()
                );
            }
            bail!(
                "step {}: {}() expects {} to {} argument(s), got {}",
                idx + 1,
                super::base_name(name),
                required,
                params.len(),
                args.len()
            );
        }
    }
    // 4. Argument evaluation: only reachable for a valid invocation.
    let mut arg_vals = Vec::with_capacity(args.len());
    for arg in args {
        arg_vals.push(super::args::evaluate_expr(arg, cx)?);
    }
    // 5. Execution by body kind.
    match entry.body {
        FuncBody::Script(def) => {
            cx.state.call_depth += 1;
            cx.state.push_scope();
            let outcome: Result<Value> = (|| {
                for ((pname, ptype), pval) in def.params.iter().zip(arg_vals) {
                    cx.state.declare_var(pname.clone(), ptype.clone(), pval)?;
                }
                let flow = super::steps::execute_steps(
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
                )?;
                match flow {
                    Flow::Done => Ok(Value::string(String::new())),
                    Flow::Return { value, .. } => Ok(value),
                    Flow::Break { idx } => {
                        bail!(
                            "step {}: BREAK cannot cross function boundary (in {name}())",
                            idx + 1
                        );
                    }
                    Flow::Continue { idx } => {
                        bail!(
                            "step {}: CONTINUE cannot cross function boundary (in {name}())",
                            idx + 1
                        );
                    }
                }
            })();
            let pop_res = cx.state.pop_scope();
            cx.state.call_depth -= 1;
            match (outcome, pop_res) {
                (Ok(value), Ok(())) => Ok(value),
                (Err(e), _) => Err(e),
                (Ok(_), Err(e)) => Err(e),
            }
        }
        FuncBody::Pure(func) => {
            let value = func(arg_vals)
                .with_context(|| format!("step {}: function `{name}` failed", idx + 1))?;
            super::args::check_host_return_value(
                &format!(
                    "step {}: function `{}` returned",
                    idx + 1,
                    super::base_name(name)
                ),
                &entry.meta.returns,
                &value,
            )?;
            Ok(value)
        }
        FuncBody::Ctx(func) | FuncBody::Both { ctx: func, .. } => {
            let value = func(cx, arg_vals)
                .with_context(|| format!("step {}: function `{name}` failed", idx + 1))?;
            super::args::check_host_return_value(
                &format!(
                    "step {}: function `{}` returned",
                    idx + 1,
                    super::base_name(name)
                ),
                &entry.meta.returns,
                &value,
            )?;
            Ok(value)
        }
    }
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
    let outcome: Result<Value> = (|| {
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
            Flow::Break { idx } => {
                bail!("step {}: BREAK cannot cross block boundary", idx + 1)
            }
            Flow::Continue { idx } => {
                bail!("step {}: CONTINUE cannot cross block boundary", idx + 1)
            }
        }
    })();
    let pop_res = cx.state.pop_scope();
    match (outcome, pop_res) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e),
    }
}

/// Evaluate `RETURN <expr>` to the enclosing boundary's value: a function
/// call, an `ASYNC` task (bound by `LET $o = AWAIT $t`), or an inline
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
    idx: usize,
    cond: &Expr,
    body: &[Step],
) -> Result<Flow> {
    use std::sync::atomic::Ordering;
    loop {
        if cx.state.cancel_token.load(Ordering::SeqCst) {
            bail!("step {}: ASYNC task cancelled", idx + 1);
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
        let pop_res = cx.state.pop_scope();
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
) -> Result<()> {
    let value = super::args::evaluate_expr(expr, cx)?;
    let clean_var = var.trim_start_matches('$').to_string();
    let tag = cx.state.resolve_tag(&decl_type)?;
    cx.state.declare_var(clean_var, tag, value)?;
    Ok(())
}

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

/// Dispatch `LET $var: STRING = <sync command>` : run the command to completion with
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
pub(crate) fn assign_capture<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    generation: usize,
    idx: usize,
    var: &str,
    decl_type: String,
    cmd: &StepKind,
) -> Result<Flow> {
    use std::sync::Arc;

    if let Some((bindings, name, args)) = extract_call(cmd) {
        // `CALL` (possibly under `WITH_IO` layers): no capture sink. The
        // callee's stdout keeps its routed streams (observable via
        // `ASSERT_CONTAINS stdout`/pipes); the bound value is the `RETURN` payload.
        if bindings
            .iter()
            .any(|b| b.stream == IoStream::Stdout && b.pipe.is_some())
        {
            bail!(
                "step {}: LET capture cannot use WITH_IO [stdout=$var]; the capture binds the RETURN value",
                idx + 1
            );
        }
        if bindings.is_empty() {
            let value = call_func_value(cx, idx, name, args)?;
            let clean_var = var.trim_start_matches('$').to_string();
            let tag = cx.state.resolve_tag(&decl_type)?;
            cx.state.declare_var(clean_var, tag, value)?;
            return Ok(Flow::Done);
        }
        let (step_stdin, expose_stdin, step_stdout, step_stderr, out_pipe, stdin_pipe) =
            resolve_io_streams(cx, idx, &bindings, cmd)?;
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
        let value = call_func_value(&mut sub_cx, idx, name, args)?;
        let tag = sub_cx.state.resolve_tag(&decl_type)?;
        sub_cx
            .state
            .declare_var(var.trim_start_matches('$').to_string(), tag, value)?;
        return Ok(Flow::Done);
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
            bail!("step {}: BREAK outside loop (cannot be captured)", idx + 1);
        }
        Flow::Continue { idx } => {
            bail!(
                "step {}: CONTINUE outside loop (cannot be captured)",
                idx + 1
            );
        }
        Flow::Return { idx, .. } => {
            bail!(
                "step {}: RETURN outside function (cannot be captured)",
                idx + 1
            );
        }
    }
    let text = sink
        .drain_string_strict()
        .map_err(|e| anyhow!("LET ${var} capture is not valid UTF-8: {e}"))?;
    let clean_var = var.trim_start_matches('$').to_string();
    let tag = cx.state.resolve_tag(&decl_type)?;
    cx.state.declare_var(clean_var, tag, Value::string(text))?;
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
    let PipeTarget::Var(var) = target;
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
        | StepKind::Isolated { body }
        | StepKind::For { body, .. }
        | StepKind::While { body, .. } => {
            for step in body {
                collect_body_bindings(&step.kind, state, out);
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
        | StepKind::AssignAsync { .. } => {}
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
    run(cx, 0, &cmd)
}

pub(crate) fn dispatch_run_exec<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::RunExec { argv } = step else {
        unreachable!()
    };
    let resolved = resolve_run_exec_argv(argv, cx)?;
    run_argv(cx, 0, &resolved)
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
            Flow::Break { idx } => {
                anyhow::bail!("step {}: BREAK cannot cross ASYNC boundary", idx + 1);
            }
            Flow::Continue { idx } => {
                anyhow::bail!("step {}: CONTINUE cannot cross ASYNC boundary", idx + 1);
            }
            Flow::Return { idx, .. } => {
                anyhow::bail!("step {}: RETURN cannot cross ASYNC boundary", idx + 1);
            }
        }
    });

    // Store the thread handle as a background handle in the parent's state.
    cx.state
        .bg_children
        .push(Box::new(super::steps::ThreadJoinHandle::new(
            join,
            cancel_token,
            cancel_hub,
            active_process,
            worker,
        )));
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
    workdir(cx, 0, &path)
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
    super::remote::run_remote_block(cx, target, vars, env, body, 0)
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
    copy(cx, 0, from_workspace.clone(), &from_resolved, &to_resolved)
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
    archive(cx, 0, &src_resolved, &dst_resolved, *raw)
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
    unarchive(cx, 0, &src_resolved, &dst_resolved, cap_resolved.as_deref())
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
        0,
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
    symlink(cx, 0, from_workspace.clone(), &from_resolved, &to_resolved)
}

pub(crate) fn dispatch_mkdir<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Mkdir(arg) = step else {
        unreachable!()
    };
    let path = super::args::resolve_arg(arg, cx)?;
    mkdir(cx, 0, &path)
}

pub(super) fn install<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    from: &str,
    to: &str,
) -> Result<()> {
    let from_abs = cx
        .state
        .fs
        .resolve_read(&cx.state.cwd, from)
        .with_context(|| format!("step {}: INSTALL {from} {to}", idx + 1))?;
    let to_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, to)
        .with_context(|| format!("step {}: INSTALL {from} {to}", idx + 1))?;
    cx.state
        .fs
        .install_file_atomic(&from_abs, &to_abs, 0o755)
        .with_context(|| format!("step {}: INSTALL {from} {to}", idx + 1))?;
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
    install(cx, 0, &from_resolved, &to_resolved)
}

pub(crate) fn dispatch_ls<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Ls(arg) = step else {
        unreachable!()
    };
    let resolved = super::args::resolve_arg_opt(arg, cx)?;
    ls(cx, 0, &resolved)
}

pub(crate) fn dispatch_cwd<P: ProcessManager>(
    _step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    cwd(cx, 0)
}

pub(crate) fn dispatch_read<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Read(arg) = step else {
        unreachable!()
    };
    let resolved = super::args::resolve_arg_opt(arg, cx)?;
    read(cx, 0, &resolved)
}

pub(crate) fn dispatch_read_line<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::ReadLine { var } = step else {
        unreachable!()
    };
    read_line(cx, 0, var)
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
    idx: usize,
    var: &str,
    message: &str,
    default: Option<&str>,
) -> Result<()> {
    #[cfg(miri)]
    {
        let _ = (cx, idx, var, message, default);
        bail!(
            "step {}: PROMPT unavailable under Miri (no terminal)",
            idx + 1
        );
    }
    #[cfg(not(miri))]
    {
        let clean_var = var.trim_start_matches('$').to_string();
        // Interactivity needs both a host TTY and an unredirected
        // stdin: a `WITH_IO [stdin=...]` pipe can never answer a
        // prompt, so PROMPT short-circuits instead of blocking on it.
        if !cx.process.stdin_is_terminal() || cx.stdin.is_redirected() {
            match default {
                Some(d) => {
                    eprintln!("non-interactive: using default for {message}");
                    let text = Value::string(d.to_string());
                    bind_prompt_var(cx, idx, &clean_var, text)?;
                    return Ok(());
                }
                None => {
                    bail!(
                        "step {}: PROMPT {message}: non-TTY stdin has no answer and no default",
                        idx + 1
                    );
                }
            }
        }
        let rendered = format_prompt(message, default);
        {
            use std::io::Write;
            let mut err = std::io::stderr().lock();
            err.write_all(rendered.as_bytes()).with_context(|| {
                format!("step {}: PROMPT {message}: failed to write prompt", idx + 1)
            })?;
            err.flush().with_context(|| {
                format!("step {}: PROMPT {message}: failed to flush prompt", idx + 1)
            })?;
        }
        let mut line = String::new();
        let n = std::io::stdin()
            .read_line(&mut line)
            .with_context(|| format!("step {}: PROMPT {message}: failed reading stdin", idx + 1))?;
        if n == 0 && line.is_empty() {
            bail!(
                "step {}: PROMPT {message}: EOF on stdin before any input",
                idx + 1
            );
        }
        let trimmed = line
            .strip_suffix("\r\n")
            .or_else(|| line.strip_suffix('\n'))
            .unwrap_or(&line);
        let text = Value::string(apply_prompt_answer(trimmed, default));
        bind_prompt_var(cx, idx, &clean_var, text)?;
        Ok(())
    }
}

#[cfg_attr(miri, allow(dead_code))]
fn bind_prompt_var<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    clean_var: &str,
    text: Value,
) -> Result<()> {
    if cx.state.get_var_typed(clean_var).is_some() {
        cx.state
            .mutate_var(clean_var, text)
            .with_context(|| format!("step {}: PROMPT ${clean_var}", idx + 1))?;
    } else {
        cx.state
            .declare_var(clean_var.to_string(), TypeTag::String, text)
            .with_context(|| format!("step {}: PROMPT ${clean_var}", idx + 1))?;
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
    prompt(cx, 0, var, &message_resolved, default_resolved.as_deref())
}

/// Dispatch `LIST_APPEND $list <item>` : append to a LIST binding in
/// place (copy-on-write: sole owners mutate with no copy, aliases
/// detach). Touches no stream: like SLEEP it is a step effect, not an
/// IO filter, so pipeline bindings resolve but carry no bytes.
pub(super) fn push_into<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    var: &str,
    item: Value,
) -> Result<()> {
    cx.state
        .push_into_list(var, item)
        .with_context(|| format!("step {}: LIST_APPEND ${var}", idx + 1))
}

pub(crate) fn dispatch_push_into_step<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::ListAppend { list, item } = step else {
        unreachable!()
    };
    let value = super::args::evaluate_assert_operand(item, cx)?;
    push_into(cx, 0, list, value)
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
    write(cx, 0, &path_resolved, contents_resolved.as_deref())
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
    append(cx, 0, &path_resolved, contents_resolved.as_deref())
}

pub(super) fn remove<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    path: &str,
    recursive: bool,
) -> Result<()> {
    let cwd = cx.state.cwd.clone();
    let target = cx
        .state
        .fs
        .resolve_write(&cwd, path)
        .with_context(|| format!("step {}: REMOVE {path}", idx + 1))?;
    // Root and working-directory deletion guard: removing the cwd (or any
    // of its ancestors), the workspace root, or the effective root (`/`
    // under `WORKSPACE SYSTEM`) would destroy the execution context or
    // the host root, so fail closed before any filesystem driver runs.
    // Both sides compare canonicalized: symlinked prefixes would
    // otherwise defeat a lexical equality check.
    let cwd_canon = cx
        .state
        .fs
        .canonicalize(&cwd)
        .unwrap_or_else(|_| cwd.clone());
    if target == cwd_canon
        || cwd_canon.as_path().starts_with(target.as_path())
        || Some(&target) == cx.state.fs.workspace_root()
        || target == *cx.state.fs.root()
    {
        bail!(
            "step {}: REMOVE {path}: cannot remove working directory or workspace root",
            idx + 1
        );
    }
    // Lexical link probe: canonical resolution routes through the
    // final link, so a link would otherwise read as its target and the
    // target would be deleted instead of the link.
    if cx
        .state
        .fs
        .remove_link_if_symlink(&cwd, path)
        .with_context(|| format!("step {}: REMOVE {path}", idx + 1))?
    {
        return Ok(());
    }
    match cx.state.fs.entry_kind_no_follow(&target) {
        Err(_) => bail!("step {}: REMOVE {path}: no such file or directory", idx + 1),
        Ok(EntryKind::File) => {
            cx.state
                .fs
                .remove_file(&target)
                .with_context(|| format!("step {}: REMOVE {path}", idx + 1))?;
            Ok(())
        }
        // A link reaching here appeared between the probe and the
        // resolve: fail closed rather than touch its target.
        Ok(EntryKind::Symlink) => bail!(
            "step {}: REMOVE {path}: link changed during removal",
            idx + 1
        ),
        Ok(EntryKind::Dir) => {
            if recursive {
                cx.state
                    .fs
                    .remove_dir_all(&target)
                    .with_context(|| format!("step {}: REMOVE {path}", idx + 1))?;
                Ok(())
            } else {
                cx.state.fs.remove_empty_dir(&target).with_context(|| {
                    format!(
                        "step {}: REMOVE {path}: directory not empty (use --recursive)",
                        idx + 1
                    )
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
    remove(cx, 0, &path_resolved, *recursive)
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
    replace(cx, 0, &path_resolved, &overrides_resolved)
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
    let target = super::steps::resolve_assert_target(actual, cx)?;
    let expected_val = match expected {
        Some(e) => Some(super::args::evaluate_assert_operand(e, cx)?),
        None => None,
    };
    assert_eq(cx, 0, 0, 0, hash, &target, expected_val.as_ref())
}

pub(crate) fn dispatch_assert_contains<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::AssertContains { haystack, needle } = step else {
        unreachable!()
    };
    let target = super::steps::resolve_assert_target(haystack, cx)?;
    assert_contains(cx, 0, 0, 0, &target, needle)
}

pub(crate) fn dispatch_hash_sha256<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::HashSha256 { path } = step else {
        unreachable!()
    };
    let path_resolved = super::args::resolve_arg(path, cx)?;
    hash_sha256(cx, 0, &path_resolved)
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

pub(crate) fn dispatch_assign<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Assign {
        var,
        decl_type,
        expr,
    } = step
    else {
        unreachable!()
    };
    assign(cx, var, decl_type.clone(), expr)
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
    let StepKind::FuncDef { name, params, body } = step else {
        unreachable!()
    };
    define_func(cx, name, params, body)
}

pub(crate) fn dispatch_call<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Call { name, args } = step else {
        unreachable!()
    };
    call_func_value(cx, 0, name, args).map(|_| ())
}

pub(crate) fn dispatch_return<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Return { expr } = step else {
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
    top_level_flow(while_loop(cx, 0, cond, body)?)
}

pub(crate) fn dispatch_break<P: ProcessManager>(
    step: &StepKind,
    _cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Break = step else {
        unreachable!()
    };
    bail!("BREAK outside loop");
}

pub(crate) fn dispatch_continue<P: ProcessManager>(
    step: &StepKind,
    _cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Continue = step else {
        unreachable!()
    };
    bail!("CONTINUE outside loop");
}

// ── AWAIT / AssignAsync handlers ─────────────────────────────────────────

/// Whether an ASYNC body can publish a value: true when any step in its
/// own scope is a `RETURN`. Nested `FUNC` definitions and nested `ASYNC`
/// bodies belong to inner scopes and never count, and a bare `Call`
/// statement discards its value (only the single-call fast path
/// publishes). A body that can return but falls off the end fails its
/// capture loudly, so a missing fallback branch can never silently yield
/// the void-task zero.
fn body_returns_value(body: &[Step]) -> bool {
    body.iter().any(|step| step_returns_value(&step.kind))
}

fn step_returns_value(kind: &StepKind) -> bool {
    match kind {
        StepKind::Return { .. } => true,
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
        StepKind::FuncDef { .. } | StepKind::AsyncBlock { .. } | StepKind::AssignAsync { .. } => {
            false
        }
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
    // main flow. A task's *value* (for `LET $o = AWAIT $t`) travels only
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
    // A single-`CALL` body (possibly under `WITH_IO` layers) runs as a
    // function invocation whose `RETURN` value is published into the entry
    // for `LET $o = AWAIT $t`; block bodies publish a `RETURN` value the
    // same way. Control flow never crosses the thread boundary: stray
    // BREAK/CONTINUE become errors here.
    let call_task: Option<(Vec<IoBinding>, String, Vec<Expr>)> = match body.as_slice() {
        [step] => extract_call(&step.kind)
            .map(|(bindings, name, args)| (bindings, name.to_string(), args.to_vec())),
        _ => None,
    };
    // Single-call bodies always yield their call value; block bodies yield
    // one only when they can RETURN. Recorded on the entry so a later
    // capture can tell "void task" (binds 0) from "fell off the end"
    // (loud error).
    let returns_value = call_task.is_some() || body_returns_value(&body);
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
            .map_err(|_| anyhow::anyhow!("ASYNC task entry unavailable"))?;
        if let Some((bindings, name, args)) = call_task {
            let mut child_cx = super::steps::StepCtx {
                state: &mut child_state,
                process: &mut child_process,
                stdin,
                expose_stdin,
                out,
                err,
                out_pipe,
                stdin_pipe,
                tty: None,
            };
            // Apply call-site bindings (e.g. stdin pipes) like the inline
            // path; stdout stays shared-live (parse rejects stdout pipes).
            let value = if bindings.is_empty() {
                call_func_value(&mut child_cx, 0, &name, &args)?
            } else {
                let (task_stdin, task_expose, task_out, task_err, task_out_pipe, task_stdin_pipe) =
                    resolve_io_streams(&mut child_cx, 0, &bindings, &body[0].kind)?;
                let state = &mut *child_cx.state;
                let process = &mut *child_cx.process;
                let mut sub_cx = super::steps::StepCtx {
                    state,
                    process,
                    stdin: task_stdin,
                    expose_stdin: task_expose,
                    out: task_out,
                    err: task_err,
                    out_pipe: task_out_pipe.or(child_cx.out_pipe.clone()),
                    stdin_pipe: task_stdin_pipe.or(child_cx.stdin_pipe.clone()),
                    tty: None,
                };
                call_func_value(&mut sub_cx, 0, &name, &args)?
            };
            entry
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .return_value = Some(value);
            return Ok(());
        }
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
            Flow::Break { idx } => {
                anyhow::bail!("step {}: BREAK cannot cross ASYNC boundary", idx + 1);
            }
            Flow::Continue { idx } => {
                anyhow::bail!("step {}: CONTINUE cannot cross ASYNC boundary", idx + 1);
            }
            Flow::Return { value, .. } => {
                // Block bodies publish an explicit result like single-call
                // bodies and function calls do; there is no stdout
                // sniffing. `LET $o = AWAIT $t` binds this value.
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
        .declare_var(var.to_string(), tag, Value::handle(task_id))?;
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
        .ok_or_else(|| anyhow::anyhow!("variable '${var}' is not defined"))?;
    let Some(task_id) = val.as_handle() else {
        bail!("variable '${var}' is not a task handle");
    };
    resolve_task_entry_by_id(task_id, &format!("${var}"), cx)
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
        bail!("task handle for '{label}' was not found or has already been awaited");
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
                bail!("AWAIT task '${var}' was cancelled");
            }
            TaskPhase::Completed | TaskPhase::Awaiting => {
                bail!("task handle for '${var}' was not found or has already been awaited");
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
                            return Err(err);
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
                bail!("AWAIT task '${var}' was cancelled");
            }
            Decision::KillOnTimeout => {
                if let Some(mut handle) = timeout_kill {
                    let _ = handle.kill();
                }
                entry.finish_teardown();
                bail!("AWAIT task '${var}' interrupted by TIMEOUT");
            }
            Decision::Done(status) => {
                entry.finish_teardown();
                if !status.success() {
                    bail!("AWAIT task '${var}' failed with status {status}");
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
        .ok_or_else(|| anyhow::anyhow!("variable '${var}' is not defined"))?;
    // A LIST of HANDLEs awaits every member in order, so worker pools
    // collected with PUSH join as a group; an empty LIST is a no-op. The
    // first failing member bails like sequential single AWAITs would.
    // `LET $o = AWAIT $t` stays single-handle: capture needs one output.
    // Bare AWAIT is join-only: task output already streamed live during
    // the run, so there is nothing to forward.
    if let Some(items) = val.as_list().cloned() {
        for (idx, item) in items.iter().enumerate() {
            // `member` feeds callees whose templates add the `$`
            // themselves (`await_task_entry`); `label` feeds this
            // function's own `'{label}'` messages. Splitting the two
            // keeps every rendering at exactly one `$`.
            let member = format!("{var}[{idx}]");
            let label = format!("${member}");
            let Some(task_id) = item.as_handle() else {
                bail!(
                    "AWAIT '{label}' is not a task handle, got {}",
                    item.type_name()
                );
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

/// Dispatch `LET $out: TYPE = AWAIT $task` : join like bare `AWAIT` (identical
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
) -> Result<()> {
    let entry = resolve_task_entry(task_var, cx)?;
    await_task_entry(&entry, &cx.state.cancel_token, task_var)?;
    let (return_value, returns_value) = {
        let guard = entry.state.lock().unwrap_or_else(|e| e.into_inner());
        (guard.return_value.clone(), guard.returns_value)
    };
    if let Some(value) = return_value {
        let tag = cx.state.resolve_tag(&out_type)?;
        cx.state
            .declare_var(out_var.trim_start_matches('$').to_string(), tag, value)?;
        return Ok(());
    }
    if returns_value {
        anyhow::bail!(
            "LET ${out_var} = AWAIT ${task_var} fell off the end without RETURN: the task body can return a value on some path but this run produced none"
        );
    }
    let tag = cx.state.resolve_tag(&out_type)?;
    cx.state.declare_var(
        out_var.trim_start_matches('$').to_string(),
        tag,
        Value::int(0),
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
        .ok_or_else(|| anyhow::anyhow!("variable '${var}' is not defined"))?;
    let Some(task_id) = val.as_handle() else {
        bail!("variable '${var}' is not a task handle");
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
        bail!("CANCEL: task '${var}' has already been awaited or does not exist");
    };

    // Transition to Cancelled and take the handle under a short lock.
    let handle = {
        let mut guard = entry.state.lock().unwrap_or_else(|e| e.into_inner());
        match guard.phase {
            TaskPhase::Cancelled => {
                bail!("CANCEL: task '${var}' was already cancelled");
            }
            TaskPhase::Completed => {
                bail!("CANCEL: task '${var}' has already been awaited or does not exist");
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

/// Pipeline dispatch wrapper for `AssignAsync`
pub(crate) fn dispatch_assign_async_step<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::AssignAsync {
        var,
        decl_type,
        body,
    } = step
    else {
        unreachable!()
    };
    dispatch_assign_async(var, decl_type.clone(), body, cx)
}

/// Pipeline dispatch wrapper for `Await`
pub(crate) fn dispatch_await_step<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Await { var } = step else {
        unreachable!()
    };
    dispatch_await(var, cx)
}

/// Pipeline dispatch wrapper for `AssignCapture`
pub(crate) fn dispatch_assign_capture_step<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::AssignCapture {
        var,
        decl_type,
        cmd,
    } = step
    else {
        unreachable!()
    };
    top_level_flow(assign_capture(
        cx,
        super::steps::allocate_assert_generation(),
        0,
        var,
        decl_type.clone(),
        cmd,
    )?)
}

/// Pipeline dispatch wrapper for `AwaitCapture`
pub(crate) fn dispatch_await_capture_step<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::AwaitCapture {
        out_var,
        out_type,
        task_var,
    } = step
    else {
        unreachable!()
    };
    dispatch_await_capture(out_var, out_type.clone(), task_var, cx)
}

/// Pipeline dispatch wrapper for `Cancel`
pub(crate) fn dispatch_cancel_step<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Cancel { var } = step else {
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
    top_level_flow(timeout(cx, 0, &duration, body)?)
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

/// Dispatch `TIMEOUT <duration> <body>`: run `body` on the current thread
/// with a deadline. If the deadline elapses first, cancel the state,
/// `kill()` the active foreground process (when one is registered), and
/// return a deadline error wrapping the body result; a body error with
/// time still left returns unwrapped.
///
/// `idx` is the 0-based step index used for error attribution.
pub(crate) fn timeout<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    duration: &std::time::Duration,
    body: &[Step],
) -> Result<Flow> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    let initial_cancel = cx.state.cancel_token.load(Ordering::SeqCst);
    let deadline = Instant::now() + *duration;
    let done = Arc::new(AtomicBool::new(false));
    let fired = Arc::new(AtomicBool::new(false));
    let cancel_token = Arc::clone(&cx.state.cancel_token);
    let cancel_hub = Arc::clone(&cx.state.cancel_hub);
    let active_process = Arc::clone(&cx.state.active_process);

    // Deadline watcher: enforces the timeout while the body runs on this
    // thread. Exits as soon as `done` is set; joined below, so no thread
    // outlives this call.
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
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    // Deadline enforcement for nested RUNs: spawn cancellable so a blocking
    // foreground process registers in active_process, where the watcher can
    // reach it. Restored afterwards (nesting-safe: an outer TIMEOUT already
    // sets this, and the restore preserves it).
    //
    // The scope never joins background work (`wait_at_end` stays false,
    // like IF/FOR/WHILE/ISOLATED bodies): joining here would wait for tasks
    // started outside the scope, so a long-lived server task turns every
    // later TIMEOUT into a wait for its deadline. Tasks the body itself
    // backgrounds are reaped at the pipeline top as usual; a genuinely
    // stuck body is still killed through the cancellation token and the
    // active-process handle above.
    let prev_cancellable = cx.state.cancellable;
    cx.state.cancellable = true;
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
        false,
    );
    cx.state.cancellable = prev_cancellable;

    done.store(true, Ordering::SeqCst);
    let _ = watcher.join();

    // Restore pre-existing cancellation state if deadline watcher did not fire
    if !fired.load(Ordering::SeqCst) {
        cx.state
            .cancel_token
            .store(initial_cancel, Ordering::SeqCst);
    }

    let budget = oxdock_parser::command::format_duration(duration);
    match result {
        Ok(Flow::Done) => {
            if fired.load(Ordering::SeqCst) {
                bail!(
                    "step {}: TIMEOUT after {} — deadline exceeded",
                    idx + 1,
                    budget
                );
            }
            Ok(Flow::Done)
        }
        Ok(flow) => {
            if fired.load(Ordering::SeqCst) {
                bail!(
                    "step {}: TIMEOUT after {} — deadline exceeded",
                    idx + 1,
                    budget
                );
            }
            Ok(flow)
        }
        Err(err) => {
            if fired.load(Ordering::SeqCst) {
                Err(err).with_context(|| {
                    format!(
                        "step {}: TIMEOUT after {} — deadline exceeded",
                        idx + 1,
                        budget
                    )
                })
            } else {
                Err(err)
            }
        }
    }
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
    result.with_context(|| "inside ISOLATED block".to_string())
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
        bail!("isolated allowlist outside ISOLATED");
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
