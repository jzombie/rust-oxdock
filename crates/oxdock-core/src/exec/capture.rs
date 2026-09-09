//! Spillable byte sink shared by pipe backlogs and `LET` output capture.
//!
//! Small outputs stay in memory (`VecDeque`). Past [`SPILL_THRESHOLD`] the
//! buffer spills to a guarded temp file (owned `GuardedTempDir`, PID-lock
//! GC) with a [`MAX_BACKLOG`] cap that bails with `OutOfMemory` instead of
//! OOMing. Draining truncates the file; dropping the buffer deletes the
//! directory. Under Miri the buffer stays memory-only (no host I/O).

use std::collections::VecDeque;
use std::io::{self, Read, Write};
#[cfg(not(miri))]
use std::io::{Seek, SeekFrom};
use std::sync::{Arc, Mutex};

#[cfg(not(miri))]
use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver, SpillFile};
use oxdock_process::SharedOutput;

/// Memory threshold before spilling to disk.
#[cfg(not(test))]
#[cfg_attr(miri, allow(dead_code))]
pub(super) const SPILL_THRESHOLD: usize = 8 * 1024 * 1024; // 8 MiB
#[cfg(test)]
#[cfg_attr(miri, allow(dead_code))]
pub(super) const SPILL_THRESHOLD: usize = 1024 * 1024; // 1 MiB for tests

/// Maximum active backlog before returning an error.
#[cfg(not(test))]
#[cfg_attr(miri, allow(dead_code))]
pub(super) const MAX_BACKLOG: u64 = 100 * 1024 * 1024; // 100 MiB
#[cfg(test)]
#[cfg_attr(miri, allow(dead_code))]
pub(super) const MAX_BACKLOG: u64 = 2 * 1024 * 1024; // 2 MiB for tests

/// Thread-safe spillable buffer. Share via `Arc`; hand out writers with
/// [`SpillBuffer::writer`]; drain once the producer has finished.
pub(super) struct SpillBuffer {
    inner: Mutex<SpillInner>,
}

enum SpillInner {
    Memory(VecDeque<u8>),
    #[cfg(not(miri))]
    Disk(DiskSpill),
}

/// File handles are declared ABOVE `_tempdir`: Rust drops struct fields
/// top-to-bottom, so OS handles close and release their locks before
/// `GuardedTempDir::drop` runs `remove_dir_all` (Windows `PERMISSION_DENIED`
/// otherwise).
#[cfg(not(miri))]
#[allow(clippy::disallowed_types)]
struct DiskSpill {
    writer: SpillFile,
    reader: SpillFile,
    write_pos: u64,
    read_pos: u64,
    #[cfg_attr(not(test), allow(dead_code))]
    #[allow(clippy::disallowed_types)]
    spill_path: std::path::PathBuf,
    _tempdir: GuardedTempDir,
}

/// `SharedOutput`-compatible writer that appends into the buffer.
struct SpillWriter {
    buf: Arc<SpillBuffer>,
}

impl Write for SpillWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.push_bytes(data)?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SpillBuffer {
    pub(super) fn new() -> Self {
        Self {
            inner: Mutex::new(SpillInner::Memory(VecDeque::new())),
        }
    }

