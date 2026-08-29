use anyhow::{Context, Result, anyhow, bail};
use std::sync::Arc;

use oxdock_fs::EntryKind;
use oxdock_parser::{Expr, IoBinding, IoStream, Step, StepKind, TemplateString, Value, WorkspaceTarget};
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
    // Resolve every key against the pre-mutation environment first; no
    // `CommandContext` (or any other `Arc` clone of `state.envs`) is alive
    // when we mutate, keeping `Arc::make_mut` O(1) in the common case.
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
    path: &TemplateString,
) -> Result<()> {
    let rendered = super::expand_template(path, cx);
    cx.state.cwd = cx
        .state
        .fs
        .resolve_workdir(&cx.state.cwd, &rendered)
        .with_context(|| format!("step {}: WORKDIR {}", idx + 1, rendered))?;
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
    value: &TemplateString,
) -> Result<()> {
    let rendered = super::expand_template(value, cx);
    Arc::make_mut(&mut cx.state.envs).insert(key.to_owned(), rendered);
    Ok(())
}

pub(super) fn run<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    cmd: &TemplateString,
) -> Result<()> {
    let ctx = cx.state.command_ctx()?;
    let rendered = super::expand_template(cmd, cx);
    let step_stdin = if cx.expose_stdin {
        cx.stdin.clone()
    } else {
        None
    };

    // Check for an environment variable that forces stdout inheritance.
    // This is useful when we want to bypass output capturing (e.g. for build steps)
    // and stream directly to the terminal, even if a capture stream was provided.
    let inherit_override = cx
        .state
        .envs
        .get("OXDOCK_INHERIT_STDOUT")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if std::env::var("OXBOOK_DEBUG").is_ok() {
        eprintln!(
            "DEBUG: step RUN {} inherit_override={}",
            rendered, inherit_override
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
        .run_command(&ctx, &rendered, options)
        .with_context(|| format!("step {}: RUN {}", idx + 1, rendered))?
    {
        CommandResult::Completed => Ok(()),
        CommandResult::Captured(_) => {
            bail!(
                "step {}: RUN {} unexpectedly captured output",
                idx + 1,
                rendered
            )
        }
        CommandResult::Background(_) => {
            bail!(
                "step {}: RUN {} returned background handle",
                idx + 1,
                rendered
            )
        }
    }
}

pub(super) fn echo<P: ProcessManager>(cx: &mut StepCtx<'_, P>, msg: &TemplateString) -> Result<()> {
    let rendered = super::expand_template(msg, cx);
    write_stdout(cx.out.clone(), |writer| {
        writeln!(writer, "{}", rendered)?;
        Ok(())
    })?;
    Ok(())
}

pub(super) fn run_bg<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    cmd: &TemplateString,
) -> Result<()> {
    let ctx = cx.state.command_ctx()?;
    let rendered = super::expand_template(cmd, cx);
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
        .run_command(&ctx, &rendered, options)
        .with_context(|| format!("step {}: RUN_BG {}", idx + 1, rendered))?
    {
        CommandResult::Background(handle) => {
            cx.state.bg_children.push(handle);
            Ok(())
        }
        CommandResult::Completed => {
            bail!(
                "step {}: RUN_BG {} finished synchronously",
                idx + 1,
                rendered
            )
        }
        CommandResult::Captured(_) => {
            bail!(
                "step {}: RUN_BG {} attempted to capture output",
                idx + 1,
                rendered
            )
        }
    }
}

pub(super) fn copy<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    from_current_workspace: bool,
    from: &TemplateString,
    to: &TemplateString,
) -> Result<()> {
    let from_rendered = super::expand_template(from, cx);
    let to_rendered = super::expand_template(to, cx);
    let from_abs = if from_current_workspace {
        cx.state
            .fs
            .resolve_copy_source_from_workspace(&from_rendered)
            .with_context(|| format!("step {}: COPY {} {}", idx + 1, from_rendered, to_rendered))?
    } else {
        cx.state
            .fs
            .resolve_copy_source(&from_rendered)
            .with_context(|| format!("step {}: COPY {} {}", idx + 1, from_rendered, to_rendered))?
    };
    let to_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, &to_rendered)
        .with_context(|| format!("step {}: COPY {} {}", idx + 1, from_rendered, to_rendered))?;
    copy_entry(cx.state.fs.as_ref(), &from_abs, &to_abs)
        .with_context(|| format!("step {}: COPY {} {}", idx + 1, from_rendered, to_rendered))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn copy_git<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    rev: &TemplateString,
    from: &TemplateString,
    to: &TemplateString,
    include_dirty: bool,
) -> Result<()> {
    let rev_rendered = super::expand_template(rev, cx);
    let from_rendered = super::expand_template(from, cx);
    let to_rendered = super::expand_template(to, cx);
    let to_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, &to_rendered)
        .with_context(|| {
            format!(
                "step {}: COPY_GIT {} {} {}",
                idx + 1,
                rev_rendered,
                from_rendered,
                to_rendered
            )
        })?;
    cx.state
        .fs
        .copy_from_git(&rev_rendered, &from_rendered, &to_abs, include_dirty)
        .with_context(|| {
            format!(
                "step {}: COPY_GIT {} {} {}",
                idx + 1,
                rev_rendered,
                from_rendered,
                to_rendered
            )
        })?;
    Ok(())
}

