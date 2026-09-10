use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use anyhow::Result;
#[cfg(not(miri))]
use anyhow::bail;
use oxdock_process::{CommandStderr, CommandStdin, CommandStdout, SharedInput, SharedOutput};
#[cfg(not(miri))]
use oxdock_process::{OsPipeReader, OsPipeWriter, create_os_pipe};

use super::pipe::{PipeEndpoint, PipeOutputs, ScriptPipe};

/// Shared pipe registry. All threads in the same execution context
/// reference the same registry, so pipes created by the parent are
/// visible to child threads.
#[derive(Clone, Default)]
pub(super) struct PipeRegistry {
    input: Arc<Mutex<HashMap<String, SharedInput>>>,
    output: Arc<Mutex<HashMap<String, PipeOutputs>>>,
    #[cfg(not(miri))]
    os: Arc<Mutex<HashMap<String, OsPipeEntry>>>,
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
}

impl PipeRegistry {
    fn ensure_pipe(&self, name: &str) {
        let mut input = self.input.lock().expect("pipe lock poisoned");
        let mut output = self.output.lock().expect("pipe lock poisoned");
        if input.contains_key(name) || output.contains_key(name) {
            return;
        }
        let pipe = ScriptPipe::new();
        input.insert(name.to_string(), pipe.reader());
        let endpoint = PipeEndpoint::script(pipe.endpoint());
        let outputs = PipeOutputs {
            stdout: Some(endpoint.clone()),
            stderr: Some(endpoint),
        };
        output.insert(name.to_string(), outputs);
    }

    fn input_pipe(&self, name: &str) -> Option<SharedInput> {
        self.input
            .lock()
            .expect("pipe lock poisoned")
            .get(name)
            .cloned()
    }

    fn output_pipe_stdout(&self, name: &str) -> Option<PipeEndpoint> {
        self.output
            .lock()
            .expect("pipe lock poisoned")
            .get(name)
            .and_then(|pipe| pipe.stdout.clone())
    }

    fn output_pipe_stderr(&self, name: &str) -> Option<PipeEndpoint> {
        self.output
            .lock()
            .expect("pipe lock poisoned")
            .get(name)
            .and_then(|pipe| pipe.stderr.clone())
    }

    /// True when any entry, script or OS, exists under this name.
    fn has_pipe(&self, name: &str) -> bool {
        let input = self.input.lock().expect("pipe lock poisoned");
        let output = self.output.lock().expect("pipe lock poisoned");
        if input.contains_key(name) || output.contains_key(name) {
            return true;
        }
        #[cfg(not(miri))]
        return self
            .os
            .lock()
            .expect("pipe lock poisoned")
            .contains_key(name);
        #[cfg(miri)]
        return false;
    }

    #[cfg(not(miri))]
    pub(super) fn has_os_pipe(&self, name: &str) -> bool {
        self.os
            .lock()
            .expect("pipe lock poisoned")
            .contains_key(name)
    }

