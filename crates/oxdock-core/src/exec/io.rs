use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use anyhow::Result;
#[cfg(not(miri))]
use anyhow::bail;
#[cfg(not(miri))]
use oxdock_pipe::OsPipeWriter;
use oxdock_pipe::{
    KeeperGuard, Materialized, PipeHandle, PipeInfo, PipeInner, SharedInput, SharedOutput,
    inspect as inspect_handle, materialize, peek as peek_handle, script_backend,
};
use oxdock_process::{CommandStderr, CommandStdin, CommandStdout};

/// Handle-scoped pipe resolution. No central index exists: every method
/// below takes the `PIPE` value's backend cell, materializing it on first
/// binding (first-binding-wins under the cell lock) and adapting later
/// bindings through the existing machinery. The `ExecIo` receiver carries
/// no pipe state; methods live here (rather than as free functions) so
/// call sites keep their `cx.state.io.*` shape.
#[derive(Clone, Default)]
pub(super) struct PipeRegistry;

/// Loud take-twice error for OS handles: a second consumer or producer on
/// one end can never steal the descriptor, and (unlike the old recycling
/// behavior) never silently receives a fresh pair either : the remedy is a
/// fresh declaration. No rebind rule exists by design: no rule can tell
/// sequential loop reuse apart from concurrent fan-in sharing.
#[cfg(not(miri))]
fn spent_handle(idx: usize) -> anyhow::Error {
    anyhow::anyhow!(
        "step {}: OS pipe handle has already been consumed by another binding; declare a fresh LET $x: PIPE for a new session",
        idx + 1,
    )
}

impl PipeRegistry {
    /// First-binding-wins materialization for one binding site: an unbound
    /// handle decides its kind from `promote` (the caller supplies full
    /// usage context); a decided handle is returned unchanged. Callers
    /// adapt mismatches through resolution below, never here.
    pub(super) fn ensure_handle(handle: &PipeHandle, promote: bool) -> Result<()> {
        materialize(handle, promote)?;
        Ok(())
    }

    /// Non-destructive snapshot of a script handle's buffered bytes for
    /// pipe-content assertions. Delegates to the backend without creating:
    /// unbound handles and OS pairs bail loudly instead of yielding empty
    /// content.
    ///
    /// Public so out-of-crate harnesses can assert on script-owned pipes
    /// after a run completes, via the `PIPE` values in the returned
    /// bindings. Only meaningful once writers detached (post-run).
    pub fn peek_pipe_content(handle: &PipeHandle) -> Result<Vec<u8>> {
        peek_handle(handle)
    }

    /// Snapshot one handle for `INSPECT()` diagnostics.
    pub(super) fn inspect_pipe(handle: &PipeHandle) -> PipeInfo {
        inspect_handle(handle)
    }

    /// Pin a keeper slot on a script backend so transient writer churn can
    /// never observe zero writers. Returns `None` for OS-materialized and
    /// unbound handles, which need no pin. Callers ensure the handle
    /// first so OS promotion is honored and this never forces a script
    /// backend into existence.
    pub(super) fn pin_keeper(handle: &PipeHandle) -> Result<Option<KeeperGuard>> {
        Ok(script_backend(handle).map(KeeperGuard::new))
    }

    /// Resolve a stdin binding against a decided handle. Script backends
    /// hand out a shared reader (plus the backend for timeout-bounded
    /// bridge reads); OS pairs hand the take-once reader to `RUN` directly
    /// and bridge it to a shared handle for DSL commands. A second take
    /// on one end bails loudly with the fresh-declaration remedy instead
    /// of stealing the descriptor.
    pub(super) fn resolve_stdin(
        idx: usize,
        handle: &PipeHandle,
        direct: bool,
        promote: bool,
    ) -> Result<(CommandStdin, Option<Arc<PipeInner>>)> {
        // `idx` and `direct` serve only the OS-pipe arms below, which are
        // compiled out under Miri.
        let _ = idx;
        let _ = direct;
        match materialize(handle, promote)? {
            Materialized::Script(backend) => {
                Ok((CommandStdin::Stream(backend.reader_handle()), Some(backend)))
            }
            #[cfg(not(miri))]
            Materialized::Os(entry) => {
                if direct {
                    return Ok((CommandStdin::OsPipe(entry.reader.clone()), None));
                }
                let owned = entry.reader.take().map_err(|_| spent_handle(idx))?;
                Ok((
                    CommandStdin::Stream(Arc::new(std::sync::Mutex::new(owned))),
                    None,
                ))
            }
        }
    }