pub(super) fn hash_sha256<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    path: &TemplateString,
) -> Result<()> {
    let rendered = super::expand_template(path, cx);
    let target = cx
        .state
        .fs
        .resolve_read(&cx.state.cwd, &rendered)
        .with_context(|| format!("step {}: HASH_SHA256 {}", idx + 1, rendered))?;
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
    from: &TemplateString,
    to: &TemplateString,
) -> Result<()> {
    let from_rendered = super::expand_template(from, cx);
    let to_rendered = super::expand_template(to, cx);
    let to_abs = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, &to_rendered)
        .with_context(|| {
            format!(
                "step {}: SYMLINK {} {}",
                idx + 1,
                from_rendered,
                to_rendered
            )
        })?;
    let from_abs = cx
        .state
        .fs
        .resolve_copy_source(&from_rendered)
        .with_context(|| {
            format!(
                "step {}: SYMLINK {} {}",
                idx + 1,
                from_rendered,
                to_rendered
            )
        })?;
    cx.state.fs.symlink(&from_abs, &to_abs).with_context(|| {
        format!(
            "step {}: SYMLINK {} {}",
            idx + 1,
            from_rendered,
            to_rendered
        )
    })?;
    Ok(())
}

pub(super) fn mkdir<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    path: &TemplateString,
) -> Result<()> {
    let rendered = super::expand_template(path, cx);
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, &rendered)
        .with_context(|| format!("step {}: MKDIR {}", idx + 1, rendered))?;
    cx.state
        .fs
        .create_dir_all(&target)
        .with_context(|| format!("failed to create dir {}", target.display()))?;
    Ok(())
}

