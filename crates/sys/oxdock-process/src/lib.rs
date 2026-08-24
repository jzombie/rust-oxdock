pub mod builtin_env;
#[cfg(feature = "mock-process")]
mod mock;
pub mod serial_cargo_env;
mod shell;

use anyhow::{Context, Result, anyhow, bail};
pub use builtin_env::BuiltinEnv;
use oxdock_fs::{GuardedPath, PolicyPath};
pub use oxdock_sys_test_utils::TestEnvGuard;
use shell::shell_cmd;
pub use shell::{ShellLauncher, shell_program, spawn_interactive_shell};
use std::collections::HashMap;
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::fs::File;
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::path::{Path, PathBuf};
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::process::{Child, Command as ProcessCommand, ExitStatus, Output as StdOutput, Stdio};
use std::{
    ffi::{OsStr, OsString},
    iter::IntoIterator,
    mem,
};

#[cfg(miri)]
use oxdock_fs::PathResolver;

#[cfg(feature = "mock-process")]
pub use mock::{MockHandle, MockProcessManager, MockRunCall, MockSpawnCall, MockStreamMode};

/// Context passed to process managers describing the current execution
/// environment. Clones are cheap and explicit so background handles can own
/// their working roots without juggling lifetimes.
#[derive(Clone, Debug)]
pub struct CommandContext {
    cwd: PolicyPath,
    envs: HashMap<String, String>,
    cargo_target_dir: GuardedPath,
    workspace_root: GuardedPath,
    build_context: GuardedPath,
}

impl CommandContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cwd: &PolicyPath,
        envs: &HashMap<String, String>,
        cargo_target_dir: &GuardedPath,
        workspace_root: &GuardedPath,
        build_context: &GuardedPath,
    ) -> Self {
        Self {
            cwd: cwd.clone(),
            envs: envs.clone(),
            cargo_target_dir: cargo_target_dir.clone(),
            workspace_root: workspace_root.clone(),
            build_context: build_context.clone(),
        }
    }

    pub fn cwd(&self) -> &PolicyPath {
        &self.cwd
    }

    pub fn envs(&self) -> &HashMap<String, String> {
        &self.envs
    }

    pub fn cargo_target_dir(&self) -> &GuardedPath {
        &self.cargo_target_dir
    }

    pub fn workspace_root(&self) -> &GuardedPath {
        &self.workspace_root
    }

    pub fn build_context(&self) -> &GuardedPath {
        &self.build_context
    }
}

fn expand_with_lookup<F>(input: &str, mut lookup: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            if let Some(&'{') = chars.peek() {
                chars.next(); // consume second '{'
                let mut content = String::new();
                let mut closed = false;
                // Look ahead for closing }}
                let mut inner_chars = chars.clone();
                while let Some(ch) = inner_chars.next() {
                    if ch == '}'
                        && let Some(&'}') = inner_chars.peek()
                    {
                        closed = true;
                        break;
                    }
                    content.push(ch);
                }

                if closed {
                    // Advance main iterator past content and closing braces.
                    // Count chars, not bytes: content may contain multi-byte
                    // UTF-8 (e.g. non-ASCII placeholder names).
                    for _ in 0..content.chars().count() {
                        chars.next();
                    }
                    chars.next(); // first }
                    chars.next(); // second }

                    let key = content.trim();
                    if !key.is_empty() {
                        out.push_str(&lookup(key).unwrap_or_default());
                    }
                } else {
                    out.push('{');
                    out.push('{');
                }
            } else {
                out.push('{');
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn expand_script_env(input: &str, script_envs: &HashMap<String, String>) -> String {
    expand_with_lookup(input, |name| {
        if let Some(key) = name.strip_prefix("env:") {
            script_envs
                .get(key)
                .cloned()
                .or_else(|| std::env::var(key).ok())
        } else {
            None
        }
    })
}

pub fn expand_command_env(input: &str, ctx: &CommandContext) -> String {
    expand_with_lookup(input, |name| {
        if let Some(key) = name.strip_prefix("env:") {
            ctx.envs().get(key).cloned()
        } else {
            None
        }
    })
}

/// Handle for background processes spawned by a [`ProcessManager`].
pub trait BackgroundHandle {
    fn try_wait(&mut self) -> Result<Option<ExitStatus>>;
    fn kill(&mut self) -> Result<()>;
    fn wait(&mut self) -> Result<ExitStatus>;
}

/// Abstraction for running shell commands both in the foreground and
/// background. `oxdock-core` relies on this trait to decouple the executor
/// from `std::process::Command`, which in turn enables Miri-friendly test
/// doubles.
use std::sync::{Arc, Mutex};

pub type SharedInput = Arc<Mutex<dyn std::io::Read + Send>>;
pub type SharedOutput = Arc<Mutex<dyn std::io::Write + Send>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CommandMode {
    #[default]
    Foreground,
    Background,
}

#[derive(Clone, Default)]
pub enum CommandStdout {
    #[default]
    Inherit,
    Stream(SharedOutput),
    Capture,
}

#[derive(Clone, Default)]
pub enum CommandStderr {
    #[default]
    Inherit,
    Stream(SharedOutput),
}

#[derive(Clone, Default)]
pub struct CommandOptions {
    pub mode: CommandMode,
    pub stdin: Option<SharedInput>,
    pub stdout: CommandStdout,
    pub stderr: CommandStderr,
}

impl CommandOptions {
    pub fn foreground() -> Self {
        Self::default()
    }

    pub fn background() -> Self {
        Self {
            mode: CommandMode::Background,
            ..Self::default()
        }
    }
}

pub enum CommandResult<H> {
    Completed,
    Captured(Vec<u8>),
    Background(H),
}

pub trait ProcessManager: Clone {
    type Handle: BackgroundHandle;

    fn run_command(
        &mut self,
        ctx: &CommandContext,
        script: &str,
        options: CommandOptions,
    ) -> Result<CommandResult<Self::Handle>>;
}

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
                    return Ok(CommandResult::Captured(mem::take(&mut *guard)));
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
    command.envs(ctx.envs());
    if let Some(val) = ctx.envs().get("CARGO_TARGET_DIR") {
        command.env("CARGO_TARGET_DIR", val);
    } else {
        command.env(
            "CARGO_TARGET_DIR",
            oxdock_fs::command_path(ctx.cargo_target_dir()).into_owned(),
        );
    }
}

#[derive(Debug)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub struct ChildHandle {
    child: Child,
    io_threads: Vec<std::thread::JoinHandle<()>>,
}

