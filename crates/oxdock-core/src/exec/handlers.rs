use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::sync::Arc;

use oxdock_fs::EntryKind;
use oxdock_parser::{
    Arg, Expr, IoBinding, IoStream, PipeTarget, Step, StepKind, TypeKind, Value, WorkspaceTarget,
};
use oxdock_process::{
    BackgroundHandle, CommandOptions, CommandResult, CommandStderr, CommandStdin, CommandStdout,
    INHERIT_STDOUT_ENV_VAR, PROCESS_DEBUG_ENV_VAR, ProcessManager,
};
use sha2::{Digest, Sha256};

use super::fs_ops::{canonical_cwd, copy_entry, hash_path};
use super::io::{StreamHandle, write_stdout};
use super::pipe::KeeperGuard;
use super::state::{ExecState, FuncDefData, MAX_CALL_DEPTH};
use super::steps::{Flow, StepCtx};

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
    match target {
        WorkspaceTarget::Snapshot => {
            cx.state.fs.set_root(&cx.snapshot_root);
            cx.state.cwd = cx.state.fs.root().clone();
        }
        WorkspaceTarget::Local => {
            cx.state.fs.set_root(&cx.build_context);
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
            Arg::Expr(Expr::Literal(Value::String(s))) => {
                out.push(super::args::expand_string(s, &cx.state.envs, cx.state)?);
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
    match val {
        Value::String(s) => out.push(s.clone()),
        Value::Int(i) => out.push(i.to_string()),
        Value::Float(f) => out.push(f.to_string()),
        Value::Bool(b) => out.push(b.to_string()),
        Value::Pipe(n) => out.push(format!("pipe:{n}")),
        Value::Duration(d) => out.push(oxdock_parser::command::format_duration(d)),
        Value::Path(p) => out.push(p.to_string_lossy().to_string()),
        Value::List(items) => {
            for item in items {
                flatten_exec_value(item, out)?;
            }
        }
        Value::Map(_) => bail!("RUN exec form element must be a string, got map"),
        Value::TaskHandle(id) => {
            bail!("RUN exec form element must be a string, got task handle task#{id}")
        }
    }
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
        cx.state.cwd.clone()
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
    let real = canonical_cwd(cx.state.fs.as_ref(), &cx.state.cwd).with_context(|| {
        format!(
            "step {}: CWD failed to canonicalize {}",
            idx + 1,
            cx.state.cwd.display()
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
    let text = Value::String(line.to_string());
    if cx.state.get_var_typed(&clean_var).is_some() {
        cx.state.mutate_var(&clean_var, text)?;
    } else {
        cx.state.declare_var(clean_var, TypeKind::String, text)?;
    }
    Ok(())
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

pub(super) fn assert_file<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    hash: &Option<String>,
    path: &str,
    contents: Option<&str>,
) -> Result<()> {
    let target = cx
        .state
        .fs
        .resolve_read(&cx.state.cwd, path)
        .with_context(|| format!("step {}: ASSERT_FILE {}", idx + 1, path))?;
    if !matches!(cx.state.fs.entry_kind(&target)?, EntryKind::File) {
        bail!("step {}: ASSERT_FILE {} is not a file", idx + 1, path);
    }
    if let Some(expected) = hash {
        let mut hasher = Sha256::new();
        hash_path(cx.state.fs.as_ref(), &target, "", &mut hasher)?;
        let digest = hasher.finalize();
        let bytes: &[u8] = digest.as_ref();
        let actual: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        if !actual.eq_ignore_ascii_case(expected) {
            bail!(
                "step {}: ASSERT_FILE --hash mismatch for {}: expected {}, computed {}",
                idx + 1,
                path,
                expected,
                actual
            );
        }
        return Ok(());
    }
    if let Some(expected_body) = contents {
        let actual =
            cx.state.fs.read_file(&target).with_context(|| {
                format!("step {}: ASSERT_FILE {} could not be read", idx + 1, path)
            })?;
        if actual != expected_body.as_bytes() {
            bail!(
                "step {}: ASSERT_FILE content mismatch for {}\nexpected: {:?}\nactual:   {:?}",
                idx + 1,
                path,
                expected_body,
                String::from_utf8_lossy(&actual)
            );
        }
    }
    Ok(())
}

pub(super) fn assert_dir<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    path: &str,
) -> Result<()> {
    let target = cx
        .state
        .fs
        .resolve_read(&cx.state.cwd, path)
        .with_context(|| format!("step {}: ASSERT_DIR {}", idx + 1, path))?;
    if !matches!(cx.state.fs.entry_kind(&target)?, EntryKind::Dir) {
        bail!("step {}: ASSERT_DIR {} is not a directory", idx + 1, path);
    }
    Ok(())
}

pub(super) fn assert_absent<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    path: &str,
) -> Result<()> {
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, path)
        .with_context(|| format!("step {}: ASSERT_ABSENT {}", idx + 1, path))?;
    if cx.state.fs.entry_kind(&target).is_ok() {
        bail!("step {}: ASSERT_ABSENT {} exists", idx + 1, path);
    }
    Ok(())
}

pub(super) fn assert_stdout<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    generation: usize,
    idx_step: usize,
    needle: &str,
) -> Result<()> {
    // Mode 1: Piped stdin — actively consume stream and check
    if let CommandStdin::Stream(input_stream) = cx.stdin.clone() {
        let mut guard = input_stream
            .lock()
            .map_err(|_| anyhow!("failed to lock stdin for ASSERT_STDOUT"))?;
        let mut window = super::io::SlidingWindow::new(needle.as_bytes().to_vec());
        let mut buf = [0u8; super::io::CHUNK_SIZE];
        let mut read_any = false;
        loop {
            let n = guard
                .read(&mut buf)
                .context("failed to read from stdin for ASSERT_STDOUT")?;
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
                "step {}: ASSERT_STDOUT did not contain '{}'; emitted:\n{}",
                idx + 1,
                needle,
                emitted.trim_end()
            );
        }
    }

    // Mode 2: Step scope — check pre-registered window
    let windows = cx
        .state
        .assert_windows
        .lock()
        .map_err(|_| anyhow!("assert_windows poisoned"))?;
    let key = (generation, idx_step);
    match windows.get(&key) {
        Some(w) if w.matched => Ok(()),
        Some(w) => {
            let emitted = String::from_utf8_lossy(&w.ring_buffer()).into_owned();
            bail!(
                "step {}: ASSERT_STDOUT did not contain '{}'; emitted:\n{}",
                idx + 1,
                needle,
                emitted.trim_end()
            )
        }
        _ => bail!(
            "step {}: ASSERT_STDOUT did not contain '{}'",
            idx + 1,
            needle
        ),
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
    let (step_stdin, next_expose_stdin, step_stdout, step_stderr) =
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
    )
}

