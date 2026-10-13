//! Recording process manager for script conformance tests: stands in
//! for cargo, git, gh, tar, and installers so the real `.oxfile`
//! pipelines execute end to end with zero side effects. Every
//! invocation lands in one shared log; canned stdout is identical
//! everywhere, which is exactly what equality gates compare. Tests
//! needing distinct outputs per command script them with
//! [`RecordingManager::respond`]: first registered substring match
//! wins, so register specific needles before general ones.

use anyhow::Result;
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
    responders: Arc<Mutex<Vec<Responder>>>,
}

impl RecordingManager {
    pub fn new() -> Self {
        Self {
            log: Arc::new(Mutex::new(Vec::new())),
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
        _ctx: &oxdock_process::CommandContext,
        script: &str,
        options: oxdock_process::CommandOptions,
    ) -> Result<oxdock_process::CommandResult<Self::Handle>> {
        self.log
            .lock()
            .expect("log")
            .push(Call::Shell(script.to_string()));
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
        _ctx: &oxdock_process::CommandContext,
        argv: &[String],
        options: oxdock_process::CommandOptions,
    ) -> Result<oxdock_process::CommandResult<Self::Handle>> {
        self.log
            .lock()
            .expect("log")
            .push(Call::Argv(argv.to_vec()));
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