pub(super) fn ls<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    arg: &Option<TemplateString>,
) -> Result<()> {
    let target_dir = if let Some(p) = arg {
        let rendered = super::expand_template(p, cx);
        cx.state
            .fs
            .resolve_read(&cx.state.cwd, &rendered)
            .with_context(|| format!("step {}: LS {}", idx + 1, rendered))?
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
    // Print the canonical (physical) current working directory to stdout.
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
    path_opt: &Option<TemplateString>,
) -> Result<()> {
    let data = if let Some(path) = path_opt {
        let rendered = super::expand_template(path, cx);
        let target = cx
            .state
            .fs
            .resolve_read(&cx.state.cwd, &rendered)
            .with_context(|| format!("step {}: READ {}", idx + 1, rendered))?;
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
    path: &TemplateString,
    contents: &Option<TemplateString>,
) -> Result<()> {
    let path_rendered = super::expand_template(path, cx);
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, &path_rendered)
        .with_context(|| format!("step {}: WRITE {}", idx + 1, path_rendered))?;
    cx.state
        .fs
        .ensure_parent_dir(&target)
        .with_context(|| format!("failed to create parent for {}", target.display()))?;
    if let Some(body) = contents {
        let rendered = super::expand_template(body, cx);
        cx.state
            .fs
            .write_file(&target, rendered.as_bytes())
            .with_context(|| format!("failed to write {}", target.display()))?;
    } else {
        let Some(input_stream) = cx.stdin.clone() else {
            bail!(
                "step {}: WRITE {} requires stdin (use WITH_IO [stdin=...] WRITE)",
                idx + 1,
                path_rendered
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
    path: &TemplateString,
    contents: &str,
) -> Result<()> {
    let path_rendered = super::expand_template(path, cx);
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, &path_rendered)
        .with_context(|| format!("step {}: RAW_WRITE {}", idx + 1, path_rendered))?;
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
    path: &TemplateString,
    contents: &Option<TemplateString>,
) -> Result<()> {
    let path_rendered = super::expand_template(path, cx);
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, &path_rendered)
        .with_context(|| format!("step {}: APPEND {}", idx + 1, path_rendered))?;
    cx.state
        .fs
        .ensure_parent_dir(&target)
        .with_context(|| format!("failed to create parent for {}", target.display()))?;
    if let Some(body) = contents {
        let rendered = super::expand_template(body, cx);
        cx.state
            .fs
            .append_file(&target, rendered.as_bytes())
            .with_context(|| format!("failed to append to {}", target.display()))?;
    } else {
        let Some(input_stream) = cx.stdin.clone() else {
            bail!(
                "step {}: APPEND {} requires stdin (use WITH_IO [stdin=...] APPEND)",
                idx + 1,
                path_rendered
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
    path_opt: &Option<TemplateString>,
    overrides: &[(String, TemplateString)],
) -> Result<()> {
    let ctx = cx.state.command_ctx()?;

    // Resolve override values
    let resolved_overrides: Vec<(String, String)> = overrides
        .iter()
        .map(|(k, v)| (k.clone(), super::expand_template(v, cx)))
        .collect();

    let mut expander = oxdock_process::StreamingExpand::new(&resolved_overrides, ctx.envs());
    let mut out_buf = Vec::with_capacity(super::io::CHUNK_SIZE);

    write_stdout(cx.out.clone(), |w| {
        if let Some(path) = path_opt {
            // Streaming file read
            let rendered = super::expand_template(path, cx);
            let target = cx
                .state
                .fs
                .resolve_read(&cx.state.cwd, &rendered)
                .with_context(|| format!("step {}: EXPAND {}", idx + 1, rendered))?;
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
            // Streaming stdin — error if no stdin available
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

        // Flush remaining buffer (incomplete placeholders → literals)
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
    path: &TemplateString,
    contents: &Option<TemplateString>,
) -> Result<()> {
    let rendered = super::expand_template(path, cx);
    let target = cx
        .state
        .fs
        .resolve_read(&cx.state.cwd, &rendered)
        .with_context(|| format!("step {}: ASSERT_FILE {}", idx + 1, rendered))?;
    if !matches!(cx.state.fs.entry_kind(&target)?, EntryKind::File) {
        bail!("step {}: ASSERT_FILE {} is not a file", idx + 1, rendered);
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
                rendered,
                expected,
                actual
            );
        }
        return Ok(());
    }
    if let Some(body) = contents {
        let expected = super::expand_template(body, cx);
        let actual = cx.state.fs.read_file(&target).with_context(|| {
            format!(
                "step {}: ASSERT_FILE {} could not be read",
                idx + 1,
                rendered
            )
        })?;
        if actual != expected.as_bytes() {
            bail!(
                "step {}: ASSERT_FILE content mismatch for {}\nexpected: {:?}\nactual:   {:?}",
                idx + 1,
                rendered,
                expected,
                String::from_utf8_lossy(&actual)
            );
        }
    }
    Ok(())
}

pub(super) fn assert_dir<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    path: &TemplateString,
) -> Result<()> {
    let rendered = super::expand_template(path, cx);
    let target = cx
        .state
        .fs
        .resolve_read(&cx.state.cwd, &rendered)
        .with_context(|| format!("step {}: ASSERT_DIR {}", idx + 1, rendered))?;
    if !matches!(cx.state.fs.entry_kind(&target)?, EntryKind::Dir) {
        bail!(
            "step {}: ASSERT_DIR {} is not a directory",
            idx + 1,
            rendered
        );
    }
    Ok(())
}

pub(super) fn assert_absent<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    path: &TemplateString,
) -> Result<()> {
    let rendered = super::expand_template(path, cx);
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, &rendered)
        .with_context(|| format!("step {}: ASSERT_ABSENT {}", idx + 1, rendered))?;
    // Containment was already enforced by `resolve_write`; a lookup failure
    // therefore means the path is absent, which is this command's success
    // condition.
    if cx.state.fs.entry_kind(&target).is_ok() {
        bail!("step {}: ASSERT_ABSENT {} exists", idx + 1, rendered);
    }
    Ok(())
}

pub(super) fn assert_stdout<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    needle: &TemplateString,
) -> Result<()> {
    let rendered = super::expand_template(needle, cx);

    // Mode 1: Piped stdin — actively consume stream and check
    if let Some(input_stream) = cx.stdin.clone() {
        let mut guard = input_stream
            .lock()
            .map_err(|_| anyhow!("failed to lock stdin for ASSERT_STDOUT"))?;
        let mut window = super::io::SlidingWindow::new(rendered.as_bytes().to_vec());
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
            // Tee to downstream — preserve pipeline continuity
            super::io::write_stdout(cx.out.clone(), |w| {
                w.write_all(&buf[..n])?;
                Ok(())
            })?;
        }
        // If stdin had data, the local window is authoritative — fail immediately
        if read_any {
            if window.matched {
                return Ok(());
            }
            let emitted = String::from_utf8_lossy(&window.ring_buffer()).into_owned();
            bail!(
                "step {}: ASSERT_STDOUT did not contain '{}'; emitted:\n{}",
                idx + 1,
                rendered,
                emitted.trim_end()
            );
        }
        // If stdin was empty (e.g. /dev/null subprocess), fall through to step-scope mode
    }

    // Mode 2: Step scope — check pre-registered window
    let windows = cx
        .state
        .assert_windows
        .lock()
        .map_err(|_| anyhow!("assert_windows poisoned"))?;
    match windows.get(&idx) {
        Some(w) if w.matched => Ok(()),
        Some(w) => {
            // Window exists but didn't match — include ring buffer content for debugging
            let emitted = String::from_utf8_lossy(&w.ring_buffer()).into_owned();
            bail!(
                "step {}: ASSERT_STDOUT did not contain '{}'; emitted:\n{}",
                idx + 1,
                rendered,
                emitted.trim_end()
            )
        }
        _ => bail!(
            "step {}: ASSERT_STDOUT did not contain '{}'",
            idx + 1,
            rendered
        ),
    }
}

pub(super) fn with_io<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    bindings: &[IoBinding],
    cmd: &StepKind,
) -> Result<()> {
    let inner_step = Step {
        guard: None,
        kind: cmd.clone(),
        scope_enter: 0,
        scope_exit: 0,
    };
    let steps = vec![inner_step];

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

    super::steps::execute_steps(
        cx.state,
        cx.process,
        &steps,
        step_stdin,
        next_expose_stdin,
        step_stdout,
        step_stderr,
        false,
    )?;
    Ok(())
}

