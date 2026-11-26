use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use super::diagnostics as state_diag;
use oxdock_diagnostics::DiagnosticResult;
use oxdock_fs::{CargoScratch, GuardedPath, WorkspaceFs, WorkspaceSelection};
use oxdock_parser::{
    Field, GuardExpr, KEYWORD_TYPE, LIST_TYPE_NAME, Step, TypeDescriptor, TypeTag, Value, sigil_var,
};
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

/// Single message shape for every refused `CONST` write: `$var = <expr>`
/// reassignment, `LIST_APPEND`, `MAP_SET`/`MERGE_MAPS`, and frozen-buffer
/// render this prefix, so tests and diagnostics match one string.
/// Free function (not a method) so host-function extractors and
/// macro-generated wrappers share it without a state handle.
pub fn const_mutation_error(what: &str) -> anyhow::Error {
    anyhow::Error::new(state_diag::eval_const_mutation(what))
}

/// Execution state for one script run. Public so hosts can register
/// functions and introspect listings; all fields stay crate-private so the
/// scope, recursion-budget, and task invariants cannot be broken from outside.
pub struct ExecState<P: ProcessManager> {
    pub(super) fs: Box<dyn WorkspaceFs>,
    /// Pre-reserved guarded scratch dir (session temp for child
    /// writes). Opaque [`CargoScratch`]: renderable for the child
    /// environment but not nameable as a `&GuardedPath`, so host code
    /// cannot `ensure()` or `create_dir_all` it. The child `cargo` creates it on demand.
    pub(super) cargo_scratch: CargoScratch,
    /// Pure session seed from [`oxdock_process::seed_defaults`], stored at
    /// init alongside the layered `envs`. `ISOLATED` resets clone this
    /// (not the layered map), and transport sanitize diffs against it.
    /// Built once: re-seeding would re-read host fallbacks and break
    /// state stability.
    pub(super) seed: Arc<HashMap<String, String>>,
    pub(super) cwd: GuardedPath,
    pub(super) envs: Arc<HashMap<String, String>>,
    /// Each entry is the script env as it stood at that block's entry, so
    /// the block allowlist resolves against the enclosing scope even as
    /// the live envs are scrubbed underneath. Nesting snapshots the
    /// already-scrubbed parent, so narrowing is monotonic by
    /// construction. Cloned (not reset) by `fork()` so tasks spawned
    /// inside a block inherit its scrub.
    pub(super) isolated_stack: Vec<Arc<HashMap<String, String>>>,
    pub(super) bg_children: Vec<BgChild>,
    pub(super) scope_stack: Vec<ScopeSnapshot>,
    pub(super) io: ExecIo,
    /// Pre-registered SlidingWindow observers for stream assertion steps.
    /// Keyed by (generation, step index). TeeWriter pushes every chunk to all windows.
    pub(super) assert_windows: Arc<Mutex<HashMap<(usize, usize), SlidingWindow>>>,
    /// Same observer map fed by the stderr tee for `ASSERT_CONTAINS stderr`.
    pub(super) assert_windows_stderr: Arc<Mutex<HashMap<(usize, usize), SlidingWindow>>>,
    /// Exact-match stdout accumulators for `ASSERT_EQ`/`ASSERT_NE stdout`, keyed by
    /// generation and fed by the same tee. Entries allocated during
    /// pre-registration observe bytes from scope entry; the common
    /// top-level case therefore sees trial-cumulative output.
    pub(super) exact_stdout: Arc<Mutex<HashMap<usize, ExactCapture>>>,
    /// Variable scopes for $variable bindings (FOR loops, LET assignments).
    /// Innermost scope is last. Variables are looked up from innermost to outermost.
    /// Each entry carries its declared type name alongside the value.
    pub(super) var_scopes: Vec<HashMap<String, (TypeTag, Value)>>,
    /// `CONST` names per variable scope frame, pushed and popped in
    /// lockstep with `var_scopes`. A name present here refuses `$var = <expr>`
    /// reassignment, `LIST_APPEND`, and rebinding; the value-level frozen flag
    /// additionally refuses writes through `LET`-bound aliases.
    pub(super) const_scopes: Vec<HashSet<String>>,
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
    /// Digest of the type environment above (aliases, schemas, custom
    /// types), folded per mutation: interned rendered tags key on
    /// spelling plus this digest, so one run's alias-dependent
    /// shapes never share entries with another run's.
    pub(super) type_env_digest: u64,
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
    /// Scope floor for the end-of-body reaper: task ids at or above this
    /// joined at scope end, older ones ignored. Set only by `TIMEOUT`
    /// (snapshot of the task-id counter at scope entry), so a worker the
    /// body leaves running gets up to the deadline while pre-existing
    /// tasks keep running past the scope. `None` joins everything: the
    /// pipeline top and every other scope.
    pub(super) reap_floor: Option<u64>,
    /// The single function registry: DSL `FUNC` definitions, builtins, and
    /// host extensions. Script entries scope lexically through the
    /// registry's own frames (managed by `push_scope`/`pop_scope`);
    /// native entries persist. Shared across `fork()` via clone.
    pub(super) functions: FunctionRegistry<P>,
    /// Entry script file (#115), when the run started from a file: base
    /// for top-level relative `IMPORT` paths. `None` for stdin and inline
    /// runs (imports resolve against the workspace root).
    pub(super) entry_file: Option<GuardedPath>,
    /// Active file-import chain (#115), innermost last: cycle detection
    /// names the full chain, and the importer dir is the top's parent.
    pub(super) import_stack: Vec<GuardedPath>,
    /// One file module per resolved import path (#115): parse once,
    /// validate once, initialize once. Every alias is a view onto the
    /// same record and the same shared state.
    pub(super) modules: HashMap<String, ModuleRecord>,
    pub(super) validated_modules: HashSet<String>,
    /// Files the static closure pass already proved (#115): entry plus
    /// every transitively imported file, checked with full nested
    /// knowledge. Runtime re-validation of the same registry-cached
    /// body would only false-positive on nested calls, so imports skip
    /// the single-file re-proof when their key is present. Unseen
    /// files (direct API use, no static pass) still validate here.
    /// Active module context (#115), innermost last: `Some(key)` while
    /// module-owned code runs (init body or module-owned `FUNC`),
    /// `None` past an entry-function boundary (entry `FUNC`s stay
    /// params-only and never see module state). Reads and mutations
    /// consult the topmost `Some` store after lexical frames miss.
    pub(super) module_stack: Vec<Option<String>>,
    /// Executed file-import alias targets per unit (#115): alias to
    /// defining file keys for imports that actually ran, keyed by the
    /// importing unit (`"entry"` for entry steps, the file key for
    /// module bodies). Unit-top layer only: unit-top-level imports
    /// land here (anything executed without a live lexical scope);
    /// function-local imports land on the innermost
    /// [`ExecState::call_alias_frames`] entry instead, and block
    /// imports on the innermost [`ExecState::scope_alias_frames`]
    /// entry, so neither leaks past its boundary. Mirrors the static
    /// per-unit `outbound_alias_targets` for the runtime placeholder
    /// gate, so file-backed `EXPAND` templates (invisible to static
    /// checking) classify known aliases identically: host harvests
    /// record an explicit host branch, and module bodies resolve
    /// against their own file's imports, never the importer's.
    pub(super) executed_alias_targets: HashMap<String, super::native::AliasTargets>,
    /// Block-scoped import frames, innermost last: one entry per
    /// live lexical scope that executed an `IMPORT`, tagged with the
    /// scope-stack depth at creation and the importing unit. Reads
    /// merge every live frame for their unit (outer-to-inner) over
    /// the unit-top map, so an `IMPORT` inside `IF`/`FOR`/`WHILE`/
    /// `SWITCH`/`ISOLATED` resolves within its block and unwinds on
    /// exit. `pop_scope` drops frames deeper than the surviving
    /// stack; `fork` flattens live frames into the child's unit-top
    /// map (the child manages its own scopes). Function bodies keep
    /// using [`ExecState::call_alias_frames`], checked first.
    pub(super) scope_alias_frames: Vec<ScopeAliasFrame>,
    /// Active function-call import frames: one entry per live
    /// [`call_func_value`](super::handlers::call_func_value) invocation,
    /// holding that invocation's own local imports. The innermost frame
    /// for the active unit shadows the unit-top map exactly like the
    /// static per-function overlays shadow the unit imports; popping on
    /// return (via an RAII guard) restores the caller's view. Each
    /// frame records the scope-stack depth at call entry: imports
    /// execute into the call frame only at that depth (the body's own
    /// top level), while deeper block scopes record into
    /// [`ExecState::scope_alias_frames`] so they unwind on block exit;
    /// reads merge only scope frames deeper than the innermost live
    /// call frame, so a caller block's imports never leak into a
    /// same-unit callee.
    pub(super) call_alias_frames: Vec<CallAliasFrame>,
    /// Frame-2..N floor (#322): `functions.scopes` index at which the
    /// current module call (or init) body starts. Nested-frame lookup
    /// walks down to this floor and no further, so caller frames from
    /// other units are unreachable by construction. Pushed alongside
    /// `call_alias_frames` (and init scopes) on every path. Entry
    /// calls walk all live frames: with inits popped, every live
    /// frame is entry-owned by construction.
    pub(super) func_floors: Vec<usize>,
    /// Bare-harvest visibility per importing unit (#322): (unit, base)
    /// to defining file key. Harvests persist through this map past
    /// init pops, never through scope frames.
    pub(super) harvest_map: HashMap<(String, String), String>,
    /// Parse-time module table (#115): imported files parse with the same
    /// table as the entry script.
    pub(super) module_table: oxdock_parser::ModuleTable,
    /// Current nested function-call depth on this thread. Enforced against
    /// `MAX_CALL_DEPTH`; cloned (not reset) by `fork()` so async children
    /// inherit the caller's depth budget.
    pub(super) call_depth: usize,
    /// Executed `INHERIT_ENV` keys per live module init, innermost last.
    /// Pushed empty before the init body runs and popped after, so
    /// `inherit_env` records keys as they actually execute under their
    /// guards instead of re-scanning steps against final envs.
    pub(super) init_inherit_stack: Vec<Vec<String>>,
    /// Scope-stack depths that are module-init top levels, innermost
    /// last: `ensure_module_init` (`super::import`) pushes its isolation
    /// scope here so `record_executed_alias` can tell a module's own
    /// top level (unit-top, like the entry's) from a lexical block
    /// (scope frame, unwinds on exit). Nested inits nest strictly,
    /// so depths never repeat while live; `fork` clears it (the
    /// child's scopes restart empty).
    pub(super) init_scope_depths: Vec<usize>,
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
    pub(super) selection: WorkspaceSelection,
    pub(super) envs: Arc<HashMap<String, String>>,
}

