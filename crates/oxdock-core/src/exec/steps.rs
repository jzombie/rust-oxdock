use std::collections::HashMap;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::Result;
use oxdock_diagnostics::{Diagnostic, DiagnosticResult, Span};
use oxdock_fs::GuardedPath;
use oxdock_parser::{
    Arg, AssertTarget, Command, KEYWORD_BREAK, KEYWORD_CONTINUE, KEYWORD_RETURN, Step, StepKind,
    Value, guard_option_allows,
};
use oxdock_process::{BackgroundHandle, CommandStdin, ProcessManager, SharedInput, SharedOutput};

/// Create an ExitStatus from a raw exit code. Cross-platform.
fn exit_status_from_code(code: i32) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }
}

use super::diagnostics as step_diag;
use super::handlers;
use super::io::{ExactCapture, SlidingWindow, StreamHandle};
use super::native::PureTable;
use super::state::{ExecState, TaskEntry, TaskPhase};
use oxdock_pipe::PipeInner;

/// A background handle wrapping a `std::thread::JoinHandle` for ASYNC blocks
/// that execute commands in a background thread.
pub(super) struct ThreadJoinHandle {
    join: Option<std::thread::JoinHandle<Result<()>>>,
    cancel_token: Arc<AtomicBool>,
    cancel_hub: Arc<oxdock_process::CancelHub>,
    active_process: Arc<Mutex<Option<Box<dyn BackgroundHandle>>>>,
    /// Identity of the worker thread, published by the child on entry.
    /// Forked worker state shares the parent task registry via `Arc`, so a
    /// parent that errors out can drop its registry reference while the
    /// worker is still alive. The worker then becomes the last registry
    /// owner and would drop (and join) its own handle on thread exit, which
    /// is undefined behavior (`pthread_join` on self). Detect that case and
    /// detach instead of joining.
    worker: Arc<Mutex<Option<std::thread::ThreadId>>>,
    /// Preserved error from the child thread, if any. Stored as a
    /// `Diagnostic` (`Clone` removes the flatten re-emit): key and
    /// span survive the join boundary without string flattening.
    /// Outer frames ride separately in `thread_context` and re-attach
    /// natively via `with_contexts` on re-emit (no anyhow layers),
    /// so the handle crosses `DiagnosticResult` boundaries.
    thread_error: Option<Diagnostic>,
    /// Plain-text contexts captured above the reclaimed diagnostic
    /// (outer `step N` positions, call frames), outermost first.
    /// They replay natively via `with_contexts` on re-emit; the
    /// diagnostic itself already carries its innermost position
    /// natively via `step_error`'s `with_span` sync.
    /// `ExitRequest` nodes are never captured here; the code rides in
    /// `thread_exit_code` instead.
    thread_context: Vec<String>,
    /// `EXIT` code from the child thread, if it exited. The code rides
    /// alongside because the join boundary cannot move the typed
    /// request by value; re-emit re-attaches it when the stored
    /// diagnostic chain lacks one.
    thread_exit_code: Option<i64>,
}

impl ThreadJoinHandle {
    pub(super) fn new(
        join: std::thread::JoinHandle<Result<()>>,
        cancel_token: Arc<AtomicBool>,
        cancel_hub: Arc<oxdock_process::CancelHub>,
        active_process: Arc<Mutex<Option<Box<dyn BackgroundHandle>>>>,
        worker: Arc<Mutex<Option<std::thread::ThreadId>>>,
    ) -> Self {
        Self {
            join: Some(join),
            cancel_token,
            cancel_hub,
            active_process,
            worker,
            thread_error: None,
            thread_context: Vec::new(),
            thread_exit_code: None,
        }
    }

    /// Re-emit the preserved thread error as a `Diagnostic`. Branches
    /// on the stored pair: a diagnostic clones directly (key, message,
    /// span, hint, and fields survive, with an `EXIT` code chained
    /// through the request itself only when the stored chain lacks
    /// one); a bare exit relays as the keyed exit-requested diagnostic
    /// (code in the field, no identical source); neither stored means
    /// the thread vanished without an error. Captured plain-text
    /// contexts replay natively via `with_contexts` (outermost first),
    /// preserving the full call path without anyhow layers.
    fn reemit_error(&self) -> Diagnostic {
        let diag = match (self.thread_error.clone(), self.thread_exit_code) {
            (Some(mut diag), maybe_code) => {
                if let Some(code) = maybe_code
                    && super::exit::exit_code_of_diag(&diag).is_none()
                {
                    let exit = match diag.shared_source() {
                        Some(prior) => super::exit::ExitRequest::with_source(code, prior),
                        None => super::exit::ExitRequest::new(code),
                    };
                    diag = diag.with_source(Arc::new(exit));
                }
                diag
            }
            (None, Some(code)) => step_diag::eval_exit_requested(code),
            (None, None) => {
                step_diag::eval_async_thread_panicked("background thread failed without an error")
            }
        };
        diag.with_contexts(self.thread_context.clone())
    }

    /// Whether the caller is the worker thread owned by this handle.
    fn is_self(&self) -> bool {
        let guard = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        guard.is_some_and(|id| id == std::thread::current().id())
    }

    /// Reap the thread if finished, preserving any error.
    fn reap(&mut self) {
        if self.join.is_none() {
            return;
        }
        if self.is_self() {
            // The worker is dropping the last registry reference on its own
            // exit path (parent already tore down or errored out). Detach
            // instead of joining self, which is undefined behavior.
            let _ = self.join.take();
            return;
        }
        let handle = self.join.take().unwrap();
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                self.thread_exit_code = super::exit::exit_code_of(&e);
                // Reclaim the keyed payload by walking the chain:
                // parents attach `.context(...)` during migration, so
                // the `Diagnostic` is rarely outermost. Layers above
                // it are plain-text contexts (step positions, call
                // frames); keep each frame so the full call path still
                // renders. The diagnostic itself already carries its
                // innermost position natively (`step_error` sync).
                // `ExitRequest` nodes are never captured as text: the
                // code already rides in `thread_exit_code`. Cloning
                // keeps key, span, hint, and fields. Genuinely unkeyed
                // failures flatten to the `reason` field (no source
                // attached, matching every other foreign boundary).
                // A bare exit stores its frames with no diagnostic.
                let mut outer: Vec<String> = Vec::new();
                let mut found: Option<Diagnostic> = None;
                for cause in e.chain() {
                    if let Some(diag) = cause.downcast_ref::<Diagnostic>() {
                        found = Some(diag.clone());
                        break;
                    }
                    if cause.downcast_ref::<super::exit::ExitRequest>().is_none() {
                        outer.push(cause.to_string());
                    }
                }
                match found {
                    Some(diag) => {
                        self.thread_context = outer;
                        self.thread_error = Some(diag);
                    }
                    None if self.thread_exit_code.is_some() => {
                        self.thread_context = outer;
                    }
                    None => {
                        self.thread_error = Some(step_diag::eval_dispatch_failed(&e));
                    }
                }
            }
            Err(panic) => {
                let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "thread panicked".to_string()
                };
                self.thread_error = Some(step_diag::eval_async_thread_panicked(&msg));
            }
        }
    }
}

impl BackgroundHandle for ThreadJoinHandle {
    fn try_wait(&mut self) -> DiagnosticResult<Option<ExitStatus>> {
        if let Some(join) = &self.join {
            if join.is_finished() {
                self.reap();
            } else {
                return Ok(None);
            }
        }
        // The stored diagnostic clones directly, so repeated polls
        // re-emit without moving or flattening the chain. A bare
        // exit (no diagnostic) re-emits too.
        if self.thread_error.is_some() || self.thread_exit_code.is_some() {
            Err(self.reemit_error())
        } else {
            Ok(Some(exit_status_from_code(0)))
        }
    }

    fn kill(&mut self) -> DiagnosticResult<()> {
        // Signal cancellation
        self.cancel_token.store(true, Ordering::SeqCst);
        // Fan out to pump supervisors waiting on the hub (blocking
        // reads and channel waits cannot poll the token).
        self.cancel_hub.mark_cancelled();
        // Kill any active OS process to interrupt blocking wait
        if let Ok(mut guard) = self.active_process.lock()
            && let Some(ref mut proc) = *guard
        {
            let _ = proc.kill();
        }
        // Join the thread to ensure it completes before returning
        self.reap();
        Ok(())
    }

    fn wait(&mut self) -> DiagnosticResult<ExitStatus> {
        self.reap();
        // Same chain-preserving re-emit as `try_wait` above.
        if self.thread_error.is_some() || self.thread_exit_code.is_some() {
            Err(self.reemit_error())
        } else {
            Ok(exit_status_from_code(0))
        }
    }
}

