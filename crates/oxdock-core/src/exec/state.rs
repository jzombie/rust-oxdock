use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::Result;
use oxdock_fs::{CargoScratch, GuardedPath, WorkspaceFs};
use oxdock_parser::{Field, Step, TypeDescriptor, TypeTag, Value};
use oxdock_process::{BackgroundHandle, CancelHub, CommandContext, ProcessManager};

use super::exit::exit_code_of;
use super::io::{ExactCapture, ExecIo, SlidingWindow, TeeStream};
use super::native::FunctionRegistry;
use oxdock_pipe::KeeperGuard;

/// Maximum nested function-call depth. Guards the host thread stack against
/// runaway recursion; the error names the function that overflowed.
pub(super) const MAX_CALL_DEPTH: usize = 64;

/// Retained finished-task cap. Bounds `named_tasks` tombstones plus
/// their tail buffers in long-lived sessions: spawning past the cap
/// evicts the oldest finished entry. Running entries are never evicted.
pub(super) const MAX_RETAINED_FINISHED: usize = 128;

/// Live-task concurrency cap. Enforced globally through the shared
/// counter below: any ASYNC spawn, in any nested worker, sees the
/// same count. Refusal is backpressure, not silent kills and not an
/// unbounded queue.
pub const MAX_RUNNING_TASKS: usize = 64;

/// RAII slot for the global live-task count. Created first inside
/// every ASYNC worker thread; dropping on return or unwind releases
/// the slot. The count therefore tracks threads actually alive, never
/// table entries, so un-awaited finished tasks cannot squat slots and
/// nested workers cannot bypass the cap.
pub(super) struct TaskSlotGuard {
    counter: Arc<AtomicU64>,
}

impl TaskSlotGuard {
    pub(super) fn new(counter: Arc<AtomicU64>) -> Self {
        Self { counter }
    }
}

impl Drop for TaskSlotGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Per-stream live tail cap for background tasks. Matches the
/// foreground capture scale: the same never-grows guarantee.
const TASK_TAIL_CAP: usize = 64 * 1024;

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
    /// Pure session seed from [`oxdock_process::seed_defaults`], stored at
    /// init alongside the layered `envs`. `ISOLATED` resets clone this
    /// (not the layered map), and transport sanitize diffs against it.
    /// Built once: re-seeding would re-read host fallbacks and break
    /// state stability.
    pub(super) seed: Arc<HashMap<String, String>>,
    pub(super) cwd: GuardedPath,
    pub(super) envs: Arc<HashMap<String, String>>,
    /// Outer-scope env snapshots for `ISOLATED` blocks, innermost last.
    /// Each entry is the script env as it stood at that block's entry, so
    /// the block allowlist resolves against the enclosing scope even as
    /// the live envs are scrubbed underneath. Nesting snapshots the
    /// already-scrubbed parent, so narrowing is monotonic by
    /// construction. Cloned (not reset) by `fork()` so tasks spawned
    /// inside a block inherit its scrub.
    pub(super) isolated_stack: Vec<Arc<HashMap<String, String>>>,
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
    pub(super) var_scopes: Vec<HashMap<String, (TypeTag, Value)>>,
    /// Name directory for type resolution: startup descriptors plus the
    /// run's host descriptors. Words carry their own vtables, so this map
    /// serves only name queries (declarations, `TYPES()`, `TYPE_DESCRIBE`).
    pub(super) types: HashMap<String, &'static TypeDescriptor>,
    /// Named record schemas for shaped MAP bindings: schema name to its
    /// field table. Populated from host modules alongside descriptors, so
    /// `LET $s: SSH_SESSION_INFO` resolves exactly when the owning module
    /// is registered. Impossible to reference an unregistered schema.
    pub(super) record_schemas: HashMap<String, Arc<[Field]>>,
    /// Module-shared state stashed at registration, keyed by owning
    /// module name plus value type. Bodies fetch it through
    /// `StepCtx::module_state` instead of hand-built closures, so
    /// every entry can stay `#[oxdock_func]`.
    pub(super) module_states:
        HashMap<(String, std::any::TypeId), Arc<dyn std::any::Any + Send + Sync>>,
    /// Script-level type aliases for this run: alias name to canonical
    /// target spelling (`PERSON` to `MAP<name:STRING>`). Populated once
    /// from top-level `TYPE` steps before the static pass, so forward
    /// references resolve run-wide at first coercion.
    pub(super) type_aliases: HashMap<String, String>,
    /// Cancellation token for background thread teardown.
    #[allow(dead_code)]
    pub(super) cancel_token: Arc<AtomicBool>,
    /// Event hub fanning the same cancellation to pump supervisors.
    /// Marked everywhere the token is stored, so blocking pumps wake
    /// without polling. Forks get a fresh hub with their fresh token.
    #[allow(dead_code)]
    pub(super) cancel_hub: Arc<CancelHub>,
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
    /// Shared live-task count across every forked worker. Forks clone
    /// the `Arc`, so nested spawns see the same total: the cap is
    /// global, not per scope. Incremented atomically at allocation,
    /// released by the worker thread's slot guard on exit.
    pub(super) live_tasks: Arc<AtomicU64>,
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
    /// Identity of the task this state executes (`0` = root flow).
    /// Stamped onto pipe handles minted here and compared by the promotion
    /// check: only the declaring task may promote its own pipes.
    pub(super) task_id: u64,
    /// Declared `--to-host` transfers recorded strictly at execution time:
    /// each executed flagged `COPY` appends its evaluated
    /// `(guest_src, host_dst)` pair. The guest serve loop packs exactly
    /// these entries, so dead branches contribute nothing and dynamic
    /// paths (`$var`, `{{ ... }}`) resolve normally. Shared with the sink
    /// in [`ExecIo`] when staged.
    pub(super) push_manifest: Vec<(String, String)>,
    pub(super) _marker: PhantomData<P>,
}

