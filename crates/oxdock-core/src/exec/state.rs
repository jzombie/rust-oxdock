use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use oxdock_fs::{GuardedPath, WorkspaceFs};
use oxdock_process::{CommandContext, ProcessManager};

use super::io::{ExecIo, SlidingWindow};

pub(super) struct ExecState<P: ProcessManager> {
    pub(super) fs: Box<dyn WorkspaceFs>,
    pub(super) cargo_target_dir: GuardedPath,
    pub(super) cwd: GuardedPath,
    pub(super) envs: Arc<HashMap<String, String>>,
    pub(super) bg_children: Vec<P::Handle>,
    pub(super) scope_stack: Vec<ScopeSnapshot>,
    pub(super) io: ExecIo,
    /// Pre-registered SlidingWindow observers for ASSERT_STDOUT steps.
    /// Keyed by step index. TeeWriter pushes every chunk to all windows.
    pub(super) assert_windows: Arc<Mutex<HashMap<usize, SlidingWindow>>>,
}

pub(super) struct ScopeSnapshot {
    pub(super) cwd: GuardedPath,
    pub(super) root: GuardedPath,
    pub(super) envs: Arc<HashMap<String, String>>,
}

impl<P: ProcessManager> ExecState<P> {
    pub(super) fn command_ctx(&self) -> Result<CommandContext> {
        // Build a CommandContext snapshot for this step. The `cargo_target_dir`
        // here is the executor default; if callers want to override it they
        // must do so via the env map (e.g. ENV CARGO_TARGET_DIR=...), which
        // apply_ctx respects when spawning processes.
        Ok(CommandContext::new(
            &self.cwd.clone().into(),
            Arc::clone(&self.envs),
            &self.cargo_target_dir,
            self.fs.root(),
            self.fs.build_context(),
        ))
    }
}
