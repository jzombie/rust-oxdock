use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::process::Command as ProcessCommand;
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::process::Stdio;

use oxdock_fs::PolicyPath;

use crate::child::ChildHandle;
use crate::contract::{
    BackgroundHandle, CommandContext, CommandMode, CommandOptions, CommandResult, CommandStderr,
    CommandStdout, ProcessManager, SharedInput, SharedOutput,
};
use crate::shell::shell_cmd;

/// Default process manager that shells out using the system shell.
#[derive(Clone, Default)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub struct ShellProcessManager;

impl ProcessManager for ShellProcessManager {
    type Handle = ChildHandle;

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn run_command(
        &mut self,
        ctx: &CommandContext,
        script: &str,
        options: CommandOptions,
    ) -> Result<CommandResult<Self::Handle>> {
        if std::env::var_os("OXBOOK_DEBUG").is_some() {
            eprintln!("oxbook run_command: {script}");
        }
        let mut command = shell_cmd(script);
        apply_ctx(&mut command, ctx);
        let CommandOptions {
            mode,
            stdin,
            stdout,
            stderr,
        } = options;

        let (stdout_stream, capture_buf) = match stdout {
            CommandStdout::Inherit => (None, None),
            CommandStdout::Stream(stream) => (Some(stream), None),
            CommandStdout::Capture => {
                if matches!(mode, CommandMode::Background) {
                    bail!("cannot capture stdout for background command");
                }
                let buf = Arc::new(Mutex::new(Vec::new()));
                let writer: SharedOutput = buf.clone();
                (Some(writer), Some(buf))
            }
        };

        let stderr_stream = match stderr {
            CommandStderr::Inherit => None,
            CommandStderr::Stream(stream) => Some(stream),
        };

        let need_null_stdin = stdin.is_none();
        if need_null_stdin {
            // Do not inherit stdin by default; ensure isolation unless WITH_STDIN is used.
            command.stdin(Stdio::null());
        }
        let desc = format!("{:?}", command);

        match mode {
            CommandMode::Foreground => {
                let mut handle =
                    spawn_child_with_streams(&mut command, stdin, stdout_stream, stderr_stream)?;
                let status = handle
                    .wait()
                    .with_context(|| format!("failed to run {desc}"))?;
                if !status.success() {
                    bail!("command {desc} failed with status {}", status);
                }
                if let Some(buf) = capture_buf {
                    let mut guard = buf.lock().map_err(|_| anyhow!("capture stdout poisoned"))?;
                    return Ok(CommandResult::Captured(std::mem::take(&mut *guard)));
                }
                Ok(CommandResult::Completed)
            }
            CommandMode::Background => {
                let handle =
                    spawn_child_with_streams(&mut command, stdin, stdout_stream, stderr_stream)?;
                Ok(CommandResult::Background(handle))
            }
        }
    }
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn apply_ctx(command: &mut ProcessCommand, ctx: &CommandContext) {
    // Use command_path to strip Windows verbatim prefixes (\\?\) before passing to Command.
    // While Rust's `std::process::Command` handles verbatim paths in current_dir correctly,
    // environment variables are passed as-is. If we pass a verbatim path in `CARGO_TARGET_DIR`,
    // tools that don't understand it (or shell scripts echoing it) might misbehave or produce
    // unexpected output. Normalizing here ensures consistency.
    //
    // Why the `\\?\` verbatim prefixes?
    // On Windows we intentionally keep the canonical verbatim path (e.g. `\\?\C:\\repo`)
    // inside every `GuardedPath`. This avoids MAX_PATH truncation and prevents subtle
    // `PathBuf` casing/drive-letter surprises when the guard is later joined, copied,
    // or passed through `std::fs`. When you need a human-readable path, call
    // [`command_path`] (native separators, prefix stripped) or [`normalized_path`]
    // (forward slashes) or use the `Display` impl, which already defers to
    // `command_path`. Keep the debug view raw so diagnostics can show the exact path
    // we are guarding.
    let cwd_path: std::borrow::Cow<std::path::Path> = match ctx.cwd() {
        PolicyPath::Guarded(p) => oxdock_fs::command_path(p),
        PolicyPath::Unguarded(p) => std::borrow::Cow::Borrowed(p.as_path()),
    };
    command.current_dir(cwd_path);
    command.envs(ctx.envs().as_ref());
    if let Some(val) = ctx.envs().get("CARGO_TARGET_DIR") {
        command.env("CARGO_TARGET_DIR", val);
    } else {
        command.env(
            "CARGO_TARGET_DIR",
            oxdock_fs::command_path(ctx.cargo_target_dir()).into_owned(),
        );
    }
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn spawn_child_with_streams(
    cmd: &mut ProcessCommand,
    stdin: Option<SharedInput>,
    stdout: Option<SharedOutput>,
    stderr: Option<SharedOutput>,
) -> Result<ChildHandle> {
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    if stdout.is_some() {
        cmd.stdout(Stdio::piped());
    }
    if stderr.is_some() {
        cmd.stderr(Stdio::piped());
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {:?}", cmd))?;
    let mut io_threads = Vec::new();

    if let Some(stdin_stream) = stdin
        && let Some(mut child_stdin) = child.stdin.take()
    {
        let thread = std::thread::spawn(move || {
            if let Ok(mut guard) = stdin_stream.lock() {
                let _ = std::io::copy(&mut *guard, &mut child_stdin);
            }
        });
        io_threads.push(thread);
    }

    if let Some(stdout_stream) = stdout
        && let Some(mut child_stdout) = child.stdout.take()
    {
        let stream_clone = stdout_stream.clone();
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                match std::io::Read::read(&mut child_stdout, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut guard) = stream_clone.lock() {
                            if std::io::Write::write_all(&mut *guard, &buf[..n]).is_err() {
                                break;
                            }
                            let _ = std::io::Write::flush(&mut *guard);
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        io_threads.push(thread);
    }

    if let Some(stderr_stream) = stderr
        && let Some(mut child_stderr) = child.stderr.take()
    {
        let stream_clone = stderr_stream.clone();
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                match std::io::Read::read(&mut child_stderr, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut guard) = stream_clone.lock() {
                            if std::io::Write::write_all(&mut *guard, &buf[..n]).is_err() {
                                break;
                            }
                            let _ = std::io::Write::flush(&mut *guard);
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        io_threads.push(thread);
    }

    Ok(ChildHandle::new(child, io_threads))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxdock_fs::GuardedPath;
    use std::collections::HashMap;

    fn make_ctx(envs: &[(&str, &str)]) -> (oxdock_fs::GuardedTempDir, CommandContext) {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let guard = temp.as_guarded_path().clone();
        let cwd: PolicyPath = guard.clone().into();
        let map: HashMap<String, String> = envs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        let ctx = CommandContext::from_map(&cwd, &map, &guard, &guard, &guard);
        (temp, ctx)
    }

    #[test]
    fn background_capture_stdout_bails_without_spawning() {
        let (_temp, ctx) = make_ctx(&[]);
        let mut pm = ShellProcessManager;
        let options = CommandOptions {
            mode: CommandMode::Background,
            stdout: CommandStdout::Capture,
            ..Default::default()
        };
        let err = match pm.run_command(&ctx, "echo hi", options) {
            Err(err) => err,
            Ok(_) => panic!("background capture must bail"),
        };
        assert!(
            err.to_string().contains("cannot capture stdout"),
            "unexpected error: {err}"
        );
    }

    #[cfg_attr(
        miri,
        ignore = "spawns processes; Miri does not support process execution"
    )]
    #[test]
    fn foreground_capture_returns_child_stdout_bytes() {
        let (_temp, ctx) = make_ctx(&[]);
        let mut pm = ShellProcessManager;
        let options = CommandOptions {
            stdout: CommandStdout::Capture,
            ..Default::default()
        };
        match pm
            .run_command(&ctx, "echo hello-capture", options)
            .expect("run")
        {
            CommandResult::Captured(bytes) => {
                let out = String::from_utf8_lossy(&bytes);
                assert!(out.contains("hello-capture"), "captured: {out}");
            }
            CommandResult::Completed => panic!("expected Captured, got Completed"),
            CommandResult::Background(_) => panic!("expected Captured, got Background"),
        }
    }

    fn large_output_script() -> &'static str {
        #[cfg(windows)]
        {
            "for /l %i in (1,1,20000) do @echo 0123456789abcdef"
        }
        #[cfg(not(windows))]
        {
            "i=0; while [ $i -lt 20000 ]; do echo 0123456789abcdef; i=$((i+1)); done"
        }
    }

    #[cfg_attr(
        miri,
        ignore = "spawns processes; Miri does not support process execution"
    )]
    #[test]
    fn streams_large_stdout_through_shared_output_without_deadlock() {
        let (_temp, ctx) = make_ctx(&[]);
        let mut pm = ShellProcessManager;
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let options = CommandOptions {
            stdout: CommandStdout::Stream(buffer.clone()),
            ..Default::default()
        };
        pm.run_command(&ctx, large_output_script(), options)
            .expect("run");
        let bytes = buffer.lock().expect("buffer lock").len();
        // 20000 lines x 17 bytes ~= 340KB, far beyond any OS pipe buffer, so
        // the reader pump must have iterated continuously.
        assert!(bytes >= 300_000, "streamed only {bytes} bytes");
    }