impl BackgroundHandle for ChildHandle {
    fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    fn wait(&mut self) -> Result<ExitStatus> {
        let status = self.child.wait()?;
        // Wait for IO threads to finish to ensure all output is captured
        for thread in self.io_threads.drain(..) {
            let _ = thread.join();
        }
        Ok(status)
    }

    fn kill(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            let _ = self.child.kill();
        }
        Ok(())
    }
}

// Synthetic process manager for Miri. Commands are interpreted with a tiny
// shell that supports the patterns exercised in tests: sleep, printf/echo with
// env interpolation, redirection, and exit codes. IO is routed through the
// workspace filesystem so we never touch the host.
#[cfg(miri)]
#[derive(Clone, Default)]
pub struct SyntheticProcessManager;

#[cfg(miri)]
#[derive(Clone)]
pub struct SyntheticBgHandle {
    ctx: CommandContext,
    actions: Vec<Action>,
    remaining: std::time::Duration,
    last_polled: std::time::Instant,
    status: ExitStatus,
    applied: bool,
    killed: bool,
}

#[cfg(miri)]
#[derive(Clone)]
enum Action {
    WriteFile { target: GuardedPath, data: Vec<u8> },
}

#[cfg(miri)]
impl BackgroundHandle for SyntheticBgHandle {
    fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        if self.killed {
            self.applied = true;
            return Ok(Some(self.status));
        }
        if self.applied {
            return Ok(Some(self.status));
        }
        let now = std::time::Instant::now();
        let elapsed = now.saturating_duration_since(self.last_polled);
        const MAX_ADVANCE: std::time::Duration = std::time::Duration::from_millis(15);
        let advance = elapsed.min(MAX_ADVANCE).min(self.remaining);
        self.remaining = self.remaining.saturating_sub(advance);
        self.last_polled = now;

        if self.remaining.is_zero() {
            apply_actions(&self.ctx, &self.actions)?;
            self.applied = true;
            Ok(Some(self.status))
        } else {
            Ok(None)
        }
    }

    fn kill(&mut self) -> Result<()> {
        self.killed = true;
        Ok(())
    }

    fn wait(&mut self) -> Result<ExitStatus> {
        if self.killed {
            self.applied = true;
            return Ok(self.status);
        }
        if !self.applied {
            if !self.remaining.is_zero() {
                std::thread::sleep(self.remaining);
            }
            apply_actions(&self.ctx, &self.actions)?;
            self.applied = true;
        }
        Ok(self.status)
    }
}

#[cfg(miri)]
impl ProcessManager for SyntheticProcessManager {
    type Handle = SyntheticBgHandle;

