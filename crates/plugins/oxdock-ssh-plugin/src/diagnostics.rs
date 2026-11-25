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
        "SSH_DEQUEUE requires ASYNC: wrap it as LET $t: HANDLE<T> = ASYNC { SSH_DEQUEUE($server.server) }"
            .to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_ACCEPT` outside `ASYNC`. Parameterless: the wrap hint lives
/// here.
pub fn accept_requires_async() -> Diagnostic {
    Diagnostic::new(
        "ssh.accept.requires_async",
        "SSH_ACCEPT requires ASYNC: wrap it as LET $t: HANDLE<T> = ASYNC { SSH_ACCEPT($server, $in, $out) }"
            .to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_PUMP_CHANNEL` outside `ASYNC`. Parameterless: the wording is
/// fixed.
pub fn pump_channel_requires_async() -> Diagnostic {
    Diagnostic::new(
        "ssh.pump_channel.requires_async",
        "SSH_PUMP_CHANNEL requires ASYNC: pump it in its own task after SSH_DEQUEUE".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_CONNECT` outside `ASYNC`. Parameterless: the wording is
/// fixed.
pub fn connect_requires_async() -> Diagnostic {
    Diagnostic::new(
        "ssh.connect.requires_async",
        "SSH_CONNECT requires ASYNC: run it in its own task beside the SSH_PUMP tasks".to_string(),
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
        "SSH_PTY_RUN requires ASYNC: run it in its own task beside the session pump task"
            .to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `SSH_RUN` outside `ASYNC`. Parameterless: the wording is fixed.
pub fn run_requires_async() -> Diagnostic {
    Diagnostic::new(
        "ssh.run.requires_async",
        "SSH_RUN requires ASYNC: run it in its own task beside the session pump task".to_string(),
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
            host_key_must_be_ed25519("SSH_SERVE", "p", &"Ed25519").key(),
            host_key_insecure_permissions("SSH_SERVE", "p", 0o644).key(),
            pump_output_lock_poisoned().key(),
            pump_input_read_failed(&err).key(),
            pump_worker_panicked("input worker").key(),
            pump_interrupted_by_cancellation().key(),
            pump_source_read_failed(&err).key(),
            pump_target_lock_poisoned().key(),
            pump_func_input_read_failed("SSH run", &err).key(),
            authentication_rejected("SSH_EXEC").key(),
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
            run_output_lock_poisoned().key(),
            run_needs_program().key(),
            run_spawn_lost_stderr().key(),
            run_interrupted_by_cancellation().key(),
            pty_output_lock_poisoned().key(),
            pty_interrupted_by_cancellation().key(),
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
            authentication_rejected("SSH_EXEC").message(),
            "SSH_EXEC authentication rejected"
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
            pty_output_lock_poisoned().message(),
            "SSH pty output lock poisoned"
        );
        assert_eq!(
            pty_interrupted_by_cancellation().message(),
            "SSH pty run interrupted by cancellation"
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
