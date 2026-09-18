use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::Result;
use oxdock_fs::{CargoScratch, GuardedPath, WorkspaceFs};
use oxdock_parser::{Step, TypeDescriptor, Value};
use oxdock_process::{BackgroundHandle, CommandContext, ProcessManager};

use super::capture::SpillBuffer;
use super::io::{ExactCapture, ExecIo, SlidingWindow};
use super::native::FunctionRegistry;
use oxdock_pipe::KeeperGuard;

/// Maximum nested function-call depth. Guards the host thread stack against
/// runaway recursion; the error names the function that overflowed.
pub(super) const MAX_CALL_DEPTH: usize = 64;

/// Execution state for one script run. Public so hosts can register
/// functions and introspect listings; all fields stay crate-private so the
/// scope, recursion-budget, and task invariants cannot be broken from outside.
pub struct ExecState<P: ProcessManager> {
    pub(super) fs: Box<dyn WorkspaceFs>,
    /// Pre-reserved guarded scratch name for `CARGO_TARGET_DIR` (issue #131).
    /// Opaque [`CargoScratch`]: renderable for the child environment but not
    /// nameable as a `&GuardedPath`, so host code cannot `ensure()` or
    /// `create_dir_all` it. The child `cargo` creates it on demand.
    pub(super) cargo_scratch: CargoScratch,
    pub(super) cwd: GuardedPath,
    pub(super) envs: Arc<HashMap<String, String>>,
    pub(super) bg_children: Vec<Box<dyn BackgroundHandle>>,
    pub(super) scope_stack: Vec<ScopeSnapshot>,
    pub(super) io: ExecIo,
    /// Pre-registered SlidingWindow observers for stream assertion steps.
    /// Keyed by (generation, step index). TeeWriter pushes every chunk to all windows.
    pub(super) assert_windows: Arc<Mutex<HashMap<(usize, usize), SlidingWindow>>>,
    /// Same observer map fed by the stderr tee for `ASSERT_CONTAINS stderr`.
    pub(super) assert_windows_stderr: Arc<Mutex<HashMap<(usize, usize), SlidingWindow>>>,
    /// Exact-match stdout accumulators for `ASSERT_EQ stdout`, keyed by
    /// generation and fed by the same tee. Entries allocated during
    /// pre-registration observe bytes from scope entry; the common
    /// top-level case therefore sees trial-cumulative output.
    pub(super) exact_stdout: Arc<Mutex<HashMap<usize, ExactCapture>>>,
    /// Variable scopes for $variable bindings (FOR loops, LET assignments).
    /// Innermost scope is last. Variables are looked up from innermost to outermost.
    /// Each entry carries its declared type name alongside the value.
    pub(super) var_scopes: Vec<HashMap<String, (String, Value)>>,
    /// Name directory for type resolution: startup descriptors plus the
    /// run's host descriptors. Words carry their own vtables, so this map
    /// serves only name queries (declarations, `TYPES()`, `TYPE_DESCRIBE`).
    pub(super) types: HashMap<String, &'static TypeDescriptor>,
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
    /// Counter for anonymous pipe backends minted by bare `LET $p: PIPE`.
    /// Shared across subscopes via Arc so forked workers never re-mint a
    /// name; the key contains a space no `pipe:` literal can spell, so
    /// generated names never collide with user-named pipes.
    pub(super) next_pipe_id: Arc<AtomicU64>,
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
    /// The single function registry: DSL `FUNC` definitions, builtins, and
    /// host extensions. Script entries scope lexically through the
    /// registry's own frames (managed by `push_scope`/`pop_scope`);
    /// native entries persist. Shared across `fork()` via clone.
    pub(super) functions: FunctionRegistry<P>,
    /// Current nested function-call depth on this thread. Enforced against
    /// `MAX_CALL_DEPTH`; cloned (not reset) by `fork()` so async children
    /// inherit the caller's depth budget.
    pub(super) call_depth: usize,
    pub(super) _marker: PhantomData<P>,
}

pub(super) struct ScopeSnapshot {
    pub(super) cwd: GuardedPath,
    pub(super) root: GuardedPath,
    pub(super) envs: Arc<HashMap<String, String>>,
}

