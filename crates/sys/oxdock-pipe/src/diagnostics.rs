//! Pipe backend failures behind single constructors with stable keys.
//!
//! This module owns every pipe template and key (`pipe.*`). It builds on
//! the generic [`oxdock_diagnostics::Diagnostic`] transport but owns its
//! wording: changing pipe text happens here, never at the call sites.
//! Call sites pass structured data only, never `format!` strings.
//! Lock-poison and consumed-handle failures are parameterless (the
//! wording is fixed); I/O and UTF-8 failures take the concrete error.

use oxdock_diagnostics::Diagnostic;

/// Structured field keys. Single spelling for the whole module:
/// constructors reference these, never literals, so a key rename is
/// one edit.
const FIELD_ERR: &str = "err";

/// OS pipe reader lock poisoned. Parameterless: the wording is fixed.
pub fn os_reader_lock_poisoned() -> Diagnostic {
    Diagnostic::new(
        "pipe.slot.os_reader_lock_poisoned",
        "os pipe reader lock poisoned".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// OS pipe writer lock poisoned. Parameterless: the wording is fixed.
pub fn os_writer_lock_poisoned() -> Diagnostic {
    Diagnostic::new(
        "pipe.slot.os_writer_lock_poisoned",
        "os pipe writer lock poisoned".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// OS pipe handle already consumed. Shared by the reader and writer
/// take paths: same failure mode, one key. Parameterless: the wording
/// is fixed.
pub fn os_handle_consumed() -> Diagnostic {
    Diagnostic::new(
        "pipe.slot.os_handle_consumed",
        "os pipe handle has already been consumed by another process".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Pipe handle cell lock poisoned. Parameterless: the wording is fixed.
pub fn handle_lock_poisoned() -> Diagnostic {
    Diagnostic::new(
        "pipe.slot.handle_lock_poisoned",
        "pipe handle lock poisoned".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Peek of an unbound pipe. Parameterless: the wording is fixed.
pub fn peek_unbound() -> Diagnostic {
    Diagnostic::new(
        "pipe.peek.unbound",
        "cannot peek unbound pipe: bind it to a command first".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Peek of an OS-materialized pipe. Parameterless: the wording is fixed.
pub fn peek_os_materialized() -> Diagnostic {
    Diagnostic::new(
        "pipe.peek.os_materialized",
        "cannot peek OS-materialized pipe: drain it through a bound command instead".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Peek backend read failed. Takes the I/O failure; rendering lives here.
pub fn peek_failed(err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("failed to peek pipe: {err_text}");
    Diagnostic::new(
        "pipe.peek.failed",
        message,
        Diagnostic::make_fields(&[(FIELD_ERR, err_text.as_str())]),
    )
}

/// Captured stdout is not valid UTF-8. Takes the conversion failure;
/// rendering lives here.
pub fn captured_stdout_invalid_utf8(err: &std::string::FromUtf8Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("captured stdout is not valid UTF-8: {err_text}");
    Diagnostic::new(
        "pipe.spill.captured_stdout_invalid_utf8",
        message,
        Diagnostic::make_fields(&[(FIELD_ERR, err_text.as_str())]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io_err(msg: &str) -> std::io::Error {
        std::io::Error::other(msg.to_string())
    }

    #[test]
    fn each_failure_has_a_distinct_key() {
        let err = io_err("boom");
        let utf8_err = String::from_utf8(vec![0xff]).unwrap_err();
        let keys = [
            os_reader_lock_poisoned().key(),
            os_writer_lock_poisoned().key(),
            os_handle_consumed().key(),
            handle_lock_poisoned().key(),
            peek_unbound().key(),
            peek_os_materialized().key(),
            peek_failed(&err).key(),
            captured_stdout_invalid_utf8(&utf8_err).key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn messages_match_canonical_wording() {
        let err = io_err("boom");
        assert_eq!(
            os_reader_lock_poisoned().message(),
            "os pipe reader lock poisoned"
        );
        assert_eq!(
            os_writer_lock_poisoned().message(),
            "os pipe writer lock poisoned"
        );
        assert_eq!(
            os_handle_consumed().message(),
            "os pipe handle has already been consumed by another process"
        );
        assert_eq!(
            handle_lock_poisoned().message(),
            "pipe handle lock poisoned"
        );
        assert_eq!(
            peek_unbound().message(),
            "cannot peek unbound pipe: bind it to a command first"
        );
        assert_eq!(
            peek_os_materialized().message(),
            "cannot peek OS-materialized pipe: drain it through a bound command instead"
        );
        assert_eq!(peek_failed(&err).message(), "failed to peek pipe: boom");
        let utf8_err = String::from_utf8(vec![0xff]).unwrap_err();
        assert!(
            captured_stdout_invalid_utf8(&utf8_err)
                .message()
                .starts_with("captured stdout is not valid UTF-8: ")
        );
    }
}