    #[cfg_attr(
        miri,
        ignore = "spawns processes; Miri does not support process execution"
    )]
    #[test]
    fn foreground_stdin_is_piped_through_copy_thread() {
        let (_temp, ctx) = make_ctx(&[]);
        let mut pm = ShellProcessManager;
        let payload: SharedInput = std::sync::Arc::new(std::sync::Mutex::new(
            std::io::Cursor::new(b"b\na\n".to_vec()),
        ));
        let options = CommandOptions {
            stdin: Some(payload),
            stdout: CommandStdout::Capture,
            ..Default::default()
        };
        // `sort` reads stdin to EOF on both Unix and Windows before printing.
        match pm.run_command(&ctx, "sort", options).expect("run") {
            CommandResult::Captured(bytes) => {
                assert!(bytes.starts_with(b"a"), "sorted output: {:?}", bytes);
                assert!(windows_compatible_contains(&bytes, b"b"));
            }
            CommandResult::Completed => panic!("expected Captured, got Completed"),
            CommandResult::Background(_) => panic!("expected Captured, got Background"),
        }
    }

    fn windows_compatible_contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn apply_ctx_sets_cwd_and_cargo_target_dir_precedence() {
        // Default branch: executor-provided cargo target dir wins when the
        // env map has no explicit override.
        let (temp_a, ctx_a) = make_ctx(&[]);
        let expected_default = oxdock_fs::command_path(ctx_a.cargo_target_dir())
            .to_string_lossy()
            .into_owned();
        let mut cmd = ProcessCommand::new("prog");
        apply_ctx(&mut cmd, &ctx_a);
        let envs_a: HashMap<String, String> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(envs_a.get("CARGO_TARGET_DIR"), Some(&expected_default));
        drop(temp_a);

        // Override branch: an explicit CARGO_TARGET_DIR env mapping wins over
        // the executor default.
        let (temp_b, ctx_b) = make_ctx(&[("CARGO_TARGET_DIR", "custom-target"), ("FOO", "bar")]);
        let mut cmd = ProcessCommand::new("prog");
        apply_ctx(&mut cmd, &ctx_b);
        let envs_b: HashMap<String, String> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            envs_b.get("CARGO_TARGET_DIR").map(String::as_str),
            Some("custom-target")
        );
        assert_eq!(envs_b.get("FOO").map(String::as_str), Some("bar"));
        drop(temp_b);
    }

    #[test]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn apply_ctx_sets_working_directory_from_guarded_cwd() {
        let (_temp, ctx) = make_ctx(&[]);
        let mut cmd = ProcessCommand::new("prog");
        apply_ctx(&mut cmd, &ctx);
        let expected = oxdock_fs::command_path(match ctx.cwd() {
            PolicyPath::Guarded(guarded) => guarded,
            PolicyPath::Unguarded(_) => panic!("expected guarded cwd"),
        });
        assert_eq!(cmd.get_current_dir(), Some(expected.as_ref()));
    }
}
