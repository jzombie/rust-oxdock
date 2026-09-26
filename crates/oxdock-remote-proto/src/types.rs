//! Handshake and exec control types. All control frames are `serde_json`:
//! human readable in logs, and version skew surfaces as legible errors.
//! Tarball bytes never ride inside JSON: they stream as raw muxio byte
//! chunks with length plus sha256 carried here.

use serde::{Deserialize, Serialize};

/// Client hello for [`HANDSHAKE`](crate::methods::HANDSHAKE).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeHello {
    /// Content digest of the framing contract both sides compiled.
    pub proto_digest: String,
    /// OxDock release version, diagnostics only (never gates).
    pub oxdock_version: String,
    /// Sorted registered module names, diagnostics only (never gates).
    /// Data, not cfg flags: any downstream library's modules appear here
    /// with no per-feature code, and mismatches name the difference.
    pub modules: Vec<String>,
    /// sha256 over sorted module names plus function sets: the language
    /// surface identity. Agreement by construction: the host never invokes
    /// what the guest lacks.
    pub module_hash: String,
}

/// Server verdict for [`HANDSHAKE`](crate::methods::HANDSHAKE). Reject
/// unless digests are equal AND module hashes are equal; the error names
/// both tuples plus remediation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeVerdict {
    pub accepted: bool,
    pub proto_digest: String,
    pub oxdock_version: String,
    pub modules: Vec<String>,
    pub module_hash: String,
    pub error: Option<String>,
}

/// Header opening an [`EXEC`](crate::methods::EXEC) stream. Carries the
/// inventory target plus the digest: the guest re-checks identity per
/// exec, so drift between blocks fails here even if the handshake passed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecHeader {
    pub target: String,
    pub proto_digest: String,
}

/// Terminal result closing an [`EXEC`](crate::methods::EXEC) stream.
/// Variable outputs do not exist: the guest scope unwinds and only files
/// plus stdio cross. `result_tar_sha256` covers the result bytes that
/// follow; empty when the guest produced no workspace changes.
///
/// End-to-end integrity: muxio frames detect structural corruption but
/// carry no checksum, so the guest echoes hashes over the bytes AS
/// RECEIVED and AS PRODUCED (post-framing). The host verifies both before
/// applying anything: `script_sha256` must equal the shipped script text,
/// `stdout_sha256` must equal the received stdout bytes. A mismatch is a
/// transport error (host wins). Over ssh this never fires (the tunnel
/// MACs everything); over raw pipes it catches random corruption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResult {
    pub ok: bool,
    pub error: Option<String>,
    pub result_tar_sha256: String,
    pub script_sha256: String,
    pub stdout_sha256: String,
}

/// Integrity descriptor for one tarball riding the stream as raw chunks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TarDescriptor {
    /// Hex sha256 over the exact gzip bytes the receiver must verify
    /// before unpacking.
    pub sha256: String,
    /// Uncompressed entry count, for progress and sanity caps.
    pub entry_count: u64,
}

/// Frame tag for stdout versus stderr byte chunks inside `exec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StdioKind {
    Stdout,
    Stderr,
    Stdin,
}
