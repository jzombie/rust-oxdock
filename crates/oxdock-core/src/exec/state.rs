use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::Result;
use oxdock_fs::{GuardedPath, WorkspaceFs};
use oxdock_parser::{Step, Value};
use oxdock_process::{BackgroundHandle, CommandContext, ProcessManager};

use super::capture::SpillBuffer;
use super::io::{ExecIo, SlidingWindow};
use super::pipe::KeeperGuard;

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
    /// Named task registry for AWAIT/CANCEL support. Shared across subscopes
    /// via Arc. Each entry is a synchronized state machine (`TaskEntry`):
    /// the handle lives inside the entry so `CANCEL` can synchronously tear
    /// down a task even while a concurrent `AWAIT` is waiting on it.
    /// Entries are retained as `Cancelled`/`Completed` tombstones so later
    /// `AWAIT`/`CANCEL` report precise errors instead of `TaskNotFound`.
    #[allow(dead_code)]
    pub(super) named_tasks: Arc<Mutex<HashMap<u64, Arc<TaskEntry>>>>,
    /// Counter for generating unique task IDs. Shared across subscopes via Arc.
    #[allow(dead_code)]
    pub(super) next_task_id: Arc<AtomicU64>,
    /// Whether we're inside an ASYNC block thread. When true, `handlers::run()`
    /// spawns in background mode so the handle can be registered for cancellation.
    pub(super) inside_async: bool,
    /// Step-indexed keeper expiry for one `ASYNC` worker. Each guard pins a
    /// pipe the worker produces to, bridging the spawn-to-first-attach
    /// window and every transient gap between producer steps. Guards keyed
    /// to step `k` drop when the worker completes its top-level step `k`
    /// (matched by slice identity, so nested bodies never discharge them);
    /// leftovers drop with the worker thread. Always `None` outside
    /// workers; `fork` never inherits it.
    pub(super) keeper_expiry: Option<KeeperExpiry>,
    /// Whether `handlers::run()` must spawn in background mode so the handle
    /// registers in `active_process` for cancellation. Set while a `TIMEOUT`
    /// body executes on the current thread so the deadline watcher can kill
    /// a blocking foreground process. Unlike `inside_async`, this does not
    /// affect end-of-pipeline named-task reaping.
    pub(super) cancellable: bool,
    pub(super) _marker: PhantomData<P>,
}

pub(super) struct ScopeSnapshot {
    pub(super) cwd: GuardedPath,
    pub(super) root: GuardedPath,
    pub(super) envs: Arc<HashMap<String, String>>,
}

/// Lifecycle phase of a named background task (`LET $var = ASYNC ...`).
/// `Running` and `Awaiting` both hold the live handle inside the entry;
/// `Cancelled` and `Completed` are terminal tombstones with no handle.
pub(super) enum TaskPhase {
    Running,
    Awaiting,
    Cancelled,
    Completed,
}

pub(super) struct TaskEntryState {
    pub(super) phase: TaskPhase,
    pub(super) handle: Option<Box<dyn BackgroundHandle>>,
    /// True once the handle has been consumed and its thread joined
    /// (`kill()` for cancellations, `try_wait`-reap for natural completion).
    /// Threads observing `Cancelled` must wait on `done` until `reaped`
    /// before resuming, so no caller outruns OS process teardown.
    pub(super) reaped: bool,
    /// Per-task stdout sink (`LET $t = ASYNC ...`). The child thread writes
    /// here instead of the parent writer. Exactly one consumer takes it:
    /// `LET $o = AWAIT $t` binds it, bare `AWAIT $t` forwards it to the
    /// parent stdout, and end-poll reaping forwards un-awaited output.
    pub(super) sink: Option<Arc<SpillBuffer>>,
}

/// Synchronized named-task entry shared by every scope that can observe the
/// task (`AWAIT`, `CANCEL`, end-poll reaping). Exactly one thread ever takes
/// the handle and performs teardown; all other observers rendezvous on
/// `done`/`reaped`.
pub(super) struct TaskEntry {
    pub(super) state: Mutex<TaskEntryState>,
    pub(super) done: Condvar,
}

