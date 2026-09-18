use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use anyhow::Result;
#[cfg(not(miri))]
use anyhow::bail;
use oxdock_process::{CommandStderr, CommandStdin, CommandStdout, SharedInput, SharedOutput};
#[cfg(not(miri))]
use oxdock_process::{OsPipeReader, OsPipeWriter, create_os_pipe};

use super::pipe::{KeeperGuard, PipeEndpoint, PipeInner, PipeOutputs, ScriptPipe};

/// Shared pipe registry. All threads in the same execution context
/// reference the same registry, so pipes created by the parent are
/// visible to child threads.
///
/// All maps live behind a single mutex so check-then-act sequences
/// (exists? create; create then pin) are atomic: one lock acquisition
/// covers the whole decision, and concurrent workers can never allocate
/// duplicate entries under the same name.
#[derive(Default)]
struct RegistryInner {
    input: HashMap<String, SharedInput>,
    output: HashMap<String, PipeOutputs>,
    /// Live script-pipe backends keyed by pipe name. Host-injected raw
    /// handles have no backend here; keeper pins are no-ops for them.
    inners: HashMap<String, Arc<PipeInner>>,
    #[cfg(not(miri))]
    os: HashMap<String, OsPipeEntry>,
}

#[derive(Clone, Default)]
pub(super) struct PipeRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

/// One anonymous OS kernel pipe pair behind take once slots. The first
/// producer and the first consumer each take their half; any further
/// binding to the same name bails deterministically instead of
/// interleaving bytes or stealing the descriptor.
#[cfg(not(miri))]
#[derive(Clone)]
pub(super) struct OsPipeEntry {
    writer: OsPipeWriter,
    reader: OsPipeReader,
}

#[cfg(not(miri))]
impl OsPipeEntry {
    fn new() -> Result<Self> {
        let (reader, writer) = create_os_pipe()?;
        Ok(Self { writer, reader })
    }

    /// Both halves taken. The takers hold raw descriptors outside the
    /// registry, so a spent entry can never serve another resolve; it is
    /// either replaced on next ensure or left for the loud take error.
    fn is_spent(&self) -> bool {
        self.reader.is_consumed() && self.writer.is_consumed()
    }
}

