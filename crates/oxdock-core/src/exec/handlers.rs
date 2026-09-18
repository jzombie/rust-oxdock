use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use oxdock_parser::{
    Arg, Expr, IoBinding, IoStream, PipeTarget, Step, StepKind, Value, WorkspaceTarget,
};
use oxdock_process::{
    BackgroundHandle, CommandOptions, CommandResult, CommandStderr, CommandStdin, CommandStdout,
    INHERIT_STDOUT_ENV_VAR, PROCESS_DEBUG_ENV_VAR, ProcessManager, SharedInput,
};
use sha2::{Digest, Sha256};

use super::SNAPSHOT_PENDING_DISPLAY;
use super::fs_ops::{canonical_cwd, copy_entry, hash_path};
use super::io::{StreamHandle, write_stdout};
use super::native::FuncBody;
use super::state::{ExecState, MAX_CALL_DEPTH, TaskPhase};
use super::steps::{Flow, StepCtx};
use oxdock_pipe::{KeeperGuard, PipeHandle, PipeInner};

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
            bail!("step {}: RETURN outside function", idx + 1);
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
    match target {
        WorkspaceTarget::Snapshot => {
            cx.state.fs.switch_to_snapshot();
            cx.state.cwd = cx.state.fs.root().clone();
        }
        WorkspaceTarget::Local => {
            cx.state.fs.switch_to_local();
            cx.state.cwd = cx.state.fs.root().clone();
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
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
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
        // Inside an ASYNC block — use background mode so we can register
        // the handle for cancellation via active_process.
        CommandOptions::background()
    } else {
        CommandOptions::foreground()
    };
    options.stdin = step_stdin;
    options.stdout = stdout_mode;
    options.stderr = stderr_mode;

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
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
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
        // Inside an ASYNC block — use background mode so we can register
        // the handle for cancellation via active_process.
        CommandOptions::background()
    } else {
        CommandOptions::foreground()
    };
    options.stdin = step_stdin;
    options.stdout = stdout_mode;
    options.stderr = stderr_mode;

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

/// Resolve exec-form (`RUN [...]`) argv elements with explicit coercion:
/// `Arg::String`/`Arg::Parts` resolve to exactly one entry each;
/// `Arg::Expr` evaluates and coerces by value — `String`/`Int`/`Bool` push
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
        // Opaque rendering: a handle in argv position stringifies like
        // anywhere else (`<pipe>`).
        out.push(format!("{val}"));
        return Ok(());
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
    write_stdout(cx.out.clone(), |writer| {
        writeln!(writer, "{}", msg)?;
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

/// Pipeline dispatch wrapper for `Connect`. Used by the generated pipeline;
/// the inline steps matches resolve with the real step index instead.
pub(crate) fn dispatch_connect_step<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Connect {
        endpoint,
        timeout,
        no_half_close,
    } = step
    else {
        unreachable!()
    };
    let endpoint = super::args::resolve_arg(endpoint, cx)?;
    let timeout = timeout
        .as_ref()
        .map(|flag| super::args::resolve_arg_as_duration(flag, cx))
        .transpose()?;
    super::net_bridge::connect(cx, 0, &endpoint, timeout, !no_half_close)
}

/// Pipeline dispatch wrapper for `Listen`. Used by the generated pipeline;
/// the inline steps matches resolve with the real step index instead.
pub(crate) fn dispatch_listen_step<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::Listen {
        bind,
        no_half_close,
    } = step
    else {
        unreachable!()
    };
    let bind = super::args::resolve_arg(bind, cx)?;
    super::net_bridge::listen(cx, 0, &bind, !no_half_close)
}

/// Dispatch `SLEEP <duration>` — park the step without spawning a shell.
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

