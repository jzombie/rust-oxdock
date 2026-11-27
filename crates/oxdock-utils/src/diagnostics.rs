//! Tarball failures behind single constructors with stable keys.
//!
//! This module owns every tar template and key (`tar.*`). It builds on
//! the generic [`oxdock_diagnostics::Diagnostic`] transport but owns its
//! wording: changing tar text happens here, never at the call sites.
//! Call sites pass structured data only, never `format!` strings.
//! Empty-archive failures are parameterless (the wording is fixed);
//! path failures take the raw entry name; the digest mismatch takes
//! both hex strings; the truncation takes the byte count; unsupported
//! types take the entry type for `Debug` rendering here.

use oxdock_diagnostics::Diagnostic;

/// Structured field keys. Single spelling for the whole module:
/// constructors reference these, never literals, so a key rename is
/// one edit.
const FIELD_RAW: &str = "raw";
const FIELD_REASON: &str = "reason";
const FIELD_EXPECTED: &str = "expected";
const FIELD_ACTUAL: &str = "actual";
const FIELD_FILLED: &str = "filled";
const FIELD_OTHER: &str = "other";

/// Tarball member with an empty path. Parameterless: the wording is
/// fixed.
pub fn empty_path() -> Diagnostic {
    Diagnostic::new(
        "tar.sanitize.empty_path",
        "tarball entry has an empty path".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Tarball member with a backslash path. Takes the raw entry name;
/// `Debug` quoting lives here.
pub fn backslash_path(raw: &str) -> Diagnostic {
    let message = format!("tarball entry has a backslash path: {raw:?}");
    Diagnostic::new(
        "tar.sanitize.backslash_path",
        message,
        Diagnostic::make_fields(&[(FIELD_RAW, raw)]),
    )
}

/// Tarball member with an absolute path. Takes the raw entry name;
/// `Debug` quoting lives here.
pub fn absolute_path(raw: &str) -> Diagnostic {
    let message = format!("tarball entry has an absolute path: {raw:?}");
    Diagnostic::new(
        "tar.sanitize.absolute_path",
        message,
        Diagnostic::make_fields(&[(FIELD_RAW, raw)]),
    )
}

/// Tarball member escaping its root. Takes the raw entry name;
/// `Debug` quoting lives here.
pub fn escapes_root(raw: &str) -> Diagnostic {
    let message = format!("tarball entry escapes its root: {raw:?}");
    Diagnostic::new(
        "tar.sanitize.escapes_root",
        message,
        Diagnostic::make_fields(&[(FIELD_RAW, raw)]),
    )
}

/// Tarball member normalizing to an empty path. Takes the raw entry
/// name; `Debug` quoting lives here.
pub fn empty_path_normalized(raw: &str) -> Diagnostic {
    let message = format!("tarball entry has an empty path: {raw:?}");
    Diagnostic::new(
        "tar.sanitize.empty_path_normalized",
        message,
        Diagnostic::make_fields(&[(FIELD_RAW, raw)]),
    )
}

/// Sha256 mismatch over staged bytes. Takes both hex digests;
/// rendering lives here.
pub fn sha256_mismatch(expected: &str, actual: &str) -> Diagnostic {
    let message = format!("tarball sha256 mismatch: expected {expected}, got {actual}");
    Diagnostic::new(
        "tar.verify.sha256_mismatch",
        message,
        Diagnostic::make_fields(&[(FIELD_EXPECTED, expected), (FIELD_ACTUAL, actual)]),
    )
}

/// Empty archive input. Parameterless: the wording is fixed.
pub fn archive_empty() -> Diagnostic {
    Diagnostic::new(
        "tar.unpack.archive_empty",
        "archive is empty".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Truncated raw archive. Takes the buffered byte count; rendering
/// lives here.
pub fn archive_truncated(filled: usize) -> Diagnostic {
    let message = format!("archive is truncated: {filled} bytes cannot hold a tar header");
    Diagnostic::new(
        "tar.unpack.archive_truncated",
        message,
        Diagnostic::make_fields(&[(FIELD_FILLED, &filled.to_string())]),
    )
}

/// Unsupported tar entry type. Takes the entry type plus the raw
/// entry name; `Debug` rendering lives here.
pub fn unsupported_type(other: &impl std::fmt::Debug, raw: &str) -> Diagnostic {
    let other_text = format!("{other:?}");
    let message = format!("tarball entry has unsupported type {other_text}: {raw:?}");
    Diagnostic::new(
        "tar.unpack.unsupported_type",
        message,
        Diagnostic::make_fields(&[(FIELD_OTHER, other_text.as_str()), (FIELD_RAW, raw)]),
    )
}

/// Tarball pack I/O failure. Takes the underlying failure;
/// `{reason:#}` formatting lives here so call sites stay
/// `format!`-free. Message is the reason verbatim (like
/// `build.operation.failed`) so reclaimed chains render
/// byte-identical.
pub fn pack_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "tar.pack.failed",
        reason_text.clone(),
        Diagnostic::make_fields(&[(FIELD_REASON, reason_text.as_str())]),
    )
}

/// Tarball unpack I/O failure. Takes the underlying failure;
/// rendering lives here. Already-keyed payloads are reclaimed by
/// [`from_anyhow`], so this only names genuinely unkeyed fallout.
pub fn unpack_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "tar.unpack.failed",
        reason_text.clone(),
        Diagnostic::make_fields(&[(FIELD_REASON, reason_text.as_str())]),
    )
}