impl Drop for ThreadJoinHandle {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

/// Monotonically increasing generation counter for assert_windows key scoping.
/// Each execute_steps invocation gets a unique generation, preventing key
/// collisions between nested scopes (for_loop bodies, WithIo blocks).
static ASSERT_GENERATION: AtomicUsize = AtomicUsize::new(0);

/// Intra-thread control-flow signal (`BREAK`/`CONTINUE`/`RETURN`).
/// Produced by steps, consumed by the nearest loop (`Break`/`Continue`) or
/// `call_func` (`Return`). Anything reaching a thread boundary (`ASYNC`
/// spawn, `await` reaping) or the pipeline top becomes a step-numbered
/// error. `idx` is the 0-based index of the originating step in its own
/// body, so boundary errors can name it.
#[derive(Debug)]
pub(super) enum Flow {
    Done,
    Break { idx: usize },
    Continue { idx: usize },
    Return { idx: usize, value: Value },
}

pub(super) fn allocate_assert_generation() -> usize {
    ASSERT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Which stream a stream-targeted assertion observes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AssertStream {
    Stdout,
    Stderr,
}

/// Extract the substring needle from a step asserting over a stream:
/// `ASSERT_CONTAINS stdout|stderr`, handling both top-level and
/// WITH_IO-wrapped variants. Returns the observed stream and the needle.
fn extract_stream_needle(kind: &StepKind) -> Option<(AssertStream, &Arg)> {
    let step = match kind {
        StepKind::WithIo { cmd, .. } => cmd.as_ref(),
        other => other,
    };
    match step {
        StepKind::AssertContains { haystack, needle } => match haystack {
            AssertTarget::Stdout => Some((AssertStream::Stdout, needle)),
            AssertTarget::Stderr => Some((AssertStream::Stderr, needle)),
            _ => None,
        },
        _ => None,
    }
}

/// Whether the step needs the exact-match stdout accumulator:
/// `ASSERT_EQ`/`ASSERT_NE stdout`, top-level or WITH_IO-wrapped.
fn needs_exact_stdout(kind: &StepKind) -> bool {
    let step = match kind {
        StepKind::WithIo { cmd, .. } => cmd.as_ref(),
        other => other,
    };
    matches!(
        step,
        StepKind::AssertEq {
            actual: AssertTarget::Stdout,
            ..
        } | StepKind::AssertNe {
            actual: AssertTarget::Stdout,
            ..
        }
    )
}

/// An assertion first argument evaluated far enough to check:
/// values stay typed, streams stay references to live buffers.
pub(super) enum ResolvedAssertTarget {
    Value(Value),
    Stdout,
    Stderr,
    Pipe(Vec<u8>),
}

/// Evaluate an assertion target. `Arg::Expr` evaluates typed;
/// strings, templates, and parts render to `String`; stream markers
/// resolve to live buffers (peeked, never consumed). A `$var` holding a
/// `PIPE` likewise peeks its backend bytes: lowering cannot know variable
/// types, so the pipe dispatch lives here where the value exists.
pub(super) fn resolve_assert_target<P: ProcessManager>(
    target: &AssertTarget,
    cx: &mut StepCtx<'_, P>,
) -> DiagnosticResult<ResolvedAssertTarget> {
    match target {
        AssertTarget::Value(arg) => {
            let value = super::args::evaluate_assert_operand(arg, cx)?;
            if let Some(handle) = value.as_pipe_handle() {
                let bytes = cx.state.io.peek_pipe_content(&handle).map_err(|e| {
                    step_diag::eval_pipe_assertion_read_failed(&anyhow::Error::from(e))
                })?;
                return Ok(ResolvedAssertTarget::Pipe(bytes));
            }
            Ok(ResolvedAssertTarget::Value(value))
        }
        AssertTarget::Stdout => Ok(ResolvedAssertTarget::Stdout),
        AssertTarget::Stderr => Ok(ResolvedAssertTarget::Stderr),
    }
}

/// Pre-register stream assertion observers so tees feed them data before
/// the steps execute. Substring needles (`ASSERT_CONTAINS stdout|stderr`)
/// get per-step `SlidingWindow`s; `ASSERT_EQ`/`ASSERT_NE stdout` allocates the
/// generation's exact accumulator. Uses `args::resolve_arg_state` for
/// actual template expansion. Handles both top-level and WITH_IO-wrapped
/// assertions via `extract_stream_needle` / `needs_exact_stdout`.
pub(super) fn pre_register_assertions<P: ProcessManager>(
    state: &mut ExecState<P>,
    steps: &[Step],
    generation: usize,
) -> DiagnosticResult<()> {
    let mut windows = match state.assert_windows.lock() {
        Ok(guard) => guard,
        Err(_) => {
            return Err(super::diagnostics::std_assert_lock_poisoned(
                "assert_windows",
            ));
        }
    };
    let mut stderr_windows = match state.assert_windows_stderr.lock() {
        Ok(guard) => guard,
        Err(_) => {
            return Err(super::diagnostics::std_assert_lock_poisoned(
                "assert_windows_stderr",
            ));
        }
    };
    let mut exact = match state.exact_stdout.lock() {
        Ok(guard) => guard,
        Err(_) => return Err(super::diagnostics::std_assert_lock_poisoned("exact_stdout")),
    };
    for (idx, step) in steps.iter().enumerate() {
        if let Some((stream, arg)) = extract_stream_needle(&step.kind) {
            let resolved = super::args::resolve_arg_state(arg, state)?;
            let map = match stream {
                AssertStream::Stdout => &mut windows,
                AssertStream::Stderr => &mut stderr_windows,
            };
            map.insert((generation, idx), SlidingWindow::new(resolved.into_bytes()));
        }
        if needs_exact_stdout(&step.kind) {
            exact.entry(generation).or_insert_with(ExactCapture::new);
        }
    }
    Ok(())
}

/// After an environment mutation (ENV or INHERIT_ENV), re-expand all
/// substring assertion needles for the current generation to reflect new
/// env values. Preserves ring buffer history via `update_needle`. Handles
/// both top-level and WITH_IO-wrapped assertions. Exact accumulators hold
/// no needle and need no sync.
#[allow(clippy::collapsible_if)]
pub(super) fn sync_iteration_assert_needles<P: ProcessManager>(
    state: &ExecState<P>,
    steps: &[Step],
    generation: usize,
) -> DiagnosticResult<()> {
    let mut windows = match state.assert_windows.lock() {
        Ok(guard) => guard,
        Err(_) => {
            return Err(super::diagnostics::std_assert_lock_poisoned(
                "assert_windows",
            ));
        }
    };
    let mut stderr_windows = match state.assert_windows_stderr.lock() {
        Ok(guard) => guard,
        Err(_) => {
            return Err(super::diagnostics::std_assert_lock_poisoned(
                "assert_windows_stderr",
            ));
        }
    };
    for (idx, step) in steps.iter().enumerate() {
        if let Some((stream, arg)) = extract_stream_needle(&step.kind) {
            let map = match stream {
                AssertStream::Stdout => &mut windows,
                AssertStream::Stderr => &mut stderr_windows,
            };
            if let Some(w) = map.get_mut(&(generation, idx)) {
                let resolved = super::args::resolve_arg_state(arg, state)?;
                w.update_needle(resolved.into_bytes());
            }
        }
    }
    Ok(())
}

/// Pseudo-terminal request carried from `WITH_IO [tty]` bindings to
/// the `RUN` handlers: terminal geometry for the pty master. `None`
/// (the common case) keeps hermetic pipes. Only `with_io` populates
/// this from the step bindings; every other context constructs `None`.
/// Dimensions stay optional until spawn: unnamed sides read the host
/// terminal when one sits behind our own stdio, falling back to the
/// shared default otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtyRequest {
    /// Terminal rows (`rows = <int>`), or `None` to read the host.
    pub rows: Option<u16>,
    /// Terminal columns (`cols = <int>`), or `None` to read the host.
    pub cols: Option<u16>,
}

/// Per-step execution context handed to every command handler.
///
/// Host-registered functions receive this context: read script state through
/// the public accessors (`get_var`, `get_env`, `cwd`) and return a `Value`.
/// The fields stay crate-private so execution invariants hold for hosts.
///
/// Output contract (load-bearing for `LET`-capture, pipes, and stream assertions):
/// handlers must emit stdout/stderr ONLY through `out`/`err` : via
/// `write_stdout` or `StreamHandle::to_stdout`/`to_stderr` : and never write
/// to host stdout directly. The step runner swaps these handles per context:
/// `LET $x: STRING = <command>` installs a spillable capture sink, `WITH_IO`
/// installs named-pipe endpoints, and the root installs the assertion tee. A handler that bypasses its context handles silently breaks all three.
pub struct StepCtx<'a, P: ProcessManager> {
    pub(super) state: &'a mut ExecState<P>,
    pub(super) process: &'a mut P,
    pub(super) stdin: CommandStdin,
    pub(super) expose_stdin: bool,
    pub(super) out: Option<StreamHandle>,
    pub(super) err: Option<StreamHandle>,
    /// Pipe backend backing `out`, when a `WITH_IO` stdout binding resolved
    /// to a script pipe. Uniform context enrichment (populated for every
    /// command, read only by consumers that need the backend, like the
    /// network bridge). `None` for inherited, captured, and tee outputs,
    /// and for OS pairs (kernel bytes are invisible).
    pub(super) out_pipe: Option<Arc<PipeInner>>,
    /// Pipe backend backing `stdin`, when a `WITH_IO` stdin binding
    /// resolved to a script pipe. Lets bridge workers run timeout-bounded
    /// reads without touching shared pipe semantics (`None` for OS pairs,
    /// which fall back to blocking reads).
    pub(super) stdin_pipe: Option<Arc<PipeInner>>,
    /// Pseudo-terminal request from an enclosing `WITH_IO [tty]`
    /// binding. `RUN` handlers spawn through the shared pty pump when
    /// this is `Some`; every other command ignores it (lowering rejects
    /// `tty` outside `RUN` before execution reaches them).
    pub(super) tty: Option<TtyRequest>,
}

