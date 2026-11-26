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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_has_a_distinct_key() {
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
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn messages_match_canonical_wording() {
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
    }
}
