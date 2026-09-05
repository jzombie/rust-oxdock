use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use oxdock_process::{SharedInput, SharedOutput};

/// Memory threshold before spilling to disk.
#[cfg(not(test))]
const PIPE_SPILL_THRESHOLD: usize = 8 * 1024 * 1024; // 8 MiB
#[cfg(test)]
pub(super) const PIPE_SPILL_THRESHOLD: usize = 1024 * 1024; // 1 MiB for tests

/// Maximum active backlog before returning an error.
#[cfg(not(test))]
const PIPE_MAX_BACKLOG: u64 = 100 * 1024 * 1024; // 100 MiB
#[cfg(test)]
pub(super) const PIPE_MAX_BACKLOG: u64 = 2 * 1024 * 1024; // 2 MiB for tests

/// Unique temp file naming counter.
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

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

impl PipeInner {
    fn new() -> Self {
        Self {
            state: Mutex::new(PipeState::new()),
            ready: Condvar::new(),
        }
    }

    #[cfg(test)]
    #[allow(clippy::disallowed_types)]
    fn temp_path(&self) -> Option<std::path::PathBuf> {
        let state = self.lock_state();
        match &state.buffer {
            PipeBuffer::Memory(_) => None,
            #[cfg(not(miri))]
            PipeBuffer::Disk(disk) => Some(disk.path.clone()),
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

    fn push_bytes(&self, data: &[u8]) -> io::Result<()> {
        let mut state = self.lock_state();
        match &mut state.buffer {
            PipeBuffer::Memory(vec) => {
                let original_len = vec.len();
                vec.extend(data.iter().copied());
                #[cfg(not(miri))]
                if vec.len() > PIPE_SPILL_THRESHOLD {
                    match DiskInner::create_from_vec(vec) {
                        Ok(disk) => {
                            state.buffer = PipeBuffer::Disk(disk);
                        }
                        Err(e) => {
                            vec.truncate(original_len);
                            return Err(e);
                        }
                    }
                }
            }
            #[cfg(not(miri))]
            PipeBuffer::Disk(disk) => {
                disk.write_bytes(data)?;
            }
        }
        drop(state);
        self.ready.notify_all();
        Ok(())
    }

    fn read_into(&self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut state = self.lock_state();
        loop {
            match &mut state.buffer {
                PipeBuffer::Memory(vec) => {
                    if !vec.is_empty() {
                        let mut read = 0;
                        while read < buf.len() && !vec.is_empty() {
                            buf[read] = vec.pop_front().unwrap();
                            read += 1;
                        }
                        return Ok(read);
                    }
                }
                #[cfg(not(miri))]
                PipeBuffer::Disk(disk) => {
                    if disk.available() > 0 {
                        let n = disk.read_bytes(buf)?;
                        return Ok(n);
                    }
                }
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

enum PipeBuffer {
    Memory(VecDeque<u8>),
    #[cfg(not(miri))]
    Disk(DiskInner),
}

impl PipeBuffer {
    fn new() -> Self {
        PipeBuffer::Memory(VecDeque::new())
    }
}

#[cfg(not(miri))]
#[allow(clippy::disallowed_types)]
struct DiskInner {
    writer: Option<std::fs::File>,
    reader: Option<std::fs::File>,
    write_pos: u64,
    read_pos: u64,
    path: std::path::PathBuf,
}

#[cfg(not(miri))]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
impl DiskInner {
    fn write_bytes(&mut self, data: &[u8]) -> io::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::other("disk writer closed"))?;
        writer.seek(SeekFrom::Start(self.write_pos))?;
        let new_backlog = (self.write_pos - self.read_pos) + data.len() as u64;
        if new_backlog > PIPE_MAX_BACKLOG {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!(
                    "pipe buffer exceeded maximum active backlog ({} MiB)",
                    PIPE_MAX_BACKLOG / (1024 * 1024)
                ),
            ));
        }
        writer.write_all(data)?;
        self.write_pos += data.len() as u64;
        Ok(())
    }

    fn read_bytes(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.write_pos - self.read_pos;
        if available == 0 {
            return Ok(0);
        }
        let reader = self
            .reader
            .as_mut()
            .ok_or_else(|| io::Error::other("disk reader closed"))?;
        reader.seek(SeekFrom::Start(self.read_pos))?;
        let to_read = (buf.len() as u64).min(available) as usize;
        let n = reader.read(&mut buf[..to_read])?;
        self.read_pos += n as u64;
        if self.read_pos == self.write_pos {
            self.read_pos = 0;
            self.write_pos = 0;
            if let Some(writer) = &mut self.writer {
                writer.set_len(0)?;
                writer.seek(SeekFrom::Start(0))?;
            }
            if let Some(reader) = &mut self.reader {
                reader.seek(SeekFrom::Start(0))?;
            }
        }
        Ok(n)
    }

    fn available(&self) -> u64 {
        self.write_pos - self.read_pos
    }

    fn create_from_vec(vec: &mut VecDeque<u8>) -> io::Result<Self> {
        let id = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("oxdock-pipe-{pid}-{id}.tmp"));

        let mut writer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;

        vec.make_contiguous();
        let (slice, _) = vec.as_slices();
        writer.write_all(slice)?;
        writer.seek(SeekFrom::Start(0))?;

        let reader = std::fs::File::open(&path)?;

        Ok(Self {
            writer: Some(writer),
            reader: Some(reader),
            write_pos: vec.len() as u64,
            read_pos: 0,
            path,
        })
    }
}

#[cfg(not(miri))]
#[allow(clippy::disallowed_methods)]
impl Drop for DiskInner {
    fn drop(&mut self) {
        self.writer.take();
        self.reader.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

struct PipeState {
    buffer: PipeBuffer,
    writers: usize,
    closed: bool,
}

impl PipeState {
    fn new() -> Self {
        Self {
            buffer: PipeBuffer::new(),
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
