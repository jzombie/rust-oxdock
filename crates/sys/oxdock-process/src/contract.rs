use std::collections::HashMap;

use anyhow::{Result, bail};
use oxdock_fs::{CargoScratch, GuardedPath, PolicyPath};
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::process::ExitStatus;

use std::sync::Arc;

// Shared-IO handles and take-once OS pipe halves live in `oxdock-pipe`
// (leaf crate, no cycle); re-exported here so every existing
// `oxdock_process::` path keeps resolving. The OS items keep their
// `not(miri)` gates: kernel pipes are compiled out under Miri isolation.
#[cfg(not(miri))]
pub use oxdock_pipe::{OsPipeReader, OsPipeWriter, create_os_pipe};
pub use oxdock_pipe::{SharedInput, SharedOutput};

/// Context passed to process managers describing the current execution
/// environment. Clones are cheap and explicit so background handles can own
/// their working roots without juggling lifetimes.
#[derive(Clone, Debug)]
pub struct CommandContext {
    cwd: PolicyPath,
    envs: Arc<HashMap<String, String>>,
    cargo_target_dir: CargoScratch,
    workspace_root: GuardedPath,
    build_context: GuardedPath,
}

impl CommandContext {
    pub fn new(
        cwd: &PolicyPath,
        envs: Arc<HashMap<String, String>>,
        cargo_target_dir: &CargoScratch,
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
        cargo_target_dir: &CargoScratch,
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

    pub fn cargo_target_dir(&self) -> &CargoScratch {
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
    /// Direct OS kernel pipe writer for concurrent pipelines. Single use:
    /// the handle is taken on spawn and the parent retains no copy, so the
    /// reader observes EOF once the producer exits. Only valid with
    /// concurrently spawned consumers (`ASYNC`); never for sequential steps.
    #[cfg(not(miri))]
    OsPipe(OsPipeWriter),
}

#[derive(Clone, Default)]
pub enum CommandStdin {
    /// Isolated null stdin. Preserves the previous `None` behavior.
    #[default]
    Null,
    Inherit,
    Stream(SharedInput),
    /// Direct OS kernel pipe reader for concurrent pipelines. See
    /// [`CommandStdout::OsPipe`] for the single use contract.
    #[cfg(not(miri))]
    OsPipe(OsPipeReader),
}

impl From<Option<SharedInput>> for CommandStdin {
    fn from(stdin: Option<SharedInput>) -> Self {
        match stdin {
            Some(reader) => CommandStdin::Stream(reader),
            None => CommandStdin::Null,
        }
    }
}

#[derive(Clone, Default)]
pub enum CommandStderr {
    #[default]
    Inherit,
    Stream(SharedOutput),
    /// Direct OS kernel pipe writer, mirroring [`CommandStdout::OsPipe`].
    /// Merging stdout and stderr into one live name takes the same slot
    /// twice, so the second take bails; merge in shell via `2>&1` instead.
    #[cfg(not(miri))]
    OsPipe(OsPipeWriter),
}

#[derive(Clone, Default)]
pub struct CommandOptions {
    pub mode: CommandMode,
    pub stdin: CommandStdin,
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

/// Host environment variable that forces spawned children to inherit the
/// parent's stdout/stderr instead of using the executor's stream routing.
/// Recognized values are `"1"` and case-insensitive `"true"`. Set on the
/// script environment (an `ENV` step or host inherit), not the process
/// environment: the executor reads it from [`CommandContext::envs`].
pub const INHERIT_STDOUT_ENV_VAR: &str = "OXDOCK_INHERIT_STDOUT";

/// Host process-environment variable enabling `eprintln!` diagnostics for
/// every spawned command (program plus argv/script). Read from the process
/// environment at spawn time; any value (including empty) enables it.
pub const PROCESS_DEBUG_ENV_VAR: &str = "OXBOOK_DEBUG";

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

    /// Run an executable directly with an argument vector (no shell).
    /// Backs the `RUN ["exe", "arg", ...]` exec form. The default
    /// implementation bails so existing out-of-tree managers keep
    /// compiling; in-tree managers override this.
    fn run_argv(
        &mut self,
        _ctx: &CommandContext,
        argv: &[String],
        _options: CommandOptions,
    ) -> Result<CommandResult<Self::Handle>> {
        bail!("run_argv not implemented for argv {argv:?}")
    }

    /// Spawn an argv command without waiting for completion. The default
    /// implementation delegates to `run_argv`, mirroring `spawn_command`.
    fn spawn_argv(
        &mut self,
        ctx: &CommandContext,
        argv: &[String],
        options: CommandOptions,
    ) -> Result<CommandResult<Self::Handle>> {
        self.run_argv(ctx, argv, options)
    }
}
