use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::process::Command as ProcessCommand;
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::process::Stdio;

use oxdock_fs::PolicyPath;
use oxdock_fs::env as oxdock_env;

use crate::child::ChildHandle;
use crate::contract::{
    BackgroundHandle, CommandContext, CommandMode, CommandOptions, CommandResult, CommandStderr,
    CommandStdin, CommandStdout, PROCESS_DEBUG_ENV_VAR, ProcessManager, SharedInput, SharedOutput,
};
use crate::shell::{batch_shell, direct_cmd, shell_cmd};

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
        if std::env::var_os(PROCESS_DEBUG_ENV_VAR).is_some() {
            eprintln!("oxbook run_command: {script}");
        }
        let mut command = shell_cmd(script);
        let program = batch_shell();
        // Shell form always requires PATH: the shell binary may resolve
        // without it, but every command inside the script text resolves
        // through the child's PATH.
        apply_ctx(&mut command, ctx, &program, true)?;
        let CommandOptions {
            mode,
            stdin,
            stdout,
            stderr,
        } = options;
        run_prepared(&mut command, mode, stdin, stdout, stderr)
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn run_argv(
        &mut self,
        ctx: &CommandContext,
        argv: &[String],
        options: CommandOptions,
    ) -> Result<CommandResult<Self::Handle>> {
        if std::env::var_os(PROCESS_DEBUG_ENV_VAR).is_some() {
            eprintln!("oxbook run_argv: {argv:?}");
        }
        let mut command = direct_cmd(argv)?;
        let program = argv.first().map(String::as_str).unwrap_or_default();
        apply_ctx(&mut command, ctx, program, false)?;
        let CommandOptions {
            mode,
            stdin,
            stdout,
            stderr,
        } = options;
        run_prepared(&mut command, mode, stdin, stdout, stderr)
    }
}