/// Lifecycle phase of a named background task (`LET $var: HANDLE = ASYNC ...`).
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
    /// Per-task stdout sink (`LET $t: HANDLE = ASYNC ...`). The child thread writes
    /// here instead of the parent writer. Exactly one consumer takes it:
    /// `LET $o: STRING = AWAIT $t` binds it, bare `AWAIT $t` forwards it to the
    /// parent stdout, and end-poll reaping forwards un-awaited output.
    pub(super) sink: Option<Arc<SpillBuffer>>,
    /// Return value of a background `CALL` task (`LET $t: HANDLE = ASYNC CALL
    /// FOO(...)`). Set under the entry lock before `done.notify_all()`; read
    /// by `LET $o: TYPE = AWAIT $t` when the task body was a single `Call`.
    /// `None` for block tasks and for tasks that have not finished.
    pub(super) return_value: Option<Value>,
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
                return_value: None,
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
        // Resolve the working directory through the snapshot choke point: a
        // snapshot-rooted cwd materializes here (so `RUN` executes against a
        // real directory) while a local cwd resolves purely lexically with
        // zero I/O. `CARGO_TARGET_DIR` is the pre-reserved scratch name (never
        // ensured by us); callers may still override it via the env map, which
        // apply_ctx respects when spawning processes.
        let cwd = self.fs.resolve_write(&self.cwd, ".")?;
        Ok(CommandContext::new(
            &cwd.into(),
            Arc::clone(&self.envs),
            &self.cargo_scratch,
            self.fs.root(),
            self.fs.build_context(),
        ))
    }

    /// Fork the execution state for a child thread. The child gets:
    /// - A cloned filesystem handle (shared snapshot backing, independent root selection)
    /// - Cloned envs, cwd, cargo scratch name, var_scopes
    /// - Fresh bg_children, scope_stack (empty -- child manages its own)
    /// - Shared assert_windows, assert_windows_stderr, exact_stdout (Arc clones)
    /// - Cloned io configuration
    /// - Independent cancel_token, active_process (child manages its own)
    /// - Shared named_tasks, next_task_id, and next_pipe_id (via Arc clone)
    #[allow(dead_code)]
    pub(super) fn fork(&self) -> Self {
        Self {
            fs: self.fs.clone_box(),
            cargo_scratch: self.cargo_scratch.clone(),
            cwd: self.cwd.clone(),
            envs: Arc::clone(&self.envs),
            bg_children: Vec::new(),
            scope_stack: Vec::new(),
            io: self.io.clone(),
            assert_windows: Arc::clone(&self.assert_windows),
            assert_windows_stderr: Arc::clone(&self.assert_windows_stderr),
            exact_stdout: Arc::clone(&self.exact_stdout),
            var_scopes: self.var_scopes.clone(),
            cancel_token: Arc::new(AtomicBool::new(false)),
            active_process: Arc::new(Mutex::new(None)),
            named_tasks: Arc::clone(&self.named_tasks),
            next_task_id: Arc::clone(&self.next_task_id),
            next_pipe_id: Arc::clone(&self.next_pipe_id),
            inside_async: true,
            keeper_expiry: None,
            cancellable: self.cancellable,
            functions: self.functions.clone(),
            types: self.types.clone(),
            call_depth: self.call_depth,
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
    /// Function definitions scope through the registry's own frames.
    pub(super) fn push_scope(&mut self) {
        self.scope_stack.push(ScopeSnapshot {
            cwd: self.cwd.clone(),
            root: self.fs.root().clone(),
            envs: Arc::clone(&self.envs),
        });
        self.functions.push_scope();
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
        self.functions.pop_scope();
        self.pop_var_scope();
        Ok(())
    }
    pub(super) fn declare_var(&mut self, key: String, kind: String, value: Value) -> Result<()> {
        if self
            .var_scopes
            .last()
            .map(|s| s.contains_key(&key))
            .unwrap_or(false)
        {
            anyhow::bail!(
                "redeclaration error: ${} already declared in this scope; use ${} = ... to mutate",
                key,
                key
            );
        }
        let coerced = super::args::coerce_value(value, &kind, &*self)?;
        let scope = self
            .var_scopes
            .last_mut()
            .ok_or_else(|| anyhow::anyhow!("no variable scope for declaration"))?;
        scope.insert(key, (kind, coerced));
        Ok(())
    }

    pub(super) fn mutate_var(&mut self, key: &str, value: Value) -> Result<()> {
        let kind = self
            .var_scopes
            .iter()
            .rev()
            .find_map(|s| s.get(key).map(|(k, _)| k.clone()))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "undeclared variable ${key}: declare it first with LET ${key}: TYPE = ..."
                )
            })?;
        let coerced = super::args::coerce_value(value, &kind, &*self)?;
        for scope in self.var_scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(key) {
                slot.1 = coerced;
                return Ok(());
            }
        }
        anyhow::bail!("undeclared variable ${key}");
    }

    pub(super) fn get_var(&self, key: &str) -> Option<Value> {
        // Walk scopes from innermost to outermost
        for scope in self.var_scopes.iter().rev() {
            if let Some((_, value)) = scope.get(key) {
                return Some(value.clone());
            }
        }
        None
    }

    pub(super) fn get_var_typed(&self, key: &str) -> Option<(String, Value)> {
        for scope in self.var_scopes.iter().rev() {
            if let Some(entry) = scope.get(key) {
                return Some(entry.clone());
            }
        }
        None
    }

    /// Get a flattened view of all variables across all scopes.
    /// Inner scopes take precedence over outer scopes.
    pub(super) fn all_vars(&self) -> HashMap<String, Value> {
        let mut result = HashMap::new();
        for scope in self.var_scopes.iter().rev() {
            for (k, (_, v)) in scope {
                result.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        result
    }
}