impl<'a, P: ProcessManager> StepCtx<'a, P> {
    /// Minimal context for headless MCP tool dispatch: no stdin, caller
    /// supplied output handles, no pipe backends. Stdin is always Null
    /// so tools can never block on host input.
    pub(super) fn for_mcp(
        state: &'a mut ExecState<P>,
        process: &'a mut P,
        stdin: CommandStdin,
        out: Option<StreamHandle>,
        err: Option<StreamHandle>,
    ) -> Self {
        Self {
            state,
            process,
            stdin,
            expose_stdin: false,
            out,
            err,
            out_pipe: None,
            stdin_pipe: None,
            tty: None,
        }
    }

    /// Shared writer behind the step error stream, when one exists (never
    /// for OS kernel pairs). Lets streaming hosts pump guest stderr live
    /// instead of buffering it to the end of the block.
    pub(super) fn err_shared(&self) -> Option<SharedOutput> {
        match self.err.clone() {
            Some(StreamHandle::Stream(writer)) => Some(writer),
            _ => None,
        }
    }

    /// Look up a script variable by name (innermost scope first).
    pub fn get_var(&self, key: &str) -> Option<Value> {
        self.state.get_var(key)
    }

    /// Look up an environment variable visible to the script: the
    /// script map, seeded at session start. Exact hit first,
    /// case-insensitive fallback on Windows.
    pub fn get_env(&self, key: &str) -> Option<String> {
        self.state.get_env(key).map(str::to_string)
    }

    /// Snapshot of the script-visible environment: `ENV` assignments
    /// layered over inherited entries, as currently scoped. Hosts staging
    /// child processes layer this over the host environment (the same
    /// contract `RUN` honors through `CommandContext`), so block-scoped
    /// `ENV` reaches the child and reverts at scope exit with no extra
    /// machinery.
    pub fn env_snapshot(&self) -> HashMap<String, String> {
        self.state.envs.as_ref().clone()
    }

    /// Materialize the session scratch hierarchy (root + `tmp`) for
    /// spawn sites that stage children without a `CommandContext`
    /// (PTY): the seeded `HOME`/`TMPDIR` paths must exist when the
    /// guest writes them, even when no `RUN` step ran first.
    pub fn ensure_scratch_materialized(&self) -> DiagnosticResult<()> {
        self.state
            .cargo_scratch
            .ensure_materialized()
            .map_err(|err| super::diagnostics::eval_scratch_materialize_failed(&err))
    }

    /// Current working directory (guarded; stays inside the workspace).
    pub fn cwd(&self) -> &GuardedPath {
        &self.state.cwd
    }

    /// Run root (guarded): the workspace the run was launched over.
    /// Unlike [`cwd`](Self::cwd), this never follows module-scope
    /// directories, so host helpers that read run-level files resolve
    /// identically from entry and hook scope.
    pub fn workspace_root(&self) -> &GuardedPath {
        self.state.fs.root()
    }

    /// Shared snapshot of every registered pure function, keyed by
    /// qualified name. Lets host-driven expansion resolve
    /// `{{ MODULE::FUNC(args) }}` through the live registry instead of
    /// hardcoding module or function names. Stateful, script, and pipe
    /// backed entries never appear here: placeholder evaluation has no
    /// step context and must stay side effect free. Dual entries
    /// contribute their template arm.
    pub fn pure_functions(&self) -> std::sync::Arc<PureTable> {
        self.state.pure_function_table()
    }

    /// Mint a fresh unbound pipe handle, like bare `LET $p: PIPE`. The
    /// backend materializes lazily on first binding; return it from a
    /// host function to hand the DSL a pipe it can bind. Tagged with the
    /// current task so promotion checks see the declaration origin.
    pub fn new_pipe(&self) -> Value {
        Value::pipe_fresh_in_task(self.state.task_id)
    }

    /// Borrow the read half of a `PIPE` value for byte streaming (see
    /// [`PipeStream`](super::PipeStream)). Unbound handles materialize as script pipes :
    /// hosts cannot spawn `RUN`, so script is the only sensible kind,
    /// and a later `RUN` binding adapts through the shared path. DSL,
    /// bridge, and host bindings on an OS-materialized handle resolve
    /// through the single-take bridge: the first call takes, repeats bail
    /// loudly (same contract as DSL consumers; use script-backed pipes
    /// for repeat or multi access).
    pub fn pipe_reader(&self, value: &Value) -> DiagnosticResult<SharedInput> {
        use oxdock_pipe::{Materialized, materialize};
        let Some(handle) = value.as_pipe_handle() else {
            return Err(super::diagnostics::std_pipe_needs_pipe(
                "pipe_reader",
                value.type_name(),
            ));
        };
        match materialize(&handle, false)? {
            Materialized::Script(backend) => Ok(backend.reader_handle()),
            #[cfg(not(miri))]
            Materialized::Os(entry) => {
                let owned = entry
                    .reader
                    .take()
                    .map_err(|_| super::diagnostics::std_pipe_handle_spent())?;
                Ok(Arc::new(Mutex::new(owned)))
            }
        }
    }

    /// Borrow the write half of a `PIPE` value for byte streaming (see
    /// [`PipeStream`](super::PipeStream)). Same materialization and take-once contract as
    /// [`StepCtx::pipe_reader`].
    pub fn pipe_writer(&self, value: &Value) -> DiagnosticResult<SharedOutput> {
        use oxdock_pipe::{Materialized, materialize};
        let Some(handle) = value.as_pipe_handle() else {
            return Err(super::diagnostics::std_pipe_needs_pipe(
                "pipe_writer",
                value.type_name(),
            ));
        };
        match materialize(&handle, false)? {
            Materialized::Script(backend) => Ok(backend.writer_handle()),
            #[cfg(not(miri))]
            Materialized::Os(entry) => {
                let owned = entry
                    .writer
                    .take()
                    .map_err(|_| super::diagnostics::std_pipe_handle_spent())?;
                Ok(Arc::new(Mutex::new(owned)))
            }
        }
    }

    /// Explicitly close a script pipe: readers drain buffered bytes, then
    /// observe EOF regardless of live writers or keeper pins. Unbound
    /// handles bail (closing a never-bound pipe is a caller bug), and
    /// OS-materialized handles bail (kernel pairs close by dropping their
    /// taken halves : drop the value instead).
    pub fn close_pipe(&self, value: &Value) -> DiagnosticResult<()> {
        let Some(handle) = value.as_pipe_handle() else {
            return Err(super::diagnostics::std_pipe_needs_pipe(
                "close_pipe",
                value.type_name(),
            ));
        };
        let Some(backend) = oxdock_pipe::script_backend(&handle) else {
            return Err(super::diagnostics::std_pipe_close_needs_script());
        };
        backend.force_close();
        Ok(())
    }

