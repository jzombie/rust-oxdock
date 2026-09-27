//! Channel framing for the three-method protocol.
//!
//! Requests and responses multiplex sections inside single muxio streams;
//! muxio itself correlates calls and routes events, so no stream-level
//! tags exist anymore. Sections use explicit length prefixes
//! ([`pack_prefixed`] / [`split_prefixed`], [`pack_bytes`] /
//! [`split_bytes`]) because chunk boundaries are never significant.
//!
//! Exec request layout (host to guest, one streaming call):
//! `[prefixed descriptor+tar][prefixed script][stdin raw until end]`.
//! Exec response (guest to host, one response stream): tagged chunks,
//! one tag byte per response item (`TAG_STDOUT`, `TAG_STDERR`,
//! `TAG_RESULT` + prefixed result); result bytes accumulate until the
//! response stream ends.

/// Guest to host: raw stdout bytes follow the tag in this item.
pub const TAG_STDOUT: u8 = 6;
/// Guest to host: raw stderr bytes follow the tag in this item.
pub const TAG_STDERR: u8 = 7;
/// Guest to host: length-prefixed [`ExecResult`](crate::types::ExecResult)
/// JSON followed by raw push tar bytes. Accumulate across items until the
/// response stream ends, then split.
pub const TAG_RESULT: u8 = 8;

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

/// Pack a length-prefixed byte section (no JSON): `[u32 LE len][bytes]`.
pub fn pack_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

/// Split a length-prefixed byte section. `None` while incomplete.
pub fn split_bytes(buffer: &[u8]) -> Option<(&[u8], &[u8])> {
    split_prefixed(buffer)
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
        let tags = [TAG_STDOUT, TAG_STDERR, TAG_RESULT];
        let mut seen = std::collections::HashSet::new();
        for tag in tags {
            assert!(seen.insert(tag), "duplicate channel tag {tag}");
        }
    }

    #[test]
    fn bytes_sections_round_trip() {
        let packed = pack_bytes(b"script text");
        let (section, rest) = split_bytes(&packed).unwrap();
        assert_eq!(section, b"script text");
        assert!(rest.is_empty());
        assert!(split_bytes(&packed[..2]).is_none());
    }
}