pub(super) struct ScopeSnapshot {
    pub(super) cwd: GuardedPath,
    pub(super) root: GuardedPath,
    pub(super) envs: Arc<HashMap<String, String>>,
}

/// Task completion record, written once by the first observer of a
/// finished task and read by later polls. Never consumed: polls and a
/// subsequent `AWAIT` all read the same record.
#[derive(Clone)]
pub(super) struct TaskCompletion {
    pub(super) ok: bool,
    pub(super) exit_code: i64,
    pub(super) result: Option<Value>,
    pub(super) error: String,
}

/// Point-in-time task view for `TASK_POLL`. Owned strings, bounded by
/// the tail caps.
pub(super) struct TaskObservation {
    pub(super) running: bool,
    pub(super) ok: bool,
    pub(super) exit_code: i64,
    pub(super) result: Option<Value>,
    pub(super) error: String,
}

/// Bounded per-stream accumulator for one background task. Same
/// drop-the-head discipline as the foreground tail rings.
pub(super) struct TaskTails {
    out: Vec<u8>,
    err: Vec<u8>,
    total_out: u64,
    total_err: u64,
}

impl TaskTails {
    pub(super) fn new() -> Self {
        Self {
            out: Vec::new(),
            err: Vec::new(),
            total_out: 0,
            total_err: 0,
        }
    }

    pub(super) fn push(&mut self, stream: TeeStream, bytes: &[u8]) {
        let (buf, total) = match stream {
            TeeStream::Stdout => (&mut self.out, &mut self.total_out),
            TeeStream::Stderr => (&mut self.err, &mut self.total_err),
        };
        *total += bytes.len() as u64;
        buf.extend_from_slice(bytes);
        let excess = buf.len().saturating_sub(TASK_TAIL_CAP);
        if excess > 0 {
            buf.drain(..excess);
        }
    }

