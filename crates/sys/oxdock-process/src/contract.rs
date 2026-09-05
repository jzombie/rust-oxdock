use std::collections::HashMap;

use anyhow::Result;
use oxdock_fs::{GuardedPath, PolicyPath};
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::process::ExitStatus;

use std::sync::{Arc, Mutex};

/// Context passed to process managers describing the current execution
/// environment. Clones are cheap and explicit so background handles can own
/// their working roots without juggling lifetimes.
#[derive(Clone, Debug)]
pub struct CommandContext {
    cwd: PolicyPath,
    envs: Arc<HashMap<String, String>>,
    cargo_target_dir: GuardedPath,
    workspace_root: GuardedPath,
    build_context: GuardedPath,
}

impl CommandContext {
    pub fn new(
        cwd: &PolicyPath,
        envs: Arc<HashMap<String, String>>,
        cargo_target_dir: &GuardedPath,
        workspace_root: &GuardedPath,
        build_context: &GuardedPath,
    ) -> Self {
        Self {
            cwd: cwd.clone(),
            envs,
            cargo_target_dir: cargo_target_dir.clone(),
            workspace_root: workspace_root.clone(),
            build_context: build_context.clone(),
        }
    }

    /// Convenience constructor cloning a plain map into a fresh `Arc`.
    pub fn from_map(
        cwd: &PolicyPath,
        envs: &HashMap<String, String>,
        cargo_target_dir: &GuardedPath,
        workspace_root: &GuardedPath,
        build_context: &GuardedPath,
    ) -> Self {
        Self::new(
            cwd,
            Arc::new(envs.clone()),
            cargo_target_dir,
            workspace_root,
            build_context,
        )
    }

    pub fn cwd(&self) -> &PolicyPath {
        &self.cwd
    }

    pub fn envs(&self) -> &Arc<HashMap<String, String>> {
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

/// Handle for background processes spawned by a [`ProcessManager`].
pub trait BackgroundHandle: Send {
    fn try_wait(&mut self) -> Result<Option<ExitStatus>>;
    fn kill(&mut self) -> Result<()>;
    fn wait(&mut self) -> Result<ExitStatus>;
}

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

/// Abstraction for running shell commands both in the foreground and
/// background. `oxdock-core` relies on this trait to decouple the executor
/// from `std::process::Command`, which in turn enables Miri-friendly test
/// doubles.
pub trait ProcessManager: Clone + Send + 'static {
    type Handle: BackgroundHandle + Clone + Send + 'static;

    fn run_command(
        &mut self,
        ctx: &CommandContext,
        script: &str,
        options: CommandOptions,
    ) -> Result<CommandResult<Self::Handle>>;

    /// Spawn a command without waiting for completion. Returns a background
    /// handle that can be polled or waited on later. The default implementation
    /// delegates to `run_command` with `CommandMode::Background`.
    fn spawn_command(
        &mut self,
        ctx: &CommandContext,
        script: &str,
        options: CommandOptions,
    ) -> Result<CommandResult<Self::Handle>> {
        self.run_command(ctx, script, options)
    }
}