/// Resolve `WITH_IO` bindings against the active context handles, shared by
/// `with_io` and the `CALL` fast paths (`LET`-capture / `ASYNC` tasks) so a
/// `CALL` under `WITH_IO` layers observes identical stream wiring whether
/// it runs inline or for its return value.
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
)> {
    let mut step_stdin = CommandStdin::Null;
    let mut step_stdout = cx.out.clone();
    let mut step_stderr = cx.err.clone();
    let mut next_expose_stdin = false;
    let mut seen_stdin = false;
    let mut seen_stdout = false;
    let mut seen_stderr = false;
    let trigger = promotion_trigger(cmd, cx.state.inside_async);
    let direct = run_terminated(cmd);

    for binding in bindings {
        if let Some(target) = &binding.pipe {
            let pipe = resolve_pipe_name(cx, idx, target)?;
            cx.state.io.ensure_pipe_for(&pipe, trigger)?;
        }
        match binding.stream {
            IoStream::Stdin => {
                if seen_stdin {
                    bail!("step {}: WITH_IO declared stdin more than once", idx + 1);
                }
                seen_stdin = true;
                next_expose_stdin = true;
                step_stdin = if let Some(target) = &binding.pipe {
                    let pipe = resolve_pipe_name(cx, idx, target)?;
                    cx.state.io.resolve_stdin(idx, &pipe, direct)?
                } else {
                    cx.stdin.clone()
                };
            }
            IoStream::Stdout => {
                if seen_stdout {
                    bail!("step {}: WITH_IO declared stdout more than once", idx + 1);
                }
                seen_stdout = true;
                step_stdout = if let Some(target) = &binding.pipe {
                    let pipe = resolve_pipe_name(cx, idx, target)?;
                    Some(cx.state.io.resolve_stdout(idx, &pipe, direct)?)
                } else {
                    cx.out.clone()
                };
            }
            IoStream::Stderr => {
                if seen_stderr {
                    bail!("step {}: WITH_IO declared stderr more than once", idx + 1);
                }
                seen_stderr = true;
                step_stderr = if let Some(target) = &binding.pipe {
                    let pipe = resolve_pipe_name(cx, idx, target)?;
                    Some(cx.state.io.resolve_stderr(idx, &pipe, direct)?)
                } else {
                    cx.err.clone()
                };
            }
        }
    }

    Ok((step_stdin, next_expose_stdin, step_stdout, step_stderr))
}