/// One live block's imports: the scope-stack depth at creation plus
/// the importing unit's alias targets. Single owner for the
/// scope-frame shape; `record_executed_alias` writes it,
/// `unit_alias_targets` merges it, `pop_scope` reaps it.
#[derive(Clone, Default)]
pub(super) struct ScopeAliasFrame {
    pub(super) depth: usize,
    pub(super) unit: String,
    pub(super) targets: super::native::AliasTargets,
}

/// One live function call's imports: the importing unit, the
/// scope-stack depth at call entry (right after the call scope
/// pushes), and the call's own local imports. The depth tells
/// `record_executed_alias` apart the body's own top level (records
/// here) from nested blocks (record into [`ScopeAliasFrame`]), and
/// tells `unit_alias_targets` which scope frames belong to this
/// invocation (deeper ones) versus caller context. A forked child
/// restarts its scope stack at zero, so a cloned frame whose depth
/// exceeds the child's stack is stale: the child still merges its
/// targets (inherited visibility) but applies no depth floor from
/// it. Single owner for the call-frame shape.
#[derive(Clone, Default)]
pub(super) struct CallAliasFrame {
    pub(super) unit: String,
    pub(super) depth: usize,
    pub(super) targets: super::native::AliasTargets,
}

/// One parsed file module (#115), keyed by resolved display path: parse
/// once, validate once, initialize once. `init` is `None` until the
/// first import executes the body; every alias is a view onto the
/// same record and the same shared state.
#[derive(Clone, Default)]
pub(super) struct ModuleRecord {
    pub(super) steps: Vec<Step>,
    /// Single-file `EXPORT` metadata: `EXPORT` func names, `EXPORT TYPE`
    /// names, and `EXPORT CONST` names, each in source order. One struct
    /// (never three parallel lists) so the MCP server never confuses a
    /// type for a tool, and only `CONST` names are readable through
    /// `$ALIAS::NAME`.
    pub(super) exports: oxdock_parser::Exports,
    pub(super) init: Option<ModuleInit>,
    /// Frame-1 file scope (#322): every top-level `FUNC` of the file
    /// (exported, harvested, and private alike), indexed once at load
    /// so calls resolve without scanning steps on every invocation.
    /// Single owner for the defining-file view; the runtime walk and
    /// the static home index read the same shape.
    pub(super) top_funcs: HashMap<String, Vec<RawFuncDef>>,
}