    /// Resolve a stdout binding. Mirrors
    /// [`PipeRegistry::resolve_stdin`] with `StreamHandle` outputs so
    /// `RUN` keeps zero copy `Stdio` handoff, plus the script backend for
    /// the bridge's socket-EOF force-close.
    pub(super) fn resolve_stdout(
        idx: usize,
        handle: &PipeHandle,
        direct: bool,
        promote: bool,
    ) -> Result<(StreamHandle, Option<Arc<PipeInner>>)> {
        // `idx` and `direct` serve only the OS-pipe arms below, which are
        // compiled out under Miri.
        let _ = idx;
        let _ = direct;
        match materialize(handle, promote)? {
            Materialized::Script(backend) => {
                Ok((StreamHandle::Stream(backend.writer_handle()), Some(backend)))
            }
            #[cfg(not(miri))]
            Materialized::Os(entry) => {
                if direct {
                    return Ok((StreamHandle::Os(entry.writer.clone()), None));
                }
                let owned = entry.writer.take().map_err(|_| spent_handle(idx))?;
                Ok((
                    StreamHandle::Stream(Arc::new(std::sync::Mutex::new(owned))),
                    None,
                ))
            }
        }
    }

    /// Resolve a stderr binding. Mirrors
    /// [`PipeRegistry::resolve_stdout`]: `RUN` keeps zero copy handoff,
    /// DSL commands get a bridged shared handle. Binding `stdout` and
    /// `stderr` to one live OS handle takes the same slot twice, so the
    /// second take bails deterministically; merge in shell via `2>&1`
    /// instead.
    pub(super) fn resolve_stderr(
        idx: usize,
        handle: &PipeHandle,
        direct: bool,
        promote: bool,
    ) -> Result<StreamHandle> {
        // `idx` and `direct` serve only the OS-pipe arms below, which are
        // compiled out under Miri.
        let _ = idx;
        let _ = direct;
        match materialize(handle, promote)? {
            Materialized::Script(backend) => Ok(StreamHandle::Stream(backend.writer_handle())),
            #[cfg(not(miri))]
            Materialized::Os(entry) => {
                if direct {
                    return Ok(StreamHandle::Os(entry.writer.clone()));
                }
                let owned = entry.writer.take().map_err(|_| spent_handle(idx))?;
                Ok(StreamHandle::Stream(Arc::new(std::sync::Mutex::new(owned))))
            }
        }
    }
}

#[derive(Clone, Default)]
pub struct ExecIo {
    stdin: Option<SharedInput>,
    stdout: Option<SharedOutput>,
    stderr: Option<SharedOutput>,
    inherit_env_overrides: HashMap<String, String>,
    inherit_env_removed: HashSet<String>,
    remote_runners: HashMap<String, RemoteRunnerArc>,
    remote_guest: bool,
}

/// Shared handle to the transport behind `REMOTE` blocks. Staged here (not
/// threaded through every `run_steps_*` signature) so existing callers keep
/// compiling: `None` means remote execution is unavailable and every
/// `REMOTE` step bails naming the NET plugin.
pub type RemoteRunnerArc = Arc<dyn super::remote::RemoteRunner>;

/// Standard chunk size for all I/O handlers.
pub const CHUNK_SIZE: usize = 8192;

/// Minimum ring buffer capacity. Actual capacity scales with needle length.
const MIN_RING_CAPACITY: usize = 1024;

/// Sliding window for streaming pattern matching in stream assertions.
/// Maintains a ring buffer and detects matches inline as chunks pass through.
pub(crate) struct SlidingWindow {
    pub(crate) needle: Vec<u8>,
    ring: VecDeque<u8>,
    pub matched: bool,
}