    /// Ensure an entry exists for this binding. Fresh names become OS
    /// kernel pairs when promotion fired, script pipes otherwise. Existing
    /// entries keep their type: first binding wins, so sequential fan in
    /// and host injected pipes never change shape underfoot.
    fn ensure_pipe_for(&self, name: &str, promote: bool) -> Result<()> {
        if self.has_pipe(name) {
            return Ok(());
        }
        #[cfg(not(miri))]
        if promote {
            return self.ensure_os_pipe(name);
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
    /// An existing script entry keeps store and forward semantics; an
    /// existing OS entry is reused so the second producer fails
    /// deterministically at handle take time, never by interleaving.
    #[cfg(not(miri))]
    fn ensure_os_pipe(&self, name: &str) -> Result<()> {
        if self.has_os_pipe(name) {
            return Ok(());
        }
        {
            let input = self.input.lock().expect("pipe lock poisoned");
            let output = self.output.lock().expect("pipe lock poisoned");
            if input.contains_key(name) || output.contains_key(name) {
                bail!("pipe '{name}' is already bound as a script pipe");
            }
        }
        let mut os = self.os.lock().expect("pipe lock poisoned");
        if !os.contains_key(name) {
            os.insert(name.to_string(), OsPipeEntry::new()?);
        }
        Ok(())
    }

    #[cfg(not(miri))]
    fn os_writer(&self, name: &str) -> Option<OsPipeWriter> {
        self.os
            .lock()
            .expect("pipe lock poisoned")
            .get(name)
            .map(|entry| entry.writer.clone())
    }

    #[cfg(not(miri))]
    fn os_reader(&self, name: &str) -> Option<OsPipeReader> {
        self.os
            .lock()
            .expect("pipe lock poisoned")
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

/// Sliding window for streaming pattern matching in ASSERT_STDOUT.
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

/// Wraps the configured stdout sink so every byte written to it is also
/// pushed to all registered SlidingWindow observers for `ASSERT_STDOUT`.
/// When no sink is configured (`inner` is `None`) bytes are forwarded to
/// real stdout so interactive CLI output still reaches the terminal.
struct TeeWriter {
    inner: Option<SharedOutput>,
    windows: Arc<Mutex<HashMap<(usize, usize), SlidingWindow>>>,
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
            None => io::stdout().write_all(buf)?,
        }
        // Push to ALL registered assertion windows (O(1) per byte per window)
        if let Ok(mut windows) = self.windows.lock() {
            for window in windows.values_mut() {
                window.push_chunk(buf);
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
            None => io::stdout().flush()?,
        }
        Ok(())
    }
}

/// Installs the tee around `sink` (or real stdout when absent).
pub(crate) fn teed_stdout(
    sink: Option<SharedOutput>,
    windows: Arc<Mutex<HashMap<(usize, usize), SlidingWindow>>>,
) -> SharedOutput {
    Arc::new(Mutex::new(TeeWriter {
        inner: sink,
        windows,
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
        self.pipes
            .input
            .lock()
            .expect("pipe lock poisoned")
            .insert(name.into(), reader);
    }

    pub fn insert_output_pipe<S: Into<String>>(&mut self, name: S, writer: SharedOutput) {
        let mut output = self.pipes.output.lock().expect("pipe lock poisoned");
        let entry = output.entry(name.into()).or_default();
        entry.stdout = Some(PipeEndpoint::stream(writer.clone()));
        entry.stderr = Some(PipeEndpoint::stream(writer));
    }

    pub fn insert_output_pipe_stdout<S: Into<String>>(&mut self, name: S, writer: SharedOutput) {
        let mut output = self.pipes.output.lock().expect("pipe lock poisoned");
        let entry = output.entry(name.into()).or_default();
        entry.stdout = Some(PipeEndpoint::stream(writer));
    }

    pub fn insert_output_pipe_stderr<S: Into<String>>(&mut self, name: S, writer: SharedOutput) {
        let mut output = self.pipes.output.lock().expect("pipe lock poisoned");
        let entry = output.entry(name.into()).or_default();
        entry.stderr = Some(PipeEndpoint::stream(writer));
    }

    pub fn insert_output_pipe_stdout_inherit<S: Into<String>>(&mut self, name: S) {
        let mut output = self.pipes.output.lock().expect("pipe lock poisoned");
        let entry = output.entry(name.into()).or_default();
        entry.stdout = Some(PipeEndpoint::Inherit);
    }

    pub fn insert_output_pipe_stderr_inherit<S: Into<String>>(&mut self, name: S) {
        let mut output = self.pipes.output.lock().expect("pipe lock poisoned");
        let entry = output.entry(name.into()).or_default();
        entry.stderr = Some(PipeEndpoint::Inherit);
    }

    /// Ensure an entry exists for this binding, promoting fresh names to
    /// OS kernel pairs when asked. Existing entries keep their type.
    pub(super) fn ensure_pipe_for(&self, name: &str, promote: bool) -> Result<()> {
        self.pipes.ensure_pipe_for(name, promote)
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