impl ModuleInit {
    /// Fresh entry-unit init store (#4): the session seed plus the
    /// entry top-level's own `ENV` contributions, tracked as they
    /// execute. Entry `FUNC` calls reset to this map (never caller
    /// env), mirroring how module calls reset to `ModuleInit::envs`.
    /// `vars`/`const_vars` hold the entry prelude `CONST`s seeded
    /// into every entry call; `inherit_names` re-reads from the
    /// engine per call. Single owner for the entry record shape, so
    /// `new_state` and the test constructors can never drift.
    pub(super) fn entry_seed(envs: HashMap<String, String>, cwd: GuardedPath) -> Self {
        Self {
            vars: HashMap::new(),
            const_vars: HashSet::new(),
            return_value: Value::string(String::new()),
            envs,
            inherit_names: Vec::new(),
            selection: WorkspaceSelection::Snapshot,
            cwd,
        }
    }
}

impl ModuleRecord {
    /// Fresh entry-unit record (#4): empty steps/top-funcs plus the
    /// seeded init store above. Single owner for the entry shape, so
    /// every constructor seeds the same fields.
    pub(super) fn entry_record(envs: HashMap<String, String>, cwd: GuardedPath) -> Self {
        Self {
            steps: Vec::new(),
            exports: oxdock_parser::Exports::default(),
            init: Some(ModuleInit::entry_seed(envs, cwd)),
            top_funcs: HashMap::new(),
        }
    }
}