/// Shared foreground/background runner for an already-configured
/// `std::process::Command`: stdio mapping, null-stdin isolation,
/// capture semantics, and non-zero failures behave identically for
/// shell (`run_command`) and direct-spawn (`run_argv`) paths.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn run_prepared(
    command: &mut ProcessCommand,
    mode: CommandMode,
    stdin: CommandStdin,
    stdout: CommandStdout,
    stderr: CommandStderr,
) -> Result<CommandResult<ChildHandle>> {
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
        #[cfg(not(miri))]
        CommandStdout::OsPipe(writer) => {
            // Move the FD into the child. `take` empties the parent slot,
            // and `options` drops at the end of this function, so no parent
            // copy survives to starve the reader of EOF.
            let owned = writer.take()?;
            command.stdout(Stdio::from(owned));
            (None, None)
        }
    };

    let stderr_stream = match stderr {
        CommandStderr::Inherit => None,
        CommandStderr::Stream(stream) => Some(stream),
        #[cfg(not(miri))]
        CommandStderr::OsPipe(writer) => {
            // Same move and drop discipline as stdout above.
            let owned = writer.take()?;
            command.stderr(Stdio::from(owned));
            None
        }
    };

    let stdin_stream: Option<SharedInput> = match stdin {
        // Do not inherit stdin by default; ensure isolation unless requested.
        CommandStdin::Null => {
            command.stdin(Stdio::null());
            None
        }
        CommandStdin::Inherit => None,
        CommandStdin::Stream(reader) => Some(reader),
        #[cfg(not(miri))]
        CommandStdin::OsPipe(reader) => {
            // Same move and drop discipline as stdout above.
            let owned = reader.take()?;
            command.stdin(Stdio::from(owned));
            None
        }
    };
    let desc = format!("{:?}", command);

    match mode {
        CommandMode::Foreground => {
            let mut handle =
                spawn_child_with_streams(command, stdin_stream, stdout_stream, stderr_stream)?;
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
                spawn_child_with_streams(command, stdin_stream, stdout_stream, stderr_stream)?;
            Ok(CommandResult::Background(handle))
        }
    }
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn apply_ctx(
    command: &mut ProcessCommand,
    ctx: &CommandContext,
    program: &str,
    require_path: bool,
) -> Result<()> {
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
    // Tiered child env: script env layered over engine-managed
    // substrate (Tier 1 OS + Tier 2 scratch temp + Tier 3 PATH
    // baseline), never host inherit. Explicit script values win within
    // their tier; Tier 1A invariants cannot be overridden. Anchor
    // diagnostics live in one place (`env_policy`) shared with every
    // other spawn site, so the messages cannot drift per call site.
    let mut staged = ctx.substrate().assemble(ctx.envs().as_ref(), true);
    // `CARGO_TARGET_DIR` rides alongside, not inside, the tiers: an
    // explicit script value wins, else the session scratch target.
    if !crate::env_policy::has_var(ctx.envs().as_ref(), oxdock_env::cargo::TARGET_DIR) {
        staged.insert(
            oxdock_env::cargo::TARGET_DIR.to_string(),
            ctx.cargo_target_dir()
                .command_path()
                .to_string_lossy()
                .into_owned(),
        );
    }
    crate::env_policy::check_spawn_anchors(program, &staged, require_path)?;
    command.env_clear();
    command.envs(&staged);
    Ok(())
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
    let mut stdin_thread = None;

    if let Some(stdin_stream) = stdin
        && let Some(mut child_stdin) = child.stdin.take()
    {
        let thread = std::thread::spawn(move || {
            if let Ok(mut guard) = stdin_stream.lock() {
                let _ = std::io::copy(&mut *guard, &mut child_stdin);
            }
        });
        stdin_thread = Some(thread);
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

    Ok(ChildHandle::new(child, stdin_thread, io_threads))
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
        let scratch = oxdock_fs::reserve_cargo_scratch().expect("scratch");
        let substrate = crate::env_policy::SubstrateEnv::capture_for_scratch(&scratch);
        let ctx = CommandContext::from_map(&cwd, &map, &scratch, &substrate, &guard, &guard);
        (temp, ctx)
    }

    /// Host executable-lookup env for real-spawn tests: closed children
    /// resolve binaries only through the script env, so tests that spawn
    /// real tools must route the host lookup path explicitly, exactly
    /// like workflow legs route `INHERIT_ENV [PATH]`.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn host_exec_env() -> HashMap<String, String> {
        let mut map = HashMap::new();
        if let Ok(path) = std::env::var(oxdock_env::os::PATH) {
            map.insert(oxdock_env::os::PATH.to_string(), path);
        }
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            for key in [
                oxdock_env::os::windows::SYSTEM_ROOT,
                oxdock_env::os::windows::PATHEXT,
                oxdock_env::os::windows::COMSPEC,
                oxdock_env::os::windows::TEMP,
                oxdock_env::os::windows::TMP,
            ] {
                if let Ok(value) = std::env::var(key) {
                    map.insert(key.to_string(), value);
                }
            }
        }
        map
    }

    fn make_ctx_with_host_exec(
        extra: &[(&str, &str)],
    ) -> (oxdock_fs::GuardedTempDir, CommandContext) {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let guard = temp.as_guarded_path().clone();
        let cwd: PolicyPath = guard.clone().into();
        let mut map = host_exec_env();
        for (key, value) in extra {
            map.insert(key.to_string(), value.to_string());
        }
        let scratch = oxdock_fs::reserve_cargo_scratch().expect("scratch");
        let substrate = crate::env_policy::SubstrateEnv::capture_for_scratch(&scratch);
        let ctx = CommandContext::from_map(&cwd, &map, &scratch, &substrate, &guard, &guard);
        (temp, ctx)
    }

    #[test]
    fn background_capture_stdout_bails_without_spawning() {
        let (_temp, ctx) = make_ctx_with_host_exec(&[]);
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
        let (_temp, ctx) = make_ctx_with_host_exec(&[]);
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

    #[cfg_attr(
        miri,
        ignore = "spawns processes; Miri does not support process execution"
    )]
    #[test]
    fn run_argv_spawns_directly_without_shell() {
        // `cargo` here is a rustup shim: it resolves through the host
        // PATH the helper bridges, and needs a writable home for its
        // toolchain lookup. Managed HOME points at scratch (no
        // toolchains), so this test bridges the host home explicitly:
        // production scripts needing toolchains route Tier 4 the same
        // way instead of inheriting it ambiently.
        #[allow(clippy::disallowed_macros)]
        #[cfg(unix)]
        let home = std::env::var(oxdock_env::os::unix::HOME).unwrap_or_default();
        #[allow(clippy::disallowed_macros)]
        #[cfg(windows)]
        let home = std::env::var(oxdock_env::os::windows::USERPROFILE).unwrap_or_default();
        #[allow(clippy::disallowed_macros)]
        #[cfg(unix)]
        let home_key = oxdock_env::os::unix::HOME;
        #[allow(clippy::disallowed_macros)]
        #[cfg(windows)]
        let home_key = oxdock_env::os::windows::USERPROFILE;
        let (_temp, ctx) = make_ctx_with_host_exec(&[(home_key, home.as_str())]);
        let mut pm = ShellProcessManager;
        let options = CommandOptions {
            stdout: CommandStdout::Capture,
            ..Default::default()
        };
        // `cargo` drives the test suite itself, so it is present on every
        // platform without relying on shell builtins (`echo` is not a
        // Windows executable).
        let argv = vec!["cargo".to_string(), "--version".to_string()];
        match pm.run_argv(&ctx, &argv, options).expect("run_argv") {
            CommandResult::Captured(bytes) => {
                let out = String::from_utf8_lossy(&bytes);
                assert!(out.contains("cargo"), "captured: {out}");
            }
            CommandResult::Completed => panic!("expected Captured, got Completed"),
            CommandResult::Background(_) => panic!("expected Captured, got Background"),
        }
    }

    #[test]
    fn run_argv_rejects_empty_argv_without_spawning() {
        let (_temp, ctx) = make_ctx_with_host_exec(&[]);
        let mut pm = ShellProcessManager;
        let err = match pm.run_argv(&ctx, &[], CommandOptions::foreground()) {
            Err(err) => err,
            Ok(_) => panic!("empty argv must bail"),
        };
        assert!(
            err.to_string().contains("at least one argument"),
            "unexpected error: {err}"
        );
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
        let (_temp, ctx) = make_ctx_with_host_exec(&[]);
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
        let (_temp, ctx) = make_ctx_with_host_exec(&[]);
        let mut pm = ShellProcessManager;
        // Windows `sort` requires `\r\n` line endings to recognize lines.
        #[cfg(windows)]
        let input: &[u8] = b"b\r\na\r\n";
        #[cfg(not(windows))]
        let input: &[u8] = b"b\na\n";
        let payload: SharedInput =
            std::sync::Arc::new(std::sync::Mutex::new(std::io::Cursor::new(input.to_vec())));
        let options = CommandOptions {
            stdin: CommandStdin::Stream(payload),
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

    /// Concurrent producer and consumer wired through one kernel pipe. The
    /// consumer (`sort`) only returns after EOF, which proves the parent
    /// retained no writer copy after spawning the producer.
    #[cfg(not(miri))]
    #[test]
    fn os_pipe_streams_producer_to_consumer_with_eof() {
        use crate::contract::{BackgroundHandle, create_os_pipe};

        let (_temp, ctx) = make_ctx_with_host_exec(&[]);
        let (reader, writer) = create_os_pipe().expect("os pipe");
        let mut pm = ShellProcessManager;

        let producer = match pm
            .run_command(
                &ctx,
                "echo hello-os-pipe",
                CommandOptions {
                    mode: CommandMode::Background,
                    stdout: CommandStdout::OsPipe(writer),
                    ..Default::default()
                },
            )
            .expect("spawn producer")
        {
            CommandResult::Background(handle) => handle,
            _ => panic!("expected background producer handle"),
        };

        let options = CommandOptions {
            stdin: CommandStdin::OsPipe(reader),
            stdout: CommandStdout::Capture,
            ..Default::default()
        };
        // `sort` reads stdin to EOF on every platform before printing.
        let captured = match pm.run_command(&ctx, "sort", options).expect("run consumer") {
            CommandResult::Captured(bytes) => bytes,
            _ => panic!("expected captured consumer output"),
        };
        assert!(
            windows_compatible_contains(&captured, b"hello-os-pipe"),
            "piped output: {:?}",
            String::from_utf8_lossy(&captured)
        );

        let mut producer = producer;
        let status = producer.wait().expect("wait producer");
        assert!(status.success(), "producer failed: {status:?}");

        // A spent handle cannot be reused for a second spawn.
        let (spent_reader, spent_writer) = create_os_pipe().expect("os pipe");
        spent_reader.take().expect("first reader take");
        assert!(
            spent_reader.take().is_err(),
            "reader take must be single use"
        );
        spent_writer.take().expect("first writer take");
        assert!(
            spent_writer.take().is_err(),
            "writer take must be single use"
        );
    }

    #[test]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn apply_ctx_sets_cwd_and_cargo_target_dir_precedence() {
        // Default branch: executor-provided cargo target dir wins when the
        // env map has no explicit override. PATH is seeded because the
        // closed child env requires it for bare program names.
        let (temp_a, ctx_a) = make_ctx(&[(oxdock_env::os::PATH, "test-path")]);
        let expected_default = ctx_a
            .cargo_target_dir()
            .command_path()
            .to_string_lossy()
            .into_owned();
        let mut cmd = ProcessCommand::new("prog");
        apply_ctx(&mut cmd, &ctx_a, "prog", false).expect("apply with explicit PATH");
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
        assert_eq!(
            envs_a.get(oxdock_env::cargo::TARGET_DIR),
            Some(&expected_default)
        );
        drop(temp_a);

        // Override branch: an explicit CARGO_TARGET_DIR env mapping wins over
        // the executor default.
        let (temp_b, ctx_b) = make_ctx(&[
            (oxdock_env::cargo::TARGET_DIR, "custom-target"),
            ("FOO", "bar"),
            (oxdock_env::os::PATH, "test-path"),
        ]);
        let mut cmd = ProcessCommand::new("prog");
        apply_ctx(&mut cmd, &ctx_b, "prog", false).expect("apply with explicit PATH");
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
            envs_b
                .get(oxdock_env::cargo::TARGET_DIR)
                .map(String::as_str),
            Some("custom-target")
        );
        assert_eq!(envs_b.get("FOO").map(String::as_str), Some("bar"));
        drop(temp_b);
    }

    #[test]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn apply_ctx_sets_working_directory_from_guarded_cwd() {
        let (_temp, ctx) = make_ctx(&[(oxdock_env::os::PATH, "test-path")]);
        let mut cmd = ProcessCommand::new("prog");
        apply_ctx(&mut cmd, &ctx, "prog", false).expect("apply with explicit PATH");
        let expected = oxdock_fs::command_path(match ctx.cwd() {
            PolicyPath::Guarded(guarded) => guarded,
            PolicyPath::Unguarded(_) => panic!("expected guarded cwd"),
        });
        assert_eq!(cmd.get_current_dir(), Some(expected.as_ref()));
    }

    /// Closed child env: a host variable never routed through the script env
    /// must not appear in the staged command env, while declared names do.
    #[test]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn apply_ctx_clears_host_env() {
        // RAII-guarded: a mid-test failure still restores process env.
        let _probe_guard = crate::TestEnvGuard::set("OXDOCK_CLOSED_ENV_PROBE", "leaked");
        let (_temp, ctx) = make_ctx(&[(oxdock_env::os::PATH, "test-path"), ("DECLARED", "yes")]);
        let mut cmd = ProcessCommand::new("prog");
        apply_ctx(&mut cmd, &ctx, "prog", false).expect("apply with explicit PATH");
        let envs: HashMap<String, String> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            })
            .collect();
        assert!(
            !envs.contains_key("OXDOCK_CLOSED_ENV_PROBE"),
            "host env must not reach children: {envs:?}"
        );
        assert_eq!(envs.get("DECLARED").map(String::as_str), Some("yes"));
        assert_eq!(
            envs.get(oxdock_env::os::PATH).map(String::as_str),
            Some("test-path")
        );
    }

    /// Tier 3: a bare program name without a script PATH still stages
    /// the sanctioned baseline instead of bailing, so core utilities
    /// resolve with zero prelude.
    #[test]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn apply_ctx_stages_baseline_path_for_bare_program() {
        let (_temp, ctx) = make_ctx(&[("FOO", "bar")]);
        let mut cmd = ProcessCommand::new("prog");
        apply_ctx(&mut cmd, &ctx, "prog", false).expect("baseline must satisfy the anchor");
        let envs: HashMap<String, String> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            })
            .collect();
        #[allow(clippy::disallowed_macros)]
        let baseline = if cfg!(windows) {
            oxdock_env::os::windows::substrate::SANCTIONED_PATH
        } else {
            oxdock_env::os::unix::substrate::SANCTIONED_PATH
        };
        assert_eq!(
            envs.get(oxdock_env::os::PATH).map(String::as_str),
            Some(baseline)
        );
    }

    /// Pre-flight skips separator-carrying programs: absolute and relative
    /// pathed invocations resolve without `PATH` and stay hermetic.
    #[test]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn apply_ctx_skips_path_check_for_pathed_program() {
        let (_temp, ctx) = make_ctx(&[("FOO", "bar")]);
        let mut unix_cmd = ProcessCommand::new("/bin/prog");
        apply_ctx(&mut unix_cmd, &ctx, "/bin/prog", false).expect("absolute path needs no PATH");
        let mut rel_cmd = ProcessCommand::new("sub/prog");
        apply_ctx(&mut rel_cmd, &ctx, "sub/prog", false).expect("relative path needs no PATH");
    }

    /// Tier 1A: the NT loader anchor injects on Windows, so the anchor
    /// check passes with zero prelude; elsewhere there is nothing to
    /// check and staging still succeeds.
    #[test]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn apply_ctx_systemroot_check_follows_platform() {
        let (_temp, ctx) = make_ctx(&[(oxdock_env::os::PATH, "test-path")]);
        let mut cmd = ProcessCommand::new("prog");
        apply_ctx(&mut cmd, &ctx, "prog", false).expect("tiers satisfy anchors on every platform");
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            let envs: HashMap<String, String> = cmd
                .get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|value| value.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                    )
                })
                .collect();
            assert!(
                envs.contains_key(oxdock_env::os::windows::substrate::SYSTEM_ROOT),
                "Tier 1A must stage SystemRoot: {envs:?}"
            );
        }
    }

    /// Windows casing: `SYSTEMROOT` and `Path` spellings satisfy the
    /// case-insensitive Tier 1A drop (script values for loader
    /// invariants are replaced by the injected anchor, never honored).
    #[test]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn apply_ctx_accepts_windows_casing_variants() {
        // Deliberate non-canonical spellings: this test proves alternate
        // casings hit the same Tier 1A rule, so the literals stay.
        let (_temp, ctx) = make_ctx(&[("SYSTEMROOT", "C:\\Windows"), ("Path", "test-path")]);
        let mut cmd = ProcessCommand::new("prog");
        apply_ctx(&mut cmd, &ctx, "prog", false).expect("tiers stage on every platform");
        let envs: HashMap<String, String> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            })
            .collect();
        // Explicit PATH always wins over the baseline, any casing.
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            let path_val = envs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(oxdock_env::os::PATH))
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            assert_eq!(path_val, "test-path");
        } else {
            assert_eq!(envs.get("Path").map(String::as_str), Some("test-path"));
            assert_eq!(
                envs.get(oxdock_env::os::PATH).map(String::as_str),
                Some(oxdock_env::os::unix::substrate::SANCTIONED_PATH)
            );
        }
    }

    /// End-to-end closedness: a real child sees declared vars and cannot see
    /// a host-only sentinel, proving the boundary past `apply_ctx` staging.
    #[cfg_attr(miri, ignore = "spawns a real child process, unsupported under Miri")]
    #[test]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn spawned_child_does_not_see_host_env() {
        // RAII-guarded: a mid-test failure still restores process env.
        let _probe_guard = crate::TestEnvGuard::set("OXDOCK_CLOSED_ENV_PROBE", "leaked");
        let (_temp, ctx) = make_ctx_with_host_exec(&[("OXDOCK_CHILD_MARKER", "here")]);
        let mut pm = ShellProcessManager;
        #[cfg(windows)]
        let script = "if defined OXDOCK_CLOSED_ENV_PROBE (echo PROBE-SEEN) else (echo PROBE-CLEAN)& echo MARKER-%OXDOCK_CHILD_MARKER%";
        #[cfg(not(windows))]
        let script = "echo \"probe=${OXDOCK_CLOSED_ENV_PROBE:-unset} marker=${OXDOCK_CHILD_MARKER:-missing}\"";
        let options = CommandOptions {
            stdout: CommandStdout::Capture,
            ..Default::default()
        };
        let out = match pm.run_command(&ctx, script, options).expect("run") {
            CommandResult::Captured(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            CommandResult::Completed => panic!("expected Captured, got Completed"),
            CommandResult::Background(_) => panic!("expected Captured, got Background"),
        };
        #[cfg(windows)]
        {
            assert!(out.contains("PROBE-CLEAN"), "child saw host env: {out}");
            assert!(
                out.contains("MARKER-here"),
                "child missed script env: {out}"
            );
        }
        #[cfg(not(windows))]
        {
            assert!(out.contains("probe=unset"), "child saw host env: {out}");
            assert!(
                out.contains("marker=here"),
                "child missed script env: {out}"
            );
        }
    }
}
