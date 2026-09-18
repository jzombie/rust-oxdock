//! Script-pipe backend: an in-memory (spillable) byte channel with
//! writer/keeper accounting, blocking reads, and explicit close.
//!
//! A [`ScriptPipe`] bundles one [`PipeInner`] with its shared reader; writer
//! halves attach per binding through [`ScriptPipeEndpoint`]. [`KeeperGuard`]
//! pins transient gaps for background tasks. Moved verbatim from
//! `oxdock-core` so pipe handles (`crate::Slot`) can own backends without a
//! dependency cycle; behavior is unchanged.

use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use oxdock_process::{SharedInput, SharedOutput};

use crate::spill::SpillBuffer;

/// One script pipe: shared backend plus its shared reader half.
pub struct ScriptPipe {
    inner: Arc<PipeInner>,
    reader: SharedInput,
}

impl ScriptPipe {
    /// Production pipe: default spill threshold and backlog cap.
    pub fn new() -> Self {
        let inner = Arc::new(PipeInner::new());
        let reader: SharedInput = Arc::new(Mutex::new(PipeReader::new(inner.clone())));
        Self { inner, reader }
    }

    /// Pipe with explicit thresholds. Tests use small values to exercise
    /// the spill path without multi-megabyte payloads.
    pub fn with_thresholds(spill_threshold: usize, max_backlog: u64) -> Self {
        let inner = Arc::new(PipeInner::with_thresholds(spill_threshold, max_backlog));
        let reader: SharedInput = Arc::new(Mutex::new(PipeReader::new(inner.clone())));
        Self { inner, reader }
    }

    /// Shared reader half for `stdin` bindings.
    pub fn reader(&self) -> SharedInput {
        self.reader.clone()
    }

    /// Fresh writer-side endpoint for `stdout`/`stderr` bindings. Each
    /// endpoint mints one attached writer when resolved to a stream.
    pub fn endpoint(&self) -> ScriptPipeEndpoint {
        ScriptPipeEndpoint::new(self.inner.clone())
    }

    /// Backend for keepers, peeks, and diagnostics.
    pub fn pipe_inner(&self) -> Arc<PipeInner> {
        self.inner.clone()
    }

    /// Spill-file path when spilled, else `None`. Diagnostics for tests.
    #[allow(clippy::disallowed_types)]
    pub fn temp_path(&self) -> Option<std::path::PathBuf> {
        self.inner.temp_path()
    }
}

/// Writer-side endpoint of a [`ScriptPipe`]. Resolving it to a stream
/// attaches one writer (see [`PipeWriter`]); dropping the stream detaches.
#[derive(Clone)]
pub struct ScriptPipeEndpoint {
    inner: Arc<PipeInner>,
}

impl ScriptPipeEndpoint {
    fn new(inner: Arc<PipeInner>) -> Self {
        Self { inner }
    }

    /// One attached writer half for subprocess stdio or DSL output.
    pub fn stream_handle(&self) -> SharedOutput {
        Arc::new(Mutex::new(PipeWriter::new(self.inner.clone())))
    }
}

/// Shared script-pipe backend: buffer plus writer/keeper accounting.
/// Clone the `Arc`, never the state: every handle aliases one channel.
pub struct PipeInner {
    state: Mutex<PipeState>,
    ready: Condvar,
}

struct PipeState {
    buffer: SpillBuffer,
    writers: usize,
    keepers: usize,
    closed: bool,
}

impl PipeState {
    fn new(spill_threshold: usize, max_backlog: u64) -> Self {
        Self {
            buffer: SpillBuffer::with_thresholds(spill_threshold, max_backlog),
            writers: 0,
            keepers: 0,
            closed: false,
        }
    }
}

impl PipeInner {
    fn new() -> Self {
        Self {
            state: Mutex::new(PipeState::new(
                crate::spill::DEFAULT_SPILL_THRESHOLD,
                crate::spill::DEFAULT_MAX_BACKLOG,
            )),
            ready: Condvar::new(),
        }
    }

    fn with_thresholds(spill_threshold: usize, max_backlog: u64) -> Self {
        Self {
            state: Mutex::new(PipeState::new(spill_threshold, max_backlog)),
            ready: Condvar::new(),
        }
    }

