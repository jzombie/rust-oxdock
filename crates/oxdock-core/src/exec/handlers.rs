use anyhow::{Context, Result, anyhow, bail};
use std::sync::Arc;

use oxdock_fs::EntryKind;
use oxdock_parser::{Expr, IoBinding, IoStream, Step, StepKind, Value, WorkspaceTarget};
use oxdock_process::{
    BackgroundHandle, CommandOptions, CommandResult, CommandStderr, CommandStdout, ProcessManager,
};
use sha2::{Digest, Sha256};

use super::fs_ops::{canonical_cwd, copy_entry, hash_path};
use super::io::write_stdout;
use super::steps::StepCtx;

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
        None
    };

    let inherit_override = cx
        .state
        .envs
        .get("OXDOCK_INHERIT_STDOUT")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if std::env::var("OXBOOK_DEBUG").is_ok() {
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

    let mut options = CommandOptions::foreground();
    options.stdin = step_stdin;
    options.stdout = stdout_mode;
    options.stderr = stderr_mode;
    match cx
        .process
        .run_command(&ctx, cmd, options)
        .with_context(|| format!("step {}: RUN {}", idx + 1, cmd))?
    {
        CommandResult::Completed => Ok(()),
        CommandResult::Captured(_) => {
            bail!("step {}: RUN {} unexpectedly captured output", idx + 1, cmd)
        }
        CommandResult::Background(_) => {
            bail!("step {}: RUN {} returned background handle", idx + 1, cmd)
        }
    }
}

pub(super) fn echo<P: ProcessManager>(cx: &mut StepCtx<'_, P>, msg: &str) -> Result<()> {
    write_stdout(cx.out.clone(), |writer| {
        writeln!(writer, "{}", msg)?;
        Ok(())
    })?;
    Ok(())
}