impl SlidingWindow {
    pub fn new(needle: Vec<u8>) -> Self {
        Self {
            ring: VecDeque::with_capacity(needle.len().max(MIN_RING_CAPACITY)),
            needle,
            matched: false,
        }
    }

    pub fn push_chunk(&mut self, chunk: &[u8]) {
        if self.matched {
            return;
        }
        // Eviction limit scales with needle length, never below MIN_RING_CAPACITY
        let limit = self.needle.len().max(MIN_RING_CAPACITY);
        for &byte in chunk {
            self.ring.push_back(byte);
            if self.ring.len() > limit {
                self.ring.pop_front();
            }
            self.check_match();
        }
    }

    /// Replace needle without discarding ring history.
    /// Re-evaluates current ring against updated needle.
    pub fn update_needle(&mut self, new_needle: Vec<u8>) {
        if self.matched {
            return;
        }
        self.needle = new_needle;
        self.check_match();
    }

    fn check_match(&mut self) {
        if self.matched || self.ring.len() < self.needle.len() {
            return;
        }
        let start = self.ring.len() - self.needle.len();
        if self
            .ring
            .iter()
            .skip(start)
            .zip(self.needle.iter())
            .all(|(a, b)| a == b)
        {
            self.matched = true;
        }
    }

    /// Return the ring buffer contents for debugging.
    pub fn ring_buffer(&self) -> Vec<u8> {
        self.ring.iter().copied().collect()
    }
}

#[derive(Clone)]
pub(super) enum StreamHandle {
    Stream(SharedOutput),
    /// Live OS kernel pipe writer handed to one concurrent producer.
    /// Only `RUN` consumes this directly; DSL commands never observe it
    /// because `with_io` bridges OS entries to shared handles for them.
    #[cfg(not(miri))]
    Os(OsPipeWriter),
}

impl StreamHandle {
    pub(super) fn to_stdout(&self) -> CommandStdout {
        match self {
            StreamHandle::Stream(writer) => CommandStdout::Stream(writer.clone()),
            #[cfg(not(miri))]
            StreamHandle::Os(writer) => CommandStdout::OsPipe(writer.clone()),
        }
    }

    pub(super) fn to_stderr(&self) -> CommandStderr {
        match self {
            StreamHandle::Stream(writer) => CommandStderr::Stream(writer.clone()),
            #[cfg(not(miri))]
            StreamHandle::Os(writer) => CommandStderr::OsPipe(writer.clone()),
        }
    }
}

pub(super) fn write_stdout<F>(handle: Option<StreamHandle>, op: F) -> Result<()>
where
    F: FnOnce(&mut dyn Write) -> Result<()>,
{
    match handle {
        Some(StreamHandle::Stream(writer)) => {
            if let Ok(mut guard) = writer.lock() {
                op(&mut *guard)?;
            }
            Ok(())
        }
        // DSL commands never observe a live OS handle: `with_io` bridges
        // OS entries to shared handles for them, and promotion only fires
        // for single RUN bodies. This arm is defensive only.
        #[cfg(not(miri))]
        Some(StreamHandle::Os(_)) => {
            bail!("cannot write DSL output to a live OS pipe")
        }
        // `None` inherits host stdout: only explicit `Stream` bindings
        // reroute DSL output.
        None => {
            let mut stdout = io::stdout();
            op(&mut stdout)
        }
    }
}

/// Slice-based byte adapter over pipe halves for host (`#[oxdock_func]`)
///
/// stateful functions. All byte movement goes through the standard traits
/// on caller-owned buffers : `Read::read(&mut [u8])` and
/// `Write::write(&[u8])` : so hosts can hand pipes directly to `serde_json`,
/// `flate2`, `tar`, and friends with a single reused stack buffer and zero
/// per-chunk allocation. `0` read means EOF exactly like `std::io`; never
/// slurp a stream into one `Vec` (unbounded memory growth : stream it).
/// `flush()` delegates to backend flush semantics, which for script pipes
/// is a no-op that loses nothing: every `write()` wakes readers itself.
///
/// A host read blocks the calling task thread exactly like a DSL reader;
/// EOF and half-close map identically to DSL consumers. Take-once applies
/// like everywhere else: bridging an OS backend takes once, repeats bail.
pub struct PipeStream {
    reader: Option<SharedInput>,
    writer: Option<SharedOutput>,
}

