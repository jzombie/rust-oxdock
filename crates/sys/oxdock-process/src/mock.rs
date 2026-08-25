use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
#[allow(clippy::disallowed_types)]
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{Result, anyhow, bail};
use oxdock_sys_test_utils::exit_status_from_code;

use crate::{
    BackgroundHandle, CommandContext, CommandMode, CommandOptions, CommandResult, CommandStderr,
    CommandStdout, ProcessManager, SharedInput,
};

/// Which stderr configuration an invocation carried. A discriminant rather
/// than the raw `CommandStderr` so the captured calls stay `PartialEq`/`Eq`
/// (stream handles are not comparable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockStreamMode {
    Inherit,
    Stream,
}

fn stderr_mode(stderr: &CommandStderr) -> MockStreamMode {
    match stderr {
        CommandStderr::Inherit => MockStreamMode::Inherit,
        CommandStderr::Stream(_) => MockStreamMode::Stream,
    }
}

/// Captured invocation for a foreground run.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::disallowed_types)]
pub struct MockRunCall {
    pub script: String,
    pub cwd: PathBuf,
    pub envs: HashMap<String, String>,
    pub cargo_target_dir: PathBuf,
    pub stdin_provided: bool,
    pub stdin: Option<Vec<u8>>,
    pub stderr_mode: MockStreamMode,
}

/// Captured invocation for a background spawn.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::disallowed_types)]
pub struct MockSpawnCall {
    pub script: String,
    pub cwd: PathBuf,
    pub envs: HashMap<String, String>,
    pub cargo_target_dir: PathBuf,
    pub stdin_provided: bool,
    pub stdin: Option<Vec<u8>>,
    pub stderr_mode: MockStreamMode,
}

#[derive(Clone, Default)]
pub struct MockProcessManager {
    runs: Rc<RefCell<Vec<MockRunCall>>>,
    spawns: Rc<RefCell<Vec<MockSpawnCall>>>,
    killed: Rc<RefCell<Vec<String>>>,
    plans: Rc<RefCell<VecDeque<BgPlan>>>,
}

impl MockProcessManager {
    pub fn recorded_runs(&self) -> Vec<MockRunCall> {
        self.runs.borrow().clone()
    }

    pub fn spawn_log(&self) -> Vec<MockSpawnCall> {
        self.spawns.borrow().clone()
    }

    pub fn killed(&self) -> Vec<String> {
        self.killed.borrow().clone()
    }

    pub fn push_bg_plan(&self, ready_after: usize, status: std::process::ExitStatus) {
        self.plans.borrow_mut().push_back(BgPlan {
            ready_after,
            status,
        });
    }
}

impl ProcessManager for MockProcessManager {
    type Handle = MockHandle;

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
        let stdin_provided = stdin.is_some();
        let captured_stdin = capture_stdin(stdin)?;
        let recorded_stderr = stderr_mode(&stderr);

        match mode {
            CommandMode::Foreground => {
                self.runs.borrow_mut().push(MockRunCall {
                    script: script.to_string(),
                    cwd: ctx.cwd().to_path_buf(),
                    envs: (**ctx.envs()).clone(),
                    cargo_target_dir: ctx.cargo_target_dir().to_path_buf(),
                    stdin_provided,
                    stdin: captured_stdin.clone(),
                    stderr_mode: recorded_stderr,
                });
                match stdout {
                    CommandStdout::Capture => Ok(CommandResult::Captured(Vec::new())),
                    CommandStdout::Stream(_) | CommandStdout::Inherit => {
                        Ok(CommandResult::Completed)
                    }
                }
            }
            CommandMode::Background => {
                if matches!(stdout, CommandStdout::Capture) {
                    bail!("cannot capture stdout for background command");
                }
                self.spawns.borrow_mut().push(MockSpawnCall {
                    script: script.to_string(),
                    cwd: ctx.cwd().to_path_buf(),
                    envs: (**ctx.envs()).clone(),
                    cargo_target_dir: ctx.cargo_target_dir().to_path_buf(),
                    stdin_provided,
                    stdin: captured_stdin.clone(),
                    stderr_mode: recorded_stderr,
                });
                let plan = self
                    .plans
                    .borrow_mut()
                    .pop_front()
                    .unwrap_or_else(BgPlan::success);
                Ok(CommandResult::Background(MockHandle {
                    script: script.to_string(),
                    remaining: plan.ready_after,
                    status: plan.status,
                    killed: self.killed.clone(),
                    reaped: false,
                }))
            }
        }
    }
}