pub(super) fn exit<P: ProcessManager>(cx: &mut StepCtx<'_, P>, code: i32) -> Result<()> {
    for child in cx.state.bg_children.iter_mut() {
        if child.try_wait()?.is_none() {
            let _ = child.kill();
            // Reap without joining IO pump threads (see `check_bg`).
            let _ = child.try_wait();
        }
    }
    cx.state.bg_children.clear();
    bail!("EXIT requested with code {}", code);
}

pub(super) fn for_loop<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    var: &str,
    in_expr: &Expr,
    body: &[Step],
) -> Result<()> {
    let iterable = evaluate_expr(in_expr, cx)?;
    // Strip '$' prefix for storage key consistency
    let clean_var = var.trim_start_matches('$').to_string();

    if let Value::List(items) = iterable {
        for item in items {
            cx.state.push_var_scope();
            cx.state.set_var(clean_var.clone(), Value::String(item));

            // Snapshot cwd, root, and envs so loop body mutations are isolated
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

            // Restore filesystem and environment state
            cx.state.cwd = saved_cwd;
            cx.state.fs.set_root(&saved_root);
            cx.state.envs = saved_envs;

            cx.state.pop_var_scope();
            res?; // Check error AFTER pop_scope to prevent frame leak
        }
        Ok(())
    } else {
        bail!("FOR loop requires a list iterable")
    }
}