    pub(super) fn snapshot(&self) -> (String, String, u64) {
        let dropped = self.total_out.saturating_sub(self.out.len() as u64)
            + self.total_err.saturating_sub(self.err.len() as u64);
        (
            String::from_utf8_lossy(&self.out).into_owned(),
            String::from_utf8_lossy(&self.err).into_owned(),
            dropped,
        )
    }
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
    /// Explicit result of a background task, published by `RETURN` (block
    /// bodies) or the function call itself (single-`CALL` bodies). Set
    /// under the entry lock before `done.notify_all()`; read by
    /// `LET $o: TYPE = AWAIT $t`. `None` for tasks that have not finished
    /// or have fallen off the end. Task stdout is never captured: it
    /// streams live to the shared parent writer.
    pub(super) return_value: Option<Value>,
    /// Completion record written once by the first observer of a
    /// finished task (`TASK_POLL` or the teardown path). Read by later
    /// polls and post-mortem rendering; never consumed.
    pub(super) completion: Option<TaskCompletion>,
    /// Whether this task's body can publish a value: single-`CALL` bodies
    /// always can; block bodies can iff they contain a `RETURN` in their
    /// own scope. A task that can return but falls off the end fails its
    /// capture loudly; a task that cannot return yields `INT` 0.
    pub(super) returns_value: bool,
}

/// Synchronized named-task entry shared by every scope that can observe the
/// task (`AWAIT`, `CANCEL`, `TASK_POLL`, end-poll reaping). Exactly one
/// thread ever takes the handle and performs teardown; all other
/// observers rendezvous on `done`/`reaped`.
pub(super) struct TaskEntry {
    pub(super) state: Mutex<TaskEntryState>,
    pub(super) done: Condvar,
    /// Bounded live output. Fed by the worker tee, drained by
    /// `TASK_POLL` without consuming, dropped with the entry.
    pub(super) tails: Arc<Mutex<TaskTails>>,
}

impl TaskEntry {
    /// Whether this entry occupies a live slot: awaiting a join, or
    /// running with an unfinished thread. Terminal phases and reaped
    /// joins are done. Un-awaited finished tasks report done via
    /// `try_wait`, so ghosts never hold a slot. Never blocks: a
    /// still-running thread reports `Ok(None)` without joining.
    pub(super) fn is_live(&self) -> bool {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match guard.phase {
            TaskPhase::Running => match guard.handle.as_mut() {
                Some(handle) => matches!(handle.try_wait(), Ok(None)),
                None => false,
            },
            TaskPhase::Awaiting => true,
            TaskPhase::Completed | TaskPhase::Cancelled => false,
        }
    }

    pub(super) fn new(
        handle: Box<dyn BackgroundHandle>,
        returns_value: bool,
        tails: Arc<Mutex<TaskTails>>,
    ) -> Self {
        Self {
            state: Mutex::new(TaskEntryState {
                phase: TaskPhase::Running,
                handle: Some(handle),
                reaped: false,
                return_value: None,
                completion: None,
                returns_value,
            }),
            done: Condvar::new(),
            tails,
        }
    }

    /// Non-destructive snapshot of live tails plus dropped head count.
    /// Never blocks execution: a poisoned lock reads through.
    pub(super) fn snapshot_tails(&self) -> (String, String, u64) {
        self.tails
            .lock()
            .map(|tails| tails.snapshot())
            .unwrap_or_else(|e| e.into_inner().snapshot())
    }