/// One top-level `FUNC` definition as parsed: the file frame of the
/// unit scope stack (#322). Params and return stay in source spelling
/// until a call resolves tags. Guards ride along and are evaluated
/// with the live env at use time, mirroring the previous per-call
/// step scan exactly (init-gated defs behave as before). `jail` is
/// sealed at execution: the first top-level execution of this
/// definition pins the live workspace selection and cwd, so later
/// calls restore the definition site, never caller ambient. `None`
/// for never-executed defs (forward refs, skipped harvests), which
/// fall back to the defining file default at call time.
#[derive(Clone, Debug, Default)]
pub(super) struct RawFuncDef {
    pub(super) params: Vec<(String, String, bool)>,
    pub(super) return_spelling: String,
    pub(super) is_async: bool,
    pub(super) body: Vec<Step>,
    pub(super) guard: Option<GuardExpr>,
    pub(super) jail: Option<(WorkspaceSelection, GuardedPath)>,
}

/// Initialized module state (#115): the shared globals every alias of
/// one file observes. `vars` holds the module body's top-level `LET`
/// bindings, drained from the init frame when the first import runs;
/// module-owned `FUNC`s resolve names against `vars` after lexical
/// frames miss, so one file's functions share one global state instead
/// of each alias getting a private copy. `return_value` caches the
/// init body's `RETURN`, so repeated imports bind the same value.
#[derive(Clone)]
pub(super) struct ModuleInit {
    pub(super) vars: HashMap<String, (TypeTag, Value)>,
    /// Top-level `CONST` names drained with `vars`: rebinding,
    /// `$var = <expr>` reassignment, and `LIST_APPEND` against these fail;
    /// rebuilds fail on the value-level frozen flag.
    pub(super) const_vars: HashSet<String>,
    pub(super) return_value: Value,
    /// Script env the module's functions execute with (#115): the
    /// session seed plus the init body's own `ENV` contributions
    /// (captured as a delta against the pre-init map, so
    /// caller-established values stay out). `INHERIT_ENV` names are
    /// re-read from the host at each call (see `inherit_names`),
    /// since a re-inherit of an already-present value leaves no delta
    /// yet must still admit the name.
    pub(super) envs: HashMap<String, String>,
    /// Top-level `INHERIT_ENV` names of the module file, re-established
    /// from the host on every module-owned call. A module never
    /// inherits its caller's environment; each file declares what it
    /// reads, and the values flow from the host (or literals) at
    /// init/call time.
    pub(super) inherit_names: Vec<String>,
    /// The module top-level jail (#322): the workspace selection and
    /// cwd as the init body left them. Module-owned `FUNC`s restore
    /// this on entry (unless the body overrides it), so a callee
    /// executes in its defining file context no matter which
    /// workspace the caller sits in. Captured before the init scope
    /// pops; the pop restores the importer afterwards.
    pub(super) selection: WorkspaceSelection,
    pub(super) cwd: GuardedPath,
}

/// One anonymous background worker: the thread-join handle plus the
/// task id drawn for the worker at spawn. Only `ThreadJoinHandle`s are
/// ever pushed (a worker's own `RUN` steps wait inline); the id lets a
/// `TIMEOUT` scope-end reaper join only the workers its own body
/// spawned, leaving pre-existing ones alone.
pub(super) struct BgChild {
    pub(super) handle: Box<dyn BackgroundHandle>,
    pub(super) task_id: u64,
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

/// Lifecycle phase of a named background task (`LET $var: HANDLE<T> = ASYNC ...`).
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

/// Defining-file call context (#322): (envs, inherit names,
/// selection, cwd). One alias shared by the resolver and every call
/// site, never a restated tuple.
pub(super) type FileCallCtx = (
    HashMap<String, String>,
    Vec<String>,
    WorkspaceSelection,
    GuardedPath,
);

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