pub(super) fn assign<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    var: &str,
    expr: &Expr,
) -> Result<()> {
    let value = evaluate_expr(expr, cx)?;
    // Strip '$' prefix for storage key consistency
    let clean_var = var.trim_start_matches('$').to_string();
    cx.state.set_var(clean_var, value);
    Ok(())
}

fn evaluate_expr<P: ProcessManager>(expr: &Expr, cx: &mut StepCtx<'_, P>) -> Result<Value> {
    match expr {
        Expr::Literal(Value::String(s)) => {
            // Resolve embedded $variable references in string literals
            Ok(Value::String(resolve_dollar_vars(s, cx.state)))
        }
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Var(name) => cx
            .state
            .get_var(name)
            .ok_or_else(|| anyhow!("undefined variable ${name}")),
        Expr::List(items) => {
            let mut result = Vec::new();
            for item in items {
                match evaluate_expr(item, cx)? {
                    Value::String(s) => result.push(s),
                    other => bail!("list items must evaluate to strings, got: {:?}", other),
                }
            }
            Ok(Value::List(result))
        }
        Expr::Call { name, args } => match name.as_str() {
            "GLOB" => evaluate_glob(args, cx),
            _ => bail!("unknown function {name}"),
        },
    }
}

fn evaluate_glob<P: ProcessManager>(args: &[Expr], cx: &mut StepCtx<'_, P>) -> Result<Value> {
    if args.is_empty() {
        bail!("GLOB requires a pattern argument");
    }

    let pattern_val = evaluate_expr(&args[0], cx)?;
    let raw_pattern = match pattern_val {
        Value::String(s) => s,
        _ => bail!("GLOB pattern argument must evaluate to a string"),
    };

    // Normalize backslashes to forward slashes before escaping (Windows paths)
    let norm_root = cx.state.fs.root().as_path().to_string_lossy().replace('\\', "/");
    let escaped_root = glob::Pattern::escape(&norm_root);
    let full_pattern = format!("{}/{}", escaped_root, raw_pattern.trim_start_matches('/'));

    let root = cx.state.fs.root().as_path().to_path_buf();
    let mut entries: Vec<String> = glob::glob(&full_pattern)?
        .filter_map(|e| e.ok())
        // Strip root prefix to return workspace-relative paths
        .filter_map(|p| {
            p.strip_prefix(&root)
                .ok()
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        })
        .collect();

    entries.sort(); // Deterministic order
    Ok(Value::List(entries))
}

pub(crate) fn resolve_dollar_vars<P: ProcessManager>(input: &str, state: &super::state::ExecState<P>) -> String {
    let mut result = String::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            let mut name = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_alphanumeric() || ch == '_' {
                    name.push(ch);
                    chars.next();
                } else {
                    break;
                }
            }
            // Lookup uses clean name (without '$' prefix)
            if let Some(value) = state.get_var(&name) {
                match value {
                    Value::String(s) => result.push_str(&s),
                    Value::List(items) => result.push_str(&items.join(" ")),
                    Value::Bool(b) => result.push_str(&b.to_string()),
                }
            } else {
                result.push('$');
                result.push_str(&name);
            }
        } else {
            result.push(c);
        }
    }
    result
}