pub(super) fn run_bg<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    cmd: &str,
) -> Result<()> {
    let ctx = cx.state.command_ctx()?;
    let step_stdin = if cx.expose_stdin {
        cx.stdin.clone()
    } else {
        None
    };
    let stdout_mode = cx
        .out
        .clone()
        .map(|handle| handle.to_stdout())
        .unwrap_or(CommandStdout::Inherit);
    let stderr_mode = cx
        .err
        .clone()
        .map(|handle| handle.to_stderr())
        .unwrap_or(CommandStderr::Inherit);
    let mut options = CommandOptions::background();
    options.stdin = step_stdin;
    options.stdout = stdout_mode;
    options.stderr = stderr_mode;
    match cx
        .process
        .run_command(&ctx, cmd, options)
        .with_context(|| format!("step {}: RUN_BG {}", idx + 1, cmd))?
    {
        CommandResult::Background(handle) => {
            cx.state.bg_children.push(handle);
            Ok(())
        }
        CommandResult::Completed => {
            bail!("step {}: RUN_BG {} finished synchronously", idx + 1, cmd)
        }
        CommandResult::Captured(_) => {
            bail!(
                "step {}: RUN_BG {} attempted to capture output",
                idx + 1,
                cmd
            )
        }
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
    let data = if let Some(path) = path_opt {
        let target = cx
            .state
            .fs
            .resolve_read(&cx.state.cwd, path)
            .with_context(|| format!("step {}: READ {}", idx + 1, path))?;
        cx.state
            .fs
            .read_file(&target)
            .with_context(|| format!("failed to read {}", target.display()))?
    } else {
        let mut buf = Vec::new();
        if let Some(input_stream) = cx.stdin.clone()
            && let Ok(mut guard) = input_stream.lock()
        {
            guard
                .read_to_end(&mut buf)
                .context("failed to read from stdin")?;
        }

        buf
    };
    write_stdout(cx.out.clone(), |writer| {
        writer
            .write_all(&data)
            .context("failed to write to output")?;
        Ok(())
    })?;
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
        let Some(input_stream) = cx.stdin.clone() else {
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

pub(super) fn raw_write<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    path: &str,
    contents: &str,
) -> Result<()> {
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, path)
        .with_context(|| format!("step {}: RAW_WRITE {}", idx + 1, path))?;
    cx.state
        .fs
        .ensure_parent_dir(&target)
        .with_context(|| format!("failed to create parent for {}", target.display()))?;
    cx.state
        .fs
        .write_file(&target, contents.as_bytes())
        .with_context(|| format!("failed to write {}", target.display()))?;
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
        let Some(input_stream) = cx.stdin.clone() else {
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
            let Some(input_stream) = cx.stdin.clone() else {
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
    if let Some(input_stream) = cx.stdin.clone() {
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

pub(super) fn with_io<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    generation: usize,
    idx: usize,
    bindings: &[IoBinding],
    cmd: &StepKind,
) -> Result<()> {
    let mut step_stdin = None;
    let mut step_stdout = cx.out.clone();
    let mut step_stderr = cx.err.clone();
    let mut next_expose_stdin = false;
    let mut seen_stdin = false;
    let mut seen_stdout = false;
    let mut seen_stderr = false;

    for binding in bindings {
        if let Some(pipe) = &binding.pipe {
            cx.state.io.ensure_script_pipe(pipe);
        }
        match binding.stream {
            IoStream::Stdin => {
                if seen_stdin {
                    bail!("step {}: WITH_IO declared stdin more than once", idx + 1);
                }
                seen_stdin = true;
                next_expose_stdin = true;
                step_stdin = if let Some(pipe) = &binding.pipe {
                    Some(cx.state.io.input_pipe(pipe).ok_or_else(|| {
                        anyhow!(
                            "step {}: WITH_IO stdin pipe '{}' is undefined",
                            idx + 1,
                            pipe
                        )
                    })?)
                } else {
                    cx.stdin.clone()
                };
            }
            IoStream::Stdout => {
                if seen_stdout {
                    bail!("step {}: WITH_IO declared stdout more than once", idx + 1);
                }
                seen_stdout = true;
                step_stdout = if let Some(pipe) = &binding.pipe {
                    Some(
                        cx.state
                            .io
                            .output_pipe_stdout(pipe)
                            .ok_or_else(|| {
                                anyhow!(
                                    "step {}: WITH_IO stdout pipe '{}' is undefined",
                                    idx + 1,
                                    pipe
                                )
                            })?
                            .to_stream_handle(),
                    )
                } else {
                    cx.out.clone()
                };
            }
            IoStream::Stderr => {
                if seen_stderr {
                    bail!("step {}: WITH_IO declared stderr more than once", idx + 1);
                }
                seen_stderr = true;
                step_stderr = if let Some(pipe) = &binding.pipe {
                    Some(
                        cx.state
                            .io
                            .output_pipe_stderr(pipe)
                            .ok_or_else(|| {
                                anyhow!(
                                    "step {}: WITH_IO stderr pipe '{}' is undefined",
                                    idx + 1,
                                    pipe
                                )
                            })?
                            .to_stream_handle(),
                    )
                } else {
                    cx.err.clone()
                };
            }
        }
    }

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
    )?;
    Ok(())
}

pub(super) fn exit<P: ProcessManager>(cx: &mut StepCtx<'_, P>, code: i32) -> Result<()> {
    for child in cx.state.bg_children.iter_mut() {
        if child.try_wait()?.is_none() {
            let _ = child.kill();
            let _ = child.try_wait();
        }
    }
    cx.state.bg_children.clear();
    bail!("EXIT requested with code {}", code);
}

pub(super) fn for_loop<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    key_var: Option<&str>,
    val_var: &str,
    in_expr: &Expr,
    body: &[Step],
) -> Result<()> {
    let iterable = super::args::evaluate_expr(in_expr, cx)?;
    let clean_val_var = val_var.trim_start_matches('$').to_string();

    match iterable {
        Value::List(items) => {
            for (i, item) in items.into_iter().enumerate() {
                cx.state.push_var_scope();
                if let Some(idx_name) = key_var {
                    let clean_idx = idx_name.trim_start_matches('$').to_string();
                    cx.state.set_var(clean_idx, Value::Int(i as i64));
                }
                cx.state.set_var(clean_val_var.clone(), item);

                let saved_cwd = cx.state.cwd.clone();
                let saved_root = cx.state.fs.root().clone();
                let saved_envs = Arc::clone(&cx.state.envs);

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

                cx.state.cwd = saved_cwd;
                cx.state.fs.set_root(&saved_root);
                cx.state.envs = saved_envs;

                cx.state.pop_var_scope();
                res?;
            }
            Ok(())
        }
        Value::Map(map) => {
            let key_name = key_var.ok_or_else(|| {
                anyhow!("FOR loop over Map requires key and value bindings: FOR $k, $v IN $map")
            })?;
            let clean_key_var = key_name.trim_start_matches('$').to_string();
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();

            for k in keys {
                let v = map[&k].clone();
                cx.state.push_var_scope();
                cx.state.set_var(clean_key_var.clone(), Value::String(k.clone()));
                cx.state.set_var(clean_val_var.clone(), v);

                let saved_cwd = cx.state.cwd.clone();
                let saved_root = cx.state.fs.root().clone();
                let saved_envs = Arc::clone(&cx.state.envs);

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

                cx.state.cwd = saved_cwd;
                cx.state.fs.set_root(&saved_root);
                cx.state.envs = saved_envs;

                cx.state.pop_var_scope();
                res?;
            }
            Ok(())
        }
        other => bail!("FOR loop requires a List or Map iterable, found {:?}", other),
    }
}

pub(super) fn assign<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    var: &str,
    expr: &Expr,
) -> Result<()> {
    let value = super::args::evaluate_expr(expr, cx)?;
    let clean_var = var.trim_start_matches('$').to_string();
    cx.state.set_var(clean_var, value);
    Ok(())
}

pub(super) fn if_then<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    cond: &Expr,
    then_body: &[Step],
    else_ifs: &[(Box<Expr>, Vec<Step>)],
    else_body: &Option<Vec<Step>>,
) -> Result<()> {
    let val = super::args::evaluate_expr(cond, cx)?;
    if super::args::is_truthy(&val)? {
        return super::steps::execute_steps(
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
            return super::steps::execute_steps(
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
        super::steps::execute_steps(
            cx.state,
            cx.process,
            body,
            cx.stdin.clone(),
            false,
            cx.out.clone(),
            cx.err.clone(),
            false,
        )?;
    }
    Ok(())
}