    /// Whether the current task was cancelled (`CANCEL`/`TIMEOUT`). For
    /// external host modules running blocking pumps: poll each tick so
    /// silent-but-open pipes cannot strand the task thread.
    pub fn is_cancelled(&self) -> bool {
        self.state
            .cancel_token
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Shared cancellation flag for pump threads spawned by host
    /// functions: clone into the worker and poll each tick, same as
    /// `is_cancelled`, so `CANCEL`/`TIMEOUT` ends the pump and its
    /// writer drop closes the output pipe for downstream readers.
    pub fn cancel_token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.state.cancel_token)
    }

    /// Event hub for the same cancellation: supervisors of blocking
    /// pumps wait on this instead of polling the token, and every
    /// `kill`/`TIMEOUT` path marks it alongside the token store.
    pub fn cancel_hub(&self) -> Arc<oxdock_process::CancelHub> {
        Arc::clone(&self.state.cancel_hub)
    }

    /// Whether this step runs on an `ASYNC` task thread. Blocking pumps
    /// must refuse the main sequential flow.
    pub fn is_async_task(&self) -> bool {
        self.state.inside_async
    }

    /// Module-shared state stashed at registration (`HostModule::with_state`).
    /// Lets `#[oxdock_func]` bodies reach state that cannot ride in a
    /// value (endpoint registries) without hand-built closures. `None`
    /// when the module registered no such state: a programmer error at
    /// module-build time, never a script failure.
    pub fn module_state<T: Send + Sync + 'static>(
        &self,
        module: &str,
    ) -> Option<std::sync::Arc<T>> {
        self.state
            .module_states
            .get(&(module.to_string(), std::any::TypeId::of::<T>()))?
            .clone()
            .downcast::<T>()
            .ok()
    }

    /// Resolve an explicitly passed `PIPE` value to its script backend for
    /// backend-aware pump operations (explicit close, cancel). This is value-based on
    /// purpose: the ambient `out_pipe`/`stdin_pipe` fields only populate via
    /// engine-level `WITH_IO` resolution, which never runs for host function
    /// calls. Returns `None` for unbound and OS-materialized handles, which
    /// fall back to plain blocking I/O.
    pub fn pipe_backend(&self, value: &Value) -> Option<Arc<PipeInner>> {
        let handle = value.as_pipe_handle()?;
        oxdock_pipe::script_backend(&handle)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn execute_steps<P: ProcessManager>(
    state: &mut ExecState<P>,
    process: &mut P,
    steps: &[Step],
    stdin: CommandStdin,
    expose_stdin: bool,
    out: Option<StreamHandle>,
    err: Option<StreamHandle>,
    out_pipe: Option<Arc<PipeInner>>,
    stdin_pipe: Option<Arc<PipeInner>>,
    wait_at_end: bool,
) -> Result<Flow> {
    let generation = allocate_assert_generation();
    let flow = match execute_steps_inner(InnerArgs {
        state,
        process,
        generation,
        steps,
        stdin,
        expose_stdin,
        out,
        err,
        out_pipe,
        stdin_pipe,
        wait_at_end,
    }) {
        Ok(flow) => flow,
        Err(e) => {
            // A step failed before end-of-pipeline reaping ran. Join
            // background work now so the parent owns teardown: otherwise the
            // parent drops its task-registry reference while a worker still
            // lives, leaving the worker as the last registry owner to drop
            // (and join) its own handle on thread exit.
            teardown_tasks_on_error(state);
            cleanup_assertion_generation(state, generation)?;
            return Err(e);
        }
    };
    // Cleanup: remove all assertion state for this generation
    cleanup_assertion_generation(state, generation)?;
    Ok(flow)
}

/// Remove per-generation assertion observers. Runs on success and on step
/// failure so a failed pipeline never leaks windows into later runs.
fn cleanup_assertion_generation<P: ProcessManager>(
    state: &mut ExecState<P>,
    generation: usize,
) -> DiagnosticResult<()> {
    let mut windows = match state.assert_windows.lock() {
        Ok(guard) => guard,
        Err(_) => {
            return Err(super::diagnostics::std_assert_lock_poisoned(
                "assert_windows",
            ));
        }
    };
    windows.retain(|(g, _), _| *g != generation);
    let mut stderr_windows = match state.assert_windows_stderr.lock() {
        Ok(guard) => guard,
        Err(_) => {
            return Err(super::diagnostics::std_assert_lock_poisoned(
                "assert_windows_stderr",
            ));
        }
    };
    stderr_windows.retain(|(g, _), _| *g != generation);
    let mut exact = match state.exact_stdout.lock() {
        Ok(guard) => guard,
        Err(_) => return Err(super::diagnostics::std_assert_lock_poisoned("exact_stdout")),
    };
    exact.retain(|g, _| *g != generation);
    Ok(())
}

/// Join background work after a step failure, mirroring the end-of-pipeline
/// fail-fast teardown. Anonymous handles always belong to the current
/// thread. Named entries are root-owned: worker threads must never block on
/// sibling tasks, which may depend on the worker via AWAIT.
fn teardown_tasks_on_error<P: ProcessManager>(state: &mut ExecState<P>) {
    for survivor in state.bg_children.iter_mut() {
        let _ = survivor.handle.kill();
    }
    state.bg_children.clear();
    if state.inside_async {
        return;
    }
    let entries: Vec<Arc<TaskEntry>> = {
        let named = state.named_tasks.lock().unwrap_or_else(|e| e.into_inner());
        named.values().cloned().collect()
    };
    let mut to_kill: Vec<(Arc<TaskEntry>, Box<dyn BackgroundHandle>)> = Vec::new();
    for entry in &entries {
        let mut guard = entry.state.lock().unwrap_or_else(|e| e.into_inner());
        match guard.phase {
            TaskPhase::Running | TaskPhase::Awaiting => {
                guard.phase = TaskPhase::Cancelled;
                if let Some(handle) = guard.handle.take() {
                    to_kill.push((Arc::clone(entry), handle));
                }
            }
            TaskPhase::Cancelled | TaskPhase::Completed => {}
        }
    }
    for (entry, mut handle) in to_kill {
        let _ = handle.kill();
        entry.finish_teardown();
    }
}

/// Execute a single step with an explicit generation and index.
/// Used by `with_io` to preserve the parent step's index for assertion window keys.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_single_step_with_generation<P: ProcessManager>(
    state: &mut ExecState<P>,
    process: &mut P,
    cmd: &StepKind,
    generation: usize,
    idx: usize,
    stdin: CommandStdin,
    expose_stdin: bool,
    out: Option<StreamHandle>,
    err: Option<StreamHandle>,
    out_pipe: Option<Arc<PipeInner>>,
    stdin_pipe: Option<Arc<PipeInner>>,
    tty: Option<TtyRequest>,
) -> Result<Flow> {
    let mut cx = StepCtx {
        state,
        process,
        stdin,
        expose_stdin,
        out,
        err,
        out_pipe,
        stdin_pipe,
        tty,
    };
    // Compound steps (loops, functions, scoped wrappers) participate in
    // Flow and dispatch through the Flow path; every other variant runs
    // the leaf pipeline below and yields Done. `Bind` captures run
    // through the flow path; every other `Bind` rhs runs leaf.
    match cmd {
        StepKind::FuncDef { .. }
        | StepKind::Call { .. }
        | StepKind::Return { .. }
        | StepKind::ImportFile { .. }
        | StepKind::ImportFuncs { .. }
        | StepKind::While { .. }
        | StepKind::Break
        | StepKind::Continue
        | StepKind::For { .. }
        | StepKind::If { .. }
        | StepKind::Switch { .. }
        | StepKind::Timeout { .. }
        | StepKind::Fail { .. }
        | StepKind::Isolated { .. }
        | StepKind::WithIo { .. }
        | StepKind::Bind {
            rhs: oxdock_parser::BindRhs::Capture(_),
            ..
        } => {
            return dispatch_flow_step(cmd, &mut cx, generation, idx);
        }
        _ => {}
    }
    match cmd {
        StepKind::Run(arg) => {
            let cmd = super::args::resolve_arg(arg, &mut cx)?;
            let cmd = super::args::expand_dsl_vars(&cmd, cx.state)?;
            Ok(handlers::run(&mut cx, &cmd)?)
        }
        StepKind::RunExec { argv } => {
            let resolved = handlers::resolve_run_exec_argv(argv, &mut cx)?;
            Ok(handlers::run_argv(&mut cx, &resolved)?)
        }
        StepKind::Echo(arg) => {
            let msg = super::args::resolve_arg(arg, &mut cx)?;
            handlers::echo(&mut cx, &msg)
        }
        StepKind::AsyncBlock { .. } => handlers::dispatch_async_block(cmd, &mut cx),
        StepKind::Workdir(arg) => {
            let path = super::args::resolve_arg(arg, &mut cx)?;
            Ok(handlers::workdir(&mut cx, &path)?)
        }
        StepKind::Workspace(target) => handlers::workspace(&mut cx, target),
        StepKind::RemoteBlock {
            target,
            vars,
            env,
            body,
        } => super::remote::run_remote_block(&mut cx, target, vars, env, body),
        StepKind::Env { key, value } => {
            let resolved = super::args::resolve_arg(value, &mut cx)?;
            handlers::env(&mut cx, key, &resolved)
        }
        StepKind::InheritEnv { keys } => {
            handlers::inherit_env(&mut cx, keys)?;
            sync_iteration_assert_needles(
                cx.state,
                &[Step {
                    guard: None,
                    kind: cmd.clone(),
                    scope_enter: 0,
                    scope_exit: 0,
                    line: 0,
                    col: 0,
                }],
                generation,
            )?;
            Ok(())
        }
        StepKind::IsolatedInheritEnv { keys } => {
            handlers::isolated_inherit_env(&mut cx, keys)?;
            Ok(())
        }
        StepKind::Copy {
            from_workspace,
            from_host,
            to_host,
            from,
            to,
        } => {
            if *from_host || *to_host {
                super::remote::copy_transfer(&mut cx, *from_host, *to_host, from, to)
            } else {
                let from_resolved = super::args::resolve_arg(from, &mut cx)?;
                let to_resolved = super::args::resolve_arg(to, &mut cx)?;
                Ok(handlers::copy(
                    &mut cx,
                    from_workspace.clone(),
                    &from_resolved,
                    &to_resolved,
                )?)
            }
        }
        StepKind::CopyGit {
            rev,
            from,
            to,
            include_dirty,
        } => {
            let rev_resolved = super::args::resolve_arg(rev, &mut cx)?;
            let from_resolved = super::args::resolve_arg(from, &mut cx)?;
            let to_resolved = super::args::resolve_arg(to, &mut cx)?;
            Ok(handlers::copy_git(
                &mut cx,
                &rev_resolved,
                &from_resolved,
                &to_resolved,
                *include_dirty,
            )?)
        }
        StepKind::Archive { src, dst, raw } => {
            let src_resolved = super::args::resolve_arg(src, &mut cx)?;
            let dst_resolved = super::args::resolve_arg(dst, &mut cx)?;
            Ok(handlers::archive(
                &mut cx,
                &src_resolved,
                &dst_resolved,
                *raw,
            )?)
        }
        StepKind::Unarchive {
            src,
            dst,
            max_bytes,
        } => {
            let src_resolved = super::args::resolve_arg(src, &mut cx)?;
            let dst_resolved = super::args::resolve_arg(dst, &mut cx)?;
            let cap_resolved = super::args::resolve_arg_opt(max_bytes, &mut cx)?;
            Ok(handlers::unarchive(
                &mut cx,
                &src_resolved,
                &dst_resolved,
                cap_resolved.as_deref(),
            )?)
        }
        StepKind::Symlink {
            from_workspace,
            from,
            to,
        } => {
            let from_resolved = super::args::resolve_arg(from, &mut cx)?;
            let to_resolved = super::args::resolve_arg(to, &mut cx)?;
            Ok(handlers::symlink(
                &mut cx,
                from_workspace.clone(),
                &from_resolved,
                &to_resolved,
            )?)
        }
        StepKind::Mkdir(arg) => {
            let path = super::args::resolve_arg(arg, &mut cx)?;
            Ok(handlers::mkdir(&mut cx, &path)?)
        }
        StepKind::Install { from, to } => {
            let from_resolved = super::args::resolve_arg(from, &mut cx)?;
            let to_resolved = super::args::resolve_arg(to, &mut cx)?;
            Ok(handlers::install(&mut cx, &from_resolved, &to_resolved)?)
        }
        StepKind::SetExecutable { path } => {
            let resolved = super::args::resolve_arg(path, &mut cx)?;
            Ok(handlers::set_executable(&mut cx, &resolved)?)
        }
        StepKind::Ls(arg) => {
            let resolved = super::args::resolve_arg_opt(arg, &mut cx)?;
            Ok(handlers::ls(&mut cx, &resolved)?)
        }
        StepKind::Read(arg) => {
            let resolved = super::args::resolve_arg_opt(arg, &mut cx)?;
            Ok(handlers::read(&mut cx, &resolved)?)
        }
        StepKind::ReadLine { var } => Ok(handlers::read_line(&mut cx, var)?),
        StepKind::Prompt {
            var,
            message,
            default,
        } => {
            let message_resolved = super::args::resolve_arg(message, &mut cx)?;
            let default_resolved = match default {
                Some(d) => Some(super::args::resolve_arg(d, &mut cx)?),
                None => None,
            };
            handlers::prompt(&mut cx, var, &message_resolved, default_resolved.as_deref())
        }
        StepKind::ListAppend { list, item } => {
            let value = super::args::evaluate_assert_operand(item, &mut cx)?;
            Ok(handlers::push_into(&mut cx, list, value)?)
        }
        StepKind::Write { path, contents } => {
            let path_resolved = super::args::resolve_arg(path, &mut cx)?;
            let contents_resolved = super::args::resolve_arg_opt(contents, &mut cx)?;
            Ok(handlers::write(
                &mut cx,
                &path_resolved,
                contents_resolved.as_deref(),
            )?)
        }
        StepKind::Append { path, contents } => {
            let path_resolved = super::args::resolve_arg(path, &mut cx)?;
            let contents_resolved = super::args::resolve_arg_opt(contents, &mut cx)?;
            Ok(handlers::append(
                &mut cx,
                &path_resolved,
                contents_resolved.as_deref(),
            )?)
        }
        StepKind::AppendLine { path, contents } => {
            let path_resolved = super::args::resolve_arg(path, &mut cx)?;
            let contents_resolved = super::args::resolve_arg(contents, &mut cx)?;
            Ok(handlers::append_line(
                &mut cx,
                &path_resolved,
                &contents_resolved,
            )?)
        }
        StepKind::Remove { path, recursive } => {
            let path_resolved = super::args::resolve_arg(path, &mut cx)?;
            Ok(handlers::remove(&mut cx, &path_resolved, *recursive)?)
        }
        StepKind::Expand { path, overrides } => {
            let path_resolved = super::args::resolve_arg_opt(path, &mut cx)?;
            let overrides_resolved = super::args::resolve_overrides(overrides, &mut cx)?;
            handlers::replace(&mut cx, &path_resolved, &overrides_resolved)
        }
        StepKind::AssertEq {
            hash,
            actual,
            expected,
        }
        | StepKind::AssertNe {
            hash,
            actual,
            expected,
        } => {
            let (verb, expect_equal) = match cmd {
                StepKind::AssertEq { .. } => (Command::AssertEq.as_str(), true),
                _ => (Command::AssertNe.as_str(), false),
            };
            let target = resolve_assert_target(actual, &mut cx)?;
            let expected_resolved = match expected {
                Some(e) => Some(super::args::evaluate_assert_operand(e, &mut cx)?),
                None => None,
            };
            handlers::assert_equality(
                &mut cx,
                generation,
                verb,
                expect_equal,
                hash,
                &target,
                expected_resolved.as_ref(),
            )
        }
        StepKind::AssertContains { haystack, needle } => {
            let target = resolve_assert_target(haystack, &mut cx)?;
            handlers::assert_contains(&mut cx, generation, idx, &target, needle)
        }
        StepKind::WithIoBlock { .. } => Err(step_diag::eval_with_io_unexpanded().into()),
        StepKind::Exit(code) => {
            let code = super::args::resolve_arg_as_int(code, &mut cx)?;
            Ok(handlers::exit(&mut cx, code)?)
        }
        StepKind::Bind {
            var,
            decl_type,
            is_const,
            rhs: oxdock_parser::BindRhs::Expr(expr),
        } => handlers::assign(&mut cx, var, decl_type.clone(), expr, *is_const),
        StepKind::Bind {
            var,
            decl_type,
            is_const,
            rhs: oxdock_parser::BindRhs::Async(body),
        } => Ok(handlers::dispatch_assign_async(
            var,
            decl_type.clone(),
            body,
            &mut cx,
            *is_const,
        )?),
        StepKind::Bind {
            var,
            decl_type,
            is_const,
            rhs: oxdock_parser::BindRhs::Await { task_var, .. },
        } => Ok(handlers::dispatch_await_capture(
            var,
            decl_type.clone(),
            task_var,
            &mut cx,
            *is_const,
        )?),
        StepKind::Set { var, expr } => handlers::set_var_value(&mut cx, var, expr),
        StepKind::Await { var, .. } => Ok(handlers::dispatch_await(var, &mut cx)?),
        StepKind::Cancel { var, .. } => Ok(handlers::dispatch_cancel(var, &mut cx)?),
        StepKind::Sleep { duration } => {
            let duration = super::args::resolve_arg_as_duration(duration, &mut cx)?;
            Ok(handlers::sleep(&mut cx, &duration)?)
        }
        StepKind::TypeAlias { .. } => {
            // Static declaration: collected before the run and
            // resolved at first coercion. Nothing to execute.
            Ok(())
        }
        StepKind::FuncDef { .. }
        | StepKind::Call { .. }
        | StepKind::Return { .. }
        | StepKind::ImportFile { .. }
        | StepKind::ImportFuncs { .. }
        | StepKind::While { .. }
        | StepKind::Break
        | StepKind::Continue
        | StepKind::For { .. }
        | StepKind::If { .. }
        | StepKind::Switch { .. }
        | StepKind::Timeout { .. }
        | StepKind::Fail { .. }
        | StepKind::Isolated { .. }
        | StepKind::WithIo { .. }
        | StepKind::Bind {
            rhs: oxdock_parser::BindRhs::Capture(_),
            ..
        } => {
            unreachable!("compound steps dispatch before this match")
        }
    }?;
    Ok(Flow::Done)
}

