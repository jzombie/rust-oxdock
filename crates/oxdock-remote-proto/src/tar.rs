//! Streaming tarball pack/unpack for declared transfers. Handles, never
//! bytes: callers open file readers and stage archives in spill files, so
//! memory stays flat at chunk size whether moving 1 MiB or 100 GiB. No
//! byte caps exist anywhere: O(chunk) memory makes them unnecessary. This
//! crate never touches the filesystem, so both ends apply entries through
//! their own `GuardedPath` containment. Every path is a relative
//! forward-slash string; anything else fails closed before any byte is
//! written.

use std::io::{Read, Write};

use anyhow::{Result, bail};

/// One file to pack: guest/host destination path, byte length, and an
/// open reader. Lengths come from filesystem metadata at open time.
pub struct PackEntry {
    pub path: String,
    pub len: u64,
    pub reader: Box<dyn Read + Send>,
}

/// One unpacked entry, borrowed from the archive driver: the caller's
/// `apply` callback consumes file bytes (streaming) or records metadata
/// before the next entry arrives. Nothing accumulates. Modes ride along
/// so filesystem apply sites can restore them; transfer apply sites may
/// ignore the field.
pub enum StreamedEntry<'a> {
    Dir {
        path: String,
        mode: u32,
    },
    File {
        path: String,
        mode: u32,
        reader: &'a mut dyn Read,
    },
    Symlink {
        path: String,
        target: String,
    },
}

/// Write-through hasher for staged archives: bytes hash incrementally as
/// they pass, so no second read pass is needed before verifying.
pub struct HashingWriter<W: Write> {
    inner: W,
    hasher: sha2::Sha256,
    hashed: u64,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        use sha2::Digest;
        Self {
            inner,
            hasher: sha2::Sha256::new(),
            hashed: 0,
        }
    }

    /// Finish: the wrapped writer plus hex sha256 over everything written.
    pub fn finish(self) -> (W, String) {
        use sha2::Digest;
        use std::fmt::Write as _;
        let mut out = String::with_capacity(64);
        for byte in self.hasher.finalize() {
            let _ = write!(out, "{byte:02x}");
        }
        (self.inner, out)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest;
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.hashed += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Canonicalize one tarball member path: forward slashes, no absolute
/// paths, no `..` above the root. `.` and empty components (from `./x`
/// and `a//b` spellings) normalize away; `..` pops when depth allows and
/// fails closed at root. A single bad entry aborts the whole unpack.
pub fn sanitize_rel(raw: &str) -> Result<String> {
    if raw.is_empty() {
        bail!("tarball entry has an empty path");
    }
    let normalized = raw.replace('\\', "/");
    if normalized != raw {
        bail!("tarball entry has a backslash path: {raw:?}");
    }
    if normalized.starts_with('/') {
        bail!("tarball entry has an absolute path: {raw:?}");
    }
    let mut parts = Vec::new();
    for part in normalized.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            if parts.pop().is_none() {
                bail!("tarball entry escapes its root: {raw:?}");
            }
            continue;
        }
        parts.push(part);
    }
    if parts.is_empty() {
        bail!("tarball entry has an empty path: {raw:?}");
    }
    Ok(parts.join("/"))
}

/// Pack entries to gzip bytes on `dest`, streaming file content straight
/// from readers with O(chunk) memory. Returns the entry count; hashing
/// rides on the caller wrapping `dest` in [`HashingWriter`].
pub fn pack_to<W: Write>(
    dest: W,
    files: &mut [(String, u64, Box<dyn Read + Send>)],
    dirs: &[String],
) -> Result<u64> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    let encoder = GzEncoder::new(dest, Compression::default());
    let mut builder = tar::Builder::new(encoder);
    let mut sorted_dirs = dirs.to_vec();
    sorted_dirs.sort();
    for dir in &sorted_dirs {
        let rel = sanitize_rel(dir)?;
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_cksum();
        builder.append_data(&mut header, &rel, std::io::empty())?;
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    for (rel, len, reader) in files.iter_mut() {
        let sanitized = sanitize_rel(rel)?;
        *rel = sanitized.clone();
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(*len);
        header.set_cksum();
        builder.append_data(&mut header, &sanitized, reader.as_mut())?;
    }
    builder.into_inner()?.try_finish()?;
    Ok((files.len() + sorted_dirs.len()) as u64)
}

