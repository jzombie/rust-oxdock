//! Small shared leaf utilities for OxDock.
//!
//! Dependency-light helpers used by more than one crate live here, so a
//! single helper never costs a new crate: new one-off utilities land as
//! modules. The crate stays a leaf (third-party deps only, never
//! workspace crates), so anything can depend on it without a cycle.
//!
//! Current residents: streaming tarball pack/unpack ([`tar`], shared by
//! local archive commands and remote declared transfers alike) and the
//! canonical SHA-256 hex digest ([`sha256_hex`]).
//!
//! Handles, never bytes: the tar helpers take caller-held readers and
//! stage archives in spill files, so memory stays flat at chunk size
//! whether moving 1 MiB or 100 GiB. This crate never touches the
//! filesystem, so every consumer applies entries through its own
//! containment (core through `GuardedPath`, the remote ends through
//! their own roots).
//!
//! Split out of `oxdock-remote-proto` so local jobs share one
//! implementation without depending on the remote wire contract: the
//! proto crate keeps only the muxio methods, handshake types, and
//! protocol digest.

pub mod tar;

/// Canonical lowercase hex over digest bytes: single heap allocation
/// (pre-sized `String` plus `write!`, no per-byte temporaries). Single
/// home for the encoding half of every digest, so the formatting rule
/// cannot drift between streaming hashers, one-shot digests, and tests.
pub fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Canonical sha256 hex over raw bytes: hash, then [`hex_encode`].
/// Single home for every digest over raw bytes (tarball integrity,
/// script/stdout echo hashes, assert `--hash` comparisons). Callers
/// holding an already-finalized digest use [`hex_encode`] directly.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex_encode(Sha256::digest(bytes))
}
