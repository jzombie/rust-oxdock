//! Channel tags and stream helpers for the three-method protocol.
//!
//! Method IDs stay at three (`handshake`, `exec`, `close`) by design: no
//! per-command methods may ever be added. Channels multiplexed inside
//! those methods are framing, not commands, and are distinguished by the
//! first payload byte of each stream. Host opens host-to-guest streams,
//! guest opens guest-to-host streams; both sides dispatch on
//! `(method_id, tag)`.
//!
//! Chunk-boundary rule, load-bearing: muxio may re-chunk payloads in
//! flight, so receivers must NEVER rely on chunk boundaries. Byte flows
//! without message structure (script text, tar bytes after their
//! descriptor, stdio) accumulate until `End`. Structured prefixes use
//! explicit length prefixes ([`pack_prefixed`] / [`split_prefixed`]).

/// Host to guest: [`HandshakeHello`](crate::types::HandshakeHello) JSON,
/// accumulated until `End`. Method `handshake`.
pub const TAG_HELLO: u8 = 1;
/// Guest to host: [`HandshakeVerdict`](crate::types::HandshakeVerdict)
/// JSON, accumulated until `End`. Method `handshake`.
pub const TAG_VERDICT: u8 = 2;
/// Host to guest: DSL source text, accumulated until `End`. Method `exec`.
pub const TAG_SCRIPT: u8 = 3;
/// Host to guest: length-prefixed [`TarDescriptor`](crate::types::TarDescriptor)
/// JSON followed by raw tar bytes, until `End`. Entry names are
/// guest-destination paths for declared `--from-host` transfers.
/// Always opened, even with an empty tarball, so the guest never waits on
/// a channel that will not come. Method `exec`.
pub const TAG_FETCH: u8 = 4;
/// Host to guest: raw stdin bytes until `End` (EOF). Method `exec`.
pub const TAG_STDIN: u8 = 5;
/// Guest to host: raw stdout bytes until `End`. Method `exec`.
pub const TAG_STDOUT: u8 = 6;
/// Guest to host: raw stderr bytes until `End`. Method `exec`.
pub const TAG_STDERR: u8 = 7;
/// Guest to host: length-prefixed [`ExecResult`](crate::types::ExecResult)
/// JSON followed by raw push tar bytes, until `End`. Entry names are
/// guest-source paths for declared `--to-host` transfers; the host maps
/// them to host destinations through the declaration list and rejects
/// undeclared entries. The result `result_tar_sha256` covers the trailing
/// tar bytes. Always opened, even empty. Method `exec`.
pub const TAG_RESULT: u8 = 8;
/// Either side to the other: session teardown. Method `close`.
pub const TAG_CLOSE: u8 = 9;

/// Pack a length-prefixed frame: `[u32 LE json_len][json][rest]`.
/// Receivers split with [`split_prefixed`] regardless of chunking.
pub fn pack_prefixed(json: &[u8], rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + json.len() + rest.len());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(json);
    out.extend_from_slice(rest);
    out
}

/// Split a length-prefixed frame. Returns the JSON slice plus the
/// remainder, or `None` when fewer than the full frame arrived yet
/// (callers buffer and retry; never treat a short read as an error).
pub fn split_prefixed(buffer: &[u8]) -> Option<(&[u8], &[u8])> {
    if buffer.len() < 4 {
        return None;
    }
    let len = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    if buffer.len() < 4 + len {
        return None;
    }
    Some((&buffer[4..4 + len], &buffer[4 + len..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixed_round_trips() {
        let packed = pack_prefixed(b"{}", b"bytes");
        let (json, rest) = split_prefixed(&packed).unwrap();
        assert_eq!(json, b"{}");
        assert_eq!(rest, b"bytes");
    }

    #[test]
    fn prefixed_short_reads_wait() {
        let packed = pack_prefixed(b"{}", b"bytes");
        assert!(split_prefixed(&packed[..3]).is_none());
        assert!(split_prefixed(&packed[..5]).is_none());
    }

    #[test]
    fn tags_are_distinct() {
        let tags = [
            TAG_HELLO,
            TAG_VERDICT,
            TAG_SCRIPT,
            TAG_FETCH,
            TAG_STDIN,
            TAG_STDOUT,
            TAG_STDERR,
            TAG_RESULT,
            TAG_CLOSE,
        ];
        let mut seen = std::collections::HashSet::new();
        for tag in tags {
            assert!(seen.insert(tag), "duplicate channel tag {tag}");
        }
    }
}