#[allow(clippy::too_many_arguments)]
/// Position context wrapping a failing step's error: `step N` plus
/// `line L` / `col C` when the step carries coordinates (parsed
/// scripts do; hand-built steps keep the historical bare rendering).
/// Applied once per dispatch level, so nested bodies chain outer and
/// inner positions like a call path. The text prefix stays
/// byte-identical; when the step carries coordinates the position
/// also attaches natively via `with_span` on the reclaimed
/// diagnostic (innermost wins: span-less only, so outer dispatch
/// levels never clobber the inner span).
fn step_error(step: &Step, idx: usize, err: anyhow::Error) -> anyhow::Error {
    let mut loc = format!("step {}", idx + 1);
    if step.line > 0 {
        loc.push_str(&format!(", line {}", step.line));
        if step.col > 0 {
            loc.push_str(&format!(", col {}", step.col));
        }
    }
    let mut err = err;
    if step.line > 0 {
        let span = if step.col > 0 {
            Span::line_col(step.line, step.col)
        } else {
            Span::line_only(step.line)
        };
        if let Some(diag) = err.downcast_mut::<Diagnostic>() {
            // Fast path: the diagnostic is the outermost head, so it
            // mutates in place with no rebuild.
            if diag.span().is_none() {
                let owned = diag.clone().with_span(span);
                *diag = owned;
            }
        } else if let Some(found) = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<Diagnostic>())
            .cloned()
        {
            // Slow path: context layers bury the diagnostic (synthetic
            // inner step with no coordinates, AWAIT re-emit frames,
            // helper-added context). Reclaim it the same lossless way
            // `reap` does: outer non-request frames replay natively via
            // `with_contexts` (no anyhow layers), the exit code rides
            // typed, sources survive the clone.
            if found.span().is_none() {
                let code = super::exit::exit_code_of(&err);
                let mut outer: Vec<String> = Vec::new();
                for cause in err.chain() {
                    if cause.downcast_ref::<Diagnostic>().is_some() {
                        break;
                    }
                    if cause.downcast_ref::<super::exit::ExitRequest>().is_none() {
                        outer.push(cause.to_string());
                    }
                }
                let mut reclaimed = found.with_span(span);
                if let Some(code) = code
                    && super::exit::exit_code_of_diag(&reclaimed).is_none()
                {
                    let exit = match reclaimed.shared_source() {
                        Some(prior) => super::exit::ExitRequest::with_source(code, prior),
                        None => super::exit::ExitRequest::new(code),
                    };
                    reclaimed = reclaimed.with_source(Arc::new(exit));
                }
                let carrier = anyhow::Error::new(reclaimed.with_contexts(outer));
                err = carrier;
            }
        }
    }
    err.context(loc)
}

