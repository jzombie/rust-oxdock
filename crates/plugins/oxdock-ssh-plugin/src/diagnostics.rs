//! SSH user-facing errors behind single constructors with stable keys.
//!
//! This module owns every SSH template and key (`ssh.*`). It builds on
//! the generic [`oxdock_diagnostics::Diagnostic`] transport but owns its
//! wording: changing SSH text happens here, never at the call sites.
//! Call sites pass structured data only, never `format!` strings.
//! Lock-poison and cancellation failures are parameterless (the wording
//! is fixed); I/O failures take the concrete `std::io::Error`; worker
//! and function labels ride as structured fields with rendering here.

use oxdock_diagnostics::Diagnostic;
use oxdock_parser::{HANDLE_TYPE_NAME, KEYWORD_ASYNC, KEYWORD_LET};

/// Structured field keys. Single spelling for the whole module:
/// constructors reference these, never literals, so a key rename is
/// one edit.
const FIELD_FUNC: &str = "func";
const FIELD_WORKER: &str = "worker";
const FIELD_NAME: &str = "name";
const FIELD_RAW: &str = "raw";
const FIELD_PATH: &str = "path";
const FIELD_ALGORITHM: &str = "algorithm";
const FIELD_MODE: &str = "mode";
const FIELD_ERR: &str = "err";
const FIELD_CODE: &str = "code";
const FIELD_OPTION: &str = "option";

/// `key_path` escapes the workspace guard. Takes the invoking func
/// and the raw path; quoting lives here.
pub fn key_path_escapes(func: &str, raw: &str) -> Diagnostic {
    let message = format!("{func} key_path escapes the workspace: {raw}");
    Diagnostic::new(
        "ssh.key.escapes_workspace",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_RAW, raw)]),
    )
}

/// Pre-existing host key is not Ed25519. Takes the invoking func,
/// the display path, and the algorithm (`Debug` rendering lives
/// here).
pub fn host_key_must_be_ed25519(
    func: &str,
    path: &str,
    algorithm: &impl std::fmt::Debug,
) -> Diagnostic {
    let algorithm_text = format!("{algorithm:?}");
    let message = format!("{func} host key {path} must be Ed25519, found {algorithm_text}");
    Diagnostic::new(
        "ssh.key.must_be_ed25519",
        message,
        Diagnostic::make_fields(&[
            (FIELD_FUNC, func),
            (FIELD_PATH, path),
            (FIELD_ALGORITHM, algorithm_text.as_str()),
        ]),
    )
}

/// Pre-existing host key file has unsafe permissions. Takes the
/// invoking func, the display path, and the raw mode; octal
/// rendering lives here.
pub fn host_key_insecure_permissions(func: &str, path: &str, mode: u32) -> Diagnostic {
    let mode_text = format!("0{mode:o}");
    let message = format!(
        "{func} insecure permissions on host key file {path}: expected 0600, got {mode_text}"
    );
    Diagnostic::new(
        "ssh.key.insecure_permissions",
        message,
        Diagnostic::make_fields(&[
            (FIELD_FUNC, func),
            (FIELD_PATH, path),
            (FIELD_MODE, mode_text.as_str()),
        ]),
    )
}