    /// Observe completion without consuming the task. Never transitions
    /// phase, never takes the handle, never notifies: a later `AWAIT`
    /// still joins normally. First observation of a finished task
    /// records the completion for later polls; the `try_wait` reap it
    /// performs is the same one `AWAIT` would do, and the cached status
    /// keeps that path consistent.
    pub(super) fn observe_completion(&self) -> TaskObservation {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(done) = guard.completion.clone() {
            return TaskObservation {
                running: false,
                ok: done.ok,
                exit_code: done.exit_code,
                result: done.result,
                error: done.error,
            };
        }
        if !matches!(guard.phase, TaskPhase::Running) {
            return TaskObservation {
                running: false,
                ok: false,
                exit_code: -1,
                result: None,
                error: "task handle was not found or has already been awaited".to_string(),
            };
        }
        match guard.handle.as_mut() {
            None => TaskObservation {
                running: true,
                ok: true,
                exit_code: -1,
                result: None,
                error: String::new(),
            },
            Some(handle) => match handle.try_wait() {
                Ok(Some(status)) => {
                    let completion = TaskCompletion {
                        ok: status.success(),
                        exit_code: status.code().map(i64::from).unwrap_or(-1),
                        result: guard.return_value.clone(),
                        error: if status.success() {
                            String::new()
                        } else {
                            format!("task exited with status {status}")
                        },
                    };
                    guard.completion = Some(TaskCompletion {
                        ok: completion.ok,
                        exit_code: completion.exit_code,
                        result: completion.result.clone(),
                        error: completion.error.clone(),
                    });
                    TaskObservation {
                        running: false,
                        ok: completion.ok,
                        exit_code: completion.exit_code,
                        result: completion.result,
                        error: completion.error,
                    }
                }
                Ok(None) => TaskObservation {
                    running: true,
                    ok: true,
                    exit_code: -1,
                    result: None,
                    error: String::new(),
                },
                Err(err) => {
                    let completion = TaskCompletion {
                        ok: false,
                        exit_code: exit_code_of(&err).unwrap_or(1),
                        result: None,
                        error: format!("{err:#}"),
                    };
                    guard.completion = Some(TaskCompletion {
                        ok: completion.ok,
                        exit_code: completion.exit_code,
                        result: completion.result.clone(),
                        error: completion.error.clone(),
                    });
                    TaskObservation {
                        running: false,
                        ok: completion.ok,
                        exit_code: completion.exit_code,
                        result: completion.result,
                        error: completion.error,
                    }
                }
            },
        }
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
    /// Script-map environment lookup for DSL evaluation (`is_exec_name`,
    /// `Expr::Env`, expansions): exact hit first, case-insensitive
    /// fallback on Windows so `{{ env:Path }}` resolves the seeded
    /// `PATH`. The map is seeded at init, so no fallback layer exists.
    pub(super) fn get_env(&self, key: &str) -> Option<&str> {
        if let Some(v) = self.envs.get(key) {
            return Some(v.as_str());
        }
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            return self
                .envs
                .iter()
                .find(|(k, _)| oxdock_process::names_eq(k, key))
                .map(|(_, v)| v.as_str());
        }
        None
    }

    pub(super) fn command_ctx(&self) -> Result<CommandContext> {
        // Resolve the working directory through the snapshot choke point: a
        // snapshot-rooted cwd materializes here (so `RUN` executes against a
        // real directory) while a local cwd resolves purely lexically with
        // zero I/O. The seeded scratch materializes alongside: `HOME` and
        // `TMPDIR` point at it, so children writing temp or profile data
        // must observe real directories, never lexical candidates.
        let cwd = self.fs.resolve_write(&self.cwd, ".")?;
        self.cargo_scratch.ensure_materialized()?;
        Ok(CommandContext::new(
            &cwd.into(),
            Arc::clone(&self.envs),
            &self.cargo_scratch,
            self.fs.root(),
            self.fs.build_context(),
        ))
    }

