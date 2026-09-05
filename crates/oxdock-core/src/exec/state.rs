use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use oxdock_fs::{GuardedPath, WorkspaceFs};
use oxdock_parser::Value;
use oxdock_process::{BackgroundHandle, CommandContext, ProcessManager};

use super::io::{ExecIo, SlidingWindow};

pub(super) struct ExecState<P: ProcessManager> {
    pub(super) fs: Box<dyn WorkspaceFs>,
    pub(super) cargo_target_dir: GuardedPath,
    pub(super) cwd: GuardedPath,
    pub(super) envs: Arc<HashMap<String, String>>,
    pub(super) bg_children: Vec<Box<dyn BackgroundHandle>>,
    pub(super) scope_stack: Vec<ScopeSnapshot>,
    pub(super) io: ExecIo,
    /// Pre-registered SlidingWindow observers for ASSERT_STDOUT steps.
    /// Keyed by (generation, step index). TeeWriter pushes every chunk to all windows.
    pub(super) assert_windows: Arc<Mutex<HashMap<(usize, usize), SlidingWindow>>>,
    /// Variable scopes for $variable bindings (FOR loops, LET assignments).
    /// Innermost scope is last. Variables are looked up from innermost to outermost.
    pub(super) var_scopes: Vec<HashMap<String, Value>>,
    /// Cancellation token for background thread teardown.
    #[allow(dead_code)]
    pub(super) cancel_token: Arc<AtomicBool>,
    /// Handle to the currently executing foreground OS process, so
    /// ThreadJoinHandle::kill() can interrupt a blocking wait().
    #[allow(dead_code)]
    pub(super) active_process: Arc<Mutex<Option<Box<dyn BackgroundHandle>>>>,
    /// Named task registry for AWAIT support. Shared across subscopes via Arc.
    #[allow(dead_code)]
    pub(super) named_tasks: Arc<Mutex<HashMap<u64, Box<dyn BackgroundHandle>>>>,
    /// Counter for generating unique task IDs. Shared across subscopes via Arc.
    #[allow(dead_code)]
    pub(super) next_task_id: Arc<AtomicU64>,
    /// Whether we're inside an ASYNC block thread. When true, `handlers::run()`
    /// spawns in background mode so the handle can be registered for cancellation.
    pub(super) inside_async: bool,
    pub(super) _marker: PhantomData<P>,
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

    /// Fork the execution state for a child thread. The child gets:
    /// - A cloned filesystem handle (independent root setting, shared I/O)
    /// - Cloned envs, cwd, cargo_target_dir, var_scopes
    /// - Fresh bg_children, scope_stack (empty -- child manages its own)
    /// - Shared assert_windows (Arc clone)
    /// - Cloned io configuration
    /// - Independent cancel_token, active_process (child manages its own)
    /// - Shared named_tasks and next_task_id (via Arc clone)
    #[allow(dead_code)]
    pub(super) fn fork(&self) -> Self {
        Self {
            fs: self.fs.clone_box(),
            cargo_target_dir: self.cargo_target_dir.clone(),
            cwd: self.cwd.clone(),
            envs: Arc::clone(&self.envs),
            bg_children: Vec::new(),
            scope_stack: Vec::new(),
            io: self.io.clone(),
            assert_windows: Arc::clone(&self.assert_windows),
            var_scopes: self.var_scopes.clone(),
            cancel_token: Arc::new(AtomicBool::new(false)),
            active_process: Arc::new(Mutex::new(None)),
            named_tasks: Arc::clone(&self.named_tasks),
            next_task_id: Arc::clone(&self.next_task_id),
            inside_async: true,
            _marker: PhantomData,
        }
    }

    pub(super) fn push_var_scope(&mut self) {
        self.var_scopes.push(HashMap::new());
    }

    pub(super) fn pop_var_scope(&mut self) {
        self.var_scopes.pop();
    }

    pub(super) fn set_var(&mut self, key: String, value: Value) {
        if let Some(scope) = self.var_scopes.last_mut() {
            scope.insert(key, value);
        }
    }

    pub(super) fn get_var(&self, key: &str) -> Option<Value> {
        // Walk scopes from innermost to outermost
        for scope in self.var_scopes.iter().rev() {
            if let Some(value) = scope.get(key) {
                return Some(value.clone());
            }
        }
        None
    }

    /// Get a flattened view of all variables across all scopes.
    /// Inner scopes take precedence over outer scopes.
    pub(super) fn all_vars(&self) -> HashMap<String, Value> {
        let mut result = HashMap::new();
        for scope in self.var_scopes.iter().rev() {
            for (k, v) in scope {
                result.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        result
    }
}
