use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use oxdock_process::{CommandStderr, CommandStdout, SharedInput, SharedOutput};

use super::pipe::{PipeEndpoint, PipeOutputs, ScriptPipe};

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

#[derive(Clone, Default)]
pub struct ExecIo {
    stdin: Option<SharedInput>,
    stdout: Option<SharedOutput>,
    stderr: Option<SharedOutput>,
    input_pipes: HashMap<String, SharedInput>,
    output_pipes: HashMap<String, PipeOutputs>,
    inherit_env_overrides: HashMap<String, String>,
    inherit_env_removed: HashSet<String>,
}

#[derive(Clone)]
pub(super) enum StreamHandle {
    Inherit,
    Stream(SharedOutput),
}

impl StreamHandle {
    pub(super) fn to_stdout(&self) -> CommandStdout {
        match self {
            StreamHandle::Stream(writer) => CommandStdout::Stream(writer.clone()),
            StreamHandle::Inherit => CommandStdout::Inherit,
        }
    }

    pub(super) fn to_stderr(&self) -> CommandStderr {
        match self {
            StreamHandle::Stream(writer) => CommandStderr::Stream(writer.clone()),
            StreamHandle::Inherit => CommandStderr::Inherit,
        }
    }
}

pub(super) fn write_stdout<F>(handle: Option<StreamHandle>, op: F) -> Result<()>
where
    F: FnOnce(&mut dyn Write) -> Result<()>,
{
    if let Some(StreamHandle::Stream(writer)) = handle {
        if let Ok(mut guard) = writer.lock() {
            op(&mut *guard)?;
        }
        Ok(())
    } else {
        let mut stdout = io::stdout();
        op(&mut stdout)
    }
}

/// Wraps the configured stdout sink so every byte written to it is also
/// pushed to all registered SlidingWindow observers for `ASSERT_STDOUT`.
/// When no sink is configured (`inner` is `None`) bytes are forwarded to
/// real stdout so interactive CLI output still reaches the terminal.
struct TeeWriter {
    inner: Option<SharedOutput>,
    windows: Arc<Mutex<HashMap<usize, SlidingWindow>>>,
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
    windows: Arc<Mutex<HashMap<usize, SlidingWindow>>>,
) -> SharedOutput {
    Arc::new(Mutex::new(TeeWriter { inner: sink, windows }))
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

    pub fn insert_input_pipe<S: Into<String>>(&mut self, name: S, reader: SharedInput) {
        self.input_pipes.insert(name.into(), reader);
    }

    pub fn insert_output_pipe<S: Into<String>>(&mut self, name: S, writer: SharedOutput) {
        let entry = self.output_pipes.entry(name.into()).or_default();
        entry.stdout = Some(PipeEndpoint::stream(writer.clone()));
        entry.stderr = Some(PipeEndpoint::stream(writer));
    }

    pub fn insert_output_pipe_stdout<S: Into<String>>(&mut self, name: S, writer: SharedOutput) {
        let entry = self.output_pipes.entry(name.into()).or_default();
        entry.stdout = Some(PipeEndpoint::stream(writer));
    }

    pub fn insert_output_pipe_stderr<S: Into<String>>(&mut self, name: S, writer: SharedOutput) {
        let entry = self.output_pipes.entry(name.into()).or_default();
        entry.stderr = Some(PipeEndpoint::stream(writer));
    }

    pub fn insert_output_pipe_stdout_inherit<S: Into<String>>(&mut self, name: S) {
        let entry = self.output_pipes.entry(name.into()).or_default();
        entry.stdout = Some(PipeEndpoint::Inherit);
    }

    pub fn insert_output_pipe_stderr_inherit<S: Into<String>>(&mut self, name: S) {
        let entry = self.output_pipes.entry(name.into()).or_default();
        entry.stderr = Some(PipeEndpoint::Inherit);
    }

    pub(super) fn ensure_script_pipe(&mut self, name: &str) {
        if self.input_pipes.contains_key(name) || self.output_pipes.contains_key(name) {
            return;
        }

        let pipe = ScriptPipe::new();
        self.input_pipes.insert(name.to_string(), pipe.reader());
        let endpoint = PipeEndpoint::script(pipe.endpoint());
        let outputs = PipeOutputs {
            stdout: Some(endpoint.clone()),
            stderr: Some(endpoint),
        };
        self.output_pipes.insert(name.to_string(), outputs);
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
        self.input_pipes.get(name).cloned()
    }

    pub(super) fn output_pipe_stdout(&self, name: &str) -> Option<PipeEndpoint> {
        self.output_pipes
            .get(name)
            .and_then(|pipe| pipe.stdout.clone())
    }

    pub(super) fn output_pipe_stderr(&self, name: &str) -> Option<PipeEndpoint> {
        self.output_pipes
            .get(name)
            .and_then(|pipe| pipe.stderr.clone())
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