/// Workspace resolver for the host key cannot open. Takes the
/// invoking func and the underlying failure; the outer message
/// stays bare while the cause rides a chained source, so
/// `{err:#}` keeps the legacy `": "` join. Call sites stay
/// `format!`-free.
pub fn host_key_resolver_failed(func: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "ssh.key.resolver_failed",
        format!("{func} cannot open the workspace resolver"),
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// Ed25519 host key generation failed. Takes the concrete failure
/// by value; the outer message stays bare while the cause rides a
/// chained source.
pub fn host_key_generate_failed(
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.key.generate_failed",
        "generate Ed25519 host key".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// Ed25519 host key PEM encoding failed. Takes the concrete failure
/// by value; bare outer message plus chained source.
pub fn host_key_encode_failed(
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.key.encode_failed",
        "encode Ed25519 host key".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// `key_path` parent guard cannot anchor the workspace root. Takes
/// the invoking func and the underlying failure; bare outer message
/// plus chained source.
pub fn key_guard_failed(func: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "ssh.key.guard_failed",
        format!("{func} cannot guard the workspace root"),
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// Host key file metadata cannot be read. Takes the display path
/// and the underlying failure; bare outer message plus chained
/// source.
pub fn host_key_stat_failed(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "ssh.key.stat_failed",
        format!("cannot stat host key {path}"),
        Diagnostic::make_fields(&[(FIELD_PATH, path), (FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// Host key file bytes cannot be read. Takes the invoking func, the
/// display path, and the underlying failure; bare outer message
/// plus chained source.
pub fn host_key_read_failed(func: &str, path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "ssh.key.read_failed",
        format!("{func} cannot read host key {path}"),
        Diagnostic::make_fields(&[
            (FIELD_FUNC, func),
            (FIELD_PATH, path),
            (FIELD_ERR, &reason_text),
        ]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// Host key bytes do not parse as OpenSSH. Takes the invoking func,
/// the display path, and the concrete failure by value; bare outer
/// message plus chained source.
pub fn host_key_parse_failed(
    func: &str,
    path: &str,
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.key.parse_failed",
        format!("{func} cannot parse host key {path}"),
        Diagnostic::make_fields(&[
            (FIELD_FUNC, func),
            (FIELD_PATH, path),
            (FIELD_ERR, &reason_text),
        ]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// Host key file write failed past the first-start race. Takes the
/// underlying failure verbatim: the file layer already names the
/// path, so the message passes through unchanged.
pub fn host_key_write_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "ssh.key.write_failed",
        reason_text.clone(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
}

/// Guarded join of a `key_path` escapes the workspace. Takes the
/// invoking func, the raw path, and the guard failure; the outer
/// message names the escape while the resolved-path detail rides a
/// chained source, so it keeps its own key.
pub fn key_path_join_failed(func: &str, raw: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "ssh.key.join_failed",
        format!("{func} key_path escapes the workspace: {raw}"),
        Diagnostic::make_fields(&[
            (FIELD_FUNC, func),
            (FIELD_RAW, raw),
            (FIELD_ERR, &reason_text),
        ]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// SSH pump output lock poisoned. Parameterless: the wording is
/// fixed.
pub fn pump_output_lock_poisoned() -> Diagnostic {
    Diagnostic::new(
        "ssh.pump.output_lock_poisoned",
        "SSH pump output lock poisoned".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// SSH pump input pipe read failed. Takes the I/O failure;
/// rendering lives here.
pub fn pump_input_read_failed(err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("SSH pump input pipe read failed: {err_text}");
    Diagnostic::new(
        "ssh.pump.input_read_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_ERR, err_text.as_str())]),
    )
}

/// SSH pump worker panicked. Takes the worker label; rendering
/// lives here.
pub fn pump_worker_panicked(worker: &str) -> Diagnostic {
    let message = format!("SSH pump {worker} panicked");
    Diagnostic::new(
        "ssh.pump.worker_panicked",
        message,
        Diagnostic::make_fields(&[(FIELD_WORKER, worker)]),
    )
}

/// SSH pump interrupted by cancellation. Parameterless: the wording
/// is fixed.
pub fn pump_interrupted_by_cancellation() -> Diagnostic {
    Diagnostic::new(
        "ssh.pump.interrupted_by_cancellation",
        "SSH pump interrupted by cancellation".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_PUMP` source pipe read failed. Takes the I/O failure;
/// rendering lives here.
pub fn pump_source_read_failed(err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("SSH_PUMP source pipe read failed: {err_text}");
    Diagnostic::new(
        "ssh.pump.source_read_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_ERR, err_text.as_str())]),
    )
}

/// `SSH_PUMP` target lock poisoned. Parameterless: the wording is
/// fixed.
pub fn pump_target_lock_poisoned() -> Diagnostic {
    Diagnostic::new(
        "ssh.pump.target_lock_poisoned",
        "SSH_PUMP target lock poisoned".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Pipe-to-sink input read failed. Takes the caller label plus the
/// I/O failure; rendering lives here.
pub fn pump_func_input_read_failed(func: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("{func} input pipe read failed: {err_text}");
    Diagnostic::new(
        "ssh.pump.func_input_read_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, err_text.as_str())]),
    )
}

/// SSH pump output pipe write failed. Takes the concrete I/O
/// failure by value; bare outer message plus chained source so
/// `{err:#}` keeps the legacy `": "` join.
pub fn pump_output_write_failed(
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.pump.output_write_failed",
        "SSH pump output pipe write failed".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// SSH pump output pipe flush failed. Takes the concrete I/O
/// failure by value; bare outer plus chained source.
pub fn pump_output_flush_failed(
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.pump.output_flush_failed",
        "SSH pump output pipe flush failed".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// SSH pump cannot borrow the input pipe. Takes the underlying
/// typed failure by value; bare outer plus chained source so the
/// inner key survives traversal.
pub fn pump_borrow_input_failed(reason: Diagnostic) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.pump.borrow_input_failed",
        "SSH pump cannot borrow the input pipe".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// SSH pump cannot borrow the output pipe. Takes the underlying
/// typed failure by value; bare outer plus chained source.
pub fn pump_borrow_output_failed(reason: Diagnostic) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.pump.borrow_output_failed",
        "SSH pump cannot borrow the output pipe".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// SSH pump wait runtime failed to build. Takes the concrete
/// failure by value; bare outer plus chained source.
pub fn pump_wait_runtime_failed(
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.pump.wait_runtime_failed",
        "SSH pump wait runtime failed".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// `SSH_PUMP` cannot borrow the source pipe. Takes the underlying
/// typed failure by value; bare outer plus chained source.
pub fn pump_borrow_source_failed(reason: Diagnostic) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.pump.borrow_source_failed",
        "SSH_PUMP cannot borrow the source pipe".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// `SSH_PUMP` cannot borrow the target pipe. Takes the underlying
/// typed failure by value; bare outer plus chained source.
pub fn pump_borrow_target_failed(reason: Diagnostic) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.pump.borrow_target_failed",
        "SSH_PUMP cannot borrow the target pipe".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// `SSH_PUMP` target pipe write failed. Takes the concrete failure
/// by value; bare outer plus chained source.
pub fn pump_target_write_failed(
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.pump.target_write_failed",
        "SSH_PUMP target pipe write failed".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// `SSH_PUMP` target pipe flush failed. Takes the concrete failure
/// by value; bare outer plus chained source.
pub fn pump_target_flush_failed(
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.pump.target_flush_failed",
        "SSH_PUMP target pipe flush failed".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// Authentication rejected. Takes the invoking func (`SSH_EXEC` /
/// `SSH_CONNECT`: same failure mode, one key); rendering lives
/// here.
pub fn authentication_rejected(func: &str) -> Diagnostic {
    let message = format!("{func} authentication rejected");
    Diagnostic::new(
        "ssh.auth.rejected",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func)]),
    )
}

/// Dial failed. Takes the invoking func (`SSH_EXEC` /
/// `SSH_CONNECT`: same failure mode, one key) plus the underlying
/// failure; the russh connect boundary surfaces `anyhow::Error`,
/// so the outer stays bare while the cause rides an I/O-chained
/// source. Call sites stay `format!`-free.
pub fn dial_failed(func: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "ssh.dial_failed",
        format!("{func} dial failed"),
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// Authentication exchange failed. Takes the invoking func
/// (`SSH_EXEC` / `SSH_CONNECT`: same failure mode, one key) plus
/// the concrete failure by value; bare outer plus chained source.
pub fn auth_exchange_failed(
    func: &str,
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.auth.exchange_failed",
        format!("{func} authentication exchange failed"),
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// Channel open failed. Takes the invoking func (`SSH_EXEC` /
/// `SSH_CONNECT`: same failure mode, one key) plus the concrete
/// failure by value; bare outer plus chained source.
pub fn channel_open_failed(
    func: &str,
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.channel.open_failed",
        format!("{func} channel open failed"),
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// `SSH_EXEC` exec request failed. Takes the concrete failure by
/// value; bare outer plus chained source.
pub fn exec_request_failed(reason: impl std::error::Error + Send + Sync + 'static) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.exec.request_failed",
        "SSH_EXEC exec request failed".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// `SSH_CONNECT` shell request failed. Takes the concrete failure
/// by value; bare outer plus chained source.
pub fn shell_request_failed(reason: impl std::error::Error + Send + Sync + 'static) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.connect.shell_request_failed",
        "SSH_CONNECT shell request failed".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// Listener socket cannot go non-blocking for the runtime. Takes
/// the concrete failure by value; bare outer plus chained source.
/// The legacy wording stays byte-identical (no func prefix).
pub fn listener_prepare_failed(
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.serve.listener_prepare_failed",
        "prepare listener for the runtime".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// Listener socket cannot hand to the runtime. Takes the concrete
/// failure by value; bare outer plus chained source.
pub fn listener_handoff_failed(
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.serve.listener_handoff_failed",
        "hand listener to the runtime".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// SSH listener failed. Takes the concrete failure by value; bare
/// outer plus chained source.
pub fn listener_failed(reason: impl std::error::Error + Send + Sync + 'static) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.serve.listener_failed",
        "SSH listener failed".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// SSH client runtime failed to build. Takes the concrete failure
/// by value; bare outer plus chained source.
pub fn client_runtime_build_failed(
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.client_runtime.build_failed",
        "build SSH client runtime".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// Session pump ends already taken by `SSH_SEND_EXIT_STATUS`.
/// Parameterless: the wording is fixed.
pub fn session_pump_already_finished() -> Diagnostic {
    Diagnostic::new(
        "ssh.session.pump_already_finished",
        "SSH session pump already finished".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Exit-status queue saturated under backpressure. Parameterless:
/// the wording is fixed.
pub fn session_wire_queue_saturated() -> Diagnostic {
    Diagnostic::new(
        "ssh.session.wire_queue_saturated",
        "SSH session wire queue saturated under backpressure".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Exit-status queue receiver gone. Parameterless: the wording is
/// fixed.
pub fn session_wire_writer_gone() -> Diagnostic {
    Diagnostic::new(
        "ssh.session.wire_writer_gone",
        "SSH session wire writer is gone".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Second pump on the same session. Parameterless: the wording is
/// fixed.
pub fn session_already_pumped() -> Diagnostic {
    Diagnostic::new(
        "ssh.session.already_pumped",
        "SSH session already pumped".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Wire data for an unknown channel. Parameterless: the wording is
/// fixed.
pub fn channel_data_unknown() -> Diagnostic {
    Diagnostic::new(
        "ssh.channel.data_unknown",
        "data for unknown channel".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Pump side vanished mid-send. Parameterless: the wording is fixed.
pub fn channel_pump_gone() -> Diagnostic {
    Diagnostic::new(
        "ssh.channel.pump_gone",
        "pump went away".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Wire side vanished during EOF close. Parameterless: the wording
/// is fixed.
pub fn channel_wire_gone_eof_close() -> Diagnostic {
    Diagnostic::new(
        "ssh.channel.wire_gone_eof_close",
        "wire gone during EOF close".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Wire request for an unknown channel. Parameterless: the wording
/// is fixed.
pub fn channel_request_unknown() -> Diagnostic {
    Diagnostic::new(
        "ssh.channel.request_unknown",
        "request for unknown channel".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Channel announced twice. Parameterless: the wording is fixed.
pub fn channel_already_announced() -> Diagnostic {
    Diagnostic::new(
        "ssh.channel.already_announced",
        "channel already announced".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Child stream read failed. Takes the stream name plus the I/O
/// failure; rendering lives here.
pub fn run_child_read_failed(name: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("SSH run child {name} read failed: {err_text}");
    Diagnostic::new(
        "ssh.run.child_read_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_NAME, name), (FIELD_ERR, err_text.as_str())]),
    )
}

/// SSH run output pipe write failed. Takes the concrete failure by
/// value; bare outer plus chained source.
pub fn run_output_write_failed(
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.run.output_write_failed",
        "SSH run output pipe write failed".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// SSH run output pipe flush failed. Takes the concrete failure by
/// value; bare outer plus chained source.
pub fn run_output_flush_failed(
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.run.output_flush_failed",
        "SSH run output pipe flush failed".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// SSH run output lock poisoned. Parameterless: the wording is
/// fixed.
pub fn run_output_lock_poisoned() -> Diagnostic {
    Diagnostic::new(
        "ssh.run.output_lock_poisoned",
        "SSH run output lock poisoned".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_RUN` with no argv. Parameterless: the wording is fixed.
pub fn run_needs_program() -> Diagnostic {
    Diagnostic::new(
        "ssh.run.needs_program",
        "SSH_RUN needs at least a program".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Spawned child lost its stderr handle. Parameterless: the wording
/// is fixed.
pub fn run_spawn_lost_stderr() -> Diagnostic {
    Diagnostic::new(
        "ssh.run.spawn_lost_stderr",
        "SSH run spawn lost stderr".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// SSH run interrupted by cancellation. Parameterless: the wording
/// is fixed.
pub fn run_interrupted_by_cancellation() -> Diagnostic {
    Diagnostic::new(
        "ssh.run.interrupted_by_cancellation",
        "SSH run interrupted by cancellation".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// SSH run cannot borrow the input pipe. Takes the underlying
/// typed failure by value; bare outer plus chained source.
pub fn run_borrow_input_failed(reason: Diagnostic) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.run.borrow_input_failed",
        "SSH run cannot borrow the input pipe".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// SSH run cannot borrow the output pipe. Takes the underlying
/// typed failure by value; bare outer plus chained source.
pub fn run_borrow_output_failed(reason: Diagnostic) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.run.borrow_output_failed",
        "SSH run cannot borrow the output pipe".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// SSH run cannot borrow the error pipe. Takes the underlying
/// typed failure by value; bare outer plus chained source.
pub fn run_borrow_error_failed(reason: Diagnostic) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.run.borrow_error_failed",
        "SSH run cannot borrow the error pipe".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// SSH run spawn failed. Takes the underlying typed failure by
/// value; bare outer plus chained source.
pub fn run_spawn_failed(reason: Diagnostic) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.run.spawn_failed",
        "SSH run spawn failed".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// SSH pty output lock poisoned. Parameterless: the wording is
/// fixed.
pub fn pty_output_lock_poisoned() -> Diagnostic {
    Diagnostic::new(
        "ssh.pty.output_lock_poisoned",
        "SSH pty output lock poisoned".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// SSH pty run interrupted by cancellation. Parameterless: the
/// wording is fixed.
pub fn pty_interrupted_by_cancellation() -> Diagnostic {
    Diagnostic::new(
        "ssh.pty.interrupted_by_cancellation",
        "SSH pty run interrupted by cancellation".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// SSH pty cannot borrow the input pipe. Takes the underlying
/// typed failure by value; bare outer plus chained source.
pub fn pty_borrow_input_failed(reason: Diagnostic) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.pty.borrow_input_failed",
        "SSH pty cannot borrow the input pipe".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// SSH pty cannot borrow the output pipe. Takes the underlying
/// typed failure by value; bare outer plus chained source.
pub fn pty_borrow_output_failed(reason: Diagnostic) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.pty.borrow_output_failed",
        "SSH pty cannot borrow the output pipe".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// SSH pty session init write failed. Takes the concrete failure
/// by value; bare outer plus chained source.
pub fn pty_session_init_write_failed(
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.pty.session_init_write_failed",
        "SSH pty session init write failed".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// SSH pty session init flush failed. Takes the concrete failure
/// by value; bare outer plus chained source.
pub fn pty_session_init_flush_failed(
    reason: impl std::error::Error + Send + Sync + 'static,
) -> Diagnostic {
    let reason_text = reason.to_string();
    Diagnostic::new(
        "ssh.pty.session_init_flush_failed",
        "SSH pty session init flush failed".to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, &reason_text)]),
    )
    .with_source(std::sync::Arc::new(reason))
}

/// Dequeue wait cancelled. Takes the invoking func (`SSH_DEQUEUE` /
/// `SSH_ACCEPT`: same failure mode, one key); rendering lives here.
pub fn dequeue_task_cancelled(func: &str) -> Diagnostic {
    let message = format!("{func}: task cancelled");
    Diagnostic::new(
        "ssh.dequeue.task_cancelled",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func)]),
    )
}

/// Queue shut down while waiting. Takes the invoking func (same
/// failure mode, one key); rendering lives here.
pub fn dequeue_server_closed(func: &str) -> Diagnostic {
    let message = format!("{func}: server is closed");
    Diagnostic::new(
        "ssh.dequeue.server_closed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func)]),
    )
}

/// `SSH_DEQUEUE` outside `ASYNC`. Parameterless: the wrap hint lives
/// here.
pub fn dequeue_requires_async() -> Diagnostic {
    Diagnostic::new(
        "ssh.dequeue.requires_async",
        format!(
            "SSH_DEQUEUE requires {KEYWORD_ASYNC}: wrap it as {KEYWORD_LET} $t: {HANDLE_TYPE_NAME}<T> = {KEYWORD_ASYNC} {{ SSH_DEQUEUE($server.server) }}"
        ),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_ACCEPT` outside `ASYNC`. Parameterless: the wrap hint lives
/// here.
pub fn accept_requires_async() -> Diagnostic {
    Diagnostic::new(
        "ssh.accept.requires_async",
        format!(
            "SSH_ACCEPT requires {KEYWORD_ASYNC}: wrap it as {KEYWORD_LET} $t: {HANDLE_TYPE_NAME}<T> = {KEYWORD_ASYNC} {{ SSH_ACCEPT($server, $in, $out) }}"
        ),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_PUMP_CHANNEL` outside `ASYNC`. Parameterless: the wording is
/// fixed.
pub fn pump_channel_requires_async() -> Diagnostic {
    Diagnostic::new(
        "ssh.pump_channel.requires_async",
        format!(
            "SSH_PUMP_CHANNEL requires {KEYWORD_ASYNC}: pump it in its own task after SSH_DEQUEUE"
        ),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_CONNECT` outside `ASYNC`. Parameterless: the wording is
/// fixed.
pub fn connect_requires_async() -> Diagnostic {
    Diagnostic::new(
        "ssh.connect.requires_async",
        format!(
            "SSH_CONNECT requires {KEYWORD_ASYNC}: run it in its own task beside the SSH_PUMP tasks"
        ),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_CONNECT` while offline. Parameterless: the wording is fixed.
pub fn connect_offline() -> Diagnostic {
    Diagnostic::new(
        "ssh.connect.offline",
        "SSH_CONNECT failed: engine running in --offline mode".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_CONNECT` with an empty username. Parameterless: the wording
/// is fixed.
pub fn connect_username_empty() -> Diagnostic {
    Diagnostic::new(
        "ssh.connect.username_empty",
        "SSH_CONNECT username must not be empty".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_PTY_RUN` outside `ASYNC`. Parameterless: the wording is
/// fixed.
pub fn pty_run_requires_async() -> Diagnostic {
    Diagnostic::new(
        "ssh.pty_run.requires_async",
        format!(
            "SSH_PTY_RUN requires {KEYWORD_ASYNC}: run it in its own task beside the session pump task"
        ),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_RUN` outside `ASYNC`. Parameterless: the wording is fixed.
pub fn run_requires_async() -> Diagnostic {
    Diagnostic::new(
        "ssh.run.requires_async",
        format!(
            "SSH_RUN requires {KEYWORD_ASYNC}: run it in its own task beside the session pump task"
        ),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_EXEC` with no argv. Parameterless: the wording is fixed.
pub fn exec_needs_program() -> Diagnostic {
    Diagnostic::new(
        "ssh.exec.needs_program",
        "SSH_EXEC needs at least a program".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_EXEC` with an empty username. Parameterless: the wording is
/// fixed.
pub fn exec_username_empty() -> Diagnostic {
    Diagnostic::new(
        "ssh.exec.username_empty",
        "SSH_EXEC username must not be empty".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_EXEC` while offline. Parameterless: the wording is fixed.
pub fn exec_offline() -> Diagnostic {
    Diagnostic::new(
        "ssh.exec.offline",
        "SSH_EXEC failed: engine running in --offline mode".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_SERVE` option empty. Takes the option name (callers pass the
/// `FIELD_*` / `OPT_*` constants); quoting lives here. Shared by
/// every `SSH_SERVE` string option: one key, one mode.
pub fn serve_option_empty(option: &str) -> Diagnostic {
    let message = format!("SSH_SERVE option '{option}' must not be empty");
    Diagnostic::new(
        "ssh.serve.option_empty",
        message,
        Diagnostic::make_fields(&[(FIELD_OPTION, option)]),
    )
}

/// `SSH_SERVE` option missing. Takes the option name; quoting lives
/// here. Shared by every required `SSH_SERVE` option: one key, one
/// mode.
pub fn serve_option_required(option: &str) -> Diagnostic {
    let message = format!("SSH_SERVE option '{option}' is required");
    Diagnostic::new(
        "ssh.serve.option_required",
        message,
        Diagnostic::make_fields(&[(FIELD_OPTION, option)]),
    )
}

/// `SSH_EXEC` numeric option not positive. Takes the option name
/// (callers pass the `OPT_*` constants); quoting lives here.
/// Shared by `timeout` / `max_bytes`: one key, one mode.
pub fn exec_option_must_be_positive(option: &str) -> Diagnostic {
    let message = format!("SSH_EXEC option '{option}' must be positive");
    Diagnostic::new(
        "ssh.exec.option_must_be_positive",
        message,
        Diagnostic::make_fields(&[(FIELD_OPTION, option)]),
    )
}

/// `SSH_SEND_EXIT_STATUS` with a negative code. Takes the code;
/// rendering lives here.
pub fn send_exit_status_negative(code: i64) -> Diagnostic {
    let message = format!("SSH_SEND_EXIT_STATUS code must not be negative: {code}");
    Diagnostic::new(
        "ssh.send_exit_status.negative",
        message,
        Diagnostic::make_fields(&[(FIELD_CODE, &code.to_string())]),
    )
}

/// `SSH_SEND_EXIT_STATUS` code outside `u32`. Takes the code;
/// rendering lives here.
pub fn send_exit_status_out_of_range(code: i64) -> Diagnostic {
    let message = format!("SSH_SEND_EXIT_STATUS code out of range: {code}");
    Diagnostic::new(
        "ssh.send_exit_status.out_of_range",
        message,
        Diagnostic::make_fields(&[(FIELD_CODE, &code.to_string())]),
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
            key_path_escapes("SSH_SERVE", "k").key(),
            host_key_resolver_failed("SSH_SERVE", &anyhow::anyhow!("boom")).key(),
            host_key_generate_failed(io_err("boom")).key(),
            host_key_encode_failed(io_err("boom")).key(),
            key_guard_failed("SSH_SERVE", &anyhow::anyhow!("boom")).key(),
            key_path_join_failed("SSH_SERVE", "k", &anyhow::anyhow!("boom")).key(),
            host_key_stat_failed("p", &anyhow::anyhow!("boom")).key(),
            host_key_read_failed("SSH_SERVE", "p", &anyhow::anyhow!("boom")).key(),
            host_key_parse_failed("SSH_SERVE", "p", io_err("boom")).key(),
            host_key_write_failed(&anyhow::anyhow!("boom")).key(),
            host_key_must_be_ed25519("SSH_SERVE", "p", &"Ed25519").key(),
            host_key_insecure_permissions("SSH_SERVE", "p", 0o644).key(),
            pump_output_lock_poisoned().key(),
            pump_input_read_failed(&err).key(),
            pump_worker_panicked("input worker").key(),
            pump_interrupted_by_cancellation().key(),
            pump_source_read_failed(&err).key(),
            pump_target_lock_poisoned().key(),
            pump_func_input_read_failed("SSH run", &err).key(),
            pump_output_write_failed(io_err("boom")).key(),
            pump_output_flush_failed(io_err("boom")).key(),
            pump_borrow_input_failed(pump_output_lock_poisoned()).key(),
            pump_borrow_output_failed(pump_output_lock_poisoned()).key(),
            pump_wait_runtime_failed(io_err("boom")).key(),
            pump_borrow_source_failed(pump_output_lock_poisoned()).key(),
            pump_borrow_target_failed(pump_output_lock_poisoned()).key(),
            pump_target_write_failed(io_err("boom")).key(),
            pump_target_flush_failed(io_err("boom")).key(),
            authentication_rejected("SSH_EXEC").key(),
            dial_failed("SSH_EXEC", &anyhow::anyhow!("boom")).key(),
            auth_exchange_failed("SSH_EXEC", io_err("boom")).key(),
            channel_open_failed("SSH_EXEC", io_err("boom")).key(),
            exec_request_failed(io_err("boom")).key(),
            shell_request_failed(io_err("boom")).key(),
            listener_prepare_failed(io_err("boom")).key(),
            listener_handoff_failed(io_err("boom")).key(),
            listener_failed(io_err("boom")).key(),
            client_runtime_build_failed(io_err("boom")).key(),
            session_pump_already_finished().key(),
            session_wire_queue_saturated().key(),
            session_wire_writer_gone().key(),
            session_already_pumped().key(),
            channel_data_unknown().key(),
            channel_pump_gone().key(),
            channel_wire_gone_eof_close().key(),
            channel_request_unknown().key(),
            channel_already_announced().key(),
            run_child_read_failed("stdout", &err).key(),
            run_output_write_failed(io_err("boom")).key(),
            run_output_flush_failed(io_err("boom")).key(),
            run_output_lock_poisoned().key(),
            run_needs_program().key(),
            run_spawn_lost_stderr().key(),
            run_interrupted_by_cancellation().key(),
            run_borrow_input_failed(pump_output_lock_poisoned()).key(),
            run_borrow_output_failed(pump_output_lock_poisoned()).key(),
            run_borrow_error_failed(pump_output_lock_poisoned()).key(),
            run_spawn_failed(pump_output_lock_poisoned()).key(),
            pty_output_lock_poisoned().key(),
            pty_interrupted_by_cancellation().key(),
            pty_borrow_input_failed(pump_output_lock_poisoned()).key(),
            pty_borrow_output_failed(pump_output_lock_poisoned()).key(),
            pty_session_init_write_failed(io_err("boom")).key(),
            pty_session_init_flush_failed(io_err("boom")).key(),
            dequeue_task_cancelled("SSH_DEQUEUE").key(),
            dequeue_server_closed("SSH_DEQUEUE").key(),
            dequeue_requires_async().key(),
            accept_requires_async().key(),
            pump_channel_requires_async().key(),
            connect_requires_async().key(),
            connect_offline().key(),
            connect_username_empty().key(),
            pty_run_requires_async().key(),
            run_requires_async().key(),
            exec_needs_program().key(),
            exec_username_empty().key(),
            exec_offline().key(),
            serve_option_empty("username").key(),
            serve_option_required("username").key(),
            exec_option_must_be_positive("timeout").key(),
            send_exit_status_negative(-1).key(),
            send_exit_status_out_of_range(1 << 40).key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn shared_modes_keep_one_key() {
        assert_eq!(
            authentication_rejected("SSH_EXEC").key(),
            authentication_rejected("SSH_CONNECT").key()
        );
        assert_eq!(
            dial_failed("SSH_EXEC", &anyhow::anyhow!("boom")).key(),
            dial_failed("SSH_CONNECT", &anyhow::anyhow!("boom")).key()
        );
        assert_eq!(
            auth_exchange_failed("SSH_EXEC", io_err("boom")).key(),
            auth_exchange_failed("SSH_CONNECT", io_err("boom")).key()
        );
        assert_eq!(
            channel_open_failed("SSH_EXEC", io_err("boom")).key(),
            channel_open_failed("SSH_CONNECT", io_err("boom")).key()
        );
        assert_eq!(
            dequeue_task_cancelled("SSH_DEQUEUE").key(),
            dequeue_task_cancelled("SSH_ACCEPT").key()
        );
        assert_eq!(
            dequeue_server_closed("SSH_DEQUEUE").key(),
            dequeue_server_closed("SSH_ACCEPT").key()
        );
        assert_eq!(
            serve_option_empty("username").key(),
            serve_option_empty("password").key()
        );
        assert_eq!(
            serve_option_required("username").key(),
            serve_option_required("password").key()
        );
        assert_eq!(
            exec_option_must_be_positive("timeout").key(),
            exec_option_must_be_positive("max_bytes").key()
        );
    }

    #[test]
    fn messages_match_canonical_wording() {
        let err = io_err("boom");
        assert_eq!(
            key_path_escapes("SSH_SERVE", "k").message(),
            "SSH_SERVE key_path escapes the workspace: k"
        );
        assert_eq!(
            host_key_resolver_failed("SSH_SERVE", &anyhow::anyhow!("boom")).message(),
            "SSH_SERVE cannot open the workspace resolver"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(host_key_resolver_failed(
                    "SSH_SERVE",
                    &anyhow::anyhow!("boom")
                ))
            ),
            "SSH_SERVE cannot open the workspace resolver: boom"
        );
        assert_eq!(
            host_key_generate_failed(io_err("boom")).message(),
            "generate Ed25519 host key"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(host_key_generate_failed(io_err("boom")))
            ),
            "generate Ed25519 host key: boom"
        );
        assert_eq!(
            host_key_encode_failed(io_err("boom")).message(),
            "encode Ed25519 host key"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(host_key_encode_failed(io_err("boom")))
            ),
            "encode Ed25519 host key: boom"
        );
        assert_eq!(
            key_guard_failed("SSH_SERVE", &anyhow::anyhow!("boom")).message(),
            "SSH_SERVE cannot guard the workspace root"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(key_guard_failed("SSH_SERVE", &anyhow::anyhow!("boom")))
            ),
            "SSH_SERVE cannot guard the workspace root: boom"
        );
        assert_eq!(
            key_path_join_failed("SSH_SERVE", "k", &anyhow::anyhow!("boom")).message(),
            "SSH_SERVE key_path escapes the workspace: k"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(key_path_join_failed(
                    "SSH_SERVE",
                    "k",
                    &anyhow::anyhow!("boom")
                ))
            ),
            "SSH_SERVE key_path escapes the workspace: k: boom"
        );
        assert_eq!(
            host_key_stat_failed("p", &anyhow::anyhow!("boom")).message(),
            "cannot stat host key p"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(host_key_stat_failed("p", &anyhow::anyhow!("boom")))
            ),
            "cannot stat host key p: boom"
        );
        assert_eq!(
            host_key_read_failed("SSH_SERVE", "p", &anyhow::anyhow!("boom")).message(),
            "SSH_SERVE cannot read host key p"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(host_key_read_failed(
                    "SSH_SERVE",
                    "p",
                    &anyhow::anyhow!("boom")
                ))
            ),
            "SSH_SERVE cannot read host key p: boom"
        );
        assert_eq!(
            host_key_parse_failed("SSH_SERVE", "p", io_err("boom")).message(),
            "SSH_SERVE cannot parse host key p"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(host_key_parse_failed("SSH_SERVE", "p", io_err("boom")))
            ),
            "SSH_SERVE cannot parse host key p: boom"
        );
        assert_eq!(
            host_key_write_failed(&anyhow::anyhow!("boom")).message(),
            "boom"
        );
        assert_eq!(
            host_key_must_be_ed25519("SSH_SERVE", "p", &"Ed25519").message(),
            "SSH_SERVE host key p must be Ed25519, found \"Ed25519\""
        );
        assert_eq!(
            host_key_insecure_permissions("SSH_SERVE", "p", 0o644).message(),
            "SSH_SERVE insecure permissions on host key file p: expected 0600, got 0644"
        );
        assert_eq!(
            pump_output_lock_poisoned().message(),
            "SSH pump output lock poisoned"
        );
        assert_eq!(
            pump_input_read_failed(&err).message(),
            "SSH pump input pipe read failed: boom"
        );
        assert_eq!(
            pump_worker_panicked("input worker").message(),
            "SSH pump input worker panicked"
        );
        assert_eq!(
            pump_interrupted_by_cancellation().message(),
            "SSH pump interrupted by cancellation"
        );
        assert_eq!(
            pump_source_read_failed(&err).message(),
            "SSH_PUMP source pipe read failed: boom"
        );
        assert_eq!(
            pump_target_lock_poisoned().message(),
            "SSH_PUMP target lock poisoned"
        );
        assert_eq!(
            pump_func_input_read_failed("SSH run", &err).message(),
            "SSH run input pipe read failed: boom"
        );
        assert_eq!(
            pump_output_write_failed(io_err("boom")).message(),
            "SSH pump output pipe write failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(pump_output_write_failed(io_err("boom")))
            ),
            "SSH pump output pipe write failed: boom"
        );
        assert_eq!(
            pump_output_flush_failed(io_err("boom")).message(),
            "SSH pump output pipe flush failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(pump_output_flush_failed(io_err("boom")))
            ),
            "SSH pump output pipe flush failed: boom"
        );
        assert_eq!(
            pump_borrow_input_failed(pump_output_lock_poisoned()).message(),
            "SSH pump cannot borrow the input pipe"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(pump_borrow_input_failed(pump_output_lock_poisoned()))
            ),
            "SSH pump cannot borrow the input pipe: SSH pump output lock poisoned"
        );
        assert_eq!(
            pump_borrow_output_failed(pump_output_lock_poisoned()).message(),
            "SSH pump cannot borrow the output pipe"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(pump_borrow_output_failed(pump_output_lock_poisoned()))
            ),
            "SSH pump cannot borrow the output pipe: SSH pump output lock poisoned"
        );
        assert_eq!(
            pump_wait_runtime_failed(io_err("boom")).message(),
            "SSH pump wait runtime failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(pump_wait_runtime_failed(io_err("boom")))
            ),
            "SSH pump wait runtime failed: boom"
        );
        assert_eq!(
            pump_borrow_source_failed(pump_output_lock_poisoned()).message(),
            "SSH_PUMP cannot borrow the source pipe"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(pump_borrow_source_failed(pump_output_lock_poisoned()))
            ),
            "SSH_PUMP cannot borrow the source pipe: SSH pump output lock poisoned"
        );
        assert_eq!(
            pump_borrow_target_failed(pump_output_lock_poisoned()).message(),
            "SSH_PUMP cannot borrow the target pipe"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(pump_borrow_target_failed(pump_output_lock_poisoned()))
            ),
            "SSH_PUMP cannot borrow the target pipe: SSH pump output lock poisoned"
        );
        assert_eq!(
            pump_target_write_failed(io_err("boom")).message(),
            "SSH_PUMP target pipe write failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(pump_target_write_failed(io_err("boom")))
            ),
            "SSH_PUMP target pipe write failed: boom"
        );
        assert_eq!(
            pump_target_flush_failed(io_err("boom")).message(),
            "SSH_PUMP target pipe flush failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(pump_target_flush_failed(io_err("boom")))
            ),
            "SSH_PUMP target pipe flush failed: boom"
        );
        assert_eq!(
            authentication_rejected("SSH_EXEC").message(),
            "SSH_EXEC authentication rejected"
        );
        assert_eq!(
            dial_failed("SSH_EXEC", &anyhow::anyhow!("boom")).message(),
            "SSH_EXEC dial failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(dial_failed("SSH_EXEC", &anyhow::anyhow!("boom")))
            ),
            "SSH_EXEC dial failed: boom"
        );
        assert_eq!(
            auth_exchange_failed("SSH_EXEC", io_err("boom")).message(),
            "SSH_EXEC authentication exchange failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(auth_exchange_failed("SSH_EXEC", io_err("boom")))
            ),
            "SSH_EXEC authentication exchange failed: boom"
        );
        assert_eq!(
            channel_open_failed("SSH_EXEC", io_err("boom")).message(),
            "SSH_EXEC channel open failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(channel_open_failed("SSH_EXEC", io_err("boom")))
            ),
            "SSH_EXEC channel open failed: boom"
        );
        assert_eq!(
            exec_request_failed(io_err("boom")).message(),
            "SSH_EXEC exec request failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(exec_request_failed(io_err("boom")))
            ),
            "SSH_EXEC exec request failed: boom"
        );
        assert_eq!(
            shell_request_failed(io_err("boom")).message(),
            "SSH_CONNECT shell request failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(shell_request_failed(io_err("boom")))
            ),
            "SSH_CONNECT shell request failed: boom"
        );
        assert_eq!(
            listener_prepare_failed(io_err("boom")).message(),
            "prepare listener for the runtime"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(listener_prepare_failed(io_err("boom")))
            ),
            "prepare listener for the runtime: boom"
        );
        assert_eq!(
            listener_handoff_failed(io_err("boom")).message(),
            "hand listener to the runtime"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(listener_handoff_failed(io_err("boom")))
            ),
            "hand listener to the runtime: boom"
        );
        assert_eq!(
            listener_failed(io_err("boom")).message(),
            "SSH listener failed"
        );
        assert_eq!(
            format!("{:#}", anyhow::Error::new(listener_failed(io_err("boom")))),
            "SSH listener failed: boom"
        );
        assert_eq!(
            client_runtime_build_failed(io_err("boom")).message(),
            "build SSH client runtime"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(client_runtime_build_failed(io_err("boom")))
            ),
            "build SSH client runtime: boom"
        );
        assert_eq!(
            session_pump_already_finished().message(),
            "SSH session pump already finished"
        );
        assert_eq!(
            session_wire_queue_saturated().message(),
            "SSH session wire queue saturated under backpressure"
        );
        assert_eq!(
            session_wire_writer_gone().message(),
            "SSH session wire writer is gone"
        );
        assert_eq!(
            session_already_pumped().message(),
            "SSH session already pumped"
        );
        assert_eq!(channel_data_unknown().message(), "data for unknown channel");
        assert_eq!(channel_pump_gone().message(), "pump went away");
        assert_eq!(
            channel_wire_gone_eof_close().message(),
            "wire gone during EOF close"
        );
        assert_eq!(
            channel_request_unknown().message(),
            "request for unknown channel"
        );
        assert_eq!(
            channel_already_announced().message(),
            "channel already announced"
        );
        assert_eq!(
            run_child_read_failed("stdout", &err).message(),
            "SSH run child stdout read failed: boom"
        );
        assert_eq!(
            run_output_write_failed(io_err("boom")).message(),
            "SSH run output pipe write failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(run_output_write_failed(io_err("boom")))
            ),
            "SSH run output pipe write failed: boom"
        );
        assert_eq!(
            run_output_flush_failed(io_err("boom")).message(),
            "SSH run output pipe flush failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(run_output_flush_failed(io_err("boom")))
            ),
            "SSH run output pipe flush failed: boom"
        );
        assert_eq!(
            run_output_lock_poisoned().message(),
            "SSH run output lock poisoned"
        );
        assert_eq!(
            run_needs_program().message(),
            "SSH_RUN needs at least a program"
        );
        assert_eq!(
            run_spawn_lost_stderr().message(),
            "SSH run spawn lost stderr"
        );
        assert_eq!(
            run_interrupted_by_cancellation().message(),
            "SSH run interrupted by cancellation"
        );
        assert_eq!(
            run_borrow_input_failed(pump_output_lock_poisoned()).message(),
            "SSH run cannot borrow the input pipe"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(run_borrow_input_failed(pump_output_lock_poisoned()))
            ),
            "SSH run cannot borrow the input pipe: SSH pump output lock poisoned"
        );
        assert_eq!(
            run_borrow_output_failed(pump_output_lock_poisoned()).message(),
            "SSH run cannot borrow the output pipe"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(run_borrow_output_failed(pump_output_lock_poisoned()))
            ),
            "SSH run cannot borrow the output pipe: SSH pump output lock poisoned"
        );
        assert_eq!(
            run_borrow_error_failed(pump_output_lock_poisoned()).message(),
            "SSH run cannot borrow the error pipe"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(run_borrow_error_failed(pump_output_lock_poisoned()))
            ),
            "SSH run cannot borrow the error pipe: SSH pump output lock poisoned"
        );
        assert_eq!(
            run_spawn_failed(pump_output_lock_poisoned()).message(),
            "SSH run spawn failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(run_spawn_failed(pump_output_lock_poisoned()))
            ),
            "SSH run spawn failed: SSH pump output lock poisoned"
        );
        assert_eq!(
            pty_output_lock_poisoned().message(),
            "SSH pty output lock poisoned"
        );
        assert_eq!(
            pty_interrupted_by_cancellation().message(),
            "SSH pty run interrupted by cancellation"
        );
        assert_eq!(
            pty_borrow_input_failed(pump_output_lock_poisoned()).message(),
            "SSH pty cannot borrow the input pipe"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(pty_borrow_input_failed(pump_output_lock_poisoned()))
            ),
            "SSH pty cannot borrow the input pipe: SSH pump output lock poisoned"
        );
        assert_eq!(
            pty_borrow_output_failed(pump_output_lock_poisoned()).message(),
            "SSH pty cannot borrow the output pipe"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(pty_borrow_output_failed(pump_output_lock_poisoned()))
            ),
            "SSH pty cannot borrow the output pipe: SSH pump output lock poisoned"
        );
        assert_eq!(
            pty_session_init_write_failed(io_err("boom")).message(),
            "SSH pty session init write failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(pty_session_init_write_failed(io_err("boom")))
            ),
            "SSH pty session init write failed: boom"
        );
        assert_eq!(
            pty_session_init_flush_failed(io_err("boom")).message(),
            "SSH pty session init flush failed"
        );
        assert_eq!(
            format!(
                "{:#}",
                anyhow::Error::new(pty_session_init_flush_failed(io_err("boom")))
            ),
            "SSH pty session init flush failed: boom"
        );
        assert_eq!(
            dequeue_task_cancelled("SSH_DEQUEUE").message(),
            "SSH_DEQUEUE: task cancelled"
        );
        assert_eq!(
            dequeue_server_closed("SSH_ACCEPT").message(),
            "SSH_ACCEPT: server is closed"
        );
        assert_eq!(
            dequeue_requires_async().message(),
            "SSH_DEQUEUE requires ASYNC: wrap it as LET $t: HANDLE<T> = ASYNC { SSH_DEQUEUE($server.server) }"
        );
        assert_eq!(
            accept_requires_async().message(),
            "SSH_ACCEPT requires ASYNC: wrap it as LET $t: HANDLE<T> = ASYNC { SSH_ACCEPT($server, $in, $out) }"
        );
        assert_eq!(
            pump_channel_requires_async().message(),
            "SSH_PUMP_CHANNEL requires ASYNC: pump it in its own task after SSH_DEQUEUE"
        );
        assert_eq!(
            connect_requires_async().message(),
            "SSH_CONNECT requires ASYNC: run it in its own task beside the SSH_PUMP tasks"
        );
        assert_eq!(
            connect_offline().message(),
            "SSH_CONNECT failed: engine running in --offline mode"
        );
        assert_eq!(
            connect_username_empty().message(),
            "SSH_CONNECT username must not be empty"
        );
        assert_eq!(
            pty_run_requires_async().message(),
            "SSH_PTY_RUN requires ASYNC: run it in its own task beside the session pump task"
        );
        assert_eq!(
            run_requires_async().message(),
            "SSH_RUN requires ASYNC: run it in its own task beside the session pump task"
        );
        assert_eq!(
            exec_needs_program().message(),
            "SSH_EXEC needs at least a program"
        );
        assert_eq!(
            exec_username_empty().message(),
            "SSH_EXEC username must not be empty"
        );
        assert_eq!(
            exec_offline().message(),
            "SSH_EXEC failed: engine running in --offline mode"
        );
        assert_eq!(
            serve_option_empty("username").message(),
            "SSH_SERVE option 'username' must not be empty"
        );
        assert_eq!(
            serve_option_required("password").message(),
            "SSH_SERVE option 'password' is required"
        );
        assert_eq!(
            exec_option_must_be_positive("timeout").message(),
            "SSH_EXEC option 'timeout' must be positive"
        );
        assert_eq!(
            send_exit_status_negative(-1).message(),
            "SSH_SEND_EXIT_STATUS code must not be negative: -1"
        );
        assert_eq!(
            send_exit_status_out_of_range(1 << 40).message(),
            "SSH_SEND_EXIT_STATUS code out of range: 1099511627776"
        );
    }
}