    fn run_command(
        &mut self,
        ctx: &CommandContext,
        script: &str,
        options: CommandOptions,
    ) -> Result<CommandResult<Self::Handle>> {
        let CommandOptions {
            mode,
            stdin,
            stdout,
            stderr,
        } = options;

        if let Some(reader) = stdin
            && let Ok(mut guard) = reader.lock()
        {
            let mut sink = std::io::sink();
            let _ = std::io::copy(&mut *guard, &mut sink);
        }

        match mode {
            CommandMode::Foreground => {
                let needs_bytes = matches!(stdout, CommandStdout::Capture)
                    || matches!(stdout, CommandStdout::Stream(_));
                let (out, status) = execute_sync(ctx, script, needs_bytes)?;
                if !status.success() {
                    bail!("command {:?} failed with status {}", script, status);
                }
                if matches!(stderr, CommandStderr::Stream(_)) {
                    // Synthetic manager does not produce stderr output; warn if requested.
                    // We simply ignore the stream since no bytes are generated.
                }
                match stdout {
                    CommandStdout::Inherit => Ok(CommandResult::Completed),
                    CommandStdout::Stream(writer) => {
                        if needs_bytes && let Ok(mut guard) = writer.lock() {
                            let _ = std::io::Write::write_all(&mut *guard, &out);
                            let _ = std::io::Write::flush(&mut *guard);
                        }
                        Ok(CommandResult::Completed)
                    }
                    CommandStdout::Capture => Ok(CommandResult::Captured(out)),
                }
            }
            CommandMode::Background => match stdout {
                CommandStdout::Capture => {
                    bail!("cannot capture stdout for background command under miri")
                }
                CommandStdout::Stream(_) => {
                    bail!("stdout streaming not supported for background command under miri")
                }
                CommandStdout::Inherit => {
                    if matches!(stderr, CommandStderr::Stream(_)) {
                        bail!("stderr streaming not supported for background command under miri");
                    }
                    let plan = plan_background(ctx, script)?;
                    Ok(CommandResult::Background(plan))
                }
            },
        }
    }
}

#[cfg(miri)]
fn execute_sync(
    ctx: &CommandContext,
    script: &str,
    capture: bool,
) -> Result<(Vec<u8>, ExitStatus)> {
    let mut stdout = Vec::new();
    let mut status = exit_status_from_code(0);
    let resolver = PathResolver::new(
        ctx.workspace_root().as_path(),
        ctx.build_context().as_path(),
    )?;

    let script = normalize_shell(script);
    for raw in script.split(';') {
        let cmd = raw.trim();
        if cmd.is_empty() {
            continue;
        }
        let (action, sleep_dur, exit_code) = parse_command(cmd, ctx, &resolver, capture)?;
        if sleep_dur > std::time::Duration::ZERO {
            std::thread::sleep(sleep_dur);
        }
        if let Some(action) = action {
            match action {
                CommandAction::Write { target, data } => {
                    if let Some(parent) = target.as_path().parent() {
                        let parent_guard = GuardedPath::new(target.root(), parent)?;
                        resolver.create_dir_all(&parent_guard)?;
                    }
                    resolver.write_file(&target, &data)?;
                }
                CommandAction::Stdout { data } => {
                    stdout.extend_from_slice(&data);
                }
            }
        }
        if let Some(code) = exit_code {
            status = exit_status_from_code(code);
            break;
        }
    }

    Ok((stdout, status))
}

#[cfg(miri)]
fn plan_background(ctx: &CommandContext, script: &str) -> Result<SyntheticBgHandle> {
    let resolver = PathResolver::new(
        ctx.workspace_root().as_path(),
        ctx.build_context().as_path(),
    )?;
    let mut actions: Vec<Action> = Vec::new();
    let mut ready = std::time::Duration::ZERO;
    let mut status = exit_status_from_code(0);

    let script = normalize_shell(script);
    for raw in script.split(';') {
        let cmd = raw.trim();
        if cmd.is_empty() {
            continue;
        }
        let (action, sleep_dur, exit_code) = parse_command(cmd, ctx, &resolver, false)?;
        ready += sleep_dur;
        if let Some(CommandAction::Write { target, data }) = action {
            actions.push(Action::WriteFile { target, data });
        }
        if let Some(code) = exit_code {
            status = exit_status_from_code(code);
            break;
        }
    }

    let min_ready = std::time::Duration::from_millis(50);
    ready = ready.max(min_ready);

    let handle = SyntheticBgHandle {
        ctx: ctx.clone(),
        actions,
        remaining: ready,
        last_polled: std::time::Instant::now(),
        status,
        applied: false,
        killed: false,
    };
    Ok(handle)
}

#[cfg(miri)]
enum CommandAction {
    Write { target: GuardedPath, data: Vec<u8> },
    Stdout { data: Vec<u8> },
}

#[cfg(miri)]
fn parse_command(
    cmd: &str,
    ctx: &CommandContext,
    resolver: &PathResolver,
    capture: bool,
) -> Result<(Option<CommandAction>, std::time::Duration, Option<i32>)> {
    let (core, redirect) = split_redirect(cmd);
    let tokens: Vec<&str> = core.split_whitespace().collect();
    if tokens.is_empty() {
        return Ok((None, std::time::Duration::ZERO, None));
    }

    match tokens[0] {
        "sleep" => {
            let dur = tokens
                .get(1)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let duration = std::time::Duration::from_secs_f64(dur);
            Ok((None, duration, None))
        }
        "exit" => {
            let code = tokens
                .get(1)
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);
            Ok((None, std::time::Duration::ZERO, Some(code)))
        }
        "printf" => {
            let body = extract_body(&core, "printf %s");
            let expanded = expand_env(&body, ctx);
            let data = expanded.into_bytes();
            if let Some(path_str) = redirect {
                let target = resolve_write(resolver, ctx, &path_str)?;
                Ok((
                    Some(CommandAction::Write { target, data }),
                    std::time::Duration::ZERO,
                    None,
                ))
            } else if capture {
                Ok((
                    Some(CommandAction::Stdout { data }),
                    std::time::Duration::ZERO,
                    None,
                ))
            } else {
                Ok((None, std::time::Duration::ZERO, None))
            }
        }
        "echo" => {
            let body = core.strip_prefix("echo").unwrap_or("").trim();
            let expanded = expand_env(body, ctx);
            let mut data = expanded.into_bytes();
            data.push(b'\n');
            if let Some(path_str) = redirect {
                let target = resolve_write(resolver, ctx, &path_str)?;
                Ok((
                    Some(CommandAction::Write { target, data }),
                    std::time::Duration::ZERO,
                    None,
                ))
            } else if capture {
                Ok((
                    Some(CommandAction::Stdout { data }),
                    std::time::Duration::ZERO,
                    None,
                ))
            } else {
                Ok((None, std::time::Duration::ZERO, None))
            }
        }
        _ => {
            // Fallback: treat as no-op success so Miri tests can proceed.
            Ok((None, std::time::Duration::ZERO, None))
        }
    }
}