/// Bundled inputs for [`execute_steps_inner`]. One struct parameter
/// keeps the function under the argument-count lint without an
/// override; call sites construct it from the same values.
struct InnerArgs<'a, P: ProcessManager> {
    state: &'a mut ExecState<P>,
    process: &'a mut P,
    generation: usize,
    steps: &'a [Step],
    stdin: CommandStdin,
    expose_stdin: bool,
    out: Option<StreamHandle>,
    err: Option<StreamHandle>,
    out_pipe: Option<Arc<PipeInner>>,
    stdin_pipe: Option<Arc<PipeInner>>,
    wait_at_end: bool,
}

fn execute_steps_inner<P: ProcessManager>(args: InnerArgs<'_, P>) -> Result<Flow> {
    let InnerArgs {
        state,
        process,
        generation,
        steps,
        stdin,
        expose_stdin,
        out,
        err,
        out_pipe,
        stdin_pipe,
        wait_at_end,
    } = args;
    // Pre-register assertion windows for this generation
    pre_register_assertions(state, steps, generation)?;

    for (idx, step) in steps.iter().enumerate() {
        // Check for cancellation before each step
        if state.cancel_token.load(Ordering::SeqCst) {
            return Err(step_diag::eval_async_cancelled().into());
        }
        if step.scope_enter > 0 {
            for _ in 0..step.scope_enter {
                state.push_scope();
            }
        }

        let should_run = guard_option_allows(step.guard.as_ref(), &state.envs);
        // One position wrap per iteration: the closure funnels every
        // error path (including `?` inside match arms, which would
        // otherwise return past a post-hoc wrap) through `step_error`.
        let flow_result: Result<Flow> = (|| {
            if !should_run {
                Ok(Flow::Done)
            } else {
                // Single dispatch table: compound steps run the Flow path;
                // every other variant runs the shared leaf pipeline in
                // `execute_single_step_with_generation`, so the two loops
                // can never drift.
                let flow_result: Result<Flow> = match &step.kind {
                    StepKind::FuncDef { .. }
                    | StepKind::Call { .. }
                    | StepKind::Return { .. }
                    | StepKind::ImportFile { .. }
                    | StepKind::ImportFuncs { .. }
                    | StepKind::While { .. }
                    | StepKind::Break
                    | StepKind::Continue
                    | StepKind::For { .. }
                    | StepKind::If { .. }
                    | StepKind::Switch { .. }
                    | StepKind::Timeout { .. }
                    | StepKind::Fail { .. }
                    | StepKind::Isolated { .. }
                    | StepKind::WithIo { .. }
                    | StepKind::Bind {
                        rhs: oxdock_parser::BindRhs::Capture(_),
                        ..
                    } => {
                        let mut cx = StepCtx {
                            state,
                            process,
                            stdin: stdin.clone(),
                            expose_stdin,
                            out: out.clone(),
                            err: err.clone(),
                            out_pipe: out_pipe.clone(),
                            stdin_pipe: stdin_pipe.clone(),
                            tty: None,
                        };
                        dispatch_flow_step(&step.kind, &mut cx, generation, idx)
                    }
                    _ => {
                        let flow = execute_single_step_with_generation(
                            state,
                            process,
                            &step.kind,
                            generation,
                            idx,
                            stdin.clone(),
                            expose_stdin,
                            out.clone(),
                            err.clone(),
                            out_pipe.clone(),
                            stdin_pipe.clone(),
                            None,
                        )?;
                        if matches!(
                            &step.kind,
                            StepKind::Env { .. }
                                | StepKind::InheritEnv { .. }
                                | StepKind::IsolatedInheritEnv { .. }
                        ) {
                            sync_iteration_assert_needles(state, steps, generation)?;
                        }
                        Ok(flow)
                    }
                };
                flow_result
            }
        })()
        .map_err(|err| step_error(step, idx, err));

        let restore_result = restore_scopes(state, step.scope_exit);
        // Keeper expiry: drop spawn-time pins whose final producer step
        // just completed, so later consumer steps in the same task observe
        // EOF. Gated on slice identity, so nested bodies executing through
        // this same loop never discharge the worker's top-level map.
        let expiry_drained = if let Some(expiry) = state.keeper_expiry.as_mut() {
            expiry.expire_step(steps, idx)
        } else {
            false
        };
        if expiry_drained {
            state.keeper_expiry = None;
        }
        let flow = flow_result?;
        restore_result?;
        match flow {
            Flow::Done => {}
            Flow::Break { .. } | Flow::Continue { .. } | Flow::Return { .. } => {
                return Ok(flow);
            }
        }
    }

    // Poll anonymous background handles at end-of-pipeline. The shared
    // named_tasks entries are reaped only by the root context: task threads
    // must never block on sibling tasks, which may depend on this thread
    // via AWAIT (three-way deadlock). Un-awaited named tasks are still
    // reaped by the root end-poll, and explicitly awaited tasks join via
    // AWAIT. Entries are retained as Cancelled/Completed tombstones so
    // later AWAIT/CANCEL report precise errors.
    let reap_named = !state.inside_async;
    // Scope floor for `TIMEOUT`: only tasks at or above the floor join
    // at scope end. Older tasks keep running past the scope instead of
    // holding it open; `None` (pipeline top, every other scope) joins
    // everything, exactly as before.
    let floor = state.reap_floor;
    let in_scope = |id: u64| floor.map(|f| id >= f).unwrap_or(true);
    let has_bg = state.bg_children.iter().any(|c| in_scope(c.task_id));
    let named_pending = |state: &ExecState<P>| {
        reap_named
            && state
                .named_tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|(id, entry)| {
                    in_scope(*id) && !entry.state.lock().unwrap_or_else(|e| e.into_inner()).reaped
                })
    };
    let has_named = named_pending(state);
    if wait_at_end && (has_bg || has_named) {
        loop {
            let mut failed_status: Option<anyhow::Error> = None;

            // Cancellation (deadline watcher or parent teardown) must break
            // the poll loop: without this, a stuck background handle would
            // hang the reaper forever. Flows into the shared fail-fast
            // teardown below.
            if failed_status.is_none() && state.cancel_token.load(Ordering::SeqCst) {
                failed_status = Some(step_diag::eval_async_cancelled().into());
            }

            // 1. Poll anonymous background handles. Handles below the
            // scope floor belong to outer work: never poll them here, or
            // the scope would wait out tasks it did not spawn.
            let mut i = 0;
            while i < state.bg_children.len() {
                if !in_scope(state.bg_children[i].task_id) {
                    i += 1;
                    continue;
                }
                match state.bg_children[i].handle.try_wait() {
                    Ok(Some(status)) => {
                        if !status.success() && failed_status.is_none() {
                            failed_status =
                                Some(step_diag::eval_async_exit_status(None, &status).into());
                            break;
                        }
                        state.bg_children.swap_remove(i);
                    }
                    Ok(None) => {
                        i += 1;
                    }
                    Err(e) => {
                        if failed_status.is_none() {
                            failed_status = Some(e.into());
                        }
                        break;
                    }
                }
            }

            // 2. Poll un-awaited named tasks (root context only). Each entry
            // is probed under a short entry lock; terminal entries are
            // retained as tombstones, never removed.
            if failed_status.is_none() && reap_named {
                let entries: Vec<(u64, Arc<TaskEntry>)> = {
                    let named = state.named_tasks.lock().unwrap_or_else(|e| e.into_inner());
                    named
                        .iter()
                        .map(|(id, entry)| (*id, Arc::clone(entry)))
                        .collect()
                };
                for (id, entry) in &entries {
                    if !in_scope(*id) {
                        continue;
                    }
                    enum Poll {
                        Pending,
                        CompletedOk,
                        CompletedErr(anyhow::Error),
                    }
                    let poll = {
                        let mut guard = entry.state.lock().unwrap_or_else(|e| e.into_inner());
                        match guard.phase {
                            TaskPhase::Running | TaskPhase::Awaiting => {
                                match guard.handle.as_mut() {
                                    Some(handle) => match handle.try_wait() {
                                        Ok(Some(status)) => {
                                            let _ = guard.handle.take();
                                            guard.phase = TaskPhase::Completed;
                                            if status.success() {
                                                Poll::CompletedOk
                                            } else {
                                                Poll::CompletedErr(
                                                    step_diag::eval_async_exit_status(
                                                        Some(*id),
                                                        &status,
                                                    )
                                                    .into(),
                                                )
                                            }
                                        }
                                        Ok(None) => Poll::Pending,
                                        Err(e) => {
                                            let _ = guard.handle.take();
                                            guard.phase = TaskPhase::Completed;
                                            Poll::CompletedErr(e.into())
                                        }
                                    },
                                    // Handle taken by a concurrent CANCEL/AWAIT
                                    // teardown; the barrier below rendezvouses.
                                    None => Poll::Pending,
                                }
                            }
                            TaskPhase::Cancelled | TaskPhase::Completed => Poll::Pending,
                        }
                    };
                    match poll {
                        Poll::Pending => {}
                        Poll::CompletedOk => {
                            entry.finish_teardown();
                        }
                        Poll::CompletedErr(e) => {
                            entry.finish_teardown();
                            if failed_status.is_none() {
                                failed_status = Some(e);
                            }
                            break;
                        }
                    }
                }
            }

            // 3. Fail-fast teardown (named entries are root-owned; task
            // threads only tear down their own anonymous children).
            // Handles are taken under short locks and killed outside every
            // lock; tombstones are retained.
            if let Some(err) = failed_status {
                for survivor in state.bg_children.iter_mut() {
                    if in_scope(survivor.task_id) {
                        let _ = survivor.handle.kill();
                    }
                }
                if reap_named {
                    let entries: Vec<(u64, Arc<TaskEntry>)> = {
                        let named = state.named_tasks.lock().unwrap_or_else(|e| e.into_inner());
                        named
                            .iter()
                            .map(|(id, entry)| (*id, Arc::clone(entry)))
                            .collect()
                    };
                    let mut to_kill: Vec<(Arc<TaskEntry>, Box<dyn BackgroundHandle>)> = Vec::new();
                    for (id, entry) in &entries {
                        if !in_scope(*id) {
                            continue;
                        }
                        let mut guard = entry.state.lock().unwrap_or_else(|e| e.into_inner());
                        match guard.phase {
                            TaskPhase::Running | TaskPhase::Awaiting => {
                                guard.phase = TaskPhase::Cancelled;
                                if let Some(handle) = guard.handle.take() {
                                    to_kill.push((Arc::clone(entry), handle));
                                }
                            }
                            TaskPhase::Cancelled | TaskPhase::Completed => {}
                        }
                    }
                    for (entry, mut handle) in to_kill {
                        let _ = handle.kill();
                        entry.finish_teardown();
                    }
                }
                // Drop only the scope's own handles: outer work keeps
                // running past a scope teardown, and the pipeline-top
                // teardown still owns it.
                state.bg_children.retain(|c| !in_scope(c.task_id));
                return Err(err);
            }

            let bg_empty = !state.bg_children.iter().any(|c| in_scope(c.task_id));
            if bg_empty && !named_pending(state) {
                return Ok(Flow::Done);
            }
            // Rendezvous: a concurrent CANCEL/AWAIT on another thread may
            // own teardown of a Cancelled-but-unreaped entry. Wait for it
            // instead of spinning, so this thread never outruns the join.
            if reap_named {
                let unreaped: Vec<Arc<TaskEntry>> = {
                    let named = state.named_tasks.lock().unwrap_or_else(|e| e.into_inner());
                    named
                        .iter()
                        .filter(|(id, entry)| {
                            if !in_scope(**id) {
                                return false;
                            }
                            let guard = entry.state.lock().unwrap_or_else(|e| e.into_inner());
                            matches!(guard.phase, TaskPhase::Cancelled) && !guard.reaped
                        })
                        .map(|(_, entry)| Arc::clone(entry))
                        .collect()
                };
                for entry in &unreaped {
                    entry.wait_reaped();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    Ok(Flow::Done)
}

fn restore_scopes<P: ProcessManager>(
    state: &mut ExecState<P>,
    count: usize,
) -> DiagnosticResult<()> {
    for _ in 0..count {
        state.pop_scope()?;
    }
    Ok(())
}

/// Execute a `REMOTE` body locally inside a fresh lexical scope: the
/// guest-mode path, where the shipped text keeps its `REMOTE` wrapper and
/// the session already fulfilled the transfer declarations. Scope,
/// pipes, and filesystem effects behave exactly like any local block;
/// control-flow signals crossing the block edge become step-numbered
/// errors, mirroring the pipeline-top mapping.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_scoped_remote_body<P: ProcessManager>(
    state: &mut ExecState<P>,
    process: &mut P,
    steps: &[Step],
    stdin: CommandStdin,
    expose_stdin: bool,
    out: Option<StreamHandle>,
    err: Option<StreamHandle>,
    out_pipe: Option<Arc<PipeInner>>,
    stdin_pipe: Option<Arc<PipeInner>>,
) -> Result<()> {
    let flow = execute_scoped_steps(
        state,
        process,
        steps,
        stdin,
        expose_stdin,
        out,
        err,
        out_pipe,
        stdin_pipe,
        true,
    )?;
    match flow {
        Flow::Done => Ok(()),
        Flow::Break { idx } => {
            Err(step_diag::eval_flow_crosses_remote(idx + 1, KEYWORD_BREAK).into())
        }
        Flow::Continue { idx } => {
            Err(step_diag::eval_flow_crosses_remote(idx + 1, KEYWORD_CONTINUE).into())
        }
        Flow::Return { idx, .. } => {
            Err(step_diag::eval_flow_crosses_remote(idx + 1, KEYWORD_RETURN).into())
        }
    }
}

/// Execute steps inside a fresh lexical scope (IF branches, TIMEOUT bodies).
/// Blocks scope everything (LET/ENV/WORKDIR/WORKSPACE); only pipes and
/// filesystem effects cross. Restores even when the body fails. Propagates
/// Flow signals (BREAK/CONTINUE/RETURN) to the caller after restoring.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_scoped_steps<P: ProcessManager>(
    state: &mut ExecState<P>,
    process: &mut P,
    steps: &[Step],
    stdin: CommandStdin,
    expose_stdin: bool,
    out: Option<StreamHandle>,
    err: Option<StreamHandle>,
    out_pipe: Option<Arc<PipeInner>>,
    stdin_pipe: Option<Arc<PipeInner>>,
    wait_at_end: bool,
) -> Result<Flow> {
    state.push_scope();
    let res = execute_steps(
        state,
        process,
        steps,
        stdin,
        expose_stdin,
        out,
        err,
        out_pipe,
        stdin_pipe,
        wait_at_end,
    );
    // Restore the scope even when the body failed, but never let an
    // unwinding failure mask the body's own error.
    let pop_res = state.pop_scope().map_err(anyhow::Error::from);
    match (res, pop_res) {
        (Ok(flow), Ok(())) => Ok(flow),
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e),
    }
}