    /// Draw the next task id, enforcing the global live-task cap and
    /// sweeping retained finished entries. Every ASYNC allocation path
    /// calls this, so named and anonymous tasks share one choke point.
    /// The live count is atomic and shared across all forked workers,
    /// so nested spawns cannot bypass the cap that a per-state scan
    /// would miss. Refusal is backpressure: a full host fails the spawn
    /// loudly instead of wedging.
    pub(super) fn alloc_task_id(&mut self) -> anyhow::Result<u64> {
        // Purge cleanly-finished anonymous children first: their threads
        // are gone and a zero exit has no reporter, so retaining them
        // only grows the vec. Failed handles stay for the end-pipeline
        // reaper, which owns their error reporting.
        self.bg_children
            .retain_mut(|handle| match handle.try_wait() {
                Ok(None) => true,
                Ok(Some(status)) => !status.success(),
                Err(_) => true,
            });
        self.live_tasks
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_RUNNING_TASKS as u64).then_some(n + 1)
            })
            .map_err(|_| {
                anyhow::anyhow!(
                    "concurrency limit exceeded: maximum of {MAX_RUNNING_TASKS} running background tasks allowed"
                )
            })?;
        let id = self.next_task_id.fetch_add(1, Ordering::SeqCst);
        self.sweep_finished_tasks();
        Ok(id)
    }

    /// Drop finished entries past `MAX_RETAINED_FINISHED`, oldest
    /// first. Finished means a terminal phase or a completed join;
    /// `Awaiting` entries always survive (a waiter is rendezvousing)
    /// and running entries are never touched. Evicted ids read as
    /// unknown/already-awaited afterwards, like an awaited task.
    fn sweep_finished_tasks(&self) {
        let mut finished: Vec<u64> = Vec::new();
        {
            let named = self.named_tasks.lock().unwrap_or_else(|e| e.into_inner());
            for (id, entry) in named.iter() {
                // `is_live` already exempts awaiting entries: a waiter
                // is rendezvousing on them.
                if !entry.is_live() {
                    finished.push(*id);
                }
            }
        }
        if finished.len() <= MAX_RETAINED_FINISHED {
            return;
        }
        finished.sort_unstable();
        let mut named = self.named_tasks.lock().unwrap_or_else(|e| e.into_inner());
        for id in finished.iter().take(finished.len() - MAX_RETAINED_FINISHED) {
            named.remove(id);
        }
    }

    /// Fork the execution state for a child thread. The child gets:
    /// - A cloned filesystem handle (shared snapshot backing, independent root selection)
    /// - Cloned envs, cwd, cargo scratch name, var_scopes
    /// - Fresh bg_children, scope_stack (empty -- child manages its own)
    /// - Shared assert_windows, assert_windows_stderr, exact_stdout (Arc clones)
    /// - Cloned io configuration
    /// - Independent cancel_token, active_process (child manages its own)
    /// - Shared named_tasks and next_task_id (via Arc clone)
    #[allow(dead_code)]
    pub(super) fn fork(&self) -> Self {
        Self {
            fs: self.fs.clone_box(),
            cargo_scratch: self.cargo_scratch.clone(),
            seed: Arc::clone(&self.seed),
            cwd: self.cwd.clone(),
            envs: Arc::clone(&self.envs),
            bg_children: Vec::new(),
            scope_stack: Vec::new(),
            isolated_stack: self.isolated_stack.clone(),
            io: self.io.clone(),
            assert_windows: Arc::clone(&self.assert_windows),
            assert_windows_stderr: Arc::clone(&self.assert_windows_stderr),
            exact_stdout: Arc::clone(&self.exact_stdout),
            var_scopes: self.var_scopes.clone(),
            cancel_token: Arc::new(AtomicBool::new(false)),
            // Fresh cancellation, inherited telemetry: the fork's
            // workers still attribute to the originating run.
            cancel_hub: Arc::new(CancelHub::with_counter(self.cancel_hub.worker_counter())),
            active_process: Arc::new(Mutex::new(None)),
            named_tasks: Arc::clone(&self.named_tasks),
            next_task_id: Arc::clone(&self.next_task_id),
            // Shared live count, never reset: nested workers report into
            // the same cap their spawner was admitted under.
            live_tasks: Arc::clone(&self.live_tasks),
            inside_async: true,
            keeper_expiry: None,
            cancellable: self.cancellable,
            functions: self.functions.clone(),
            types: self.types.clone(),
            record_schemas: self.record_schemas.clone(),
            module_states: self.module_states.clone(),
            type_aliases: self.type_aliases.clone(),
            call_depth: self.call_depth,
            task_id: self.task_id,
            // Forks start with an empty manifest: async children record
            // their own executions; the guest packs the root manifest.
            push_manifest: Vec::new(),
            _marker: PhantomData,
        }
    }

    /// Whether `pipe` may promote to an OS kernel pair on behalf of
    /// `task_id`: only when it was declared by that same task and no
    /// spawned child can observe it. Anything else stays script-backed, so
    /// concurrent tasks can never disagree about EOF propagation.
    /// Spawning tasks pass their worker id (not the spawner's): promotion
    /// is judged from the worker's perspective, since it is the worker's
    /// bindings that will hold raw descriptors.
    pub(super) fn can_promote_in_task(pipe: &oxdock_pipe::PipeHandle, task_id: u64) -> bool {
        pipe.declaring_task() == task_id && !pipe.has_escaped()
    }

    /// Whether `pipe` may promote on the current thread (main flow and
    /// inline resolves).
    pub(super) fn can_promote_to_os(&self, pipe: &oxdock_pipe::PipeHandle) -> bool {
        Self::can_promote_in_task(pipe, self.task_id)
    }

    /// Mark every pipe visible in scope as shared-with-a-child. Called
    /// synchronously on the spawner before a worker thread starts: forking
    /// copies the whole environment, so every visible pipe is capturable
    /// whether or not the child body names it. Over-marking only costs
    /// zero-copy opportunities, never correctness.
    pub(super) fn mark_captured_pipes_escaped(&self) {
        for scope in self.var_scopes.iter() {
            for (_, value) in scope.values() {
                mark_value_pipes_escaped(value);
            }
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
    pub(super) fn declare_var(&mut self, key: String, kind: TypeTag, value: Value) -> Result<()> {
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
        let coerced = super::args::coerce_value(value, &kind)?;
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
        let coerced = super::args::coerce_value(value, &kind)?;
        for scope in self.var_scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(key) {
                slot.1 = coerced;
                return Ok(());
            }
        }
        anyhow::bail!("undeclared variable ${key}");
    }

    /// Append `item` to the LIST binding `key` in place. Copy-on-write:
    /// the buffer detaches only when shared, so aliases keep their
    /// contents; the sole owner mutates with no copy. Bails for
    /// undeclared names and non-list bindings. Shaped lists validate
    /// the item against the element tag, so a push can never silently
    /// corrupt the shape the declaration promises.
    pub(super) fn push_into_list(&mut self, key: &str, item: Value) -> Result<()> {
        let kind = self
            .var_scopes
            .iter()
            .rev()
            .find_map(|s| s.get(key).map(|(k, _)| k.clone()))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "undeclared variable ${key}: declare it first with LET ${key}: LIST<...> = ..."
                )
            })?;
        let element = match kind {
            TypeTag::List => None,
            TypeTag::ListOf(element) => Some(element.as_ref().clone()),
            _ => {
                anyhow::bail!("LIST_APPEND ${key}: variable is {}, not LIST", kind.name());
            }
        };
        for scope in self.var_scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(key) {
                let list = slot.1.as_list_mut().ok_or_else(|| {
                    anyhow::anyhow!("LIST_APPEND ${key}: variable is not a LIST value")
                })?;
                if let Some(expected) = element {
                    oxdock_parser::check_value_at(
                        &expected,
                        &item,
                        &format!("${key}[{}]", list.len()),
                    )?;
                }
                list.push(item);
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

    pub(super) fn get_var_typed(&self, key: &str) -> Option<(TypeTag, Value)> {
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

/// Mark every pipe handle reachable from `value` as shared-with-a-child.
/// Pipes travel inside lists and maps (`LET $bag: LIST = [$p]`), so a
/// shallow top-level check would miss them and let a later promotion
/// corrupt the shared EOF contract. Values form a tree, so recursion
/// terminates; host-opaque payloads cannot carry handles.
fn mark_value_pipes_escaped(value: &Value) {
    if let Some(handle) = value.as_pipe_handle() {
        handle.mark_escaped();
        return;
    }
    if let Some(items) = value.as_list() {
        for item in items {
            mark_value_pipes_escaped(item);
        }
    }
    if let Some(entries) = value.as_map() {
        for item in entries.values() {
            mark_value_pipes_escaped(item);
        }
    }
}