impl PipeRegistry {
    fn lock_inner(&self) -> std::sync::MutexGuard<'_, RegistryInner> {
        self.inner.lock().expect("pipe lock poisoned")
    }

    fn ensure_pipe(&self, name: &str) {
        let mut guard = self.lock_inner();
        if guard.input.contains_key(name) || guard.output.contains_key(name) {
            return;
        }
        let pipe = ScriptPipe::new();
        guard.inners.insert(name.to_string(), pipe.pipe_inner());
        guard.input.insert(name.to_string(), pipe.reader());
        let endpoint = PipeEndpoint::script(pipe.endpoint());
        let outputs = PipeOutputs {
            stdout: Some(endpoint.clone()),
            stderr: Some(endpoint),
        };
        guard.output.insert(name.to_string(), outputs);
    }

    fn input_pipe(&self, name: &str) -> Option<SharedInput> {
        self.lock_inner().input.get(name).cloned()
    }

    fn output_pipe_stdout(&self, name: &str) -> Option<PipeEndpoint> {
        self.lock_inner()
            .output
            .get(name)
            .and_then(|pipe| pipe.stdout.clone())
    }

    fn output_pipe_stderr(&self, name: &str) -> Option<PipeEndpoint> {
        self.lock_inner()
            .output
            .get(name)
            .and_then(|pipe| pipe.stderr.clone())
    }

    /// Whether an OS kernel pair exists under this name.
    /// Single lock acquisition for the lookup.
    #[cfg(not(miri))]
    pub(super) fn has_os_pipe(&self, name: &str) -> bool {
        self.lock_inner().os.contains_key(name)
    }

    pub(super) fn exists(&self, name: &str) -> bool {
        let guard = self.lock_inner();
        guard.input.contains_key(name)
            || guard.output.contains_key(name)
            || guard.inners.contains_key(name)
            || {
                #[cfg(not(miri))]
                {
                    guard.os.contains_key(name)
                }
                #[cfg(miri)]
                {
                    false
                }
            }
    }

    /// Backend behind a resolved stdin reader, matched by pointer identity
    /// against registered script-pipe inputs. Lets bridge workers run
    /// timeout-bounded reads without touching shared pipe semantics.
    /// Returns `None` for OS-promoted pairs, host-injected handles, and
    /// unknown readers (those fall back to blocking reads).
    pub(super) fn inner_for_reader(&self, reader: &SharedInput) -> Option<Arc<PipeInner>> {
        let guard = self.lock_inner();
        let name = guard
            .input
            .iter()
            .find(|(_, existing)| Arc::ptr_eq(existing, reader))
            .map(|(name, _)| name.clone())?;
        guard.inners.get(&name).cloned()
    }

    /// Non-destructive snapshot of a script pipe's buffered bytes for
    /// pipe-content assertions. OS-promoted pipes hold kernel bytes this
    /// cannot see, and host-injected or missing pipes have no script
    /// backend: both bail with a message directing to drains or harness
    /// pipe assertions instead of silently yielding empty content.
    pub(super) fn peek_pipe_content(&self, name: &str) -> Result<Vec<u8>> {
        #[cfg(not(miri))]
        if self.lock_inner().os.contains_key(name) {
            return Err(anyhow::anyhow!(
                "cannot peek OS-promoted pipe {name:?}: drain it or assert via harness pipe buffers"
            ));
        }
        let backend = {
            let guard = self.lock_inner();
            guard.inners.get(name).cloned()
        };
        match backend {
            Some(inner) => inner
                .peek_bytes()
                .map_err(|e| anyhow::anyhow!("failed to peek pipe {name:?}: {e}")),
            None => Err(anyhow::anyhow!("no script pipe backend for {name:?}")),
        }
    }

    /// Snapshot one pipe backend for `INSPECT()` diagnostics. Clones what
    /// is needed under one registry lock, then queries backend state after
    /// releasing it, so lock order always stays registry-before-inner.
    pub(super) fn inspect_pipe(&self, name: &str) -> super::pipe::PipeInfo {
        use super::pipe::{PipeInfo, PipeKindDesc};
        #[cfg(not(miri))]
        if self.lock_inner().os.contains_key(name) {
            return PipeInfo {
                kind: PipeKindDesc::Os,
                buffered: 0,
                readers: 1,
                writers: 1,
            };
        }
        let (backend, has_reader, has_output) = {
            let guard = self.lock_inner();
            (
                guard.inners.get(name).cloned(),
                guard.input.contains_key(name),
                guard.output.contains_key(name),
            )
        };
        match (backend, has_reader, has_output) {
            (Some(inner), has_reader, _) => PipeInfo {
                kind: PipeKindDesc::Script,
                buffered: inner.buffered_bytes(),
                readers: usize::from(has_reader),
                writers: inner.writer_count(),
            },
            (None, has_reader, has_output) if has_reader || has_output => PipeInfo {
                kind: PipeKindDesc::External,
                buffered: 0,
                readers: usize::from(has_reader),
                writers: 0,
            },
            (None, _, _) => PipeInfo {
                kind: PipeKindDesc::Missing,
                buffered: 0,
                readers: 0,
                writers: 0,
            },
        }
    }

    /// Ensure an entry exists for this binding. Fresh names become OS
    /// kernel pairs when promotion fired, script pipes otherwise. Existing
    /// script entries keep their type: first binding wins, so sequential
    /// fan in and host injected pipes never change shape underfoot.
    /// Existing OS entries are kept while live and recycled once spent
    /// (see [`PipeRegistry::ensure_os_pipe`]).
    /// Atomic: existence check and insertion happen under one lock.
    fn ensure_pipe_for(&self, name: &str, promote: bool) -> Result<()> {
        {
            let guard = self.lock_inner();
            if guard.input.contains_key(name) || guard.output.contains_key(name) {
                return Ok(());
            }
        }
        #[cfg(not(miri))]
        if promote {
            return self.ensure_os_pipe(name);
        }
        #[cfg(not(miri))]
        if self.lock_inner().os.contains_key(name) {
            return self.refresh_os_pipe(name);
        }
        #[cfg(miri)]
        let _ = promote;
        self.ensure_pipe(name);
        Ok(())
    }

    /// Resolve a stdin binding. OS entries hand the reader to `RUN`
    /// directly and bridge it to a shared handle for DSL commands; script
    /// entries keep the existing lookup. Either way a second consumer of
    /// a live pair bails instead of stealing the descriptor.
    fn resolve_stdin(&self, idx: usize, name: &str, direct: bool) -> Result<CommandStdin> {
        #[cfg(not(miri))]
        if self.has_os_pipe(name) {
            if direct {
                let reader = self.os_reader(name).ok_or_else(|| {
                    anyhow::anyhow!(
                        "step {}: WITH_IO stdin pipe '{}' is undefined",
                        idx + 1,
                        name
                    )
                })?;
                return Ok(CommandStdin::OsPipe(reader));
            }
            return Ok(CommandStdin::Stream(self.bridge_os_reader(name)?));
        }
        #[cfg(miri)]
        let _ = direct;
        let reader = self.input_pipe(name).ok_or_else(|| {
            anyhow::anyhow!(
                "step {}: WITH_IO stdin pipe '{}' is undefined",
                idx + 1,
                name
            )
        })?;
        Ok(CommandStdin::Stream(reader))
    }

    /// Resolve a stdout binding. Mirrors [`ExecIo::resolve_stdin`] with
    /// `StreamHandle` outputs so `RUN` keeps zero copy `Stdio` handoff.
    fn resolve_stdout(&self, idx: usize, name: &str, direct: bool) -> Result<StreamHandle> {
        #[cfg(not(miri))]
        if self.has_os_pipe(name) {
            if direct {
                let writer = self.os_writer(name).ok_or_else(|| {
                    anyhow::anyhow!(
                        "step {}: WITH_IO stdout pipe '{}' is undefined",
                        idx + 1,
                        name
                    )
                })?;
                return Ok(StreamHandle::Os(writer));
            }
            return Ok(StreamHandle::Stream(self.bridge_os_writer(name)?));
        }
        #[cfg(miri)]
        let _ = direct;
        let endpoint = self.output_pipe_stdout(name).ok_or_else(|| {
            anyhow::anyhow!(
                "step {}: WITH_IO stdout pipe '{}' is undefined",
                idx + 1,
                name
            )
        })?;
        Ok(endpoint.to_stream_handle())
    }

    /// Resolve a stderr binding. Mirrors [`ExecIo::resolve_stdout`]:
    /// `RUN` keeps zero copy handoff, DSL commands get a bridged shared
    /// handle. Binding `stdout` and `stderr` to one live name takes the
    /// same slot twice, so the second take bails deterministically; merge
    /// in shell via `2>&1` instead.
    fn resolve_stderr(&self, idx: usize, name: &str, direct: bool) -> Result<StreamHandle> {
        #[cfg(not(miri))]
        if self.has_os_pipe(name) {
            if direct {
                let writer = self.os_writer(name).ok_or_else(|| {
                    anyhow::anyhow!(
                        "step {}: WITH_IO stderr pipe '{}' is undefined",
                        idx + 1,
                        name
                    )
                })?;
                return Ok(StreamHandle::Os(writer));
            }
            return Ok(StreamHandle::Stream(self.bridge_os_writer(name)?));
        }
        #[cfg(miri)]
        let _ = direct;
        let endpoint = self.output_pipe_stderr(name).ok_or_else(|| {
            anyhow::anyhow!(
                "step {}: WITH_IO stderr pipe '{}' is undefined",
                idx + 1,
                name
            )
        })?;
        Ok(endpoint.to_stream_handle())
    }

    /// Create the OS pair for this name unless any entry already exists.
    /// An existing script entry keeps store and forward semantics. An
    /// existing live OS entry is reused so a concurrent second take on one
    /// end still fails deterministically at handle take time. An existing
    /// SPENT entry (both ends taken) is replaced with a fresh pair: the old
    /// kernel objects live on only in their takers, disjoint from the new
    /// pair, so replacement cannot corrupt a live session — and loop
    /// iterations rebind instead of failing on consumed handles. The
    /// documented trade: a third concurrent binding mid-session previously
    /// bailed loudly and now receives a fresh disconnected pair instead.
    /// Atomic: the check and the insertion share one lock acquisition.
    #[cfg(not(miri))]
    fn ensure_os_pipe(&self, name: &str) -> Result<()> {
        let mut guard = self.lock_inner();
        if let Some(spent) = guard.os.get(name).map(|entry| entry.is_spent()) {
            if spent {
                guard.os.insert(name.to_string(), OsPipeEntry::new()?);
            }
            return Ok(());
        }
        if guard.input.contains_key(name) || guard.output.contains_key(name) {
            bail!("pipe '{name}' is already bound as a script pipe");
        }
        guard.os.insert(name.to_string(), OsPipeEntry::new()?);
        Ok(())
    }

    /// Share-or-recycle for non-promoting bindings atop an OS entry: live
    /// pairs are kept (bridge reuse), spent pairs are replaced exactly like
    /// [`PipeRegistry::ensure_os_pipe`] so loop iterations rebind.
    #[cfg(not(miri))]
    fn refresh_os_pipe(&self, name: &str) -> Result<()> {
        let mut guard = self.lock_inner();
        if guard
            .os
            .get(name)
            .map(|entry| entry.is_spent())
            .unwrap_or(false)
        {
            guard.os.insert(name.to_string(), OsPipeEntry::new()?);
        }
        Ok(())
    }

    #[cfg(not(miri))]
    fn os_writer(&self, name: &str) -> Option<OsPipeWriter> {
        self.lock_inner()
            .os
            .get(name)
            .map(|entry| entry.writer.clone())
    }

    #[cfg(not(miri))]
    fn os_reader(&self, name: &str) -> Option<OsPipeReader> {
        self.lock_inner()
            .os
            .get(name)
            .map(|entry| entry.reader.clone())
    }

    /// Bridge the OS reader into a shared handle for DSL commands by
    /// taking it out of the slot exactly once. A later `RUN` consumer
    /// bails deterministically instead of reading a stolen descriptor.
    #[cfg(not(miri))]
    fn bridge_os_reader(&self, name: &str) -> Result<SharedInput> {
        let reader = self.os_reader(name).ok_or_else(|| {
            anyhow::anyhow!("OS pipe handle '{name}' has already been consumed by another process")
        })?;
        let owned = reader.take().map_err(|_| {
            anyhow::anyhow!("OS pipe handle '{name}' has already been consumed by another process")
        })?;
        Ok(Arc::new(Mutex::new(owned)))
    }

    /// Bridge the OS writer into a shared handle for DSL commands.
    /// Same single take contract as [`PipeRegistry::bridge_os_reader`].
    #[cfg(not(miri))]
    fn bridge_os_writer(&self, name: &str) -> Result<SharedOutput> {
        let writer = self.os_writer(name).ok_or_else(|| {
            anyhow::anyhow!("OS pipe handle '{name}' has already been consumed by another process")
        })?;
        let owned = writer.take().map_err(|_| {
            anyhow::anyhow!("OS pipe handle '{name}' has already been consumed by another process")
        })?;
        Ok(Arc::new(Mutex::new(owned)))
    }

    /// Pin a keeper slot on an existing script pipe so transient writer
    /// churn can never observe zero writers. Only pins pipes that already
    /// have a script backend; returns `None` for `OsPipeEntry` handles and
    /// host-injected raw handles, which need no pin. Callers must route
    /// creation through [`PipeRegistry::ensure_pipe_for`] first so OS
    /// promotion is honored and this function never forces a script pipe
    /// into existence.
    pub(super) fn pin_keeper(&self, name: &str) -> Result<Option<KeeperGuard>> {
        let inner = self.lock_inner().inners.get(name).cloned();
        match inner {
            Some(pipe) => Ok(Some(KeeperGuard::new(pipe))),
            None => Ok(None),
        }
    }

    fn insert_input(&self, name: String, reader: SharedInput) {
        self.lock_inner().input.insert(name, reader);
    }

    fn insert_output(
        &self,
        name: &str,
        stdout: Option<PipeEndpoint>,
        stderr: Option<PipeEndpoint>,
    ) {
        let mut guard = self.lock_inner();
        let entry = guard.output.entry(name.to_string()).or_default();
        if stdout.is_some() {
            entry.stdout = stdout;
        }
        if stderr.is_some() {
            entry.stderr = stderr;
        }
    }
}