/// Dispatch one compound step (loops, functions, scoped wrappers) through
/// the Flow path. Called with the caller's generation/idx so assertion
/// windows and error attribution match the leaf pipeline.
fn dispatch_flow_step<P: ProcessManager>(
    cmd: &StepKind,
    cx: &mut StepCtx<'_, P>,
    generation: usize,
    idx: usize,
) -> Result<Flow> {
    match cmd {
        StepKind::FuncDef {
            name,
            params,
            return_type,
            is_async,
            body,
            ..
        } => {
            handlers::define_func(cx, name, params, return_type, *is_async, body)?;
            Ok(Flow::Done)
        }
        StepKind::Call { name, args } => {
            let _ = handlers::call_func_value(cx, name, args)?;
            Ok(Flow::Done)
        }
        StepKind::Return { expr, .. } => handlers::handle_return(cx, idx, expr),
        StepKind::ImportFile { path, alias } => {
            let _ = super::import::import_file(cx, path, alias)?;
            Ok(Flow::Done)
        }
        StepKind::ImportFuncs {
            path,
            alias,
            func_names,
            type_names,
        } => {
            super::import::import_funcs(cx, path, alias.as_deref(), func_names, type_names)?;
            Ok(Flow::Done)
        }
        StepKind::While { cond, body } => Ok(handlers::while_loop(cx, cond, body)?),
        StepKind::Break => Ok(Flow::Break { idx }),
        StepKind::Continue => Ok(Flow::Continue { idx }),
        StepKind::For {
            key_var,
            key_type,
            var,
            var_type,
            in_expr,
            body,
        } => Ok(handlers::for_loop(
            cx,
            key_var.as_deref(),
            key_type.clone(),
            var,
            var_type.clone(),
            in_expr,
            body,
        )?),
        StepKind::If {
            cond,
            then_body,
            else_ifs,
            else_body,
        } => handlers::if_then(cx, cond, then_body, else_ifs, else_body),
        StepKind::Switch {
            discriminant,
            cases,
            default_body,
        } => handlers::switch_run(cx, discriminant, cases, default_body),
        StepKind::Timeout { duration, body } => {
            let duration = super::args::resolve_arg_as_duration(duration, cx)?;
            handlers::timeout(cx, &duration, body)
        }
        StepKind::Fail {
            code,
            message,
            body,
        } => handlers::fail(cx, code, message, body),
        StepKind::Isolated { body } => handlers::isolated(cx, body),
        StepKind::WithIo { bindings, cmd } => handlers::with_io(cx, generation, idx, bindings, cmd),
        StepKind::Bind {
            var,
            decl_type,
            is_const,
            rhs: oxdock_parser::BindRhs::Capture(cmd),
        } => Ok(handlers::assign_capture(
            cx,
            generation,
            idx,
            var,
            decl_type.clone(),
            cmd,
            *is_const,
        )?),
        _ => {
            unreachable!("dispatch_flow_step handles only compound steps")
        }
    }
}