    /// A `SharedOutput` sink that appends every written byte to this buffer.
    pub(super) fn writer(self: &Arc<Self>) -> SharedOutput {
        Arc::new(Mutex::new(SpillWriter {
            buf: Arc::clone(self),
        }))
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, SpillInner> {
        self.inner.lock().expect("spill buffer poisoned")
    }

    /// Append bytes, spilling to disk past the threshold and enforcing the
    /// backlog cap on disk.
    pub(super) fn push_bytes(&self, data: &[u8]) -> io::Result<()> {
        let mut inner = self.lock_inner();
        match &mut *inner {
            SpillInner::Memory(vec) => {
                #[cfg(not(miri))]
                let original_len = vec.len();
                vec.extend(data.iter().copied());
                #[cfg(not(miri))]
                if vec.len() > SPILL_THRESHOLD {
                    match DiskSpill::create_from_vec(vec) {
                        Ok(disk) => {
                            *inner = SpillInner::Disk(disk);
                        }
                        Err(e) => {
                            vec.truncate(original_len);
                            return Err(e);
                        }
                    }
                }
                Ok(())
            }
            #[cfg(not(miri))]
            SpillInner::Disk(disk) => disk.write_bytes(data),
        }
    }

    /// Non-blocking read of buffered bytes (up to `buf.len()`). Returns
    /// `Ok(0)` when empty. Truncates the spill file when fully drained.
    pub(super) fn read_into(&self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut inner = self.lock_inner();
        match &mut *inner {
            SpillInner::Memory(vec) => {
                let mut read = 0;
                while read < buf.len() && !vec.is_empty() {
                    buf[read] = vec.pop_front().unwrap();
                    read += 1;
                }
                Ok(read)
            }
            #[cfg(not(miri))]
            SpillInner::Disk(disk) => disk.read_bytes(buf),
        }
    }

    /// Drain all buffered bytes, truncating the spill file when fully
    /// drained.
    pub(super) fn drain_bytes(&self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = self.read_into(&mut chunk)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n]);
        }
        Ok(out)
    }

    /// Drain and require valid UTF-8 (no silent lossy conversion), so
    /// captured stdout can enter `Value::String`.
    pub(super) fn drain_string_strict(&self) -> anyhow::Result<String> {
        let bytes = self.drain_bytes()?;
        String::from_utf8(bytes)
            .map_err(|e| anyhow::anyhow!("captured stdout is not valid UTF-8: {e}"))
    }

    #[cfg(test)]
    #[cfg_attr(miri, allow(dead_code))]
    pub(super) fn is_spilled(&self) -> bool {
        let inner = self.lock_inner();
        match &*inner {
            SpillInner::Memory(_) => false,
            #[cfg(not(miri))]
            SpillInner::Disk(_) => true,
        }
    }

    #[cfg(test)]
    #[cfg_attr(miri, allow(dead_code))]
    #[allow(clippy::disallowed_types)]
    pub(super) fn temp_path(&self) -> Option<std::path::PathBuf> {
        let inner = self.lock_inner();
        match &*inner {
            SpillInner::Memory(_) => None,
            #[cfg(not(miri))]
            SpillInner::Disk(disk) => Some(disk.spill_path.clone()),
        }
    }
}

#[cfg(not(miri))]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
impl DiskSpill {
    fn write_bytes(&mut self, data: &[u8]) -> io::Result<()> {
        self.writer.seek(SeekFrom::Start(self.write_pos))?;
        let new_backlog = (self.write_pos - self.read_pos) + data.len() as u64;
        if new_backlog > MAX_BACKLOG {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!(
                    "capture buffer exceeded maximum active backlog ({} MiB)",
                    MAX_BACKLOG / (1024 * 1024)
                ),
            ));
        }
        self.writer.write_all(data)?;
        self.write_pos += data.len() as u64;
        Ok(())
    }

    fn read_bytes(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.write_pos - self.read_pos;
        if available == 0 {
            return Ok(0);
        }
        self.reader.seek(SeekFrom::Start(self.read_pos))?;
        let to_read = (buf.len() as u64).min(available) as usize;
        let n = self.reader.read(&mut buf[..to_read])?;
        self.read_pos += n as u64;
        if self.read_pos == self.write_pos {
            self.read_pos = 0;
            self.write_pos = 0;
            self.writer.set_len(0).map_err(io::Error::other)?;
            self.writer.seek(SeekFrom::Start(0))?;
            self.reader.seek(SeekFrom::Start(0))?;
        }
        Ok(n)
    }

    fn create_from_vec(vec: &mut VecDeque<u8>) -> io::Result<Self> {
        let tempdir = GuardedPath::tempdir().map_err(io::Error::other)?;
        let root = tempdir.as_guarded_path().clone();
        let resolver =
            PathResolver::new_guarded(root.clone(), root.clone()).map_err(io::Error::other)?;
        let file = root.join("spill.tmp").map_err(io::Error::other)?;
        #[allow(clippy::disallowed_types)]
        let spill_path = file.to_path_buf();

        let mut writer = resolver
            .create_spill_file(&file)
            .map_err(io::Error::other)?;
        vec.make_contiguous();
        let (slice, _) = vec.as_slices();
        let write_pos = vec.len() as u64;
        writer.write_all(slice)?;
        writer.seek(SeekFrom::Start(0))?;
        let reader = resolver.open_spill_file(&file).map_err(io::Error::other)?;
        Ok(Self {
            writer,
            reader,
            write_pos,
            read_pos: 0,
            spill_path,
            _tempdir: tempdir,
        })
    }
}
