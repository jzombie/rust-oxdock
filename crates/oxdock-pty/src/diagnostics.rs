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

use oxdock_diagnostics::{Diagnostic, flatten_chain};

/// Structured field keys. Single spelling for the whole module:
/// constructors reference these, never literals, so a key rename is
/// one edit.
const FIELD_FUNC: &str = "func";
const FIELD_ERR: &str = "err";
const FIELD_REASON: &str = "reason";
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

/// Output write failed. Takes the caller label plus the I/O failure;
/// rendering lives here. The typed I/O error rides as the chained
/// source so `Error::source` and `{err:#}` chains keep it.
pub fn output_write_failed(func: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("{func} output write failed: {err_text}");
    Diagnostic::new(
        "pty.pump.output_write_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, err_text.as_str())]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::new(
        err.kind(),
        err_text,
    )))
}

/// Output flush failed. Takes the caller label plus the I/O failure;
/// rendering lives here. The typed I/O error rides as the chained
/// source so `Error::source` and `{err:#}` chains keep it.
pub fn output_flush_failed(func: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("{func} output flush failed: {err_text}");
    Diagnostic::new(
        "pty.pump.output_flush_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, err_text.as_str())]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::new(
        err.kind(),
        err_text,
    )))
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

/// Pty allocation failed. Takes the caller label plus the underlying
/// failure; the flattened chain lives here so call sites stay
/// `format!`-free. Third-party failures ride the lift below and never
/// attach a source.
pub fn pty_alloc_failed(func: &str, reason: &Diagnostic) -> Diagnostic {
    let reason_text = flatten_chain(reason);
    let message = format!("{func} pty allocation failed: {reason_text}");
    Diagnostic::new(
        "pty.run.alloc_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_REASON, reason_text.as_str())]),
    )
}

/// Master-terminal reader clone failed. Takes the caller label plus
/// the underlying failure; rendering lives here.
pub fn pty_master_reader_failed(func: &str, reason: &Diagnostic) -> Diagnostic {
    let reason_text = flatten_chain(reason);
    let message = format!("{func} pty master reader failed: {reason_text}");
    Diagnostic::new(
        "pty.run.master_reader_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_REASON, reason_text.as_str())]),
    )
}

/// Master-terminal writer take failed. Takes the caller label plus
/// the underlying failure; rendering lives here.
pub fn pty_master_writer_failed(func: &str, reason: &Diagnostic) -> Diagnostic {
    let reason_text = flatten_chain(reason);
    let message = format!("{func} pty master writer failed: {reason_text}");
    Diagnostic::new(
        "pty.run.master_writer_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_REASON, reason_text.as_str())]),
    )
}

