// Synthetic process manager for Miri. Commands are interpreted with a tiny
// shell that supports the patterns exercised in tests: sleep, printf/echo with
// env interpolation, redirection, and exit codes. IO is routed through the
// workspace filesystem so we never touch the host.
use anyhow::{Result, bail};
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::process::ExitStatus;

use oxdock_fs::{GuardedPath, PathResolver, PolicyPath};
use oxdock_sys_test_utils::exit_status_from_code;

use crate::contract::{
    BackgroundHandle, CommandContext, CommandMode, CommandOptions, CommandResult, CommandStderr,
    CommandStdout, ProcessManager,
};
use crate::expand::expand_with_lookup;

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
impl Drop for SyntheticBgHandle {
    fn drop(&mut self) {
        self.killed = true;
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
pub(crate) fn synthetic_status(snapshot: &crate::builder::CommandSnapshot) -> Result<ExitStatus> {
    Ok(synthetic_output(snapshot)?.status)
}

#[cfg(miri)]
pub(crate) fn synthetic_output(
    snapshot: &crate::builder::CommandSnapshot,
) -> Result<crate::builder::CommandOutput> {
    use crate::builder::CommandOutput;

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
fn simulate_git(args: &[String]) -> Result<crate::builder::CommandOutput> {
    use crate::builder::CommandOutput;

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
fn simulate_cargo(args: &[String]) -> Result<crate::builder::CommandOutput> {
    use crate::builder::CommandOutput;

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
        let ctx = CommandContext::from_map(&cwd, &map, &guard, &guard, &guard);
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