struct BgPlan {
    ready_after: usize,
    status: std::process::ExitStatus,
}

impl BgPlan {
    fn success() -> Self {
        Self {
            ready_after: 0,
            status: exit_status_from_code(0),
        }
    }
}

#[derive(Clone)]
pub struct MockHandle {
    script: String,
    remaining: usize,
    status: std::process::ExitStatus,
    killed: Rc<RefCell<Vec<String>>>,
    reaped: bool,
}

impl BackgroundHandle for MockHandle {
    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        if self.remaining == 0 {
            self.reaped = true;
            Ok(Some(self.status))
        } else {
            self.remaining -= 1;
            Ok(None)
        }
    }

    fn kill(&mut self) -> Result<()> {
        self.reaped = true;
        self.killed.borrow_mut().push(self.script.clone());
        Ok(())
    }

    fn wait(&mut self) -> Result<std::process::ExitStatus> {
        self.reaped = true;
        Ok(self.status)
    }
}

impl Drop for MockHandle {
    fn drop(&mut self) {
        // Only naturally-completed-without-teardown handles reach here
        // un-reaped; log them as killed so `pm.killed()` assertions observe
        // Drop-driven teardown exactly like explicit kills.
        if !self.reaped {
            self.killed.borrow_mut().push(self.script.clone());
        }
    }
}