impl PipeStream {
    /// Read-half adapter (e.g. over [`StepCtx::pipe_reader`](super::steps::StepCtx::pipe_reader)).
    pub fn reader(reader: SharedInput) -> Self {
        Self {
            reader: Some(reader),
            writer: None,
        }
    }

    /// Write-half adapter (e.g. over [`StepCtx::pipe_writer`](super::steps::StepCtx::pipe_writer)).
    pub fn writer(writer: SharedOutput) -> Self {
        Self {
            reader: None,
            writer: Some(writer),
        }
    }

    /// Both halves (e.g. a filter with separate in/out pipes).
    pub fn pair(reader: SharedInput, writer: SharedOutput) -> Self {
        Self {
            reader: Some(reader),
            writer: Some(writer),
        }
    }
}

impl std::io::Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let Some(reader) = &self.reader else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "pipe stream has no reader half",
            ));
        };
        let mut guard = reader
            .lock()
            .map_err(|_| std::io::Error::other("pipe reader lock poisoned"))?;
        guard.read(buf)
    }
}

impl std::io::Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let Some(writer) = &self.writer else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "pipe stream has no writer half",
            ));
        };
        let mut guard = writer
            .lock()
            .map_err(|_| std::io::Error::other("pipe writer lock poisoned"))?;
        guard.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let Some(writer) = &self.writer else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "pipe stream has no writer half",
            ));
        };
        let mut guard = writer
            .lock()
            .map_err(|_| std::io::Error::other("pipe writer lock poisoned"))?;
        guard.flush()
    }
}

/// Hard cap for `ASSERT_EQ stdout` exact accumulators, mirroring the
/// `SpillBuffer` spill threshold: exact stream matching is a test-time
/// opt-in, and unbounded trials must use pipe targets or harness captures.
pub(crate) const EXACT_STDOUT_CAP: usize = 8 * 1024 * 1024;

/// Cumulative stdout record for one execution generation backing
/// `ASSERT_EQ stdout`. Unlike `SlidingWindow` nothing is ever evicted;
/// once the cap is exceeded the entry latches `overflowed` and stops
/// growing, and the asserting step reports it with remediation guidance.
pub(crate) struct ExactCapture {
    pub(crate) bytes: Vec<u8>,
    pub(crate) overflowed: bool,
}

impl ExactCapture {
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            overflowed: false,
        }
    }

    pub fn push_chunk(&mut self, chunk: &[u8]) {
        if self.overflowed {
            return;
        }
        if self.bytes.len() + chunk.len() > EXACT_STDOUT_CAP {
            self.overflowed = true;
            return;
        }
        self.bytes.extend_from_slice(chunk);
    }
}

/// Which host stream a tee forwards to when no capture sink is configured.
#[derive(Clone, Copy)]
enum TeeStream {
    Stdout,
    Stderr,
}