/// Resolve a `WITH_IO` pipe endpoint to a live pipe name. Literals resolve
/// directly; `$var` must hold a `PIPE` value naming a registered pipe,
/// otherwise this is a step-numbered type error.
fn resolve_pipe_name<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    target: &PipeTarget,
) -> Result<String> {
    match target {
        PipeTarget::Name(name) => Ok(name.clone()),
        PipeTarget::Var(var) => match cx.state.get_var_typed(var) {
            Some((TypeKind::Pipe, Value::Pipe(name))) => {
                if cx.state.io.pipe_exists(&name) {
                    Ok(name)
                } else {
                    bail!(
                        "step {}: TypeMismatch: expected PIPE, got unregistered pipe ({name:?})",
                        idx + 1
                    );
                }
            }
            Some((kind, value)) => {
                bail!(
                    "step {}: TypeMismatch: expected PIPE, got {} ({:?})",
                    idx + 1,
                    kind.label(),
                    value
                );
            }
            None => {
                bail!("step {}: undeclared variable ${var}", idx + 1);
            }
        },
    }
}

/// If `cmd` is a `CALL` possibly nested under `WITH_IO` layers, return the
/// merged bindings (outermost first, inner wins per stream) plus the call
/// name and args. Used by `LET`-capture and `ASYNC` fast paths so
/// `WITH_IO [stdin=pipe:tx] CALL FOO()` binds the `RETURN` value instead
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