#[derive(Clone, Default)]
pub struct ExecIo {
    stdin: Option<SharedInput>,
    stdout: Option<SharedOutput>,
    stderr: Option<SharedOutput>,
    pipes: PipeRegistry,
    inherit_env_overrides: HashMap<String, String>,
    inherit_env_removed: HashSet<String>,
}

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
    Inherit,
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
            StreamHandle::Inherit => CommandStdout::Inherit,
            #[cfg(not(miri))]
            StreamHandle::Os(writer) => CommandStdout::OsPipe(writer.clone()),
        }
    }

    pub(super) fn to_stderr(&self) -> CommandStderr {
        match self {
            StreamHandle::Stream(writer) => CommandStderr::Stream(writer.clone()),
            StreamHandle::Inherit => CommandStderr::Inherit,
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
        Some(StreamHandle::Inherit) | None => {
            let mut stdout = io::stdout();
            op(&mut stdout)
        }
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

    pub fn insert_input_pipe<S: Into<String>>(&mut self, name: S, reader: SharedInput) {
        self.pipes.insert_input(name.into(), reader);
    }

    pub fn insert_output_pipe<S: Into<String>>(&mut self, name: S, writer: SharedOutput) {
        let endpoint = PipeEndpoint::stream(writer.clone());
        let endpoint2 = PipeEndpoint::stream(writer);
        self.pipes
            .insert_output(&name.into(), Some(endpoint), Some(endpoint2));
    }

    pub fn insert_output_pipe_stdout<S: Into<String>>(&mut self, name: S, writer: SharedOutput) {
        self.pipes
            .insert_output(&name.into(), Some(PipeEndpoint::stream(writer)), None);
    }

    pub fn insert_output_pipe_stderr<S: Into<String>>(&mut self, name: S, writer: SharedOutput) {
        self.pipes
            .insert_output(&name.into(), None, Some(PipeEndpoint::stream(writer)));
    }

    pub fn insert_output_pipe_stdout_inherit<S: Into<String>>(&mut self, name: S) {
        self.pipes
            .insert_output(&name.into(), Some(PipeEndpoint::Inherit), None);
    }

    pub fn insert_output_pipe_stderr_inherit<S: Into<String>>(&mut self, name: S) {
        self.pipes
            .insert_output(&name.into(), None, Some(PipeEndpoint::Inherit));
    }

    /// Ensure an entry exists for this binding, promoting fresh names to
    /// OS kernel pairs when asked. Existing entries keep their type.
    pub(super) fn ensure_pipe_for(&self, name: &str, promote: bool) -> Result<()> {
        self.pipes.ensure_pipe_for(name, promote)
    }

    pub(super) fn pipe_exists(&self, name: &str) -> bool {
        self.pipes.exists(name)
    }

    /// Script-pipe backend behind a resolved stdin reader, if any. Used
    /// only by the network bridge for timeout-bounded reads.
    pub(super) fn stdin_pipe_inner(&self, reader: &SharedInput) -> Option<Arc<PipeInner>> {
        self.pipes.inner_for_reader(reader)
    }

    /// Snapshot of one pipe for `INSPECT()` diagnostics. Single lock
    /// acquisition for the lookup; backend stats are cloned out from under
    /// their own lock (same lock order as every other registry path).
    pub(super) fn inspect_pipe(&self, name: &str) -> super::pipe::PipeInfo {
        self.pipes.inspect_pipe(name)
    }

    /// Non-destructive snapshot of a script pipe's buffered bytes for
    /// pipe-content assertions. Single lock acquisition for the lookup;
    /// content is cloned out from under its own lock.
    pub(super) fn peek_pipe_content(&self, name: &str) -> Result<Vec<u8>> {
        self.pipes.peek_pipe_content(name)
    }

    /// Pin a keeper slot on an existing script pipe. `None` for OS pipes
    /// and host-injected handles. Creation must go through
    /// [`ExecIo::ensure_pipe_for`] first so OS promotion is honored.
    pub(super) fn pin_keeper(&self, name: &str) -> Result<Option<KeeperGuard>> {
        self.pipes.pin_keeper(name)
    }

    /// Resolve a stdin binding to a runnable handle.
    pub(super) fn resolve_stdin(
        &self,
        idx: usize,
        name: &str,
        direct: bool,
    ) -> Result<CommandStdin> {
        self.pipes.resolve_stdin(idx, name, direct)
    }

    /// Resolve a stdout binding to a runnable handle.
    pub(super) fn resolve_stdout(
        &self,
        idx: usize,
        name: &str,
        direct: bool,
    ) -> Result<StreamHandle> {
        self.pipes.resolve_stdout(idx, name, direct)
    }

    /// Resolve a stderr binding to a runnable handle.
    pub(super) fn resolve_stderr(
        &self,
        idx: usize,
        name: &str,
        direct: bool,
    ) -> Result<StreamHandle> {
        self.pipes.resolve_stderr(idx, name, direct)
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

    pub fn input_pipe(&self, name: &str) -> Option<SharedInput> {
        self.pipes.input_pipe(name)
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
