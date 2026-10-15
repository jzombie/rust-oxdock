//! Recording process manager for script conformance tests: stands in
//! for cargo, git, gh, tar, and installers so the real `.oxfile`
//! pipelines execute end to end with zero side effects. Every
//! invocation lands in one shared log; canned stdout is identical
//! everywhere, which is exactly what equality gates compare. Tests
//! needing distinct outputs per command script them with
//! [`RecordingManager::respond`]: first registered substring match
//! wins, so register specific needles before general ones.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One intercepted process invocation: exec-form argv or shell-form text.
#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    Argv(Vec<String>),
    Shell(String),
}

/// Scripted stdout for one shell invocation shape, matched by
/// substring. `repeat` entries answer every match (registry probes
/// re-queried each round); one-shot entries answer once, then fall
/// through to the next match (a failed round followed by recovery).
struct Responder {
    needle: String,
    stdout: Vec<u8>,
    repeat: bool,
}

/// Process manager that records instead of spawning. Clones share one
/// log, so engine-internal clones stay visible to the test.
#[derive(Clone)]
pub struct RecordingManager {
    log: Arc<Mutex<Vec<Call>>>,
    env_log: Arc<Mutex<Vec<HashMap<String, String>>>>,
    responders: Arc<Mutex<Vec<Responder>>>,
}

impl RecordingManager {
    pub fn new() -> Self {
        Self {
            log: Arc::new(Mutex::new(Vec::new())),
            env_log: Arc::new(Mutex::new(Vec::new())),
            responders: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Answer every shell invocation containing `needle` with `stdout`.
    pub fn respond(&self, needle: &str, stdout: &str) {
        self.responders.lock().expect("responders").push(Responder {
            needle: needle.to_string(),
            stdout: stdout.as_bytes().to_vec(),
            repeat: true,
        });
    }

    /// Answer the next shell invocation containing `needle` with
    /// `stdout`, then stop matching it.
    pub fn respond_once(&self, needle: &str, stdout: &str) {
        self.responders.lock().expect("responders").push(Responder {
            needle: needle.to_string(),
            stdout: stdout.as_bytes().to_vec(),
            repeat: false,
        });
    }

    /// First registered stdout whose needle matches `script`, consuming
    /// one-shot entries. `None` keeps the historical defaults.
    fn take_response(&self, script: &str) -> Option<Vec<u8>> {
        let mut responders = self.responders.lock().expect("responders");
        let position = responders
            .iter()
            .position(|responder| script.contains(&responder.needle))?;
        let stdout = responders[position].stdout.clone();
        if !responders[position].repeat {
            responders.remove(position);
        }
        Some(stdout)
    }

    pub fn calls(&self) -> Vec<Call> {
        self.log.lock().expect("log").clone()
    }

    /// Staged child env per recorded call, index-aligned with [`Self::calls`]:
    /// exactly the script env the spawn would have received (closed child
    /// env proof reads through here). `Call` shapes stay unchanged so
    /// existing assertions keep compiling.
    pub fn call_envs(&self) -> Vec<HashMap<String, String>> {
        self.env_log.lock().expect("env log").clone()
    }

    fn record(&self, call: Call, ctx: &oxdock_process::CommandContext) {
        self.log.lock().expect("log").push(call);
        self.env_log
            .lock()
            .expect("env log")
            .push(ctx.envs().as_ref().clone());
    }
}

impl Default for RecordingManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Background handle that is never really background: every recorded
/// command completes immediately with success.
#[derive(Clone)]
pub struct RecordingHandle;

fn exit_success() -> std::process::ExitStatus {
    #[cfg(unix)]
    {
        std::os::unix::process::ExitStatusExt::from_raw(0)
    }
    #[cfg(windows)]
    {
        std::os::windows::process::ExitStatusExt::from_raw(0)
    }
}

impl oxdock_process::BackgroundHandle for RecordingHandle {
    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(Some(exit_success()))
    }
    fn kill(&mut self) -> Result<()> {
        Ok(())
    }
    fn wait(&mut self) -> Result<std::process::ExitStatus> {
        Ok(exit_success())
    }
}

impl oxdock_process::ProcessManager for RecordingManager {
    type Handle = RecordingHandle;