fn capture_stdin(stdin: Option<SharedInput>) -> Result<Option<Vec<u8>>> {
    if let Some(reader) = stdin {
        let mut guard = reader.lock().map_err(|_| anyhow!("failed to lock stdin"))?;
        let mut buf = Vec::new();
        std::io::copy(&mut *guard, &mut buf)?;
        Ok(Some(buf))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxdock_fs::{GuardedPath, PolicyPath};
    use std::collections::HashMap;

    fn test_ctx() -> (GuardedPath, CommandContext) {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let guard = temp.as_guarded_path().clone();
        let cwd: PolicyPath = guard.clone().into();
        let ctx = CommandContext::from_map(&cwd, &HashMap::new(), &guard, &guard, &guard);
        (guard, ctx)
    }

    fn ok_status() -> std::process::ExitStatus {
        exit_status_from_code(0)
    }

    fn failing_status() -> std::process::ExitStatus {
        exit_status_from_code(3)
    }

    #[test]
    fn foreground_run_records_invocation_and_stderr_mode() {
        let (_root, ctx) = test_ctx();
        let mut pm = MockProcessManager::default();
        let input: SharedInput =
            std::sync::Arc::new(std::sync::Mutex::new(std::io::Cursor::new(b"abc".to_vec())));
        let stderr_sink: crate::SharedOutput =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let options = CommandOptions {
            stdin: Some(input),
            stderr: CommandStderr::Stream(stderr_sink),
            ..Default::default()
        };

        pm.run_command(&ctx, "build-all", options).expect("run");

        let runs = pm.recorded_runs();
        assert_eq!(runs.len(), 1);
        let call = &runs[0];
        assert_eq!(call.script, "build-all");
        assert_eq!(call.cwd, ctx.cwd().to_path_buf());
        assert!(call.stdin_provided);
        assert_eq!(call.stdin.as_deref(), Some(b"abc".as_slice()));
        assert_eq!(call.stderr_mode, MockStreamMode::Stream);
    }

    #[test]
    fn background_spawn_records_stderr_inherit_mode_by_default() {
        let (_root, ctx) = test_ctx();
        let mut pm = MockProcessManager::default();
        let options = CommandOptions {
            mode: CommandMode::Background,
            ..Default::default()
        };

        match pm.run_command(&ctx, "serve", options).expect("spawn") {
            CommandResult::Background(_) => {}
            _ => panic!("expected background handle"),
        }

        let spawns = pm.spawn_log();
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0].script, "serve");
        assert!(!spawns[0].stdin_provided);
        assert_eq!(spawns[0].stderr_mode, MockStreamMode::Inherit);
    }

    #[test]
    fn background_capture_stdout_bails() {
        let (_root, ctx) = test_ctx();
        let mut pm = MockProcessManager::default();
        let options = CommandOptions {
            mode: CommandMode::Background,
            stdout: CommandStdout::Capture,
            ..Default::default()
        };
        match pm.run_command(&ctx, "x", options) {
            Err(err) => assert!(err.to_string().contains("cannot capture stdout")),
            _ => panic!("expected bail for background capture"),
        }
    }

    #[test]
    fn try_wait_counts_down_until_planned_readiness() {
        let (_root, ctx) = test_ctx();
        let mut pm = MockProcessManager::default();
        pm.push_bg_plan(2, failing_status());
        let options = CommandOptions {
            mode: CommandMode::Background,
            ..Default::default()
        };
        let mut handle = match pm.run_command(&ctx, "job", options).expect("spawn") {
            CommandResult::Background(handle) => handle,
            _ => panic!("expected background handle"),
        };

        assert!(handle.try_wait().expect("poll 1").is_none());
        assert!(handle.try_wait().expect("poll 2").is_none());
        assert_eq!(handle.try_wait().expect("poll 3"), Some(failing_status()));
    }

    #[test]
    fn wait_returns_planned_status_even_before_countdown_finishes() {
        let (_root, ctx) = test_ctx();
        let mut pm = MockProcessManager::default();
        // usize::MAX models "never becomes ready via try_wait" while `wait`
        // still reports the planned status (executor teardown semantics).
        pm.push_bg_plan(usize::MAX, failing_status());
        let options = CommandOptions {
            mode: CommandMode::Background,
            ..Default::default()
        };
        let mut handle = match pm.run_command(&ctx, "stuck", options).expect("spawn") {
            CommandResult::Background(handle) => handle,
            _ => panic!("expected background handle"),
        };
        assert!(handle.try_wait().expect("poll").is_none());
        assert_eq!(handle.wait().expect("wait"), failing_status());
    }

    #[test]
    fn missing_plan_defaults_to_immediate_success() {
        let (_root, ctx) = test_ctx();
        let mut pm = MockProcessManager::default();
        let options = CommandOptions {
            mode: CommandMode::Background,
            ..Default::default()
        };
        let mut handle = match pm.run_command(&ctx, "quick", options).expect("spawn") {
            CommandResult::Background(handle) => handle,
            _ => panic!("expected background handle"),
        };
        assert_eq!(handle.try_wait().expect("poll"), Some(ok_status()));
    }

    #[test]
    fn kill_logs_scripts_in_kill_order() {
        let (_root, ctx) = test_ctx();
        let mut pm = MockProcessManager::default();
        let options = CommandOptions {
            mode: CommandMode::Background,
            ..Default::default()
        };
        let mut a = match pm.run_command(&ctx, "a", options.clone()).expect("spawn") {
            CommandResult::Background(handle) => handle,
            _ => panic!("expected background handle"),
        };
        let mut b = match pm.run_command(&ctx, "b", options).expect("spawn") {
            CommandResult::Background(handle) => handle,
            _ => panic!("expected background handle"),
        };

        b.kill().expect("kill b");
        a.kill().expect("kill a");

        assert_eq!(pm.killed(), vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn capture_stdin_reads_stream_or_reports_none() {
        let none = capture_stdin(None).expect("none case");
        assert_eq!(none, None);

        let input: SharedInput =
            std::sync::Arc::new(std::sync::Mutex::new(std::io::Cursor::new(b"xyz".to_vec())));
        let some = capture_stdin(Some(input)).expect("some case");
        assert_eq!(some, Some(b"xyz".to_vec()));
    }
}