/// Reclaim an already-keyed diagnostic from an `anyhow` failure,
/// wrapping anything else as an unpack failure. Single owner for the
/// driver-surface mapping so `apply`-callback carriers (whose
/// `anyhow` payloads already hold keyed caller diagnostics) preserve
/// keys instead of restating the downcast. Outer `.context(...)`
/// frames above the keyed payload replay natively via
/// `with_contexts` (mirroring `lift_dispatch_error`), so no chain
/// text is lost at the flip boundary.
pub fn from_anyhow(err: anyhow::Error) -> Diagnostic {
    if let Some(found) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<Diagnostic>())
        .cloned()
    {
        let mut outer_frames: Vec<String> = Vec::new();
        for cause in err.chain() {
            if cause.downcast_ref::<Diagnostic>().is_some() {
                break;
            }
            outer_frames.push(cause.to_string());
        }
        return found.with_contexts(outer_frames);
    }
    unpack_failed(&err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_has_a_distinct_key() {
        let err = anyhow::anyhow!("boom");
        let keys = [
            empty_path().key(),
            backslash_path("a\\b").key(),
            absolute_path("/abs").key(),
            escapes_root("../evil").key(),
            empty_path_normalized(".").key(),
            sha256_mismatch("aaa", "bbb").key(),
            archive_empty().key(),
            archive_truncated(100).key(),
            unsupported_type(&"fifo", "raw").key(),
            pack_failed(&err).key(),
            unpack_failed(&err).key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn messages_match_canonical_wording() {
        let err = anyhow::anyhow!("boom");
        assert_eq!(empty_path().message(), "tarball entry has an empty path");
        assert_eq!(
            backslash_path("a\\b").message(),
            "tarball entry has a backslash path: \"a\\\\b\""
        );
        assert_eq!(
            absolute_path("/abs").message(),
            "tarball entry has an absolute path: \"/abs\""
        );
        assert_eq!(
            escapes_root("../evil").message(),
            "tarball entry escapes its root: \"../evil\""
        );
        assert_eq!(
            empty_path_normalized(".").message(),
            "tarball entry has an empty path: \".\""
        );
        assert_eq!(
            sha256_mismatch("aaa", "bbb").message(),
            "tarball sha256 mismatch: expected aaa, got bbb"
        );
        assert_eq!(archive_empty().message(), "archive is empty");
        assert_eq!(
            archive_truncated(100).message(),
            "archive is truncated: 100 bytes cannot hold a tar header"
        );
        assert!(
            unsupported_type(&"fifo", "raw")
                .message()
                .starts_with("tarball entry has unsupported type ")
        );
        assert_eq!(pack_failed(&err).message(), "boom");
        assert_eq!(unpack_failed(&err).message(), "boom");
        assert_eq!(from_anyhow(anyhow::anyhow!("boom")).message(), "boom");
        let keyed = archive_empty();
        let keyed_key = keyed.key();
        let reclaimed = from_anyhow(anyhow::Error::new(keyed));
        assert_eq!(reclaimed.key(), keyed_key);
        let framed = archive_truncated(7);
        let framed_key = framed.key();
        let wrapped = anyhow::Error::new(framed).context("outer frame");
        let reclaimed_framed = from_anyhow(wrapped);
        assert_eq!(reclaimed_framed.key(), framed_key);
        assert_eq!(reclaimed_framed.contexts(), &["outer frame".to_string()]);
    }
}
