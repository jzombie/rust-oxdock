//! Guarded tarball pack/unpack for workspace sync. Bytes only: this crate
//! never touches the filesystem, so both ends apply entries through their
//! own `GuardedPath` containment. Every path is a relative forward-slash
//! string; anything else fails closed before any byte is written.

use anyhow::{Result, bail};

use crate::types::TarDescriptor;

/// One result file: relative path plus bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// One result directory: relative path. Symlinks travel separately so the
/// caller can resolve targets against its staging root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSymlink {
    pub path: String,
    pub target: String,
}

/// Unpacked tarball: files, directories, and symlinks with targets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Unpacked {
    pub files: Vec<SyncFile>,
    pub dirs: Vec<String>,
    pub symlinks: Vec<SyncSymlink>,
}

/// Canonicalize one tarball member path: forward slashes, no absolute
/// paths, no `..` above the root. `.` and empty components (from `./x`
/// and `a//b` spellings) normalize away; `..` pops when depth allows and
/// fails closed at root. A single bad entry aborts the whole unpack
/// (skip-and-continue would silently desync the deletion manifests).
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

/// Pack collected workspace entries to gzip bytes plus a descriptor.
/// `files` and `dirs` must already be sanitized relative paths.
pub fn pack(files: &[SyncFile], dirs: &[String]) -> Result<(Vec<u8>, TarDescriptor)> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
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
    let mut sorted_files = files.to_vec();
    sorted_files.sort_by(|a, b| a.path.cmp(&b.path));
    for file in &sorted_files {
        let rel = sanitize_rel(&file.path)?;
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(file.bytes.len() as u64);
        header.set_cksum();
        builder.append_data(&mut header, &rel, file.bytes.as_slice())?;
    }
    let encoder = builder.into_inner()?;
    let bytes = encoder.finish()?;
    let descriptor = TarDescriptor {
        sha256: sha256_hex(&bytes),
        entry_count: (sorted_files.len() + sorted_dirs.len()) as u64,
    };
    Ok((bytes, descriptor))
}

/// Verify sha256 hex over `bytes` against `expected`, failing closed before
/// any unpack. Both ends call this before touching disk.
pub fn verify_sha256(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = sha256_hex(bytes);
    if actual != expected {
        bail!("tarball sha256 mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

/// Unpack gzip bytes to entries, sanitizing every path. Symlink and hard
/// link members are recorded with targets, never followed: the caller
/// resolves each target against its staging root and aborts when it points
/// outside. Malformed archives kill the session with an error, never a
/// panic: all I/O errors convert to `anyhow` here.
pub fn unpack(bytes: &[u8]) -> Result<Unpacked> {
    use flate2::read::GzDecoder;
    let decoder = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let mut out = Unpacked::default();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw = entry.path()?.to_string_lossy().to_string();
        let rel = sanitize_rel(&raw)?;
        match entry.header().entry_type() {
            t if t.is_dir() => out.dirs.push(rel),
            t if t.is_symlink() || t.is_hard_link() => {
                let target = entry
                    .link_name()?
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                out.symlinks.push(SyncSymlink { path: rel, target });
            }
            t if t.is_file() => {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut bytes)?;
                out.files.push(SyncFile { path: rel, bytes });
            }
            other => bail!("tarball entry has unsupported type {other:?}: {raw:?}"),
        }
    }
    Ok(out)
}

/// Manifest diff: entries present in the input tarball but absent from the
/// return tarball. Sorted deepest first so directory pruning removes
/// children before parents. Plain extraction cannot delete, so this diff is
/// mandatory, not an optimization.
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

use super::sha256_hex;

#[cfg(test)]
mod tests {
    use super::*;

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
    fn pack_unpack_round_trips() {
        let files = vec![
            SyncFile {
                path: "b.txt".to_string(),
                bytes: b"bee".to_vec(),
            },
            SyncFile {
                path: "a/nested.txt".to_string(),
                bytes: vec![0, 1, 2, 255],
            },
        ];
        let dirs = vec!["a".to_string(), "empty".to_string()];
        let (bytes, descriptor) = pack(&files, &dirs).unwrap();
        verify_sha256(&bytes, &descriptor.sha256).unwrap();
        assert_eq!(descriptor.entry_count, 4);
        let unpacked = unpack(&bytes).unwrap();
        assert_eq!(unpacked.files, {
            let mut sorted = files.clone();
            sorted.sort_by(|a, b| a.path.cmp(&b.path));
            sorted
        });
        assert_eq!(unpacked.dirs, vec!["a".to_string(), "empty".to_string()]);
        assert!(unpacked.symlinks.is_empty());
    }

    #[test]
    fn verify_rejects_tampered_bytes() {
        let files = vec![SyncFile {
            path: "a.txt".to_string(),
            bytes: b"data".to_vec(),
        }];
        let (mut bytes, descriptor) = pack(&files, &[]).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        assert!(verify_sha256(&bytes, &descriptor.sha256).is_err());
    }

    #[test]
    fn deletion_diff_is_deepest_first() {
        let (files, dirs) = deletion_diff(
            &["gone.txt".to_string(), "sub/keep.txt".to_string()],
            &["sub".to_string(), "sub/deep".to_string(), "gone-dir".to_string()],
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