impl TaskEntry {
    pub(super) fn new_with_sink(handle: Box<dyn BackgroundHandle>, sink: Arc<SpillBuffer>) -> Self {
        Self {
            state: Mutex::new(TaskEntryState {
                phase: TaskPhase::Running,
                handle: Some(handle),
                reaped: false,
                sink: Some(sink),
            }),
            done: Condvar::new(),
        }
    }

    /// Take the task's stdout sink exactly once. The first consumer
    /// (awaiter or end-poll reaper) wins; later calls get `None`.
    pub(super) fn take_sink(&self) -> Option<Arc<SpillBuffer>> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sink
            .take()
    }

    /// Block until the teardown owner has consumed the handle and joined
    /// the task thread. Lock-free for callers except the wait itself.
    pub(super) fn wait_reaped(&self) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while !guard.reaped {
            guard = self.done.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Mark teardown complete and wake every rendezvous waiter.
    pub(super) fn finish_teardown(&self) {
        {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.handle = None;
            guard.reaped = true;
        }
        self.done.notify_all();
    }
}

/// Step-indexed keeper expiry for one `ASYNC` worker thread. Guards are
/// keyed by the top-level body index of the final producer step for each
/// pipe: dropping the guards for step `k` once it completes keeps the
/// pipe open across every transient gap between producers, then releases
/// it so later consumer steps in the same task observe EOF.
///
/// Slice identity (`body_addr`/`body_len`) gates discharge: nested bodies
/// (`FOR`/`IF`/`TIMEOUT`/inner `ASYNC`) execute through the same stepping
/// code with different slices and must never consume the worker's
/// top-level map.
pub(super) struct KeeperExpiry {
    body_addr: usize,
    body_len: usize,
    map: HashMap<usize, Vec<KeeperGuard>>,
}

impl KeeperExpiry {
    pub(super) fn new(steps: &[Step], map: HashMap<usize, Vec<KeeperGuard>>) -> Self {
        Self {
            body_addr: steps.as_ptr() as usize,
            body_len: steps.len(),
            map,
        }
    }

    pub(super) fn matches(&self, steps: &[Step]) -> bool {
        self.body_addr == steps.as_ptr() as usize && self.body_len == steps.len()
    }

    /// Drop the guards expiring at top-level step `idx`. Returns true when
    /// the map is drained and the expiry itself can be cleared.
    pub(super) fn expire_step(&mut self, steps: &[Step], idx: usize) -> bool {
        if self.matches(steps) {
            drop(self.map.remove(&idx));
        }
        self.map.is_empty()
    }
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
            keeper_expiry: None,
            cancellable: self.cancellable,
            _marker: PhantomData,
        }
    }

    pub(super) fn push_var_scope(&mut self) {
        self.var_scopes.push(HashMap::new());
    }

    pub(super) fn pop_var_scope(&mut self) {
        self.var_scopes.pop();
    }

    /// Enter a lexical scope: snapshot cwd/root/envs and open a fresh
    /// variable scope. Blocks scope everything (LET/ENV/WORKDIR/WORKSPACE);
    /// only pipes (ExecIo) and filesystem effects cross scope boundaries.
    pub(super) fn push_scope(&mut self) {
        self.scope_stack.push(ScopeSnapshot {
            cwd: self.cwd.clone(),
            root: self.fs.root().clone(),
            envs: Arc::clone(&self.envs),
        });
        self.push_var_scope();
    }

    /// Exit a lexical scope, restoring everything `push_scope` saved.
    pub(super) fn pop_scope(&mut self) -> Result<()> {
        let snapshot = self
            .scope_stack
            .pop()
            .ok_or_else(|| anyhow::anyhow!("scope stack underflow during pop"))?;
        self.fs.set_root(&snapshot.root);
        self.cwd = snapshot.cwd;
        self.envs = snapshot.envs;
        self.pop_var_scope();
        Ok(())
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