pub(super) fn exit<P: ProcessManager>(cx: &mut StepCtx<'_, P>, code: i32) -> Result<()> {
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
    key_type: Option<TypeKind>,
    val_var: &str,
    val_type: TypeKind,
    in_expr: &Expr,
    body: &[Step],
) -> Result<Flow> {
    use oxdock_parser::TypeKind;
    let iterable = super::args::evaluate_expr(in_expr, cx)?;
    let clean_val_var = val_var.trim_start_matches('$').to_string();

    match iterable {
        Value::List(items) => {
            for (i, item) in items.into_iter().enumerate() {
                // Each iteration is a scope (same rule as every other
                // block): loop vars live inside it, and ENV/WORKDIR/
                // WORKSPACE mutations revert on every iteration boundary.
                cx.state.push_scope();
                if let Some(idx_name) = key_var {
                    let clean_idx = idx_name.trim_start_matches('$').to_string();
                    let kt = key_type.unwrap_or(TypeKind::Int);
                    cx.state.declare_var(
                        clean_idx,
                        kt,
                        super::args::coerce_value(Value::Int(i as i64), kt, &*cx.state)?,
                    )?;
                }
                cx.state.declare_var(
                    clean_val_var.clone(),
                    val_type,
                    super::args::coerce_value(item, val_type, &*cx.state)?,
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
            Ok(Flow::Done)
        }
        Value::Map(map) => {
            let key_name = key_var.ok_or_else(|| {
                anyhow!("FOR loop over Map requires key and value bindings: FOR $k: STRING, $v: TYPE IN $map")
            })?;
            let clean_key_var = key_name.trim_start_matches('$').to_string();
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();

            // Map keys are strings: only a STRING key binding is valid here.
            if key_type.is_some_and(|kt| kt != TypeKind::String) {
                anyhow::bail!(
                    "FOR loop over MAP requires a STRING key variable, got {}",
                    key_type.map(|kt| kt.label()).unwrap_or("unknown"),
                );
            }
            for k in keys {
                let v = map[&k].clone();
                cx.state.push_scope();
                cx.state.declare_var(
                    clean_key_var.clone(),
                    TypeKind::String,
                    Value::String(k.clone()),
                )?;
                cx.state.declare_var(
                    clean_val_var.clone(),
                    val_type,
                    super::args::coerce_value(v, val_type, &*cx.state)?,
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
            Ok(Flow::Done)
        }
        other => bail!(
            "FOR loop requires a List or Map iterable, found {:?}",
            other
        ),
    }
}

/// Define a user function (`FUNC NAME($p: TYPE, ...) { ... }}).
/// Copy-on-write into a fresh registry Arc so `fork()` sharers keep the
/// old view, while `push_scope`/`pop_scope` snapshots revert nested
/// definitions on block exit.
pub(crate) fn define_func<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    name: &str,
    params: &[(String, TypeKind)],
    body: &[Step],
) -> Result<()> {
    let data = FuncDefData {
        params: params.to_vec(),
        body: body.to_vec(),
    };
    let mut next = (*cx.state.funcs).clone();
    next.insert(name.to_string(), data);
    cx.state.funcs = Arc::new(next);
    Ok(())
}

/// Invoke a function by UPPERCASE name and return its value.
/// Dispatch order: DSL `funcs` first, then `host_funcs` (FFI hook),
/// else `unknown function`. Args evaluate in the caller scope; params bind
/// with `declare_var` coercion before one body step runs. The body runs in
/// a fresh lexical scope (LET/ENV/WORKDIR revert; pipes and files persist).
/// `RETURN` inside yields the value; fallthrough yields `""`;
/// `BREAK`/`CONTINUE` escaping the body are boundary errors (they must not
/// reach a caller loop).
pub(crate) fn call_func_value<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    name: &str,
    args: &[Expr],
) -> Result<Value> {
    let mut arg_vals = Vec::with_capacity(args.len());
    for arg in args {
        arg_vals.push(super::args::evaluate_expr(arg, cx)?);
    }
    let Some(func) = cx.state.funcs.get(name).cloned() else {
        if let Some(host) = cx.state.host_funcs.get(name).cloned() {
            return host(arg_vals)
                .with_context(|| format!("step {}: host function `{name}` failed", idx + 1));
        }
        bail!("step {}: unknown function `{name}`", idx + 1);
    };
    if arg_vals.len() != func.params.len() {
        bail!(
            "step {}: CALL {name} expects {} argument(s), got {}",
            idx + 1,
            func.params.len(),
            arg_vals.len()
        );
    }
    if cx.state.call_depth >= MAX_CALL_DEPTH {
        bail!(
            "step {}: recursion depth limit exceeded in FUNC {name}",
            idx + 1
        );
    }
    cx.state.call_depth += 1;
    cx.state.push_scope();
    let outcome: Result<Value> = (|| {
        for ((pname, ptype), pval) in func.params.iter().zip(arg_vals) {
            cx.state.declare_var(pname.clone(), *ptype, pval)?;
        }
        let flow = super::steps::execute_steps(
            cx.state,
            cx.process,
            &func.body,
            cx.stdin.clone(),
            false,
            cx.out.clone(),
            cx.err.clone(),
            false,
        )?;
        match flow {
            Flow::Done => Ok(Value::String(String::new())),
            Flow::Return { value, .. } => Ok(value),
            Flow::Break { idx } => {
                bail!(
                    "step {}: BREAK cannot cross function boundary (in CALL {name})",
                    idx + 1
                );
            }
            Flow::Continue { idx } => {
                bail!(
                    "step {}: CONTINUE cannot cross function boundary (in CALL {name})",
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

/// Evaluate `RETURN <expr>` inside a function call. Outside any call
/// (including at top level or with no `CALL` frame on this thread) it is a
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
    decl_type: TypeKind,
    expr: &Expr,
) -> Result<()> {
    use oxdock_parser::TypeKind;
    let _ = TypeKind::String;
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
/// passes through so `WITH_IO [stdin=pipe:p]` still works). Captured bytes
/// never tee into the parent `ASSERT_STDOUT` windows. On command failure
/// nothing is bound.
///
/// When the captured command is `CALL NAME(...)`, no sink is installed:
/// the callee's stdout keeps the active routing (observable via
/// `ASSERT_STDOUT`/pipes) and the bound value is the function's `RETURN`
/// payload (or `""` on fallthrough), coerced to the declared type.
pub(crate) fn assign_capture<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    generation: usize,
    idx: usize,
    var: &str,
    decl_type: TypeKind,
    cmd: &StepKind,
) -> Result<Flow> {
    use std::sync::Arc;

    use super::capture::SpillBuffer;

    if let Some((bindings, name, args)) = extract_call(cmd) {
        // `CALL` (possibly under `WITH_IO` layers): no capture sink. The
        // callee's stdout keeps its routed streams (observable via
        // `ASSERT_STDOUT`/pipes); the bound value is the `RETURN` payload.
        if bindings
            .iter()
            .any(|b| b.stream == IoStream::Stdout && b.pipe.is_some())
        {
            bail!(
                "step {}: LET capture cannot use WITH_IO [stdout=pipe:...]; the capture binds the RETURN value",
                idx + 1
            );
        }
        if bindings.is_empty() {
            let value = call_func_value(cx, idx, name, args)?;
            let clean_var = var.trim_start_matches('$').to_string();
            cx.state.declare_var(clean_var, decl_type, value)?;
            return Ok(Flow::Done);
        }
        let (step_stdin, expose_stdin, step_stdout, step_stderr) =
            resolve_io_streams(cx, idx, &bindings, cmd)?;
        let snapshot_root = cx.state.fs.root().clone();
        let build_context = cx.state.fs.build_context().clone();
        // Reborrow state/process for the sub-context; `cx` is unused below.
        let state = &mut *cx.state;
        let process = &mut *cx.process;
        let mut sub_cx = super::steps::StepCtx {
            state,
            process,
            snapshot_root,
            build_context,
            stdin: step_stdin,
            expose_stdin,
            out: step_stdout,
            err: step_stderr,
        };
        let value = call_func_value(&mut sub_cx, idx, name, args)?;
        sub_cx
            .state
            .declare_var(var.trim_start_matches('$').to_string(), decl_type, value)?;
        return Ok(Flow::Done);
    }
    let sink = Arc::new(SpillBuffer::new());
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
        .declare_var(clean_var, decl_type, Value::String(text))?;
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
/// bindings), same-thread only. Nested `ASYNC` bodies run on other threads
/// with their own pins and are excluded; `Timeout`/`For`/`If`/`WithIo`
/// bodies run inline and are included. Only producers pin: a task that
/// only reads a pipe relies on EOF-from-detach to complete, so pinning it
/// would deadlock. Each entry pairs the pipe name with whether OS
/// promotion applies (OR-merged across occurrences).
fn collect_steps_producers(steps: &[Step], out: &mut Vec<(String, bool)>) {
    for step in steps {
        collect_kind_producers(&step.kind, out);
    }
}

fn collect_kind_producers(kind: &StepKind, out: &mut Vec<(String, bool)>) {
    match kind {
        StepKind::WithIo { bindings, cmd } => {
            let promote = promotion_trigger(cmd, true);
            for binding in bindings {
                match binding.stream {
                    IoStream::Stdout | IoStream::Stderr => {
                        if let Some(PipeTarget::Name(pipe)) = &binding.pipe {
                            match out.iter_mut().find(|(name, _)| name == pipe) {
                                Some(entry) => {
                                    entry.1 = entry.1 || promote;
                                }
                                None => {
                                    out.push((pipe.clone(), promote));
                                }
                            }
                        }
                        // Dynamic (`$var`) endpoints resolve against live
                        // state at pin time (see `pin_async_keepers`); they
                        // are invisible to this static walk by design.
                    }
                    IoStream::Stdin => {}
                }
            }
            collect_kind_producers(cmd, out);
        }
        StepKind::Timeout { body, .. } => collect_steps_producers(body, out),
        StepKind::For { body, .. } => collect_steps_producers(body, out),
        StepKind::While { body, .. } => collect_steps_producers(body, out),
        // Deferred (FUNC bodies) or dynamic (CALL targets unknown
        // statically) bodies run elsewhere or later with their own pins.
        StepKind::FuncDef { .. } | StepKind::Call { .. } => {}
        StepKind::If {
            then_body,
            else_ifs,
            else_body,
            ..
        } => {
            collect_steps_producers(then_body, out);
            for (_, branch) in else_ifs {
                collect_steps_producers(branch, out);
            }
            if let Some(body) = else_body {
                collect_steps_producers(body, out);
            }
        }
        StepKind::AsyncBlock { .. } | StepKind::AssignAsync { .. } => {}
        _ => {}
    }
}

/// Dynamic counterpart to `collect_kind_producers`: resolves `$var` pipe
/// endpoints against the spawning thread's state so `ASYNC` tasks that
/// produce to a variable-named pipe get the same keeper coverage as static
/// ones. Unresolvable names are skipped here (execution-time resolution
/// reports the real error); promotion never applies to dynamic endpoints.
fn collect_dynamic_producers<P: ProcessManager>(
    kind: &StepKind,
    state: &ExecState<P>,
    out: &mut Vec<(String, bool)>,
) {
    match kind {
        StepKind::WithIo { bindings, cmd } => {
            for binding in bindings {
                match binding.stream {
                    IoStream::Stdout | IoStream::Stderr => {
                        if let Some(PipeTarget::Var(var)) = &binding.pipe
                            && let Some((TypeKind::Pipe, Value::Pipe(name))) =
                                state.get_var_typed(var)
                            && !out.iter().any(|(n, _)| n == &name)
                        {
                            out.push((name.clone(), false));
                        }
                    }
                    IoStream::Stdin => {}
                }
            }
            collect_dynamic_producers(cmd, state, out);
        }
        StepKind::Timeout { body, .. }
        | StepKind::For { body, .. }
        | StepKind::While { body, .. } => {
            for step in body {
                collect_dynamic_producers(&step.kind, state, out);
            }
        }
        StepKind::If {
            then_body,
            else_ifs,
            else_body,
            ..
        } => {
            for step in then_body {
                collect_dynamic_producers(&step.kind, state, out);
            }
            for (_, branch) in else_ifs {
                for step in branch {
                    collect_dynamic_producers(&step.kind, state, out);
                }
            }
            if let Some(body) = else_body {
                for step in body {
                    collect_dynamic_producers(&step.kind, state, out);
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

/// Ensure every pipe an async `body` produces to exists (honoring OS
/// promotion) and pin a keeper slot on each script pipe, synchronously on
/// the spawning thread. Pins group by the top-level index of the final
/// producer step for each pipe: the worker drops a pipe's guard once that
/// step completes, so transient gaps between producers never signal EOF
/// while later consumer steps in the same task still observe it. Returns
/// `None` when the body produces to no script pipe.
fn pin_async_keepers<P: ProcessManager>(
    cx: &StepCtx<'_, P>,
    body: &[Step],
) -> Result<Option<super::state::KeeperExpiry>> {
    let mut last: HashMap<String, (bool, usize)> = HashMap::new();
    for (idx, step) in body.iter().enumerate() {
        let mut produced = Vec::new();
        collect_kind_producers(&step.kind, &mut produced);
        collect_dynamic_producers(&step.kind, cx.state, &mut produced);
        for (name, promote) in produced {
            let entry = last.entry(name).or_insert((false, 0));
            entry.0 = entry.0 || promote;
            entry.1 = idx;
        }
    }
    let mut by_index: HashMap<usize, Vec<(String, bool)>> = HashMap::new();
    for (name, (promote, idx)) in last {
        by_index.entry(idx).or_default().push((name, promote));
    }
    let mut map: HashMap<usize, Vec<KeeperGuard>> = HashMap::new();
    for (idx, specs) in &by_index {
        for (name, promote) in specs {
            cx.state.io.ensure_pipe_for(name, *promote)?;
            if let Some(guard) = cx.state.io.pin_keeper(name)? {
                map.entry(*idx).or_default().push(guard);
            }
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

pub(crate) fn dispatch_assert_file<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::AssertFile {
        hash,
        path,
        contents,
    } = step
    else {
        unreachable!()
    };
    let path_resolved = super::args::resolve_arg(path, cx)?;
    let contents_resolved = super::args::resolve_arg_opt(contents, cx)?;
    assert_file(cx, 0, hash, &path_resolved, contents_resolved.as_deref())
}

pub(crate) fn dispatch_assert_dir<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::AssertDir(arg) = step else {
        unreachable!()
    };
    let path = super::args::resolve_arg(arg, cx)?;
    assert_dir(cx, 0, &path)
}

pub(crate) fn dispatch_assert_absent<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::AssertAbsent(arg) = step else {
        unreachable!()
    };
    let path = super::args::resolve_arg(arg, cx)?;
    assert_absent(cx, 0, &path)
}

pub(crate) fn dispatch_assert_stdout<P: ProcessManager>(
    step: &StepKind,
    cx: &mut StepCtx<'_, P>,
) -> Result<()> {
    let StepKind::AssertStdout(arg) = step else {
        unreachable!()
    };
    let needle = super::args::resolve_arg(arg, cx)?;
    assert_stdout(cx, 0, 0, 0, &needle)
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
        *key_type,
        var,
        *var_type,
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
    assign(cx, var, *decl_type, expr)
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
    decl_type: TypeKind,
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
    // `WITH_IO [stdout=pipe:tx] ASYNC ...` bindings) can never observe a
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
    let sink = std::sync::Arc::new(super::capture::SpillBuffer::new());
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
            let snapshot_root = child_state.fs.root().clone();
            let build_context = child_state.fs.build_context().clone();
            let mut child_cx = super::steps::StepCtx {
                state: &mut child_state,
                process: &mut child_process,
                snapshot_root,
                build_context,
                stdin,
                expose_stdin,
                out,
                err,
            };
            // Apply call-site bindings (e.g. stdin pipes) like the inline
            // path; stdout keeps the task sink (parse rejects stdout pipes).
            let value = if bindings.is_empty() {
                call_func_value(&mut child_cx, 0, &name, &args)?
            } else {
                let (task_stdin, task_expose, task_out, task_err) =
                    resolve_io_streams(&mut child_cx, 0, &bindings, &body[0].kind)?;
                let state = &mut *child_cx.state;
                let process = &mut *child_cx.process;
                let mut sub_cx = super::steps::StepCtx {
                    state,
                    process,
                    snapshot_root: child_cx.snapshot_root.clone(),
                    build_context: child_cx.build_context.clone(),
                    stdin: task_stdin,
                    expose_stdin: task_expose,
                    out: task_out,
                    err: task_err,
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
    // Published to the child above so single-CALL tasks can store their
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
        .declare_var(var.to_string(), decl_type, Value::TaskHandle(task_id))?;
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
    let Value::TaskHandle(task_id) = val else {
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
    out_type: TypeKind,
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
        cx.state
            .declare_var(out_var.trim_start_matches('$').to_string(), out_type, value)?;
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
        Value::String(text),
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
    let Value::TaskHandle(task_id) = val else {
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
    dispatch_assign_async(var, *decl_type, body, cx)
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
        *decl_type,
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
    dispatch_await_capture(out_var, *out_type, task_var, cx)
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

/// Dispatch `TIMEOUT <duration> <body>` — run `body` on the current thread
/// with a deadline. If the deadline elapses first, cancel the state,
/// SIGKILL the active foreground process (when one is registered), and
/// return a deadline error wrapping any body error.
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