    pub(super) fn command_ctx(&self) -> DiagnosticResult<CommandContext> {
        // Resolve the working directory through the snapshot choke point: a
        // snapshot-rooted cwd materializes here (so `RUN` executes against a
        // real directory) while a local cwd resolves purely lexically with
        // zero I/O. The seeded scratch materializes alongside: `HOME` and
        // `TMPDIR` point at it, so children writing temp or profile data
        // must observe real directories, never lexical candidates.
        let cwd = self
            .fs
            .resolve_write(&self.cwd, ".")
            .map_err(|err| state_diag::eval_workdir_resolve_failed(&err))?;
        self.cargo_scratch
            .ensure_materialized()
            .map_err(|err| state_diag::eval_scratch_materialize_failed(&err))?;
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
    pub(super) fn alloc_task_id(&mut self) -> DiagnosticResult<u64> {
        // Purge cleanly-finished anonymous children first: their threads
        // are gone and a zero exit has no reporter, so retaining them
        // only grows the vec. Failed handles stay for the end-pipeline
        // reaper, which owns their error reporting.
        self.bg_children
            .retain_mut(|child| match child.handle.try_wait() {
                Ok(None) => true,
                Ok(Some(status)) => !status.success(),
                Err(_) => true,
            });
        self.live_tasks
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_RUNNING_TASKS as u64).then_some(n + 1)
            })
            .map_err(|_| state_diag::eval_task_cap_exceeded(MAX_RUNNING_TASKS as u64))?;
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
    /// - Cloned envs, cwd, cargo scratch name, var_scopes, const_scopes
    /// - Fresh bg_children, scope_stack (empty -- child manages its own)
    /// - Block imports flattened into the cloned unit-top map (the live
    ///   frames stay with the parent's scopes, so the child observes
    ///   what the parent saw at spawn without inheriting unwindable frames)
    /// - Shared assert_windows, assert_windows_stderr, exact_stdout (Arc clones)
    /// - Cloned io configuration
    /// - Independent cancel_token, active_process (child manages its own)
    /// - Shared named_tasks and next_task_id (via Arc clone)
    #[allow(dead_code)]
    pub(super) fn fork(&self) -> Self {
        let mut executed_alias_targets = self.executed_alias_targets.clone();
        for frame in &self.scope_alias_frames {
            for (alias, targets) in &frame.targets {
                executed_alias_targets
                    .entry(frame.unit.clone())
                    .or_default()
                    .entry(alias.clone())
                    .or_default()
                    .extend(targets.iter().cloned());
            }
        }
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
            const_scopes: self.const_scopes.clone(),
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
            reap_floor: self.reap_floor,
            functions: self.functions.clone(),
            entry_file: self.entry_file.clone(),
            import_stack: self.import_stack.clone(),
            modules: self.modules.clone(),
            validated_modules: self.validated_modules.clone(),
            module_stack: self.module_stack.clone(),
            executed_alias_targets,
            call_alias_frames: self.call_alias_frames.clone(),
            scope_alias_frames: Vec::new(),
            init_scope_depths: Vec::new(),
            func_floors: self.func_floors.clone(),
            harvest_map: self.harvest_map.clone(),
            module_table: self.module_table.clone(),
            types: self.types.clone(),
            record_schemas: self.record_schemas.clone(),
            module_states: self.module_states.clone(),
            type_aliases: self.type_aliases.clone(),
            type_env_digest: self.type_env_digest,
            call_depth: self.call_depth,
            init_inherit_stack: self.init_inherit_stack.clone(),
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
        for record in self.modules.values() {
            if let Some(init) = record.init.as_ref() {
                for entry in init.vars.values() {
                    mark_value_pipes_escaped(&entry.1);
                }
            }
        }
    }

    pub(super) fn push_var_scope(&mut self) {
        self.var_scopes.push(HashMap::new());
        self.const_scopes.push(HashSet::new());
    }

    pub(super) fn pop_var_scope(&mut self) {
        self.var_scopes.pop();
        self.const_scopes.pop();
    }

    /// Reset module-owned env (#115, #322): env to the module init
    /// stored map. Workspace and cwd are reset separately by the
    /// caller to the snapshot jail, so a module never observes the
    /// importer workspace. The surrounding `push_scope` restores the
    /// caller afterwards.
    pub(super) fn reset_module_env(&mut self, envs: HashMap<String, String>) {
        self.envs = Arc::new(envs);
    }

    /// Entry top-level tracking (#322 follow-up): true only for the
    /// entry prelude itself, never inside any call, module, or
    /// lexical block. Blocks (`ISOLATED`, `IF`, `FOR`, `WHILE`,
    /// `SWITCH`, `WITH_IO`) push `scope_stack` so their env unwinds
    /// on exit; capturing those writes would leak block-scoped and
    /// isolated secrets into every later entry call. Guard-gated
    /// top-level statements execute without pushing a scope, so
    /// conditional configuration still seeds the prelude. Caller
    /// env must never flow into the entry prelude store: `INHERIT_ENV`
    /// inherits from the engine, never another script, ever.
    pub(super) fn is_entry_top_level(&self) -> bool {
        self.call_depth == 0 && self.module_stack.is_empty() && self.scope_stack.is_empty()
    }

    /// Defining-file call context (#322): the single owner resolving
    /// what a `FUNC` body restores. The definition-site jail wins
    /// when the definition sealed one; otherwise the defining file
    /// fallback applies (module init jail, entry snapshot default).
    /// Env always resolves file-level (module init delta, entry
    /// prelude) with `INHERIT_ENV` re-read from the engine, never
    /// caller ambient. One lookup for every file, entry included:
    /// the entry record lives under the sentinel, so callers never
    /// restate a `Some`/`None` branch. Spelled once as [`FileCallCtx`].
    pub(super) fn callee_file_ctx(
        &self,
        module_key: Option<&str>,
        def_jail: Option<(WorkspaceSelection, GuardedPath)>,
    ) -> Option<FileCallCtx> {
        let unit = super::import::unit_key(module_key);
        if let Some((selection, cwd)) = def_jail {
            let (envs, inherit_names) = self.modules.get(unit).and_then(|record| {
                record
                    .init
                    .as_ref()
                    .map(|init| (init.envs.clone(), init.inherit_names.clone()))
            })?;
            return Some((envs, inherit_names, selection, cwd));
        }
        self.modules.get(unit).and_then(|record| {
            record.init.as_ref().map(|init| {
                (
                    init.envs.clone(),
                    init.inherit_names.clone(),
                    init.selection,
                    init.cwd.clone(),
                )
            })
        })
    }

    /// Enter a lexical scope: snapshot cwd/root/envs and open a fresh
    /// variable scope. Blocks scope everything (LET/ENV/WORKDIR/WORKSPACE);
    /// only pipes (ExecIo) and filesystem effects cross scope boundaries.
    /// Function definitions scope through the registry's own frames.
    pub(super) fn push_scope(&mut self) {
        self.scope_stack.push(ScopeSnapshot {
            cwd: self.cwd.clone(),
            selection: self.fs.selection(),
            envs: Arc::clone(&self.envs),
        });
        self.functions.push_scope();
        self.push_var_scope();
    }

    /// Exit a lexical scope, restoring everything `push_scope` saved
    /// plus dropping block import frames deeper than the surviving
    /// stack, so block imports unwind with their block.
    pub(super) fn pop_scope(&mut self) -> DiagnosticResult<()> {
        let snapshot = self
            .scope_stack
            .pop()
            .ok_or_else(state_diag::eval_scope_underflow)?;
        self.fs.restore_selection(snapshot.selection);
        self.cwd = snapshot.cwd;
        self.envs = snapshot.envs;
        self.functions.pop_scope();
        self.pop_var_scope();
        let depth = self.scope_stack.len();
        self.scope_alias_frames.retain(|frame| frame.depth <= depth);
        Ok(())
    }
    pub(super) fn declare_var(
        &mut self,
        key: String,
        kind: TypeTag,
        value: Value,
    ) -> DiagnosticResult<()> {
        if self
            .var_scopes
            .last()
            .map(|s| s.contains_key(&key))
            .unwrap_or(false)
        {
            return Err(state_diag::eval_redeclare_var(&key));
        }
        let coerced = super::args::coerce_value(value, &kind)?;
        let scope = self
            .var_scopes
            .last_mut()
            .ok_or_else(state_diag::eval_no_var_scope)?;
        scope.insert(key, (kind, coerced));
        Ok(())
    }

    /// Declare a `CONST` binding: same namespace as
    /// [`declare_var`](Self::declare_var)
    /// (a `LET` and a `CONST` cannot share a name in one frame), then
    /// mark the frame const and deep-freeze the value, so nested
    /// containers freeze with the binding instead of aliasing a
    /// mutable buffer.
    pub(super) fn declare_const(
        &mut self,
        key: String,
        kind: TypeTag,
        value: Value,
    ) -> DiagnosticResult<()> {
        let at_entry_top = self.is_entry_top_level();
        if self
            .var_scopes
            .last()
            .map(|s| s.contains_key(&key))
            .unwrap_or(false)
        {
            return Err(state_diag::eval_redeclare_const(&key));
        }
        let coerced = super::args::coerce_value(value, &kind)?;
        let frozen = Value::deep_freeze(&coerced);
        let scope = self
            .var_scopes
            .last_mut()
            .ok_or_else(state_diag::eval_no_var_scope)?;
        scope.insert(key.clone(), (kind.clone(), frozen.clone()));
        if let Some(frame) = self.const_scopes.last_mut() {
            frame.insert(key.clone());
        }
        if at_entry_top {
            // Entry prelude capture: the entry record's init store
            // holds the top-level `CONST`s seeded into every entry
            // call, mirroring how module inits drain theirs.
            if let Some(init) = self
                .modules
                .get_mut(super::import::ENTRY_UNIT)
                .and_then(|record| record.init.as_mut())
            {
                init.vars.insert(key.clone(), (kind, frozen));
                init.const_vars.insert(key);
            }
        }
        Ok(())
    }

    /// Whether the entry-`CONST` fallback applies here: entry context
    /// (no owning module) whose caller is also entry (or top level).
    /// Entry callees called from module code stay params-only, so a
    /// module caller never observes entry file state. Single owner
    /// for the caller check every entry fallback reads.
    fn entry_fallback_allowed(&self) -> bool {
        if self.current_module().is_some() {
            return false;
        }
        match self.module_stack.len() {
            0 => true,
            1 => true,
            _ => matches!(
                self.module_stack.get(self.module_stack.len() - 2),
                None | Some(None)
            ),
        }
    }

    /// Entry-`CONST` fallback init: the entry record's init when the
    /// fallback is allowed (entry context whose caller is also entry).
    /// Single owner for the caller check plus the init borrow, so
    /// every entry fallback reads hashes `ENTRY_UNIT` and checks the
    /// caller once. Call sites filter to `const_vars`, so `LET`s never
    /// cross.
    fn entry_const_init(&self) -> Option<&ModuleInit> {
        if !self.entry_fallback_allowed() {
            return None;
        }
        self.modules.get(super::import::ENTRY_UNIT)?.init.as_ref()
    }

    /// Whether `key` names a `CONST` binding visible here: the
    /// innermost lexical frame holding the name decides (a local
    /// `LET` shadowing a module-level `CONST` stays mutable), exactly
    /// mirroring [`mutate_var`](Self::mutate_var)'s resolution; only
    /// when no lexical frame declares the name does the owning
    /// module's shared store answer. Single owner for the const check
    /// behind `$var = <expr>` reassignment and `LIST_APPEND`.
    pub(super) fn is_const_var(&self, key: &str) -> bool {
        for idx in (0..self.var_scopes.len()).rev() {
            if self.var_scopes[idx].contains_key(key) {
                return self
                    .const_scopes
                    .get(idx)
                    .is_some_and(|frame| frame.contains(key));
            }
        }
        if let Some(module) = self.current_module()
            && let Some(record) = self.modules.get(&module)
            && let Some(init) = record.init.as_ref()
            && init.vars.contains_key(key)
        {
            return init.const_vars.contains(key);
        }
        // Entry `CONST`s are visible by fallback (#318): no synthetic
        // seed frame, just the init store filtered to consts.
        self.entry_const_init()
            .is_some_and(|init| init.vars.contains_key(key) && init.const_vars.contains(key))
    }

    /// Shared declaration switch behind `LET` and `CONST` capture,
    /// async, and await paths: one call site per declare, so a new
    /// binding form cannot forget the const branch.
    pub(super) fn declare_binding(
        &mut self,
        key: String,
        kind: TypeTag,
        value: Value,
        is_const: bool,
    ) -> DiagnosticResult<()> {
        if is_const {
            self.declare_const(key, kind, value)
        } else {
            self.declare_var(key, kind, value)
        }
    }

    pub(super) fn mutate_var(&mut self, key: &str, value: Value) -> DiagnosticResult<()> {
        if self.is_const_var(key) {
            return Err(state_diag::eval_const_mutation(key));
        }
        let kind = self
            .var_scopes
            .iter()
            .rev()
            .find_map(|s| s.get(key).map(|(k, _)| k.clone()))
            .or_else(|| self.module_var(key).map(|(k, _)| k.clone()))
            .ok_or_else(|| state_diag::eval_undeclared_declare(key, KEYWORD_TYPE))?;
        let coerced = super::args::coerce_value(value, &kind)?;
        for scope in self.var_scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(key) {
                slot.1 = coerced;
                return Ok(());
            }
        }
        // Not a lexical binding: update the owning module's shared
        // store in place, so every alias observes the mutation.
        if let Some((_, stored)) = self.module_var_mut(key) {
            *stored = coerced;
            return Ok(());
        }
        Err(state_diag::eval_undeclared(key))
    }

    /// Append `item` to the LIST binding `key` in place. Copy-on-write:
    /// the buffer detaches only when shared, so aliases keep their
    /// contents; the sole owner mutates with no copy. Bails for
    /// undeclared names and non-list bindings. Shaped lists validate
    /// the item against the element tag, so a push can never silently
    /// corrupt the shape the declaration promises.
    pub(super) fn push_into_list(&mut self, key: &str, item: Value) -> DiagnosticResult<()> {
        if self.is_const_var(key) {
            return Err(state_diag::eval_const_mutation(key));
        }
        let kind = self
            .var_scopes
            .iter()
            .rev()
            .find_map(|s| s.get(key).map(|(k, _)| k.clone()))
            .or_else(|| self.module_var(key).map(|(k, _)| k.clone()))
            .ok_or_else(|| {
                state_diag::eval_undeclared_declare(key, &format!("{LIST_TYPE_NAME}<...>"))
            })?;
        let element = match kind {
            TypeTag::List => None,
            TypeTag::ListOf(element) => Some(element.as_ref().clone()),
            _ => {
                return Err(state_diag::eval_list_not_list(key, kind.name()));
            }
        };
        for scope in self.var_scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(key) {
                if slot.1.is_frozen() {
                    return Err(state_diag::eval_const_mutation(key));
                }
                let list = slot
                    .1
                    .as_list_mut()
                    .ok_or_else(|| state_diag::eval_list_value_not_list(key))?;
                if let Some(expected) = element {
                    oxdock_parser::check_value_at(
                        &expected,
                        &item,
                        &format!("{}[{}]", sigil_var(key), list.len()),
                    )
                    .map_err(|err| state_diag::eval_list_element_failed(&err))?;
                }
                list.push(item);
                return Ok(());
            }
        }
        // Not a lexical binding: append to the owning module's shared
        // store in place, so module-global lists stay appendable from
        // module-owned functions with no importer visibility.
        if let Some((_, value)) = self.module_var_mut(key) {
            if value.is_frozen() {
                return Err(state_diag::eval_const_mutation(key));
            }
            let list = value
                .as_list_mut()
                .ok_or_else(|| state_diag::eval_list_value_not_list(key))?;
            if let Some(expected) = element {
                oxdock_parser::check_value_at(
                    &expected,
                    &item,
                    &format!("{}[{}]", sigil_var(key), list.len()),
                )
                .map_err(|err| state_diag::eval_list_element_failed(&err))?;
            }
            list.push(item);
            return Ok(());
        }
        Err(state_diag::eval_undeclared(key))
    }

    pub(super) fn get_var(&self, key: &str) -> Option<Value> {
        // Walk scopes from innermost to outermost
        for scope in self.var_scopes.iter().rev() {
            if let Some((_, value)) = scope.get(key) {
                return Some(value.clone());
            }
        }
        // Module-owned code falls back to the owning module's shared
        // store (#115): one file's functions share one global state.
        if let Some((_, value)) = self.module_var(key) {
            return Some(value.clone());
        }
        // Entry `CONST` fallback (#318): same init-store read the seed
        // frame copied, filtered to consts so `LET`s never cross.
        if let Some(init) = self.entry_const_init()
            && let Some((_, value)) = init.vars.get(key)
            && init.const_vars.contains(key)
        {
            return Some(value.clone());
        }
        None
    }

    pub(super) fn get_var_typed(&self, key: &str) -> Option<(TypeTag, Value)> {
        for scope in self.var_scopes.iter().rev() {
            if let Some(entry) = scope.get(key) {
                return Some(entry.clone());
            }
        }
        if let Some(entry) = self.module_var(key) {
            return Some(entry.clone());
        }
        if let Some(init) = self.entry_const_init()
            && let Some(entry) = init.vars.get(key)
            && init.const_vars.contains(key)
        {
            return Some(entry.clone());
        }
        None
    }

    /// Owning module of the executing code (#115): the top of the
    /// module stack when it names a module, `None` past an entry
    /// boundary or outside any module. Lookups stop at the first
    /// entry from the top, so entry functions called from module
    /// code stay params-only.
    pub(super) fn current_module(&self) -> Option<String> {
        match self.module_stack.last() {
            Some(Some(key)) => Some(key.clone()),
            _ => None,
        }
    }

    /// Frame-1 file scope for one unit: entry and modules share this
    /// one lookup, so the runtime walk reads the same shape for both.
    /// `None` only when the unit record is absent (never after
    /// validation, which registers the entry record).
    pub(super) fn unit_top_funcs(&self, unit: &str) -> Option<&HashMap<String, Vec<RawFuncDef>>> {
        self.modules.get(unit).map(|record| &record.top_funcs)
    }

    /// One lookup for the owning module's shared store (#115): the
    /// single fallback every variable accessor uses after its lexical
    /// walk misses, so module-global visibility lives in exactly one
    /// place. Entry code never reaches here (its boundary pushes
    /// `None`, and `current_module` yields nothing then).
    fn module_var(&self, key: &str) -> Option<&(TypeTag, Value)> {
        self.module_store()?.get(key)
    }

    /// Mutable twin of [`ExecState::module_var`], for the two
    /// accessors that write through to the store in place.
    fn module_var_mut(&mut self, key: &str) -> Option<&mut (TypeTag, Value)> {
        self.module_store_mut()?.get_mut(key)
    }

    /// Get a flattened view of all variables across all scopes.
    /// Inner scopes take precedence over outer scopes. Module-owned
    /// code additionally sees the owning module's shared store beneath
    /// every lexical frame; entry code sees the entry `CONST` fallback
    /// (filtered to consts) instead of a seed frame.
    pub(super) fn all_vars(&self) -> HashMap<String, Value> {
        let mut result = HashMap::new();
        for scope in self.var_scopes.iter().rev() {
            for (k, (_, v)) in scope {
                result.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        if let Some(store) = self.module_store() {
            for (k, (_, v)) in store {
                result.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        if let Some(init) = self.entry_const_init() {
            for (k, (_, v)) in &init.vars {
                if init.const_vars.contains(k) {
                    result.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
        result
    }

    /// Borrow the owning module's shared store, if any. The single
    /// map behind [`ExecState::module_var`], exposed whole for the
    /// one accessor that flattens every binding.
    fn module_store(&self) -> Option<&HashMap<String, (TypeTag, Value)>> {
        let module = self.current_module()?;
        self.modules
            .get(&module)?
            .init
            .as_ref()
            .map(|init| &init.vars)
    }

    /// Mutable twin of [`ExecState::module_store`], for the accessors
    /// that write through to the store in place.
    fn module_store_mut(&mut self) -> Option<&mut HashMap<String, (TypeTag, Value)>> {
        let module = self.current_module()?;
        self.modules
            .get_mut(&module)?
            .init
            .as_mut()
            .map(|init| &mut init.vars)
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