/// Child spawn under the pty slave failed. Takes the caller label
/// plus the underlying failure; rendering lives here.
pub fn pty_spawn_failed(func: &str, reason: &Diagnostic) -> Diagnostic {
    let reason_text = flatten_chain(reason);
    let message = format!("{func} pty spawn failed: {reason_text}");
    Diagnostic::new(
        "pty.run.spawn_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_REASON, reason_text.as_str())]),
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

/// Input worker failed. Takes the caller label plus the worker's
/// typed failure by value; bare outer plus chained source.
pub fn input_worker_failed(func: &str, reason: Diagnostic) -> Diagnostic {
    let reason_text = reason.to_string();
    let message = format!("{func} input worker failed");
    Diagnostic::new(
        "pty.worker.input_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// Output worker failed. Takes the caller label plus the worker's
/// typed failure by value; bare outer plus chained source.
pub fn output_worker_failed(func: &str, reason: Diagnostic) -> Diagnostic {
    let reason_text = reason.to_string();
    let message = format!("{func} output worker failed");
    Diagnostic::new(
        "pty.worker.output_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// Generic pty-operation failure. Takes the underlying failure; the
/// flattened chain lives here so call sites stay `format!`-free.
/// Already-keyed payloads are reclaimed by [`from_anyhow`], so this
/// only names genuinely unkeyed fallout. Message is the reason
/// verbatim so reclaimed chains render byte-identical.
pub fn operation_failed(reason: &Diagnostic) -> Diagnostic {
    operation_failed_fallback(&flatten_chain(reason))
}

/// Unkeyed pty-operation fallback. Takes the pre-rendered anyhow
/// chain text verbatim; rendering lives here. Single owner for the
/// `from_anyhow` fallback so the anyhow-to-Diagnostic direction
/// keeps one keyed home.
pub fn operation_failed_fallback(reason_text: &str) -> Diagnostic {
    Diagnostic::new(
        "pty.operation.failed",
        reason_text.to_string(),
        Diagnostic::make_fields(&[(FIELD_REASON, reason_text)]),
    )
}

/// Reclaim an already-keyed diagnostic from an `anyhow` failure,
/// wrapping anything else as a generic pty-operation failure.
/// Single owner for the pty-surface mapping so portable-pty
/// carriers (whose `anyhow` payloads never hold keyed diagnostics
/// today) still preserve keys if a keyed caller ever flows through.
/// Outer `.context(...)` frames above the keyed payload replay
/// natively via `with_contexts`, so no chain text is lost at the
/// flip boundary. Call sites lift-then-wrap (`from_anyhow` plus the
/// specific `pty_*_failed` ctor) so the func-prefixed outer keeps
/// its distinct key while staying `format!`-free.
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
            // Transport echoes (`anyhow::Error::from(diag)`)
            // restate the reclaimed message verbatim; keeping
            // them would print every cause twice.
            if cause.to_string() != found.message() {
                outer_frames.push(cause.to_string());
            }
        }
        return found.with_contexts(outer_frames);
    }
    operation_failed_fallback(&format!("{err:#}"))
}

#[cfg(test)]
pub(crate) fn reason_sample(msg: &str) -> Diagnostic {
    Diagnostic::new(
        "test.reason.sample",
        msg.to_string(),
        Diagnostic::make_fields(&[]),
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
        let reason = reason_sample("boom");
        let keys = [
            master_read_failed("FUNC", &err).key(),
            output_lock_poisoned("FUNC").key(),
            output_write_failed("FUNC", &err).key(),
            output_flush_failed("FUNC", &err).key(),
            input_read_failed("FUNC", &err).key(),
            sink_lock_poisoned("FUNC").key(),
            needs_program("FUNC").key(),
            pty_alloc_failed("FUNC", &reason).key(),
            pty_master_reader_failed("FUNC", &reason).key(),
            pty_master_writer_failed("FUNC", &reason).key(),
            pty_spawn_failed("FUNC", &reason).key(),
            operation_failed(&reason).key(),
            worker_panicked("FUNC", "input", &"boom").key(),
            input_worker_failed("FUNC", output_lock_poisoned("FUNC")).key(),
            output_worker_failed("FUNC", output_lock_poisoned("FUNC")).key(),
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
        let reason = reason_sample("boom");
        assert_eq!(
            master_read_failed("SSH_PTY_RUN", &err).message(),
            "SSH_PTY_RUN master read failed: boom"
        );
        assert_eq!(
            output_lock_poisoned("SSH_PTY_RUN").message(),
            "SSH_PTY_RUN output lock poisoned"
        );
        assert_eq!(
            output_write_failed("SSH_PTY_RUN", &err).message(),
            "SSH_PTY_RUN output write failed: boom"
        );
        assert_eq!(
            output_flush_failed("SSH_PTY_RUN", &err).message(),
            "SSH_PTY_RUN output flush failed: boom"
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
            pty_alloc_failed("SSH_PTY_RUN", &reason).message(),
            "SSH_PTY_RUN pty allocation failed: boom"
        );
        assert_eq!(
            pty_master_reader_failed("SSH_PTY_RUN", &reason).message(),
            "SSH_PTY_RUN pty master reader failed: boom"
        );
        assert_eq!(
            pty_master_writer_failed("SSH_PTY_RUN", &reason).message(),
            "SSH_PTY_RUN pty master writer failed: boom"
        );
        assert_eq!(
            pty_spawn_failed("SSH_PTY_RUN", &reason).message(),
            "SSH_PTY_RUN pty spawn failed: boom"
        );
        assert_eq!(operation_failed(&reason).message(), "boom");
        assert_eq!(operation_failed_fallback("boom").message(), "boom");
        assert_eq!(from_anyhow(anyhow::anyhow!("boom")).message(), "boom");
        assert_eq!(
            from_anyhow(anyhow::anyhow!("boom")).key(),
            "pty.operation.failed"
        );
        let keyed = needs_program("FUNC");
        let keyed_key = keyed.key();
        let reclaimed = from_anyhow(anyhow::Error::new(keyed));
        assert_eq!(reclaimed.key(), keyed_key);
        let framed = needs_program("FUNC");
        let framed_key = framed.key();
        let wrapped = anyhow::Error::new(framed).context("outer frame");
        let reclaimed_framed = from_anyhow(wrapped);
        assert_eq!(reclaimed_framed.key(), framed_key);
        assert_eq!(reclaimed_framed.contexts(), &["outer frame".to_string()]);
        assert_eq!(
            worker_panicked("SSH_PTY_RUN", "input", &"boom").message(),
            "SSH_PTY_RUN input worker panicked: \"boom\""
        );
        assert_eq!(
            worker_panicked("SSH_PTY_RUN", "output", &"boom").message(),
            "SSH_PTY_RUN output worker panicked: \"boom\""
        );
        assert_eq!(
            input_worker_failed("SSH_PTY_RUN", output_lock_poisoned("SSH_PTY_RUN")).message(),
            "SSH_PTY_RUN input worker failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(input_worker_failed(
                    "SSH_PTY_RUN",
                    output_lock_poisoned("SSH_PTY_RUN"),
                ))
            ),
            "SSH_PTY_RUN input worker failed: SSH_PTY_RUN output lock poisoned"
        );
        assert_eq!(
            output_worker_failed("SSH_PTY_RUN", output_lock_poisoned("SSH_PTY_RUN")).message(),
            "SSH_PTY_RUN output worker failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(output_worker_failed(
                    "SSH_PTY_RUN",
                    output_lock_poisoned("SSH_PTY_RUN"),
                ))
            ),
            "SSH_PTY_RUN output worker failed: SSH_PTY_RUN output lock poisoned"
        );
    }

    #[test]
    fn io_ctors_preserve_typed_source_chain() {
        use std::error::Error as _;
        let err = io_err("boom");
        for diag in [
            master_read_failed("FUNC", &err),
            input_read_failed("FUNC", &err),
            output_write_failed("FUNC", &err),
            output_flush_failed("FUNC", &err),
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