#[cfg(test)]
mod thread_handle_tests {
    use super::super::exit::{exit_code_of, exit_code_of_diag};
    use super::*;

    fn bare_handle(join: Option<std::thread::JoinHandle<Result<()>>>) -> ThreadJoinHandle {
        ThreadJoinHandle {
            join,
            cancel_token: Arc::new(AtomicBool::new(false)),
            cancel_hub: Arc::new(oxdock_process::CancelHub::new()),
            active_process: Arc::new(Mutex::new(None)),
            worker: Arc::new(Mutex::new(None)),
            thread_error: None,
            thread_context: Vec::new(),
            thread_exit_code: None,
        }
    }

    #[test]
    fn panic_payload_reports_thread_panicked_key() {
        let join = std::thread::spawn(|| -> Result<()> {
            panic!("boom-payload");
        });
        let mut handle = bare_handle(Some(join));
        let err = anyhow::Error::new(handle.wait().expect_err("panic must surface"));
        let diag = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<Diagnostic>())
            .expect("keyed panic diagnostic survives");
        assert_eq!(diag.key(), "dsl.eval.async_thread_panicked");
        assert!(diag.message().contains("boom-payload"));
    }

    #[test]
    fn wrapped_diagnostic_keeps_key_and_outer_context() {
        let leaf = step_diag::eval_async_cancelled();
        let join = std::thread::spawn(|| -> Result<()> {
            Err(anyhow::Error::new(leaf).context("step 2").context("step 1"))
        });
        let mut handle = bare_handle(Some(join));
        let err = anyhow::Error::new(handle.wait().expect_err("wrapped failure must surface"));
        let diag = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<Diagnostic>())
            .expect("key survives context wrappers");
        assert_eq!(diag.key(), "dsl.eval.async_cancelled");
        assert_eq!(
            diag.contexts(),
            &["step 1".to_string(), "step 2".to_string()],
            "each outer frame replays natively in order: {err:#}"
        );
        assert_eq!(err.to_string(), "step 1: step 2: ASYNC task cancelled");
    }

    #[test]
    fn sourceless_diagnostic_keeps_key_and_exit_code() {
        let mut handle = bare_handle(None);
        handle.thread_error = Some(step_diag::eval_async_cancelled());
        handle.thread_exit_code = Some(7);
        let diag = handle.reemit_error();
        let err = anyhow::Error::new(diag.clone());
        assert_eq!(exit_code_of(&err), Some(7));
        assert_eq!(exit_code_of_diag(&diag), Some(7));
        assert_eq!(diag.key(), "dsl.eval.async_cancelled");
    }

    #[test]
    fn sourced_diagnostic_keeps_key_source_and_exit_code() {
        let sourced = step_diag::eval_async_cancelled().with_source(Arc::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "gone",
        )));
        assert!(
            std::error::Error::source(&sourced).is_some(),
            "fixture must carry a source to cover the chained branch"
        );
        let mut handle = bare_handle(None);
        handle.thread_error = Some(sourced);
        handle.thread_exit_code = Some(7);
        let diag = handle.reemit_error();
        let err = anyhow::Error::new(diag.clone());
        assert_eq!(exit_code_of(&err), Some(7));
        assert_eq!(exit_code_of_diag(&diag), Some(7));
        assert_eq!(diag.key(), "dsl.eval.async_cancelled");
        let io = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<std::io::Error>())
            .expect("original source survives behind the exit code");
        assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(
            format!("{err:#}"),
            "ASYNC task cancelled: EXIT requested with code 7: gone"
        );
    }

    #[test]
    fn unkeyed_thread_error_reports_dispatch_failed_without_duplicating_chain() {
        let join = std::thread::spawn(|| -> Result<()> {
            Err(anyhow::Error::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "denied",
            ))
            .context("step 3"))
        });
        let mut handle = bare_handle(Some(join));
        let diag = handle.wait().expect_err("unkeyed failure must surface");
        let err = anyhow::Error::new(diag.clone());
        assert_eq!(diag.key(), "dsl.eval.dispatch_failed");
        assert_eq!(format!("{err:#}"), "step 3: denied");
    }

    #[test]
    fn bare_exit_request_relays_as_keyed_diagnostic() {
        let join = std::thread::spawn(|| -> Result<()> {
            Err(anyhow::Error::new(super::super::exit::ExitRequest::new(7)).context("step 2"))
        });
        let mut handle = bare_handle(Some(join));
        let diag = handle.wait().expect_err("exit request must surface");
        let err = anyhow::Error::new(diag.clone());
        assert_eq!(exit_code_of(&err), Some(7));
        assert_eq!(exit_code_of_diag(&diag), Some(7));
        assert_eq!(diag.key(), "dsl.eval.exit_requested");
        assert_eq!(diag.fields().get("code"), Some(&"7".to_string()));
        assert_eq!(format!("{err:#}"), "step 2: EXIT requested with code 7");
    }
}

#[cfg(test)]
mod step_error_tests {
    use super::*;
    use oxdock_parser::StepKind;

    fn step_at(line: usize, col: usize) -> Step {
        Step {
            guard: None,
            kind: StepKind::Break,
            scope_enter: 0,
            scope_exit: 0,
            line,
            col,
        }
    }

    #[test]
    fn coordinates_attach_span_and_keep_text_prefix() {
        let leaf = step_diag::eval_async_cancelled();
        assert!(leaf.span().is_none());
        let err = step_error(&step_at(5, 3), 0, anyhow::Error::new(leaf));
        let diag = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<Diagnostic>())
            .expect("diagnostic survives the position wrap");
        let span = diag.span().expect("coordinates attach natively");
        assert_eq!(span.line, 5);
        assert_eq!(span.col_start, Some(3));
        assert!(
            format!("{err:#}").starts_with("step 1, line 5, col 3: "),
            "text prefix stays byte-identical: {err:#}"
        );
    }

    #[test]
    fn innermost_span_wins_on_nested_dispatch() {
        let leaf = step_diag::eval_async_cancelled();
        let inner = step_error(&step_at(5, 3), 1, anyhow::Error::new(leaf));
        let outer = step_error(&step_at(9, 1), 0, inner);
        let diag = outer
            .chain()
            .find_map(|cause| cause.downcast_ref::<Diagnostic>())
            .expect("diagnostic survives nested wraps");
        let span = diag.span().expect("span survives");
        assert_eq!((span.line, span.col_start), (5, Some(3)));
        assert!(
            format!("{outer:#}").starts_with("step 1, line 9, col 1: step 2, line 5, col 3: "),
            "both text frames chain like a call path: {outer:#}"
        );
    }

    #[test]
    fn unknown_position_keeps_bare_prefix_without_span() {
        let leaf = step_diag::eval_async_cancelled();
        let err = step_error(&step_at(0, 0), 0, anyhow::Error::new(leaf));
        let diag = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<Diagnostic>())
            .expect("diagnostic survives");
        assert!(diag.span().is_none());
        assert_eq!(format!("{err:#}"), "step 1: ASYNC task cancelled");
    }

    #[test]
    fn prespanned_outermost_diagnostic_keeps_its_span() {
        // Fast path with the guard genuinely exercised: the head IS a
        // `Diagnostic`, so `downcast_mut` succeeds and the `is_none`
        // check decides. Deleting the guard would clobber (2, 4).
        let leaf = step_diag::eval_async_cancelled().with_span(Span::line_col(2, 4));
        let err = step_error(&step_at(9, 1), 0, anyhow::Error::new(leaf));
        let diag = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<Diagnostic>())
            .expect("diagnostic survives");
        let span = diag.span().expect("span survives");
        assert_eq!((span.line, span.col_start), (2, Some(4)));
    }

    #[test]
    fn synthetic_inner_yields_to_parsed_outer_coordinates() {
        // Slow path: the inner step carries no coordinates, so its
        // wrap buries the span-less diagnostic under a context layer
        // and `downcast_mut` misses at the outer level. The outer
        // parsed step must still attach natively via reclaim.
        let leaf = step_diag::eval_async_cancelled();
        let inner = step_error(&step_at(0, 0), 1, anyhow::Error::new(leaf));
        let outer = step_error(&step_at(9, 1), 0, inner);
        let diag = outer
            .chain()
            .find_map(|cause| cause.downcast_ref::<Diagnostic>())
            .expect("diagnostic survives nested wraps");
        let span = diag.span().expect("outer coordinates attach");
        assert_eq!((span.line, span.col_start), (9, Some(1)));
        assert!(
            format!("{outer:#}").starts_with("step 1, line 9, col 1: step 2: "),
            "inner text frame replays under the outer prefix: {outer:#}"
        );
    }

    #[test]
    fn line_without_column_attaches_line_only_span() {
        let leaf = step_diag::eval_async_cancelled();
        let err = step_error(&step_at(7, 0), 0, anyhow::Error::new(leaf));
        let diag = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<Diagnostic>())
            .expect("diagnostic survives");
        let span = diag.span().expect("line-only span attaches");
        assert_eq!(span.line, 7);
        assert_eq!(span.col_start, None);
        assert!(
            format!("{err:#}").starts_with("step 1, line 7: "),
            "prefix names the line with no column: {err:#}"
        );
    }
}