    fn run_command(
        &mut self,
        ctx: &oxdock_process::CommandContext,
        script: &str,
        options: oxdock_process::CommandOptions,
    ) -> Result<oxdock_process::CommandResult<Self::Handle>> {
        self.record(Call::Shell(script.to_string()), ctx);
        // Scripted stdout feeds the capture sink behind `LET $x = RUN`:
        // the engine binds sink bytes, never the result value, so the
        // response must be written into the provided writer. Unmatched
        // invocations keep the historical empty behavior.
        let scripted = self.take_response(script);
        match options.stdout {
            oxdock_process::CommandStdout::Capture => Ok(oxdock_process::CommandResult::Captured(
                scripted.unwrap_or_else(|| b"deadbeef\n".to_vec()),
            )),
            oxdock_process::CommandStdout::Stream(writer) => {
                if let Some(stdout) = scripted
                    && let Ok(mut guard) = writer.lock()
                {
                    let _ = std::io::Write::write_all(&mut *guard, &stdout);
                }
                Ok(oxdock_process::CommandResult::Completed)
            }
            _ => Ok(oxdock_process::CommandResult::Completed),
        }
    }

    fn run_argv(
        &mut self,
        ctx: &oxdock_process::CommandContext,
        argv: &[String],
        options: oxdock_process::CommandOptions,
    ) -> Result<oxdock_process::CommandResult<Self::Handle>> {
        self.record(Call::Argv(argv.to_vec()), ctx);
        match options.stdout {
            oxdock_process::CommandStdout::Capture => Ok(oxdock_process::CommandResult::Captured(
                b"deadbeef\n".to_vec(),
            )),
            _ => Ok(oxdock_process::CommandResult::Completed),
        }
    }
}

/// Exec-form invocations from a call log, in order.
pub fn argv_calls(calls: &[Call]) -> Vec<Vec<String>> {
    calls
        .iter()
        .filter_map(|call| match call {
            Call::Argv(argv) => Some(argv.clone()),
            Call::Shell(_) => None,
        })
        .collect()
}

/// Shell-form invocations from a call log, in order.
pub fn shell_calls(calls: &[Call]) -> Vec<&str> {
    calls
        .iter()
        .filter_map(|call| match call {
            Call::Argv(_) => None,
            Call::Shell(script) => Some(script.as_str()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Staged child env stays index-aligned with calls and carries exactly
    /// the script env: a routed name arrives, a host-only name never does.
    /// The manager never spawns, so this pins the plumbing (not the OS
    /// boundary, which `shell_manager` proves with real children).
    #[test]
    fn call_envs_align_with_calls_and_hold_script_env() {
        let steps = oxdock_core::parse_script(
            "INHERIT_ENV [PROBE_KEPT]\nRUN [\"prog\", \"x\"]\nRUN \"echo hi\"\n",
        )
        .expect("probe script parses");
        // Host-only probe: RAII-guarded so a mid-test failure still restores.
        let _probe_guard =
            oxdock_sys_test_utils::TestEnvGuard::set("OXDOCK_RECORDING_PROBE_DROPPED", "host-only");
        let temp = oxdock_fs::GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let setup =
            oxdock_fs::PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");
        let fs: Box<dyn oxdock_fs::WorkspaceFs> = Box::new(setup);
        let manager = RecordingManager::new();
        let mut io = oxdock_core::ExecIo::new();
        io.insert_inherit_env("PROBE_KEPT", "routed");
        oxdock_core::run_steps_with_manager_with_modules(
            fs,
            &steps,
            manager.clone(),
            io,
            Vec::new(),
            Vec::new(),
        )
        .expect("probe script runs");

        let calls = manager.calls();
        let envs = manager.call_envs();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            envs.len(),
            calls.len(),
            "every recorded call stages exactly one env map"
        );
        for staged in &envs {
            assert_eq!(staged.get("PROBE_KEPT").map(String::as_str), Some("routed"));
            assert!(
                !staged.contains_key("OXDOCK_RECORDING_PROBE_DROPPED"),
                "host-only names never stage: {staged:?}"
            );
        }
    }
}
