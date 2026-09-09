use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex};

use oxdock_process::{SharedInput, SharedOutput};

use super::capture::SpillBuffer;

/// Memory threshold before spilling to disk (re-exported for tests).
#[cfg(test)]
pub(super) use super::capture::SPILL_THRESHOLD as PIPE_SPILL_THRESHOLD;

/// Maximum active backlog before returning an error (re-exported for tests).
#[cfg(test)]
pub(super) use super::capture::MAX_BACKLOG as PIPE_MAX_BACKLOG;

#[derive(Clone)]
pub(crate) enum PipeEndpoint {
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

    #[cfg(test)]
    #[cfg_attr(miri, allow(dead_code))]
    #[allow(clippy::disallowed_types)]
    pub(super) fn temp_path(&self) -> Option<std::path::PathBuf> {
        self.inner.temp_path()
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

struct PipeState {
    buffer: SpillBuffer,
    writers: usize,
    closed: bool,
}

impl PipeState {
    fn new() -> Self {
        Self {
            buffer: SpillBuffer::new(),
            writers: 0,
            closed: false,
        }
    }
}

impl PipeInner {
    fn new() -> Self {
        Self {
            state: Mutex::new(PipeState::new()),
            ready: Condvar::new(),
        }
    }

    #[cfg(test)]
    #[cfg_attr(miri, allow(dead_code))]
    #[allow(clippy::disallowed_types)]
    fn temp_path(&self) -> Option<std::path::PathBuf> {
        self.lock_state().buffer.temp_path()
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

    fn push_bytes(&self, data: &[u8]) -> io::Result<()> {
        let state = self.lock_state();
        let res = state.buffer.push_bytes(data);
        drop(state);
        self.ready.notify_all();
        res
    }

    fn read_into(&self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut state = self.lock_state();
        loop {
            let n = state.buffer.read_into(buf)?;
            if n > 0 {
                return Ok(n);
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
        self.inner.push_bytes(buf)?;
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