pub(super) fn copy<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    from_current_workspace: bool,
    from: &str,
    to: &str,
) -> Result<()> {
    let from_abs = if from_current_workspace {
        cx.state
            .fs
            .resolve_copy_source_from_workspace(from)
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
    copy_entry(cx.state.fs.as_ref(), &from_abs, &to_abs)
        .with_context(|| format!("step {}: COPY {} {}", idx + 1, from, to))?;
    Ok(())
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
    from: &str,
    to: &str,
) -> Result<()> {
    let to_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, to)
        .with_context(|| format!("step {}: SYMLINK {} {}", idx + 1, from, to))?;
    let from_abs = cx
        .state
        .fs
        .resolve_copy_source(from)
        .with_context(|| format!("step {}: SYMLINK {} {}", idx + 1, from, to))?;
    cx.state
        .fs
        .symlink(&from_abs, &to_abs)
        .with_context(|| format!("step {}: SYMLINK {} {}", idx + 1, from, to))?;
    Ok(())
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
        cx.state
            .declare_var(clean_var, "STRING".to_string(), text)?;
    }
    Ok(())
}

/// Structured variable snapshot backing the `INSPECT()` expression form:
/// base keys (`type`, `variable`, `name`, `value`) plus live details —
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
    map.insert("type".to_string(), Value::string(decl_type.clone()));
    map.insert("variable".to_string(), Value::string(clean_var.clone()));
    match (decl_type.as_str(), value.as_pipe_handle()) {
        ("PIPE", Some(handle)) => {
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
        ("HANDLE", _) => {
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

    let mut expander = oxdock_process::StreamingExpand::new(overrides, ctx.envs()).with_vars(&vars);
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
/// exactly one `RUN`, guarded or not. A skipped guarded `RUN` still ends
/// its single step worker, which closes the writer and delivers EOF, so
/// guards do not change promotion safety. DSL bodies (`ECHO`, `READ_LINE`,
/// `WRITE`, keepers) stay on script pipes so multi writer fan in keeps
/// working.
fn async_single_run_body(cmd: &StepKind) -> bool {
    match cmd {
        StepKind::AsyncBlock { body } | StepKind::AssignAsync { body, .. } => {
            matches!(body.as_slice(), [step] if is_single_run(&step.kind))
        }
        _ => false,
    }
}

/// Whether `WITH_IO` promotes fresh pipe names to OS kernel pairs.
/// Fires when wrapping a single `RUN` background task (endpoints are
/// allocated on this thread before the worker spawns) or when evaluated
/// inside a worker thread around a single `RUN` (the `LET $t = WITH_IO
/// [..] ASYNC RUN` lowered form). Everything else, including DSL bodies
/// and sequential steps, keeps store and forward script pipes.
#[cfg(not(miri))]
fn promotion_trigger(cmd: &StepKind, inside_async: bool) -> bool {
    async_single_run_body(cmd) || (inside_async && is_single_run(cmd))
}

#[cfg(miri)]
fn promotion_trigger(_cmd: &StepKind, _inside_async: bool) -> bool {
    false
}

/// Whether this binding resolves to a zero copy OS handle instead of a
/// bridged shared handle: the ultimate consumer or producer is a `RUN`.
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
    )
}

/// Resolve `WITH_IO` bindings against the active context handles, shared by
/// `with_io` and the `CALL` fast paths (`LET`-capture / `ASYNC` tasks) so a
/// `CALL` under `WITH_IO` layers observes identical stream wiring whether
/// it runs inline or for its return value.
///
/// Besides the runnable streams this returns the script backends behind
/// the stdin/stdout bindings (`None` for OS pairs and unbound bindings),
/// which enrich the step context for timeout-bounded bridge reads and the
/// socket-EOF force-close.
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
    let trigger = promotion_trigger(cmd, cx.state.inside_async);
    let direct = run_terminated(cmd);

    for binding in bindings {
        if let Some(target) = &binding.pipe {
            let handle = resolve_pipe_handle(cx, idx, target)?;
            // Main-flow bindings decide here with the per-step trigger
            // (verbatim semantics); task-body bindings were pre-decided by
            // the spawn-time pin walk, making this a no-op there.
            cx.state.io.ensure_handle(&handle, trigger)?;
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
                    let (stdin, backend) =
                        cx.state.io.resolve_stdin(idx, &handle, direct, trigger)?;
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
                    let (stdout, backend) =
                        cx.state.io.resolve_stdout(idx, &handle, direct, trigger)?;
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
                    Some(cx.state.io.resolve_stderr(idx, &handle, direct, trigger)?)
                } else {
                    cx.err.clone()
                };
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
        Some((kind, value)) if kind == "PIPE" => {
            let Some(handle) = value.as_pipe_handle() else {
                bail!(
                    "step {}: TypeMismatch: expected PIPE, got {} ({:?})",
                    idx + 1,
                    kind,
                    value
                );
            };
            Ok(handle)
        }
        Some((kind, value)) => {
            bail!(
                "step {}: TypeMismatch: expected PIPE, got {} ({:?})",
                idx + 1,
                kind,
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
    bail!("EXIT requested with code {}", code);
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

    if let Some(items) = iterable.as_list() {
        for (i, item) in items.iter().enumerate() {
            // Each iteration is a scope (same rule as every other
            // block): loop vars live inside it, and ENV/WORKDIR/
            // WORKSPACE mutations revert on every iteration boundary.
            cx.state.push_scope();
            if let Some(idx_name) = key_var {
                let clean_idx = idx_name.trim_start_matches('$').to_string();
                let kt = key_type.clone().unwrap_or("INT".to_string());
                cx.state.declare_var(
                    clean_idx,
                    kt.clone(),
                    super::args::coerce_value(Value::int(i as i64), &kt, &*cx.state)?,
                )?;
            }
            cx.state.declare_var(
                clean_val_var.clone(),
                val_type.clone(),
                super::args::coerce_value(item.clone(), &val_type, &*cx.state)?,
            )?;

            let res = super::steps::execute_steps(
                cx.state,
                cx.process,
                body,
                cx.stdin.clone(),
                false,
                cx.out.clone(),
                cx.err.clone(),
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
        if key_type.clone().is_some_and(|kt| kt != "STRING") {
            anyhow::bail!(
                "FOR loop over MAP requires a STRING key variable, got {}",
                key_type.clone().unwrap_or("unknown".to_string()),
            );
        }
        for k in keys {
            let v = map[&k].clone();
            cx.state.push_scope();
            cx.state.declare_var(
                clean_key_var.clone(),
                "STRING".to_string(),
                Value::string(k.clone()),
            )?;
            cx.state.declare_var(
                clean_val_var.clone(),
                val_type.clone(),
                super::args::coerce_value(v, &val_type, &*cx.state)?,
            )?;

            let res = super::steps::execute_steps(
                cx.state,
                cx.process,
                body,
                cx.stdin.clone(),
                false,
                cx.out.clone(),
                cx.err.clone(),
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
    // Runtime counterpart of the parse-time scope validation; the single
    // registry owns the reserved, duplicate, and shadowing rules.
    cx.state.functions.define_script(name, params, body)
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
    // 3. Pre-evaluation arity gate from the entry metadata.
    if let Some(params) = entry.meta.params.as_deref()
        && params.len() != args.len()
    {
        bail!(
            "step {}: {}() expects {} argument(s), got {}",
            idx + 1,
            super::base_name(name),
            params.len(),
            args.len()
        );
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
            func(arg_vals).with_context(|| format!("step {}: function `{name}` failed", idx + 1))
        }
        FuncBody::Ctx(func) => func(cx, arg_vals)
            .with_context(|| format!("step {}: function `{name}` failed", idx + 1)),
    }
}

/// Evaluate `RETURN <expr>` inside a function call. Outside any call
/// (including at top level or with no function frame on this thread) it is a
/// step-numbered error. Crossing an `ASYNC` thread boundary is rejected
/// where the thread joins, not here.
pub(crate) fn handle_return<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    expr: &Expr,
) -> Result<Flow> {
    if cx.state.call_depth == 0 {
        bail!("step {}: RETURN outside function", idx + 1);
    }
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
    cx.state.declare_var(clean_var, decl_type, value)?;
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

/// Dispatch `LET $var: STRING = <sync command>` — run the command to completion with
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
            cx.state.declare_var(clean_var, decl_type, value)?;
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
        };
        let value = call_func_value(&mut sub_cx, idx, name, args)?;
        sub_cx
            .state
            .declare_var(var.trim_start_matches('$').to_string(), decl_type, value)?;
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
    cx.state
        .declare_var(clean_var, decl_type, Value::string(text))?;
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
/// by design — with no literals left to name, there is nothing to collect
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
    run_terminated: bool,
    produces: bool,
}

/// Drill through `WITH_IO` layers to the ultimate command: a binding's
/// bytes terminate there, so that command decides RUN-termination for the
/// whole-body promotion rule below.
fn leaf_command(kind: &StepKind) -> &StepKind {
    let mut current = kind;
    while let StepKind::WithIo { cmd, .. } = current {
        current = cmd;
    }
    current
}

/// Resolve one binding endpoint to its handle, skipping unresolvable
/// variables here: execution-time resolution reports the real error.
fn binding_handle<P: ProcessManager>(
    state: &ExecState<P>,
    target: &PipeTarget,
) -> Option<PipeHandle> {
    let PipeTarget::Var(var) = target;
    match state.get_var_typed(var) {
        Some((kind, value)) if kind == "PIPE" => value.as_pipe_handle(),
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
            // Whole-body input for the strict promotion rule: each
            // binding is judged by its ultimate command, not its layer.
            let run_terminated = run_terminated(leaf_command(cmd));
            for binding in bindings {
                let Some(target) = &binding.pipe else {
                    continue;
                };
                let Some(handle) = binding_handle(state, target) else {
                    continue;
                };
                out.push(BodyBinding {
                    handle,
                    run_terminated,
                    produces: !matches!(binding.stream, IoStream::Stdin),
                });
            }
            collect_body_bindings(cmd, state, out);
        }
        StepKind::Timeout { body, .. }
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
/// materialized with the whole-body promotion decision, and pin a keeper
/// slot on each produced script pipe — all synchronously on the spawning
/// thread, so backend decisions never depend on thread scheduling.
///
/// Promotion is strict: OS iff EVERY binding on the handle in the body is
/// RUN-terminated (pure RUN pipelines, the only shape where both ends can
/// hold raw fds); any mixed scope (RUN+DSL, RUN+host, RUN+bridge) pins
/// script, with the RUN side adapting through the existing feeder/shared
/// paths. Pins group by the top-level index of the final producer step
/// for each handle: the worker drops a handle's guard once that step
/// completes, so transient gaps between producers never signal EOF while
/// later consumer steps in the same task still observe it. Returns `None`
/// when the body produces to no script pipe. A task that only reads a
/// pipe gets existence without a pin (consumers rely on EOF-from-detach,
/// so pinning them would deadlock).
fn pin_async_keepers<P: ProcessManager>(
    cx: &StepCtx<'_, P>,
    body: &[Step],
) -> Result<Option<super::state::KeeperExpiry>> {
    struct Acc {
        handle: PipeHandle,
        run_and: bool,
        last_producer: Option<usize>,
    }
    let mut accs: Vec<Acc> = Vec::new();
    for (idx, step) in body.iter().enumerate() {
        let mut found = Vec::new();
        collect_body_bindings(&step.kind, cx.state, &mut found);
        for b in found {
            match accs.iter_mut().find(|a| Arc::ptr_eq(&a.handle, &b.handle)) {
                Some(a) => {
                    a.run_and &= b.run_terminated;
                    if b.produces {
                        a.last_producer = Some(idx);
                    }
                }
                None => accs.push(Acc {
                    handle: b.handle,
                    run_and: b.run_terminated,
                    last_producer: b.produces.then_some(idx),
                }),
            }
        }
    }
    // Decide every sighted handle first (consumers included: a consumer
    // arriving before any producer still finds the decided backend
    // instead of racing the producer's setup). One flag per handle makes
    // ensure order irrelevant.
    for a in &accs {
        cx.state.io.ensure_handle(&a.handle, a.run_and)?;
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
    let cmd = super::args::expand_dsl_vars(&cmd, cx.state);
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
    let expiry = pin_async_keepers(cx, &body)?;

    // Fork the execution state for the child thread.
    // This clones the fs (via clone_box), envs, cwd, var_scopes, etc.
    // The child gets fresh bg_children and scope_stack.
    let mut forked_state = cx.state.fork();
    forked_state.keeper_expiry = expiry;
    let forked_process = cx.process.clone();
    let stdin = cx.stdin.clone();
    let expose_stdin = cx.expose_stdin;
    let out = cx.out.clone();
    let err = cx.err.clone();
    let cancel_token = std::sync::Arc::clone(&forked_state.cancel_token);
    let active_process = std::sync::Arc::clone(&forked_state.active_process);

    // Spawn a thread that executes the block's steps with subshell isolation.
    // ENV/WORKDIR/etc mutations in the block do not leak to the parent.
    // Control flow never crosses the thread boundary: a stray BREAK,
    // CONTINUE, or RETURN becomes a step-numbered error here.
    let join = std::thread::spawn(move || {
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
            active_process,
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
        from_current_workspace,
        from,
        to,
    } = step
    else {
        unreachable!()
    };
    let from_resolved = super::args::resolve_arg(from, cx)?;
    let to_resolved = super::args::resolve_arg(to, cx)?;
    copy(cx, 0, *from_current_workspace, &from_resolved, &to_resolved)
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
    let StepKind::Symlink { from, to } = step else {
        unreachable!()
    };
    let from_resolved = super::args::resolve_arg(from, cx)?;
    let to_resolved = super::args::resolve_arg(to, cx)?;
    symlink(cx, 0, &from_resolved, &to_resolved)
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

/// Dispatch `LET $var: TYPE = ASYNC { ... }` — spawn a background task and store
/// the handle in the variable scope.
pub(crate) fn dispatch_assign_async<P: ProcessManager>(
    var: &str,
    decl_type: String,
    body: &[Step],
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    // Generate a unique task ID
    let task_id = cx
        .state
        .next_task_id
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    // Pre-allocate keeper handles synchronously on this thread, before the
    // worker exists, so pipes the task produces to (e.g. keeper
    // `WITH_IO [stdout=$tx] ASYNC ...` bindings) can never observe a
    // transient-only zero-writer window. Guards expire by step index as
    // the worker completes its final producer steps.
    let body = body.to_vec();
    let expiry = pin_async_keepers(cx, &body)?;

    // Fork the execution state for the child thread
    let mut forked_state = cx.state.fork();
    forked_state.keeper_expiry = expiry;
    let forked_process = cx.process.clone();
    let stdin = cx.stdin.clone();
    let expose_stdin = cx.expose_stdin;
    // Named tasks write stdout into a per-task spillable sink instead of
    // sharing the parent writer. Bare `AWAIT $t` forwards it to the parent
    // stdout; `LET $o: STRING = AWAIT $t` binds it. Stderr keeps parent wiring.
    let sink = std::sync::Arc::new(super::capture::new_spill_buffer());
    let out = Some(super::io::StreamHandle::Stream(sink.writer()));
    let err = cx.err.clone();
    let cancel_token = std::sync::Arc::clone(&forked_state.cancel_token);
    let active_process = std::sync::Arc::clone(&forked_state.active_process);

    // Spawn the task thread. Leftover guards unpin at thread termination.
    // A single-`CALL` body (possibly under `WITH_IO` layers) runs as a
    // function invocation whose `RETURN` value is published into the entry
    // for `LET $o = AWAIT $t`; block bodies keep stdout-sink semantics.
    // Control flow never crosses the thread boundary: stray
    // BREAK/CONTINUE/RETURN become errors here.
    let call_task: Option<(Vec<IoBinding>, String, Vec<Expr>)> = match body.as_slice() {
        [step] => extract_call(&step.kind)
            .map(|(bindings, name, args)| (bindings, name.to_string(), args.to_vec())),
        _ => None,
    };
    let (entry_tx, entry_rx) = std::sync::mpsc::channel::<Arc<super::state::TaskEntry>>();
    let join = std::thread::spawn(move || {
        let mut child_state = forked_state;
        let mut child_process = forked_process;
        if let Some((bindings, name, args)) = call_task {
            let entry = entry_rx
                .recv()
                .map_err(|_| anyhow::anyhow!("ASYNC task entry unavailable"))?;
            let mut child_cx = super::steps::StepCtx {
                state: &mut child_state,
                process: &mut child_process,
                stdin,
                expose_stdin,
                out,
                err,
                out_pipe: None,
                stdin_pipe: None,
            };
            // Apply call-site bindings (e.g. stdin pipes) like the inline
            // path; stdout keeps the task sink (parse rejects stdout pipes).
            let value = if bindings.is_empty() {
                call_func_value(&mut child_cx, 0, &name, &args)?
            } else {
                let (task_stdin, task_expose, task_out, task_err, _, _) =
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
                    out_pipe: None,
                    stdin_pipe: None,
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
            Flow::Return { idx, .. } => {
                anyhow::bail!("step {}: RETURN cannot cross ASYNC boundary", idx + 1);
            }
        }
    });

    // Create the thread handle
    let handle = super::steps::ThreadJoinHandle::new(join, cancel_token, active_process);

    // Store in named_tasks as a synchronized entry. The handle lives inside
    // the entry so CANCEL can tear it down even under concurrent AWAIT.
    // Published to the child above so single-call tasks can store their
    // RETURN value under the entry lock.
    {
        let mut named = cx
            .state
            .named_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = Arc::new(super::state::TaskEntry::new_with_sink(
            Box::new(handle),
            sink,
        ));
        named.insert(task_id, Arc::clone(&entry));
        let _ = entry_tx.send(entry);
    }

    // Store the task handle in the variable scope
    cx.state
        .declare_var(var.to_string(), decl_type, Value::handle(task_id))?;
    Ok(())
}

/// Dispatch `AWAIT $var` — block until the named task completes, propagate
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
        bail!("task handle for '${var}' was not found or has already been awaited");
    };
    Ok(entry)
}

/// Claim a task entry and run the bounded await poll loop to completion.
/// Shared by bare `AWAIT` and `LET $o: STRING = AWAIT $t` so cancellation, timeout,
/// double-await, and failure semantics never diverge. Returns the child's
/// exit status; the caller owns output handling (forward vs bind).
/// The child's thread is joined before returning success, so draining the
/// task sink afterwards races with no writer.
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

/// Dispatch `AWAIT $var` — block until the named task completes, propagate
/// error if it failed, and forward the task's captured stdout to the parent
/// stdout.
///
/// State machine (`TaskEntry`): the first `AWAIT` transitions the entry
/// `Running -> Awaiting` and owns the bounded poll loop below. A concurrent
/// `CANCEL` (or a `TIMEOUT` deadline) transitions the entry to `Cancelled`;
/// this loop observes that within ~10ms and rendezvouses on teardown
/// completion before reporting cancellation.
pub(crate) fn dispatch_await<P: ProcessManager>(var: &str, cx: &mut StepCtx<'_, P>) -> Result<()> {
    let entry = resolve_task_entry(var, cx)?;
    await_task_entry(&entry, &cx.state.cancel_token, var)?;
    // Bare AWAIT keeps status-only semantics for variables but preserves the
    // observable stream: the task's stdout flows to the parent stdout.
    if let Some(sink) = entry.take_sink() {
        let bytes = sink
            .drain_bytes()
            .map_err(|e| anyhow!("AWAIT task '${var}' output drain failed: {e}"))?;
        if !bytes.is_empty() {
            super::io::write_stdout(cx.out.clone(), |writer| {
                writer
                    .write_all(&bytes)
                    .with_context(|| format!("AWAIT task '${var}' output forward failed"))?;
                Ok(())
            })?;
        }
    }
    Ok(())
}

/// Dispatch `LET $out: TYPE = AWAIT $task` — join like bare `AWAIT` (identical
/// cancellation/timeout/double-await semantics via [`await_task_entry`]),
/// then bind the task's output: for a single-`CALL` task the function's
/// `RETURN` value (coerced to the declared type), otherwise the task's
/// stdout as a string.
pub(crate) fn dispatch_await_capture<P: ProcessManager>(
    out_var: &str,
    out_type: String,
    task_var: &str,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let entry = resolve_task_entry(task_var, cx)?;
    await_task_entry(&entry, &cx.state.cancel_token, task_var)?;
    if let Some(value) = entry
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .return_value
        .clone()
    {
        cx.state.declare_var(
            out_var.trim_start_matches('$').to_string(),
            out_type.clone(),
            value,
        )?;
        return Ok(());
    }
    let text = match entry.take_sink() {
        Some(sink) => sink.drain_string_strict().map_err(|e| {
            anyhow!("LET ${out_var} = AWAIT ${task_var} capture is not valid UTF-8: {e}")
        })?,
        None => String::new(),
    };
    cx.state.declare_var(
        out_var.trim_start_matches('$').to_string(),
        out_type,
        Value::string(text),
    )?;
    Ok(())
}

/// Dispatch `CANCEL $var` — synchronously kill a named background task.
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
        true,
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