#[cfg(miri)]
fn resolve_write(resolver: &PathResolver, ctx: &CommandContext, path: &str) -> Result<GuardedPath> {
    match ctx.cwd() {
        PolicyPath::Guarded(p) => resolver.resolve_write(p, path),
        PolicyPath::Unguarded(_) => bail!("unguarded writes not supported in Miri"),
    }
}

#[cfg(miri)]
fn split_redirect(cmd: &str) -> (String, Option<String>) {
    if let Some(idx) = cmd.find('>') {
        let (left, right) = cmd.split_at(idx);
        let path = right.trim_start_matches('>').trim();
        (left.trim().to_string(), Some(path.to_string()))
    } else {
        (cmd.trim().to_string(), None)
    }
}

#[cfg(miri)]
fn extract_body(cmd: &str, prefix: &str) -> String {
    cmd.strip_prefix(prefix)
        .unwrap_or(cmd)
        .trim()
        .trim_matches('"')
        .to_string()
}

#[cfg(miri)]
fn expand_env(input: &str, ctx: &CommandContext) -> String {
    // First expand any double-brace names ({{ ... }}), then expand simple
    // shell-style `$VAR` and `${VAR}` occurrences so the synthetic Miri
    // process emulation behaves like a real shell with respect to env vars.
    let first = expand_with_lookup(input, |name| Some(env_lookup(name, ctx)));

    let mut out = String::with_capacity(first.len());
    let mut chars = first.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            match chars.peek() {
                Some('$') => {
                    // Preserve literal $$ (never a PID expansion here)
                    out.push_str("$$");
                    chars.next();
                }
                Some('{') => {
                    // ${VAR}
                    chars.next(); // consume '{'
                    let mut name = String::new();
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch == '}' {
                            break;
                        }
                        name.push(ch);
                    }
                    let val = if name == "CARGO_TARGET_DIR" {
                        ctx.cargo_target_dir().display().to_string()
                    } else {
                        ctx.envs()
                            .get(&name)
                            .cloned()
                            .or_else(|| std::env::var(&name).ok())
                            .unwrap_or_default()
                    };
                    out.push_str(&val);
                }
                Some(next) if next.is_alphanumeric() || *next == '_' => {
                    // $VAR
                    let mut name = String::new();
                    while let Some(&ch) = chars.peek() {
                        if ch.is_alphanumeric() || ch == '_' {
                            name.push(ch);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    let val = if name == "CARGO_TARGET_DIR" {
                        ctx.cargo_target_dir().display().to_string()
                    } else {
                        ctx.envs()
                            .get(&name)
                            .cloned()
                            .or_else(|| std::env::var(&name).ok())
                            .unwrap_or_default()
                    };
                    out.push_str(&val);
                }
                _ => {
                    // Not a recognized var form; keep literal '$'
                    out.push('$');
                }
            }
        } else {
            out.push(c);
        }
    }

    out
}

#[cfg(miri)]
fn env_lookup(name: &str, ctx: &CommandContext) -> String {
    // Accept both `{{ env:X }}` and bare `{{ X }}` forms, matching the
    // host-side `expand_command_env` semantics.
    let key = name.strip_prefix("env:").unwrap_or(name);
    if key == "CARGO_TARGET_DIR" {
        return ctx.cargo_target_dir().display().to_string();
    }
    ctx.envs()
        .get(key)
        .cloned()
        .or_else(|| std::env::var(key).ok())
        .unwrap_or_default()
}

