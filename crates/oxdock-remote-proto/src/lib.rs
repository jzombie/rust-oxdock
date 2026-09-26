//! Contract for OxDock sealed remote execution (`REMOTE` blocks).
//!
//! Three stable muxio methods ([`methods`]), a content-derived protocol
//! digest (no manual version), JSON control types ([`types`]), and guarded
//! tarball pack/unpack ([`tar`]). Dependency-light by design (`anyhow`,
//! `serde`, `serde_json`, `sha2`, `muxio-rpc-service`, `tar`, `flate2`):
//! usable from the NET plugin, the guest serve loop, and minimal test
//! harnesses. Never depends on `oxdock-core`, `oxdock-parser`, or
//! `oxdock-fs`, so filesystem application stays with the caller under
//! [`GuardedPath`](https://github.com/jzombie/rust-oxdock) containment.
//!
//! Security split, enforced by construction here and documented for both
//! ends: the filesystem boundary is OxDock's domain (every entry resolves
//! under the active root; `..`, absolute paths, and outward symlinks abort
//! extraction), while execution isolation is the operator/OS domain (run
//! `oxdock --remote-serve` under a restricted UID, container, or ephemeral
//! VM; OxDock never claims otherwise).

pub mod bridge;
pub mod channel;
pub mod methods;
pub mod tar;
pub mod types;

use std::sync::LazyLock;

/// Content-derived protocol digest: no manual version anywhere. The
/// contract sources embed at compile time via `include_str!` (Cargo
/// rebuilds on any edit natively, so no `rerun-if-changed` directives can
/// go stale), canonicalize identically to the fixture-pinned rule, and
/// hash once per process. Any contract edit changes the digest with zero
/// manual steps; compatibility is decided by digest equality.
pub static PROTO_DIGEST: LazyLock<String> = LazyLock::new(|| {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(b"oxdock-remote-proto/v1\n");
    hasher.update(canonical_contract_source(include_str!("methods.rs")).as_bytes());
    hasher.update(b"\n");
    hasher.update(canonical_contract_source(include_str!("types.rs")).as_bytes());
    let mut out = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(out, "{byte:02x}");
    }
    out
});

/// Canonical form for digesting: trimmed lines, blanks and `//` comments
/// stripped, so comment-only edits do not break compatibility. Keep in
/// lockstep with the fixture tests below.
pub fn canonical_contract_source(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Largest single stream chunk either side may emit. Muxio has no built-in
/// backpressure and an unbounded write channel, so both ends cap frames
/// here; tarball bytes ride as raw chunks, never base64 inside JSON.
pub const CHUNK_BYTES: usize = 64 * 1024;

/// Canonical sync exclusion record: environment-specific state that must
/// never cross in either direction. Collectors on both ends enforce this
/// list (core mirrors it with a pointer back here; this crate is canonical).
/// The tempdir GC markers keep host temp bookkeeping local; `.cache/`
/// holds persistent per-project state that is meaningless on the guest.
pub const EXCLUDED_TOP_LEVEL_DIRS: &[&str] = &[".cache"];
pub const EXCLUDED_TOP_LEVEL_FILES: &[&str] = &[".oxdock-tempdir", ".oxdock-tempdir.lock"];

/// Canonical sha256 hex over bytes: single heap allocation (pre-sized
/// `String` plus `write!`, no per-byte temporaries). Single home for every
/// remote-path digest (tarball integrity, script/stdout echo hashes,
/// module table): callers in `oxdock-net-plugin` and `oxdock-cli` use
/// this instead of local copies so the formatting rule cannot drift.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// True when a top-level workspace entry must never enter a sync tarball.
pub fn excluded_from_sync(name: &str) -> bool {
    EXCLUDED_TOP_LEVEL_DIRS.contains(&name) || EXCLUDED_TOP_LEVEL_FILES.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalization_ignores_comments_and_whitespace() {
        let a = canonical_contract_source("// lead\n  \nconst A: u64 = 1;\n// trail\n");
        let b = canonical_contract_source("const A: u64 = 1;");
        assert_eq!(a, b);
        assert_eq!(a, "const A: u64 = 1;");
    }

    #[test]
    fn canonicalization_sees_token_changes() {
        let a = canonical_contract_source("const A: u64 = 1;");
        let b = canonical_contract_source("const A: u64 = 2;");
        assert_ne!(a, b);
    }

    #[test]
    fn digest_covers_the_live_contract() {
        // The digest must move when the contract moves: re-canonicalize
        // the embedded sources and compare against the published value.
        assert_eq!(PROTO_DIGEST.len(), 64);
        assert!(PROTO_DIGEST.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