/// Wraps the configured stdout sink so every byte written to it is also
/// pushed to all registered SlidingWindow observers for stream assertions.
/// When no sink is configured (`inner` is `None`) bytes are forwarded to
/// real stdout so interactive CLI output still reaches the terminal.
struct TeeWriter {
    inner: Option<SharedOutput>,
    stream: TeeStream,
    windows: Arc<Mutex<HashMap<(usize, usize), SlidingWindow>>>,
    exact: Arc<Mutex<HashMap<usize, ExactCapture>>>,
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Forward to downstream (streaming)
        match &self.inner {
            Some(inner) => {
                let mut guard = inner
                    .lock()
                    .map_err(|_| io::Error::other("stdout sink poisoned"))?;
                guard.write_all(buf)?;
            }
            None => match self.stream {
                TeeStream::Stdout => io::stdout().write_all(buf)?,
                TeeStream::Stderr => io::stderr().write_all(buf)?,
            },
        }
        // Push to ALL registered assertion windows (O(1) per byte per window)
        if let Ok(mut windows) = self.windows.lock() {
            for window in windows.values_mut() {
                window.push_chunk(buf);
            }
        }
        if let Ok(mut exact) = self.exact.lock() {
            for capture in exact.values_mut() {
                capture.push_chunk(buf);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match &self.inner {
            Some(inner) => {
                let mut guard = inner
                    .lock()
                    .map_err(|_| io::Error::other("stdout sink poisoned"))?;
                guard.flush()?;
            }
            None => match self.stream {
                TeeStream::Stdout => io::stdout().flush()?,
                TeeStream::Stderr => io::stderr().flush()?,
            },
        }
        Ok(())
    }
}

/// Installs the tee around `sink` (or real stdout when absent).
pub(crate) fn teed_stdout(
    sink: Option<SharedOutput>,
    windows: Arc<Mutex<HashMap<(usize, usize), SlidingWindow>>>,
    exact: Arc<Mutex<HashMap<usize, ExactCapture>>>,
) -> SharedOutput {
    Arc::new(Mutex::new(TeeWriter {
        inner: sink,
        stream: TeeStream::Stdout,
        windows,
        exact,
    }))
}

/// Installs the tee around `sink` (or real stderr when absent) for
/// `ASSERT_CONTAINS stderr` substring observers. Exact matching is not
/// offered over stderr; use pipe targets or harness captures for that.
pub(crate) fn teed_stderr(
    sink: Option<SharedOutput>,
    windows: Arc<Mutex<HashMap<(usize, usize), SlidingWindow>>>,
) -> SharedOutput {
    Arc::new(Mutex::new(TeeWriter {
        inner: sink,
        stream: TeeStream::Stderr,
        windows,
        exact: Arc::new(Mutex::new(HashMap::new())),
    }))
}

impl ExecIo {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_stdin(&mut self, stdin: Option<SharedInput>) {
        self.stdin = stdin;
    }

    pub fn set_stdout(&mut self, stdout: Option<SharedOutput>) {
        self.stdout = stdout.clone();
        if self.stderr.is_none() {
            self.stderr = stdout;
        }
    }

    pub fn set_stderr(&mut self, stderr: Option<SharedOutput>) {
        self.stderr = stderr;
    }

    /// Stage the transport behind one `REMOTE` target. Chainable; the NET
    /// plugin registers one session per inventory target at CLI startup
    /// and tests stage mocks. Registration is inert (no process spawns
    /// until a block entry runs).
    pub fn set_remote_runner_for_target(
        &mut self,
        target: String,
        runner: RemoteRunnerArc,
    ) -> &mut Self {
        self.remote_runners.insert(target, runner);
        self
    }

    /// The staged remote transport for `target`, if any. `REMOTE`
    /// interception bails when the target has no runner.
    pub fn remote_runner(&self, target: &str) -> Option<RemoteRunnerArc> {
        self.remote_runners.get(target).cloned()
    }

    /// Legacy single-runner staging: registers under the empty target.
    /// Prefer [`set_remote_runner_for_target`](Self::set_remote_runner_for_target).
    pub fn set_remote_runner(&mut self, runner: RemoteRunnerArc) -> &mut Self {
        self.set_remote_runner_for_target(String::new(), runner)
    }

    /// Mark this run as executing inside a remote guest serve loop. In
    /// guest mode, flagged `COPY --from-host` / `--to-host` steps are
    /// transfer declarations already fulfilled by the session, so they
    /// acknowledge as no-ops instead of re-copying. Never set on hosts:
    /// the parser rejects flagged copies outside `REMOTE` bodies, and
    /// hosts never execute `REMOTE` bodies.
    pub fn set_remote_guest(&mut self, guest: bool) -> &mut Self {
        self.remote_guest = guest;
        self
    }

    /// Whether flagged `COPY` transfer declarations acknowledge as no-ops.
    pub fn is_remote_guest(&self) -> bool {
        self.remote_guest
    }

    pub fn insert_inherit_env<S: Into<String>, V: Into<String>>(&mut self, key: S, value: V) {
        let key = key.into();
        self.inherit_env_removed.remove(&key);
        self.inherit_env_overrides.insert(key, value.into());
    }