#[cfg(miri)]
fn normalize_shell(script: &str) -> String {
    let trimmed = script.trim();
    if let Some(rest) = trimmed.strip_prefix("sh -c ") {
        return rest.trim_matches(&['"', '\''] as &[_]).to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("cmd /C ") {
        return rest.trim_matches(&['"', '\''] as &[_]).to_string();
    }
    trimmed.to_string()
}

#[cfg(miri)]
fn apply_actions(ctx: &CommandContext, actions: &[Action]) -> Result<()> {
    let resolver = PathResolver::new(
        ctx.workspace_root().as_path(),
        ctx.build_context().as_path(),
    )?;
    for action in actions {
        match action {
            Action::WriteFile { target, data } => {
                if let Some(parent) = target.as_path().parent() {
                    let parent_guard = GuardedPath::new(target.root(), parent)?;
                    resolver.create_dir_all(&parent_guard)?;
                }
                resolver.write_file(target, data)?;
            }
        }
    }
    Ok(())
}

#[cfg(miri)]
fn exit_status_from_code(code: i32) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatusExt::from_raw(code << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatusExt::from_raw(code as u32)
    }
}

#[cfg(not(miri))]
pub type DefaultProcessManager = ShellProcessManager;

#[cfg(miri)]
pub type DefaultProcessManager = SyntheticProcessManager;

pub fn default_process_manager() -> DefaultProcessManager {
    DefaultProcessManager::default()
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

    Ok(ChildHandle { child, io_threads })
}

/// Builder wrapper that centralizes direct usages of `std::process::Command`.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub struct CommandBuilder {
    inner: ProcessCommand,
    program: OsString,
    args: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
    cwd: Option<PathBuf>,
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
impl CommandBuilder {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        let prog = program.as_ref().to_os_string();
        Self {
            inner: ProcessCommand::new(&prog),
            program: prog,
            args: Vec::new(),
            envs: Vec::new(),
            cwd: None,
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        let val = arg.as_ref().to_os_string();
        self.inner.arg(&val);
        self.args.push(val);
        self
    }

    pub fn args<S, I>(&mut self, args: I) -> &mut Self
    where
        S: AsRef<OsStr>,
        I: IntoIterator<Item = S>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        let key = key.as_ref().to_os_string();
        let value = value.as_ref().to_os_string();
        self.inner.env(&key, &value);
        self.envs.retain(|(k, _)| k != &key);
        self.envs.push((key, value));
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        let key = key.as_ref();
        self.inner.env_remove(key);
        self.envs.retain(|(k, _)| k != key);
        self
    }

    pub fn stdin_file(&mut self, file: File) -> &mut Self {
        self.inner.stdin(Stdio::from(file));
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        let path = dir.as_ref();
        self.inner.current_dir(path);
        self.cwd = Some(path.to_path_buf());
        self
    }

    pub fn status(&mut self) -> Result<ExitStatus> {
        #[cfg(miri)]
        {
            let snap = self.snapshot();
            synthetic_status(&snap)
        }

        #[cfg(not(miri))]
        {
            let desc = format!("{:?}", self.inner);
            let status = self
                .inner
                .status()
                .with_context(|| format!("failed to run {desc}"))?;
            Ok(status)
        }
    }

    pub fn output(&mut self) -> Result<CommandOutput> {
        #[cfg(miri)]
        {
            let snap = self.snapshot();
            synthetic_output(&snap)
        }

        #[cfg(not(miri))]
        {
            let desc = format!("{:?}", self.inner);
            let out = self
                .inner
                .output()
                .with_context(|| format!("failed to run {desc}"))?;
            Ok(CommandOutput::from(out))
        }
    }

    pub fn spawn(&mut self) -> Result<ChildHandle> {
        #[cfg(miri)]
        {
            bail!("spawn is not supported under miri synthetic process backend")
        }

        #[cfg(not(miri))]
        {
            let desc = format!("{:?}", self.inner);
            let child = self
                .inner
                .spawn()
                .with_context(|| format!("failed to spawn {desc}"))?;
            Ok(ChildHandle {
                child,
                io_threads: Vec::new(),
            })
        }
    }

    /// Return a lightweight snapshot of the command configuration for testing.
    pub fn snapshot(&self) -> CommandSnapshot {
        CommandSnapshot {
            program: self.program.clone(),
            args: self.args.clone(),
            envs: self.envs.clone(),
            cwd: self.cwd.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub struct CommandSnapshot {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub envs: Vec<(OsString, OsString)>,
    pub cwd: Option<PathBuf>,
}

pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.status.success()
    }
}

#[allow(clippy::disallowed_types)]
impl From<StdOutput> for CommandOutput {
    fn from(value: StdOutput) -> Self {
        Self {
            status: value.status,
            stdout: value.stdout,
            stderr: value.stderr,
        }
    }
}

#[cfg(miri)]
fn synthetic_status(snapshot: &CommandSnapshot) -> Result<ExitStatus> {
    Ok(synthetic_output(snapshot)?.status)
}

