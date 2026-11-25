//! Pty session failures behind single constructors with stable keys.
//!
//! This module owns every pty template and key (`pty.*`). It builds on
//! the generic [`oxdock_diagnostics::Diagnostic`] transport but owns its
//! wording: changing pty text happens here, never at the call sites.
//! Call sites pass structured data only, never `format!` strings.
//! Lock-poison and missing-program failures take only the caller label;
//! I/O failures take the concrete `std::io::Error`; worker panics take
//! the caller label, the worker name, and the panic payload for
//! `Debug` rendering here.

use oxdock_diagnostics::Diagnostic;

/// Structured field keys. Single spelling for the whole module:
/// constructors reference these, never literals, so a key rename is
/// one edit.
const FIELD_FUNC: &str = "func";
const FIELD_ERR: &str = "err";
const FIELD_WORKER: &str = "worker";
const FIELD_PANIC: &str = "panic";

/// Master-terminal output read failed. Takes the caller label plus the
/// I/O failure; rendering lives here. The typed I/O error rides as
/// the chained source so `Error::source` and `{err:#}` chains keep it.
pub fn master_read_failed(func: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("{func} master read failed: {err_text}");
    Diagnostic::new(
        "pty.pump.master_read_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, err_text.as_str())]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::new(
        err.kind(),
        err_text,
    )))
}

/// Output sink lock poisoned. Takes the caller label; rendering lives
/// here. Shared by the stream and tail-flush paths: one key, one mode.
pub fn output_lock_poisoned(func: &str) -> Diagnostic {
    let message = format!("{func} output lock poisoned");
    Diagnostic::new(
        "pty.pump.output_lock_poisoned",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func)]),
    )
}

/// Caller stdin read failed. Takes the caller label plus the I/O
/// failure; rendering lives here. The typed I/O error rides as the
/// chained source so `Error::source` and `{err:#}` chains keep it.
pub fn input_read_failed(func: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("{func} input read failed: {err_text}");
    Diagnostic::new(
        "pty.pump.input_read_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, err_text.as_str())]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::new(
        err.kind(),
        err_text,
    )))
}

/// Master-writer sink lock poisoned. Takes the caller label; rendering
/// lives here.
pub fn sink_lock_poisoned(func: &str) -> Diagnostic {
    let message = format!("{func} sink lock poisoned");
    Diagnostic::new(
        "pty.pump.sink_lock_poisoned",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func)]),
    )
}

/// Session request with no argv. Takes the caller label; rendering
/// lives here.
pub fn needs_program(func: &str) -> Diagnostic {
    let message = format!("{func} needs at least a program");
    Diagnostic::new(
        "pty.run.needs_program",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func)]),
    )
}

/// Pump worker panicked. Takes the caller label, the worker name
/// (`"input"` / `"output"`), and the panic payload; `Debug` rendering
/// lives here. Shared by both workers: one key, one mode.
pub fn worker_panicked(func: &str, worker: &str, panic: &impl std::fmt::Debug) -> Diagnostic {
    let panic_text = format!("{panic:?}");
    let message = format!("{func} {worker} worker panicked: {panic_text}");
    Diagnostic::new(
        "pty.worker.panicked",
        message,
        Diagnostic::make_fields(&[
            (FIELD_FUNC, func),
            (FIELD_WORKER, worker),
            (FIELD_PANIC, panic_text.as_str()),
        ]),
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
        let keys = [
            master_read_failed("FUNC", &err).key(),
            output_lock_poisoned("FUNC").key(),
            input_read_failed("FUNC", &err).key(),
            sink_lock_poisoned("FUNC").key(),
            needs_program("FUNC").key(),
            worker_panicked("FUNC", "input", &"boom").key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn shared_modes_keep_one_key() {
        let err = io_err("boom");
        assert_eq!(
            worker_panicked("FUNC", "input", &err).key(),
            worker_panicked("FUNC", "output", &err).key()
        );
    }

    #[test]
    fn messages_match_canonical_wording() {
        let err = io_err("boom");
        assert_eq!(
            master_read_failed("SSH_PTY_RUN", &err).message(),
            "SSH_PTY_RUN master read failed: boom"
        );
        assert_eq!(
            output_lock_poisoned("SSH_PTY_RUN").message(),
            "SSH_PTY_RUN output lock poisoned"
        );
        assert_eq!(
            input_read_failed("SSH_PTY_RUN", &err).message(),
            "SSH_PTY_RUN input read failed: boom"
        );
        assert_eq!(
            sink_lock_poisoned("SSH_PTY_RUN").message(),
            "SSH_PTY_RUN sink lock poisoned"
        );
        assert_eq!(
            needs_program("SSH_PTY_RUN").message(),
            "SSH_PTY_RUN needs at least a program"
        );
        assert_eq!(
            worker_panicked("SSH_PTY_RUN", "input", &"boom").message(),
            "SSH_PTY_RUN input worker panicked: \"boom\""
        );
        assert_eq!(
            worker_panicked("SSH_PTY_RUN", "output", &"boom").message(),
            "SSH_PTY_RUN output worker panicked: \"boom\""
        );
    }

    #[test]
    fn io_ctors_preserve_typed_source_chain() {
        use std::error::Error as _;
        let err = io_err("boom");
        for diag in [
            master_read_failed("FUNC", &err),
            input_read_failed("FUNC", &err),
        ] {
            let source = diag.source().expect("io ctor keeps a source");
            let io_source = source
                .downcast_ref::<std::io::Error>()
                .expect("source is the typed io error");
            assert_eq!(io_source.kind(), std::io::ErrorKind::Other);
            assert_eq!(io_source.to_string(), "boom");
        }
    }
}