    pub fn remove_inherit_env<S: Into<String>>(&mut self, key: S) {
        let key = key.into();
        self.inherit_env_overrides.remove(&key);
        self.inherit_env_removed.insert(key);
    }

    pub fn inherit_env_value(&self, key: &str) -> Option<&String> {
        self.inherit_env_overrides.get(key)
    }

    pub fn inherit_env_is_removed(&self, key: &str) -> bool {
        self.inherit_env_removed.contains(key)
    }

    pub fn inherit_env_overrides(&self) -> &std::collections::HashMap<String, String> {
        &self.inherit_env_overrides
    }

    /// Ensure a handle is materialized for one binding site (see
    /// [`PipeRegistry::ensure_handle`]). Main-flow bindings call this with
    /// the per-step trigger; task bodies are pre-decided by the spawn-time
    /// pin walk, making this a no-op there.
    pub(super) fn ensure_handle(&self, handle: &PipeHandle, promote: bool) -> Result<()> {
        PipeRegistry::ensure_handle(handle, promote)
    }

    /// Snapshot of one handle for `INSPECT()` diagnostics.
    pub(super) fn inspect_pipe(&self, handle: &PipeHandle) -> PipeInfo {
        PipeRegistry::inspect_pipe(handle)
    }

    /// Non-destructive snapshot of a script handle's buffered bytes for
    /// pipe-content assertions.
    ///
    /// Public so out-of-crate harnesses can assert on script-owned pipes
    /// after a run completes, via the `PIPE` values in the returned
    /// bindings. Only meaningful once writers detached (post-run);
    /// unbound handles and OS pairs bail loudly.
    pub fn peek_pipe_content(&self, handle: &PipeHandle) -> Result<Vec<u8>> {
        PipeRegistry::peek_pipe_content(handle)
    }

    /// Pin a keeper slot on a script backend. `None` for OS-materialized
    /// and unbound handles. Callers ensure the handle first so OS
    /// promotion is honored and this never forces a backend into
    /// existence.
    pub(super) fn pin_keeper(&self, handle: &PipeHandle) -> Result<Option<KeeperGuard>> {
        PipeRegistry::pin_keeper(handle)
    }

    /// Resolve a stdin binding to a runnable handle plus the script
    /// backend (for timeout-bounded bridge reads; `None` for OS pairs).
    pub(super) fn resolve_stdin(
        &self,
        idx: usize,
        handle: &PipeHandle,
        direct: bool,
        promote: bool,
    ) -> Result<(CommandStdin, Option<Arc<PipeInner>>)> {
        PipeRegistry::resolve_stdin(idx, handle, direct, promote)
    }

    /// Resolve a stdout binding to a runnable handle plus the script
    /// backend (for the bridge's socket-EOF force-close).
    pub(super) fn resolve_stdout(
        &self,
        idx: usize,
        handle: &PipeHandle,
        direct: bool,
        promote: bool,
    ) -> Result<(StreamHandle, Option<Arc<PipeInner>>)> {
        PipeRegistry::resolve_stdout(idx, handle, direct, promote)
    }

    /// Resolve a stderr binding to a runnable handle.
    pub(super) fn resolve_stderr(
        &self,
        idx: usize,
        handle: &PipeHandle,
        direct: bool,
        promote: bool,
    ) -> Result<StreamHandle> {
        PipeRegistry::resolve_stderr(idx, handle, direct, promote)
    }

    pub fn stdin(&self) -> Option<SharedInput> {
        self.stdin.clone()
    }

    pub fn stdout(&self) -> Option<SharedOutput> {
        self.stdout.clone()
    }

    pub fn stderr(&self) -> Option<SharedOutput> {
        self.stderr.clone().or_else(|| self.stdout.clone())
    }
}

pub(super) fn assemble_default_io(
    stdin: Option<SharedInput>,
    stdout: Option<SharedOutput>,
) -> ExecIo {
    let mut io = ExecIo::new();
    io.set_stdin(stdin);
    io.set_stdout(stdout.clone());
    io.set_stderr(stdout);
    io
}