    #[allow(clippy::disallowed_types)]
    fn temp_path(&self) -> Option<std::path::PathBuf> {
        self.lock_state().buffer.temp_path()
    }

    fn attach_writer(&self) {
        let mut state = self.lock_state();
        state.writers += 1;
        state.closed = false;
    }

    /// Live data-writer attachments (excludes keeper pins). Used for
    /// `INSPECT()` diagnostics; never blocks.
    pub fn writer_count(&self) -> usize {
        self.lock_state().writers
    }

    /// Bytes currently buffered for readers. Used for diagnostics.
    pub fn buffered_bytes(&self) -> u64 {
        self.lock_state().buffer.buffered_bytes()
    }

    /// Non-destructive snapshot of buffered bytes for pipe-content
    /// assertions. Never waits: returns what is buffered right now.
    pub fn peek_bytes(&self) -> io::Result<Vec<u8>> {
        self.lock_state().buffer.peek_bytes()
    }

    fn detach_writer(&self) {
        let mut state = self.lock_state();
        state.writers = state.writers.saturating_sub(1);
        if state.writers == 0 && state.keepers == 0 {
            state.closed = true;
        }
        drop(state);
        self.ready.notify_all();
    }

    /// Explicitly close the pipe: readers drain buffered bytes, then observe
    /// EOF regardless of live writers or keeper pins. General primitive
    /// (sockets have `shutdown`, files have `close`); pipes previously had
    /// detach-only EOF. A later writer attachment resurrects the pipe per
    /// standard attach semantics, so callers must not reuse closed pipes
    /// for new sessions.
    pub fn force_close(&self) {
        let mut state = self.lock_state();
        state.closed = true;
        drop(state);
        self.ready.notify_all();
    }

    /// Pin a keeper slot so transient writer churn can never observe zero
    /// writers. Called synchronously on the spawning thread before an
    /// `ASYNC` worker starts; the returned guard unpins on drop when the
    /// worker exits, restoring normal EOF semantics afterwards.
    /// Never touches `closed`: pinning a pipe that already reached EOF
    /// must not resurrect it into a blocking pipe.
    pub fn pin_keeper(&self) {
        let mut state = self.lock_state();
        state.keepers += 1;
    }

    /// Release one keeper slot. When the last transient writer and the
    /// last keeper are both gone the pipe closes and blocked readers see
    /// EOF.
    pub fn unpin_keeper(&self) {
        let mut state = self.lock_state();
        state.keepers = state.keepers.saturating_sub(1);
        if state.writers == 0 && state.keepers == 0 {
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

    /// Timeout-bounded variant of [`PipeInner::read_into`] for bridge worker
    /// loops: returns `Ok(None)` when the backstop elapses with no data and
    /// no close, so cancellation resolves on a tick instead of hanging on a
    /// condvar. Bridge-only caller; every DSL reader keeps blocking
    /// `read_into` with unchanged semantics.
    pub fn read_into_timeout(
        &self,
        buf: &mut [u8],
        backstop: Duration,
    ) -> io::Result<Option<usize>> {
        if buf.is_empty() {
            return Ok(Some(0));
        }
        let mut state = self.lock_state();
        loop {
            let n = state.buffer.read_into(buf)?;
            if n > 0 {
                return Ok(Some(n));
            }
            if state.closed {
                return Ok(Some(0));
            }
            let (guard, waited) = self
                .ready
                .wait_timeout(state, backstop)
                .map_err(|_| io::Error::other("pipe wait poisoned"))?;
            state = guard;
            if waited.timed_out() {
                return Ok(None);
            }
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

/// Pre-allocated keeper handle for `ASYNC` tasks. Created synchronously
/// on the spawning thread before the worker starts so the pipe can never
/// observe zero writers mid-flight; released when the worker exits.
pub struct KeeperGuard {
    inner: Option<Arc<PipeInner>>,
}

impl KeeperGuard {
    /// Pin a keeper slot on a script backend.
    pub fn new(inner: Arc<PipeInner>) -> Self {
        inner.pin_keeper();
        Self { inner: Some(inner) }
    }
}

impl Drop for KeeperGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.unpin_keeper();
        }
    }
}
