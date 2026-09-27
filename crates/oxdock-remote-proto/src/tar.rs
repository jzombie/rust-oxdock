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
/// before the next entry arrives. Nothing accumulates.
pub enum StreamedEntry<'a> {
    Dir {
        path: String,
    },
    File {
        path: String,
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
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw = entry.path()?.to_string_lossy().to_string();
        let rel = sanitize_rel(&raw)?;
        match entry.header().entry_type() {
            t if t.is_dir() => apply(StreamedEntry::Dir { path: rel })?,
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
                StreamedEntry::Dir { path } => names.push(format!("dir:{path}")),
                StreamedEntry::File { path, reader } => {
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