#[cfg(miri)]
fn synthetic_output(snapshot: &CommandSnapshot) -> Result<CommandOutput> {
    let program = snapshot.program.to_string_lossy().to_string();
    let args: Vec<String> = snapshot
        .args
        .iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect();

    if program == "git" {
        return simulate_git(&args);
    }
    if program == "cargo" {
        return simulate_cargo(&args);
    }

    Ok(CommandOutput {
        status: exit_status_from_code(0),
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

#[cfg(miri)]
fn simulate_git(args: &[String]) -> Result<CommandOutput> {
    let mut iter = args.iter();
    if matches!(iter.next(), Some(arg) if arg == "-C") {
        let _ = iter.next();
    }
    let remaining: Vec<String> = iter.map(|s| s.to_string()).collect();

    if remaining.len() >= 2 && remaining[0] == "rev-parse" && remaining[1] == "HEAD" {
        return Ok(CommandOutput {
            status: exit_status_from_code(0),
            stdout: b"HEAD\n".to_vec(),
            stderr: Vec::new(),
        });
    }

    // Default success for init/add/commit and other read-only queries.
    Ok(CommandOutput {
        status: exit_status_from_code(0),
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

#[cfg(miri)]
fn simulate_cargo(args: &[String]) -> Result<CommandOutput> {
    // Heuristic: manifests containing "build_exit_fail" should fail to mimic fixture.
    let mut status = exit_status_from_code(0);
    if args.iter().any(|a| a.contains("build_exit_fail")) {
        status = exit_status_from_code(1);
    }
    Ok(CommandOutput {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxdock_sys_test_utils::TestEnvGuard;
    use std::collections::HashMap;

    #[test]
    fn expand_script_env_prefers_script_values() {
        let mut script_envs = HashMap::new();
        script_envs.insert("FOO".into(), "from-script".into());
        script_envs.insert("ONLY".into(), "only".into());
        let _env_guard = TestEnvGuard::set("FOO", "from-env");
        let rendered = expand_script_env(
            "{{ env:FOO }}:{{ env:ONLY }}:{{ env:MISSING }}",
            &script_envs,
        );
        assert_eq!(rendered, "from-script:only:");
    }

    #[test]
    fn expand_script_env_supports_colon_separator() {
        let mut script_envs = HashMap::new();
        script_envs.insert("FOO".into(), "val".into());
        let rendered = expand_script_env("{{ env:FOO }}", &script_envs);
        assert_eq!(rendered, "val");
    }

    #[test]
    fn expand_command_env_handles_var_forms() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let guard = temp.as_guarded_path().clone();
        let cwd: PolicyPath = guard.clone().into();
        let mut envs = HashMap::new();
        envs.insert("FOO".into(), "bar".into());
        envs.insert("PCT".into(), "percent".into());
        envs.insert("CARGO_TARGET_DIR".into(), guard.display().to_string());
        envs.insert("HOST_ONLY".into(), "host".into());

        let ctx = CommandContext::new(&cwd, &envs, &guard, &guard, &guard);

        // Valid syntax: {{ env:VAR }}
        let rendered = expand_command_env(
            "{{ env:FOO }}-{{ env:PCT }}-{{ env:HOST_ONLY }}-{{ env:CARGO_TARGET_DIR }}",
            &ctx,
        );
        assert_eq!(rendered, format!("bar-percent-host-{}", guard.display()));

        // Invalid/Legacy syntax: treated as literal text
        // %FOO% -> %FOO%
        // {CARGO_TARGET_DIR} -> {CARGO_TARGET_DIR}
        // $$ -> $$
        let input_literal = "%FOO%-{CARGO_TARGET_DIR}-$$";
        let rendered_literal = expand_command_env(input_literal, &ctx);
        assert_eq!(rendered_literal, input_literal);
    }

    #[test]
    fn expand_command_env_does_not_fallback_to_host() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let guard = temp.as_guarded_path().clone();
        let cwd: PolicyPath = guard.clone().into();
        let envs = HashMap::new();
        let _env_guard = TestEnvGuard::set("HOST_ONLY", "host");

        let ctx = CommandContext::new(&cwd, &envs, &guard, &guard, &guard);
        let rendered = expand_command_env("{{ env:HOST_ONLY }}", &ctx);
        assert_eq!(rendered, "");
    }

    #[test]
    fn expand_with_lookup_handles_multibyte_placeholder_names() {
        // Regression: advancement used to count bytes instead of chars, so a
        // multi-byte placeholder name swallowed characters after `}}`.
        let rendered = expand_with_lookup("{{ env:héllo }}X", |name| {
            if name == "env:héllo" {
                Some("value".to_string())
            } else {
                None
            }
        });
        assert_eq!(rendered, "valueX");
    }

    #[test]
    fn expand_with_lookup_preserves_multibyte_text_outside_placeholders() {
        let rendered = expand_with_lookup("héllo wörld {{ env:A }} ✓", |name| {
            if name == "env:A" {
                Some("1".to_string())
            } else {
                None
            }
        });
        assert_eq!(rendered, "héllo wörld 1 ✓");
    }

    #[test]
    fn expand_with_lookup_keeps_unclosed_double_brace_literal() {
        let rendered = expand_with_lookup("a {{ b", |_| -> Option<String> {
            panic!("input without any closing braces must not produce lookups")
        });
        assert_eq!(rendered, "a {{ b");
    }

    #[test]
    fn expand_with_lookup_binds_first_open_to_next_close_across_text() {
        // Pins current greedy behavior: the first `{{` binds to the next `}}`
        // even across an interior `{{`, and the entire span is trimmed into a
        // single lookup key. With no resolver entry for that composite key,
        // the whole placeholder renders as empty text.
        let seen = std::cell::RefCell::new(None);
        let rendered = expand_with_lookup("a {{ b {{ env:X }} c", |name| {
            *seen.borrow_mut() = Some(name.to_string());
            if name == "env:X" {
                Some("V".to_string())
            } else {
                None
            }
        });
        assert_eq!(rendered, "a  c");
        assert_eq!(seen.borrow().as_deref(), Some("b {{ env:X"));
    }

    #[test]
    fn expand_with_lookup_skips_empty_and_blank_keys() {
        let rendered = expand_with_lookup("x{{}}y", |_| -> Option<String> {
            panic!("empty key must not be looked up")
        });
        assert_eq!(rendered, "xy");

        let rendered_blank = expand_with_lookup("x{{   }}y", |_| -> Option<String> {
            panic!("blank key must not be looked up")
        });
        assert_eq!(rendered_blank, "xy");
    }

    #[test]
    fn expand_with_lookup_passes_through_stray_braces() {
        let rendered_close = expand_with_lookup("a }} b", |_| -> Option<String> {
            panic!("stray closing braces must not be looked up")
        });
        assert_eq!(rendered_close, "a }} b");

        let rendered_single = expand_with_lookup("{ alone {", |_| -> Option<String> {
            panic!("single brace must not be looked up")
        });
        assert_eq!(rendered_single, "{ alone {");
    }

    #[test]
    fn expand_with_lookup_supports_adjacent_placeholders() {
        let rendered = expand_with_lookup("{{ env:A }}{{ env:B }}", |name| match name {
            "env:A" => Some("a".to_string()),
            "env:B" => Some("b".to_string()),
            _ => None,
        });
        assert_eq!(rendered, "ab");
    }

    // ---------- ShellProcessManager / internals ----------

    fn make_ctx(envs: &[(&str, &str)]) -> (oxdock_fs::GuardedTempDir, CommandContext) {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let guard = temp.as_guarded_path().clone();
        let cwd: PolicyPath = guard.clone().into();
        let map: HashMap<String, String> = envs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        let ctx = CommandContext::new(&cwd, &map, &guard, &guard, &guard);
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

    #[cfg_attr(
        miri,
        ignore = "spawns processes; Miri does not support process execution"
    )]
    #[test]
    fn child_handle_background_lifecycle_polls_then_waits() {
        let (_temp, ctx) = make_ctx(&[]);
        let mut pm = ShellProcessManager;
        let options = CommandOptions {
            mode: CommandMode::Background,
            ..Default::default()
        };
        let mut handle = match pm.run_command(&ctx, "exit 0", options).expect("run") {
            CommandResult::Background(handle) => handle,
            CommandResult::Completed => panic!("expected Background, got Completed"),
            CommandResult::Captured(_) => panic!("expected Background, got Captured"),
        };

        let mut status = None;
        for _ in 0..500 {
            if let Some(done) = handle.try_wait().expect("try_wait") {
                status = Some(done);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let status = status.expect("child should exit within polling window");
        assert!(status.success());

        let waited = handle.wait().expect("wait");
        assert!(waited.success());
    }

    fn long_running_script() -> &'static str {
        #[cfg(windows)]
        {
            "ping -n 30 127.0.0.1 >NUL"
        }
        #[cfg(not(windows))]
        {
            "sleep 30"
        }
    }

    #[cfg_attr(
        miri,
        ignore = "spawns processes; Miri does not support process execution"
    )]
    #[test]
    fn child_handle_kill_is_idempotent_and_wait_joins_threads() {
        let (_temp, ctx) = make_ctx(&[]);
        let mut pm = ShellProcessManager;
        let options = CommandOptions {
            mode: CommandMode::Background,
            stdout: CommandStdout::Stream(std::sync::Arc::new(std::sync::Mutex::new(
                Vec::<u8>::new(),
            ))),
            ..Default::default()
        };
        let mut handle = match pm
            .run_command(&ctx, long_running_script(), options)
            .expect("run")
        {
            CommandResult::Background(handle) => handle,
            CommandResult::Completed => panic!("expected Background, got Completed"),
            CommandResult::Captured(_) => panic!("expected Background, got Captured"),
        };

        handle.kill().expect("first kill");
        handle.kill().expect("second kill must be idempotent");
        let _status = handle.wait().expect("wait after kill");
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

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    #[test]
    fn command_builder_snapshot_tracks_configuration() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let dir = temp.as_guarded_path().display().to_string();

        let mut builder = CommandBuilder::new("prog");
        builder.arg("a").args(["b", "c"]);
        builder.env("K", "V");
        builder.env("K", "V2"); // re-set replaces the earlier entry
        builder.env("GONE", "x");
        builder.env_remove("GONE");
        builder.current_dir(&dir);

        let snap = builder.snapshot();
        assert_eq!(snap.program, OsString::from("prog"));
        assert_eq!(
            snap.args,
            vec![
                OsString::from("a"),
                OsString::from("b"),
                OsString::from("c")
            ]
        );
        assert!(
            snap.envs
                .contains(&(OsString::from("K"), OsString::from("V2")))
        );
        assert!(
            !snap.envs.iter().any(|(k, _)| k == "GONE"),
            "env_remove must drop tracked entries"
        );
        assert_eq!(snap.cwd.as_deref(), Some(std::path::Path::new(&dir)));
    }
}

// Unit tests for the Miri-only synthetic process backend. This module only
// compiles under Miri, where the CI `cargo miri test` job exercises it.
#[cfg(all(miri, test))]
mod synthetic_backend_tests {
    use super::*;
    use std::collections::HashMap;

    fn miri_ctx(envs: &[(&str, &str)]) -> (GuardedPath, CommandContext) {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let guard = temp.as_guarded_path().clone();
        let cwd: PolicyPath = guard.clone().into();
        let map: HashMap<String, String> = envs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        let ctx = CommandContext::new(&cwd, &map, &guard, &guard, &guard);
        (guard, ctx)
    }

    #[test]
    fn split_redirect_separates_target_and_trims() {
        assert_eq!(
            split_redirect("echo hi > out.txt"),
            ("echo hi".to_string(), Some("out.txt".to_string()))
        );
        assert_eq!(
            split_redirect("echo hi >> app.txt"),
            ("echo hi".to_string(), Some("app.txt".to_string()))
        );
        assert_eq!(split_redirect("  echo hi  "), ("echo hi".to_string(), None));
    }

    #[test]
    fn normalize_shell_strips_platform_wrappers() {
        assert_eq!(normalize_shell("sh -c \"echo hi\""), "echo hi");
        assert_eq!(normalize_shell("cmd /C \"echo hi\""), "echo hi");
        assert_eq!(normalize_shell("  echo hi "), "echo hi");
    }

    #[test]
    fn expand_env_supports_brace_and_dollar_forms() {
        let (_root, ctx) = miri_ctx(&[("FOO", "bar")]);

        assert_eq!(expand_env("{{ env:FOO }}!", &ctx), "bar!");
        assert_eq!(expand_env("$FOO!", &ctx), "bar!");
        assert_eq!(expand_env("${FOO}!", &ctx), "bar!");
        assert_eq!(expand_env("cost $$5", &ctx), "cost $$5");
        assert_eq!(expand_env("[${MISSING_XYZ}]", &ctx), "[]");

        let target = expand_env("${CARGO_TARGET_DIR}", &ctx);
        assert!(!target.is_empty(), "executor default must resolve");
    }

    #[test]
    fn parse_command_interprets_sleep_exit_and_echo() {
        let (_root, ctx) = miri_ctx(&[]);
        let resolver = PathResolver::new(
            ctx.workspace_root().as_path(),
            ctx.build_context().as_path(),
        )
        .expect("resolver");

        let (action, duration, code) =
            parse_command("sleep 0.05", &ctx, &resolver, false).expect("sleep");
        assert!(action.is_none());
        assert_eq!(duration, std::time::Duration::from_millis(50));
        assert!(code.is_none());

        let (action, _, code) = parse_command("exit 7", &ctx, &resolver, false).expect("exit");
        assert!(action.is_none());
        assert_eq!(code, Some(7));

        let (action, _, _) = parse_command("echo hi", &ctx, &resolver, true).expect("echo");
        match action {
            Some(CommandAction::Stdout { data }) => assert_eq!(data, b"hi\n"),
            _ => panic!("expected stdout action for captured echo"),
        }

        // Without capture, echo is a no-op.
        let (action, _, _) = parse_command("echo hi", &ctx, &resolver, false).expect("echo quiet");
        assert!(action.is_none());
    }

    #[test]
    fn parse_command_unknown_commands_are_noop_success() {
        let (_root, ctx) = miri_ctx(&[]);
        let resolver = PathResolver::new(
            ctx.workspace_root().as_path(),
            ctx.build_context().as_path(),
        )
        .expect("resolver");

        let (action, duration, code) =
            parse_command("cargo build --release", &ctx, &resolver, true).expect("unknown");
        assert!(action.is_none());
        assert_eq!(duration, std::time::Duration::ZERO);
        assert!(code.is_none());
    }

    #[test]
    fn parse_command_printf_redirects_write_into_workspace() {
        let (_root, ctx) = miri_ctx(&[]);
        let resolver = PathResolver::new(
            ctx.workspace_root().as_path(),
            ctx.build_context().as_path(),
        )
        .expect("resolver");

        let (action, _, _) = parse_command(
            "printf %s \"file-body\" > nested/out.txt",
            &ctx,
            &resolver,
            false,
        )
        .expect("redirect write");

        match action {
            Some(CommandAction::Write { target, data }) => {
                assert_eq!(data, b"file-body");
                assert!(target.as_path().starts_with(ctx.workspace_root().as_path()));
                assert!(target.as_path().ends_with("nested/out.txt"));
            }
            _ => panic!("expected write action"),
        }
    }
}