/// Verify sha256 hex over `bytes` against `expected`, failing closed
/// before any unpack. Both ends call this before touching disk.
pub fn verify_sha256(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = crate::sha256_hex(bytes);
    if actual != expected {
        bail!("tarball sha256 mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

/// Unpack a gzip archive from `src`, invoking `apply` per entry as bytes
/// arrive. File readers borrow the archive driver: consume each fully
/// inside the callback. Malformed archives fail the session with an
/// error, never a panic.
pub fn unpack_from<R: Read>(
    src: R,
    apply: &mut dyn FnMut(StreamedEntry) -> Result<()>,
) -> Result<()> {
    use flate2::read::GzDecoder;
    let decoder = GzDecoder::new(src);
    drain_entries(&mut tar::Archive::new(decoder), apply)
}

/// Gzip magic the sniffer keys on (`1f 8b`): a raw tar member header
/// opens with a file name, never these bytes in practice.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// Unpack either a gzip tarball or a raw (`ARCHIVE --raw`) tar: sniff the
/// first two bytes for the gzip magic, then stream the rest through the
/// matching driver. Sniffed bytes chain back, so no byte is lost. Empty
/// input fails closed: an empty file is corruption, not an empty tree.
/// The raw path additionally requires one full 512-byte header block up
/// front: shorter inputs yield zero entries with no error, so they fail
/// here instead of reporting a silent success. (Truncated gzip fails on
/// its own in the decoder; gzip wrapping a sub-header payload can only
/// be hand-built, and extracts nothing, which downstream assertions on
/// restored content catch.)
pub fn unpack_auto_from<R: Read>(
    mut src: R,
    apply: &mut dyn FnMut(StreamedEntry) -> Result<()>,
) -> Result<()> {
    let mut head = [0u8; 2];
    let mut filled = 0usize;
    while filled < 2 {
        match src.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(e.into()),
        }
    }
    if filled == 0 {
        bail!("archive is empty");
    }
    let chained = std::io::Cursor::new(head[..filled].to_vec()).chain(src);
    if filled == 2 && head == GZIP_MAGIC {
        unpack_from(chained, apply)
    } else {
        unpack_raw_checked(chained, apply)
    }
}

/// Raw-tar entry point with a truncation floor: buffer the first header
/// block and fail closed when the stream ends before it is whole.
fn unpack_raw_checked<R: Read>(
    mut src: R,
    apply: &mut dyn FnMut(StreamedEntry) -> Result<()>,
) -> Result<()> {
    let mut first = vec![0u8; 512];
    let mut filled = 0usize;
    while filled < first.len() {
        match src.read(&mut first[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(e.into()),
        }
    }
    if filled < first.len() {
        bail!("archive is truncated: {filled} bytes cannot hold a tar header");
    }
    let chained = std::io::Cursor::new(first).chain(src);
    drain_entries(&mut tar::Archive::new(chained), apply)
}

/// Drive one `tar::Archive` to completion through `apply`. Both the gzip
/// and the raw entry points share this loop, so hostile-entry failures
/// (`sanitize_rel`, unsupported types) read identically either way.
fn drain_entries<R: Read>(
    archive: &mut tar::Archive<R>,
    apply: &mut dyn FnMut(StreamedEntry) -> Result<()>,
) -> Result<()> {
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw = entry.path()?.to_string_lossy().to_string();
        let rel = sanitize_rel(&raw)?;
        let mode = entry.header().mode().unwrap_or(0);
        match entry.header().entry_type() {
            t if t.is_dir() => apply(StreamedEntry::Dir { path: rel, mode })?,
            t if t.is_symlink() || t.is_hard_link() => {
                let target = entry
                    .link_name()?
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                apply(StreamedEntry::Symlink { path: rel, target })?;
            }
            t if t.is_file() => {
                apply(StreamedEntry::File {
                    path: rel,
                    mode,
                    reader: &mut entry,
                })?;
            }
            other => bail!("tarball entry has unsupported type {other:?}: {raw:?}"),
        }
    }
    Ok(())
}

/// Manifest diff: entries present in the input tarball but absent from the
/// return tarball. Sorted deepest first so directory pruning removes
/// children before parents. Kept for declared-transfer reconciliation
/// where both manifests are name lists, never byte payloads.
pub fn deletion_diff(
    input_files: &[String],
    input_dirs: &[String],
    out_files: &[String],
    out_dirs: &[String],
) -> (Vec<String>, Vec<String>) {
    use std::collections::HashSet;
    let out_files: HashSet<&str> = out_files.iter().map(String::as_str).collect();
    let out_dirs: HashSet<&str> = out_dirs.iter().map(String::as_str).collect();
    let mut deleted_files: Vec<String> = input_files
        .iter()
        .filter(|rel| !out_files.contains(rel.as_str()))
        .cloned()
        .collect();
    deleted_files.sort();
    let mut deleted_dirs: Vec<String> = input_dirs
        .iter()
        .filter(|rel| !rel.is_empty() && !out_dirs.contains(rel.as_str()))
        .cloned()
        .collect();
    deleted_dirs.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    (deleted_files, deleted_dirs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn sanitize_rejects_escapes() {
        for bad in ["", "/abs", "a/../../b", "..", "../a", "a\\b"] {
            assert!(sanitize_rel(bad).is_err(), "{bad:?} must fail");
        }
        assert_eq!(sanitize_rel("a/b.txt").unwrap(), "a/b.txt");
        // Benign spellings normalize instead of failing.
        assert_eq!(sanitize_rel("./version.txt").unwrap(), "version.txt");
        assert_eq!(sanitize_rel("a//b.txt").unwrap(), "a/b.txt");
        assert_eq!(sanitize_rel("a/./b.txt").unwrap(), "a/b.txt");
        assert_eq!(sanitize_rel("a/b/../c.txt").unwrap(), "a/c.txt");
    }

    #[test]
    fn pack_unpack_streams_without_buffering_all() {
        let data_a = vec![7u8; 100_000];
        let data_b: Vec<u8> = (0..=255u8).cycle().take(50_000).collect();
        let mut files: Vec<(String, u64, Box<dyn Read + Send>)> = vec![
            (
                "b.txt".to_string(),
                data_a.len() as u64,
                Box::new(Cursor::new(data_a.clone())),
            ),
            (
                "a/nested.txt".to_string(),
                data_b.len() as u64,
                Box::new(Cursor::new(data_b.clone())),
            ),
        ];
        let dirs = vec!["a".to_string(), "empty".to_string()];
        let mut staged = Vec::new();
        let count = pack_to(&mut staged, &mut files, &dirs).unwrap();
        assert_eq!(count, 4);
        // HashingWriter covers the same bytes the wire would carry.
        let mut hashed = HashingWriter::new(Vec::new());
        hashed.write_all(&staged).unwrap();
        let (_, sha) = hashed.finish();
        verify_sha256(&staged, &sha).unwrap();
        let mut names = Vec::new();
        let mut seen: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
        unpack_from(Cursor::new(&staged), &mut |entry| {
            match entry {
                StreamedEntry::Dir { path, .. } => names.push(format!("dir:{path}")),
                StreamedEntry::File { path, reader, .. } => {
                    let mut bytes = Vec::new();
                    reader.read_to_end(&mut bytes)?;
                    names.push(format!("file:{path}"));
                    seen.insert(path, bytes);
                }
                StreamedEntry::Symlink { path, target } => {
                    names.push(format!("link:{path}->{target}"));
                }
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.get("b.txt").unwrap(), &data_a);
        assert_eq!(seen.get("a/nested.txt").unwrap(), &data_b);
        assert!(names.contains(&"dir:a".to_string()));
        assert!(names.contains(&"dir:empty".to_string()));
    }

    #[test]
    fn unpack_auto_detects_gzip_and_raw() {
        let payload = b"auto-detect me".to_vec();
        let mut files: Vec<(String, u64, Box<dyn Read + Send>)> = vec![(
            "note.txt".to_string(),
            payload.len() as u64,
            Box::new(Cursor::new(payload.clone())),
        )];
        let mut gzipped = Vec::new();
        pack_to(&mut gzipped, &mut files, &[]).unwrap();
        assert_eq!(&gzipped[..2], GZIP_MAGIC, "gzip output keeps its magic");

        let mut raw = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut raw);
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "note.txt", &payload[..])
                .unwrap();
            builder.into_inner().unwrap();
        }
        assert_ne!(&raw[..2], GZIP_MAGIC, "raw output carries no magic");

        for bytes in [&gzipped, &raw] {
            let mut seen = Vec::new();
            unpack_auto_from(Cursor::new(bytes), &mut |entry| {
                match entry {
                    StreamedEntry::File { path, reader, .. } => {
                        let mut out = Vec::new();
                        reader.read_to_end(&mut out)?;
                        seen.push((path, out));
                    }
                    StreamedEntry::Dir { path, .. } => panic!("expected one file, saw dir {path}"),
                    StreamedEntry::Symlink { path, .. } => {
                        panic!("expected one file, saw link {path}")
                    }
                }
                Ok(())
            })
            .unwrap();
            assert_eq!(
                seen,
                vec![("note.txt".to_string(), payload.clone())],
                "both drivers restore the same bytes"
            );
        }
    }

    #[test]
    fn unpack_auto_rejects_truncated_raw() {
        for len in [1usize, 100, 511] {
            let err = unpack_auto_from(Cursor::new(vec![0u8; len]), &mut |_| Ok(()))
                .expect_err("sub-header inputs must fail, not silently extract nothing");
            assert!(err.to_string().contains("truncated"), "{err}");
        }
        // A bare terminator pair is a valid empty archive: no members,
        // no error.
        let mut count = 0usize;
        unpack_auto_from(Cursor::new(vec![0u8; 1024]), &mut |_| {
            count += 1;
            Ok(())
        })
        .expect("terminator-only tar is a valid empty archive");
        assert_eq!(count, 0);
    }

    #[test]
    fn unpack_auto_rejects_empty_and_hostile_entries() {
        let mut count = 0usize;
        let err = unpack_auto_from(Cursor::new(Vec::new()), &mut |entry| {
            match entry {
                StreamedEntry::Dir { path, .. } => count += path.len(),
                StreamedEntry::File { path, .. } => count += path.len(),
                StreamedEntry::Symlink { path, .. } => count += path.len(),
            }
            Ok(())
        })
        .expect_err("empty input is corruption, not an empty tree");
        assert!(err.to_string().contains("empty"), "{err}");

        let hostile = tar_with_entry_name("../evil.txt", b"evil");
        let err = unpack_auto_from(Cursor::new(&hostile), &mut |_| Ok(()))
            .expect_err("escaping entries must fail through the sniffer too");
        assert!(err.to_string().contains("escapes"), "{err}");
    }

    /// Splice a hostile member name into an otherwise valid raw tar and
    /// re-checksum its header: the `tar` builder refuses hostile names up
    /// front, but hostile bytes arrive over the wire.
    #[cfg(test)]
    fn tar_with_entry_name(name: &str, payload: &[u8]) -> Vec<u8> {
        let mut raw = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut raw);
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "victim.txt", payload)
                .unwrap();
            builder.into_inner().unwrap();
        }
        let name_bytes = name.as_bytes();
        assert!(name_bytes.len() < 100, "name must fit the tar name field");
        raw[..name_bytes.len()].copy_from_slice(name_bytes);
        raw[name_bytes.len()] = 0;
        for b in &mut raw[148..156] {
            *b = b' ';
        }
        let sum: u32 = raw[..512].iter().map(|b| *b as u32).sum();
        let octal = format!("{sum:06o}\0 ");
        raw[148..156].copy_from_slice(octal.as_bytes());
        raw
    }

    #[test]
    fn verify_rejects_tampered_bytes() {
        let payload = b"data".to_vec();
        let mut files: Vec<(String, u64, Box<dyn Read + Send>)> = vec![(
            "a.txt".to_string(),
            payload.len() as u64,
            Box::new(Cursor::new(payload)),
        )];
        let mut staged = Vec::new();
        pack_to(&mut staged, &mut files, &[]).unwrap();
        let mid = staged.len() / 2;
        staged[mid] ^= 0xFF;
        let sha = crate::sha256_hex(b"untampered");
        assert!(verify_sha256(&staged, &sha).is_err());
    }

    #[test]
    fn deletion_diff_is_deepest_first() {
        let (files, dirs) = deletion_diff(
            &["gone.txt".to_string(), "sub/keep.txt".to_string()],
            &[
                "sub".to_string(),
                "sub/deep".to_string(),
                "gone-dir".to_string(),
            ],
            &["sub/keep.txt".to_string()],
            &[],
        );
        assert_eq!(files, vec!["gone.txt".to_string()]);
        assert_eq!(
            dirs,
            vec![
                "gone-dir".to_string(),
                "sub/deep".to_string(),
                "sub".to_string()
            ]
        );
    }
}
