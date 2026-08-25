use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex};

use oxdock_process::{SharedInput, SharedOutput};

#[derive(Clone)]
pub(super) enum PipeEndpoint {
    Stream(SharedOutput),
    Script(ScriptPipeEndpoint),
    Inherit,
}

impl PipeEndpoint {
    pub(super) fn stream(writer: SharedOutput) -> Self {
        PipeEndpoint::Stream(writer)
    }

    pub(super) fn script(endpoint: ScriptPipeEndpoint) -> Self {
        PipeEndpoint::Script(endpoint)
    }

    pub(super) fn to_stream_handle(&self) -> super::StreamHandle {
        match self {
            PipeEndpoint::Stream(writer) => super::StreamHandle::Stream(writer.clone()),
            PipeEndpoint::Script(endpoint) => super::StreamHandle::Stream(endpoint.stream_handle()),
            PipeEndpoint::Inherit => super::StreamHandle::Inherit,
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct PipeOutputs {
    pub(super) stdout: Option<PipeEndpoint>,
    pub(super) stderr: Option<PipeEndpoint>,
}

pub(super) struct ScriptPipe {
    inner: Arc<PipeInner>,
    reader: SharedInput,
}

impl ScriptPipe {
    pub(super) fn new() -> Self {
        let inner = Arc::new(PipeInner::new());
        let reader: SharedInput = Arc::new(Mutex::new(PipeReader::new(inner.clone())));
        Self { inner, reader }
    }

    pub(super) fn reader(&self) -> SharedInput {
        self.reader.clone()
    }

    pub(super) fn endpoint(&self) -> ScriptPipeEndpoint {
        ScriptPipeEndpoint::new(self.inner.clone())
    }
}

#[derive(Clone)]
pub(super) struct ScriptPipeEndpoint {
    inner: Arc<PipeInner>,
}

impl ScriptPipeEndpoint {
    fn new(inner: Arc<PipeInner>) -> Self {
        Self { inner }
    }

    pub(super) fn stream_handle(&self) -> SharedOutput {
        Arc::new(Mutex::new(PipeWriter::new(self.inner.clone())))
    }
}

struct PipeInner {
    state: Mutex<PipeState>,
    ready: Condvar,
}

impl PipeInner {
    fn new() -> Self {
        Self {
            state: Mutex::new(PipeState::new()),
            ready: Condvar::new(),
        }
    }

    fn attach_writer(&self) {
        let mut state = self.lock_state();
        state.writers += 1;
        state.closed = false;
    }

    fn detach_writer(&self) {
        let mut state = self.lock_state();
        state.writers = state.writers.saturating_sub(1);
        if state.writers == 0 {
            state.closed = true;
        }
        drop(state);
        self.ready.notify_all();
    }

    fn push_bytes(&self, data: &[u8]) {
        let mut state = self.lock_state();
        state.buffer.extend(data.iter().copied());
        drop(state);
        self.ready.notify_all();
    }

    fn read_into(&self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut state = self.lock_state();
        loop {
            if !state.buffer.is_empty() {
                let mut read = 0;
                while read < buf.len() && !state.buffer.is_empty() {
                    if let Some(byte) = state.buffer.pop_front() {
                        buf[read] = byte;
                        read += 1;
                    }
                }
                return Ok(read);
            }
            if state.closed {
                return Ok(0);
            }
            state = self
                .ready
                .wait(state)
                .map_err(|_| io::Error::other("pipe wait poisoned"))?;
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, PipeState> {
        self.state.lock().expect("script pipe state poisoned")
    }
}

struct PipeState {
    buffer: VecDeque<u8>,
    writers: usize,
    closed: bool,
}

impl PipeState {
    fn new() -> Self {
        Self {
            buffer: VecDeque::new(),
            writers: 0,
            closed: false,
        }
    }
}

struct PipeReader {
    inner: Arc<PipeInner>,
}

impl PipeReader {
    fn new(inner: Arc<PipeInner>) -> Self {
        Self { inner }
    }
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read_into(buf)
    }
}

struct PipeWriter {
    inner: Arc<PipeInner>,
}

impl PipeWriter {
    fn new(inner: Arc<PipeInner>) -> Self {
        inner.attach_writer();
        Self { inner }
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.push_bytes(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        self.inner.detach_writer();
    }
}
