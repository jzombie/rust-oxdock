//! Template-expansion errors behind single constructors with stable keys.
//!
//! This module owns every expansion template and key (`proc.expand.*`).
//! It builds on the generic [`oxdock_diagnostics::Diagnostic`] transport
//! but owns its wording: changing expansion text happens here, never at
//! the call sites. Call sites pass structured data only, never
//! `format!` strings. Suggestion hints render inline in the message
//! (never via `with_hint`, avoiding double-print in `Display`); the
//! `did you mean` shape lives here, call sites pass only the scope.

use oxdock_diagnostics::Diagnostic;
use oxdock_fs::env as oxdock_env;
use oxdock_parser::{
    KEYWORD_INHERIT_ENV, KEYWORD_LET, KEYWORD_RUN, PERMIT_TYPE_NAME, PIPE_TYPE_NAME,
    SEMAPHORE_TYPE_NAME, STRING_TYPE_NAME,
};

/// Structured field keys. Single spelling for the whole module:
/// constructors reference these, never literals, so a key rename is
/// one edit.
const FIELD_KEY: &str = "key";
const FIELD_NAME: &str = "name";
const FIELD_MODULE: &str = "module";
const FIELD_FUNC: &str = "func";
const FIELD_ARG: &str = "arg";
const FIELD_ENTRY: &str = "entry";
const FIELD_ROOT: &str = "root";
const FIELD_SEGMENT: &str = "segment";
const FIELD_INDEX: &str = "index";
const FIELD_LEN: &str = "len";
const FIELD_LIMIT: &str = "limit";
const FIELD_COMMAND: &str = "command";
const FIELD_STATUS: &str = "status";
const FIELD_ARGV: &str = "argv";
const FIELD_PROGRAM: &str = "program";
const FIELD_ERR: &str = "err";
const FIELD_REASON: &str = "reason";
const FIELD_STREAM: &str = "stream";

/// Suggestion scope for an undefined `$var` miss: the name also lives
/// in the step environment or the override table.
pub enum ScriptSuggest {
    /// `env:{name}` holds a value.
    Env,
    /// A step override named `{name}` exists.
    StepOverride,
}

/// Suggestion scope for a missing step-override miss: the name also
/// lives in the script variables or the environment.
pub enum OverrideSuggest {
    /// `${name}` holds a value.
    ScriptVar,
    /// `env:{name}` holds a value.
    EnvVar,
}

/// Render one `did you mean` hint. Single owner for the shape; callers
/// pass the already-derived spelling and scope label.
fn did_you_mean(spelling: &str, scope: &str) -> String {
    format!("; did you mean '{spelling}' ({scope})?")
}

/// Undefined `env:KEY`. Takes the key plus whether a same-named script
/// variable exists; the hint shape lives here.
pub fn undefined_env_var(key: &str, suggest_script_var: bool) -> Diagnostic {
    let hint = suggest_script_var
        .then(|| did_you_mean(&format!("${key}"), "script variable"))
        .unwrap_or_default();
    let message = format!("undefined environment variable: '{key}'{hint}");
    Diagnostic::new(
        "proc.expand.undefined_env_var",
        message,
        Diagnostic::make_fields(&[(FIELD_KEY, key)]),
    )
}

/// Undefined `$var`. Takes the name plus an optional suggestion scope;
/// hinted and bare share one key and one mode.
pub fn undefined_script_var(rest: &str, suggest: Option<ScriptSuggest>) -> Diagnostic {
    let hint = match suggest {
        Some(ScriptSuggest::Env) => did_you_mean(&format!("env:{rest}"), "environment variable"),
        Some(ScriptSuggest::StepOverride) => did_you_mean(rest, "step override"),
        None => String::new(),
    };
    let message = format!("undefined script variable: '${rest}'{hint}");
    Diagnostic::new(
        "proc.expand.undefined_script_var",
        message,
        Diagnostic::make_fields(&[(FIELD_NAME, rest)]),
    )
}

/// Missing step-override argument. Takes the name plus an optional
/// suggestion scope; hinted and bare share one key and one mode.
pub fn missing_step_override(name: &str, suggest: Option<OverrideSuggest>) -> Diagnostic {
    let hint = match suggest {
        Some(OverrideSuggest::ScriptVar) => did_you_mean(&format!("${name}"), "script variable"),
        Some(OverrideSuggest::EnvVar) => {
            did_you_mean(&format!("env:{name}"), "environment variable")
        }
        None => String::new(),
    };
    let message = format!("missing required step override argument: '{name}'{hint}");
    Diagnostic::new(
        "proc.expand.missing_step_override",
        message,
        Diagnostic::make_fields(&[(FIELD_NAME, name)]),
    )
}

/// Placeholder in no resolvable shape. Takes the raw key; quoting
/// lives here.
pub fn invalid_placeholder_format(key: &str) -> Diagnostic {
    let message = format!(
        "invalid placeholder format '{key}': script variables must start with '$' and environment variables with 'env:'"
    );
    Diagnostic::new(
        "proc.expand.invalid_placeholder_format",
        message,
        Diagnostic::make_fields(&[(FIELD_KEY, key)]),
    )
}

/// Placeholder call with no registered resolver. Takes the module and
/// function names; quoting lives here.
pub fn call_without_registry(module: &str, name: &str) -> Diagnostic {
    let message = format!(
        "placeholder call '{module}::{name}' has no function registry; register one with with_call_resolver"
    );
    Diagnostic::new(
        "proc.expand.call_without_registry",
        message,
        Diagnostic::make_fields(&[(FIELD_MODULE, module), (FIELD_FUNC, name)]),
    )
}

/// Placeholder call argument nesting past the literal depth cap. Takes
/// the cap; rendering lives here.
pub fn call_arg_too_deep(limit: usize) -> Diagnostic {
    let message = format!("placeholder call argument nests too deep (limit {limit})");
    Diagnostic::new(
        "proc.expand.call_arg_too_deep",
        message,
        Diagnostic::make_fields(&[(FIELD_LIMIT, &limit.to_string())]),
    )
}

/// Empty `$` variable reference in a call argument. Parameterless: the
/// wording is fixed.
pub fn call_arg_empty_var() -> Diagnostic {
    Diagnostic::new(
        "proc.expand.call_arg_empty_var",
        "invalid placeholder call argument: empty variable reference".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Unresolvable placeholder call argument. Takes the raw source;
/// quoting lives here.
pub fn call_arg_invalid(src: &str) -> Diagnostic {
    let message = format!(
        "invalid placeholder call argument '{src}': use $var.path, a string, int, float, or bool literal, or a {{key: value}} or [item] literal"
    );
    Diagnostic::new(
        "proc.expand.call_arg_invalid",
        message,
        Diagnostic::make_fields(&[(FIELD_ARG, src)]),
    )
}

/// Nested calls inside placeholder arguments. Parameterless: the
/// wording is fixed.
pub fn call_arg_nested_calls() -> Diagnostic {
    Diagnostic::new(
        "proc.expand.call_arg_nested_calls",
        "nested calls are not supported in placeholder arguments".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Map literal entry without exactly one top-level `:`. Takes the raw
/// entry; quoting lives here.
pub fn map_entry_invalid(entry: &str) -> Diagnostic {
    let message = format!("invalid map literal entry '{entry}': use key: value");
    Diagnostic::new(
        "proc.expand.map_entry_invalid",
        message,
        Diagnostic::make_fields(&[(FIELD_ENTRY, entry)]),
    )
}

/// Duplicate key in a map literal. Takes the key; quoting lives here.
pub fn map_duplicate_key(key: &str) -> Diagnostic {
    let message = format!("duplicate key '{key}' in map literal");
    Diagnostic::new(
        "proc.expand.map_duplicate_key",
        message,
        Diagnostic::make_fields(&[(FIELD_KEY, key)]),
    )
}

/// Missing property on a map during key-path traversal. Takes the root
/// variable and the segment; quoting lives here.
pub fn key_path_missing_property(root: &str, segment: &str) -> Diagnostic {
    let message = format!("property '{segment}' not found on object '${root}'");
    Diagnostic::new(
        "proc.expand.key_path_missing_property",
        message,
        Diagnostic::make_fields(&[(FIELD_ROOT, root), (FIELD_SEGMENT, segment)]),
    )
}

/// Non-numeric list index during key-path traversal. Takes the root
/// variable and the segment; quoting lives here.
pub fn key_path_bad_index(root: &str, segment: &str) -> Diagnostic {
    let message = format!("invalid array index '{segment}' on list '${root}'");
    Diagnostic::new(
        "proc.expand.key_path_bad_index",
        message,
        Diagnostic::make_fields(&[(FIELD_ROOT, root), (FIELD_SEGMENT, segment)]),
    )
}

/// Out-of-bounds list index during key-path traversal. Takes the root
/// variable, the index, and the list length; rendering lives here.
pub fn key_path_index_out_of_bounds(root: &str, idx: usize, len: usize) -> Diagnostic {
    let message = format!("index {idx} out of bounds for list '${root}' (len: {len})");
    Diagnostic::new(
        "proc.expand.key_path_index_out_of_bounds",
        message,
        Diagnostic::make_fields(&[
            (FIELD_ROOT, root),
            (FIELD_INDEX, &idx.to_string()),
            (FIELD_LEN, &len.to_string()),
        ]),
    )
}

/// Property access on a primitive during key-path traversal. Takes the
/// root variable and the segment; quoting lives here.
pub fn key_path_on_primitive(root: &str, segment: &str) -> Diagnostic {
    let message = format!("cannot access property '{segment}' on primitive value of '${root}'");
    Diagnostic::new(
        "proc.expand.key_path_on_primitive",
        message,
        Diagnostic::make_fields(&[(FIELD_ROOT, root), (FIELD_SEGMENT, segment)]),
    )
}

/// `PIPE` value interpolated as text. Parameterless: the wording and
/// the `LET` capture guidance live here.
pub fn render_pipe() -> Diagnostic {
    Diagnostic::new(
        "proc.expand.render_pipe",
        format!(
            "cannot render {PIPE_TYPE_NAME} as {STRING_TYPE_NAME}: pipes carry streams between steps, not text. Capture the producing step's output with {KEYWORD_LET} $var: {STRING_TYPE_NAME} = ... instead."
        ),
        Diagnostic::make_fields(&[]),
    )
}

/// `SEMAPHORE` value interpolated as text. Parameterless: the wording
/// lives here.
pub fn render_semaphore() -> Diagnostic {
    Diagnostic::new(
        "proc.expand.render_semaphore",
        format!(
            "cannot render {SEMAPHORE_TYPE_NAME} as {STRING_TYPE_NAME}: semaphores coordinate task access between steps, not text."
        ),
        Diagnostic::make_fields(&[]),
    )
}

/// `PERMIT` value interpolated as text. Parameterless: the wording
/// lives here.
pub fn render_permit() -> Diagnostic {
    Diagnostic::new(
        "proc.expand.render_permit",
        format!(
            "cannot render {PERMIT_TYPE_NAME} as {STRING_TYPE_NAME}: permits grant scoped task access, not text."
        ),
        Diagnostic::make_fields(&[]),
    )
}

/// `INTERPRETER` lookup under Miri isolation. Parameterless: the
/// wording is fixed.
pub fn run_interpreter_unsupported_miri() -> Diagnostic {
    Diagnostic::new(
        "proc.run.interpreter_unsupported_miri",
        "INTERPRETER is unsupported under Miri isolation".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Empty `RUN` exec argv. Parameterless: the wording is fixed.
pub fn run_exec_requires_argv() -> Diagnostic {
    Diagnostic::new(
        "proc.run.exec_requires_argv",
        format!("{KEYWORD_RUN} exec form requires at least one argument"),
        Diagnostic::make_fields(&[]),
    )
}

/// `RUN` exec form with an empty program. Parameterless: the wording
/// is fixed.
pub fn run_exec_requires_program() -> Diagnostic {
    Diagnostic::new(
        "proc.run.exec_requires_program",
        format!("{KEYWORD_RUN} exec form requires a non-empty executable"),
        Diagnostic::make_fields(&[]),
    )
}

/// Spawned command exiting non-zero. Takes the command (any
/// `Debug`: `std::process::Command`, argv slice, or script text) and
/// the wait status; both renderings live here.
pub fn run_command_failed(
    desc: &impl std::fmt::Debug,
    status: &std::process::ExitStatus,
) -> Diagnostic {
    let desc_text = format!("{desc:?}");
    let status_text = status.to_string();
    let message = format!("command {desc_text} failed with status {status_text}");
    Diagnostic::new(
        "proc.run.command_failed",
        message,
        Diagnostic::make_fields(&[
            (FIELD_COMMAND, desc_text.as_str()),
            (FIELD_STATUS, status_text.as_str()),
        ]),
    )
}

/// Current-executable lookup failed. Takes the I/O failure;
/// rendering lives here. The typed I/O error rides as the chained
/// source so `Error::source` and `{err:#}` chains keep it.
pub fn run_current_exe_failed(err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("cannot determine current executable: {err_text}");
    Diagnostic::new(
        "proc.run.current_exe_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_ERR, err_text.as_str())]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::new(
        err.kind(),
        err_text,
    )))
}

/// Spawning or waiting on a shell command failed. Takes the command
/// (any `Debug`: `std::process::Command`) and the I/O failure;
/// rendering lives here. Shared by the status-wait and piped-output
/// paths: one key, one mode. The typed I/O error rides as the
/// chained source.
pub fn run_failed(cmd: &impl std::fmt::Debug, err: &std::io::Error) -> Diagnostic {
    let cmd_text = format!("{cmd:?}");
    let err_text = err.to_string();
    let message = format!("failed to run {cmd_text}: {err_text}");
    Diagnostic::new(
        "proc.run.run_failed",
        message,
        Diagnostic::make_fields(&[
            (FIELD_COMMAND, cmd_text.as_str()),
            (FIELD_ERR, err_text.as_str()),
        ]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::new(
        err.kind(),
        err_text,
    )))
}

/// Spawning a shell command failed. Takes the command (any `Debug`:
/// `std::process::Command`) and the I/O failure; rendering lives
/// here. The typed I/O error rides as the chained source.
pub fn spawn_failed(cmd: &impl std::fmt::Debug, err: &std::io::Error) -> Diagnostic {
    let cmd_text = format!("{cmd:?}");
    let err_text = err.to_string();
    let message = format!("failed to spawn {cmd_text}: {err_text}");
    Diagnostic::new(
        "proc.run.spawn_failed",
        message,
        Diagnostic::make_fields(&[
            (FIELD_COMMAND, cmd_text.as_str()),
            (FIELD_ERR, err_text.as_str()),
        ]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::new(
        err.kind(),
        err_text,
    )))
}

/// Waiting on a spawned child failed. Takes the I/O failure;
/// rendering lives here. Shared by the `try_wait` poll and the
/// blocking `wait`: one key, one mode. The typed I/O error rides
/// as the chained source.
pub fn wait_failed(err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("failed to wait for child process: {err_text}");
    Diagnostic::new(
        "proc.run.wait_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_ERR, err_text.as_str())]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::new(
        err.kind(),
        err_text,
    )))
}

/// Windows job-object assignment failed. Takes the underlying
/// failure; `{reason:#}` formatting lives here so the call site
/// stays `format!`-free. Third-party-feeling OS errors ride the
/// anyhow passthrough and never attach a source.
pub fn job_assign_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "proc.run.job_assign_failed",
        reason_text.clone(),
        Diagnostic::make_fields(&[(FIELD_REASON, reason_text.as_str())]),
    )
}

/// Synthetic Miri-backend failure. Takes the underlying failure;
/// `{reason:#}` formatting lives here so the call site stays
/// `format!`-free. Already-keyed payloads are reclaimed by
/// [`from_anyhow`], so this only names genuinely unkeyed fallout.
pub fn synthetic_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "proc.run.synthetic_failed",
        reason_text.clone(),
        Diagnostic::make_fields(&[(FIELD_REASON, reason_text.as_str())]),
    )
}

/// Reclaim an already-keyed diagnostic from an `anyhow` failure,
/// wrapping anything else as a synthetic-backend failure. Single
/// owner for the Miri-arm mapping so both arms preserve keys
/// instead of restating the downcast.
pub fn from_anyhow(err: anyhow::Error) -> Diagnostic {
    match err.downcast::<Diagnostic>() {
        Ok(diag) => diag,
        Err(err) => synthetic_failed(&err),
    }
}

/// Spawned piped child missing a stdio half. Takes the command (any
/// `Debug`: `std::process::Command`) and the stream name (`stdin`,
/// `stdout`, or `stderr`); rendering lives here. Shared by the
/// piped and piped-with-stderr paths: one key, one mode.
pub fn spawn_pipe_half_missing(cmd: &impl std::fmt::Debug, stream: &str) -> Diagnostic {
    let cmd_text = format!("{cmd:?}");
    let message = format!("spawned process has no piped {stream}: {cmd_text}");
    Diagnostic::new(
        "proc.run.spawn_pipe_half_missing",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, cmd_text.as_str()), (FIELD_STREAM, stream)]),
    )
}

/// Interactive shell window launch failed. Takes the underlying
/// failure; `{reason:#}` formatting lives here so the call site
/// stays `format!`-free. The chained spawn diagnostic rides inline;
/// no source is attached.
pub fn spawn_interactive_window_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to start interactive shell window: {reason_text}");
    Diagnostic::new(
        "proc.run.interactive_spawn_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, reason_text.as_str())]),
    )
}

/// Shell exiting non-zero. Takes the wait status; rendering lives
/// here.
pub fn shell_exited_with_status(status: &std::process::ExitStatus) -> Diagnostic {
    let status_text = status.to_string();
    let message = format!("shell exited with status {status_text}");
    Diagnostic::new(
        "proc.run.shell_exited_with_status",
        message,
        Diagnostic::make_fields(&[(FIELD_STATUS, status_text.as_str())]),
    )
}

/// Interactive shell on a platform without one. Parameterless: the
/// wording is fixed.
pub fn run_interactive_unsupported() -> Diagnostic {
    Diagnostic::new(
        "proc.run.interactive_unsupported",
        "interactive shell unsupported on this platform".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Stdout capture for a background command. Parameterless: the
/// wording is fixed.
pub fn run_bg_capture_unsupported() -> Diagnostic {
    Diagnostic::new(
        "proc.run.bg_capture_unsupported",
        "cannot capture stdout for background command".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Stdout capture for a background command under Miri isolation.
/// Parameterless: the wording is fixed.
pub fn run_bg_capture_unsupported_miri() -> Diagnostic {
    Diagnostic::new(
        "proc.run.bg_capture_unsupported_miri",
        "cannot capture stdout for background command under Miri".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Stdout streaming for a background command under Miri isolation.
/// Parameterless: the wording is fixed.
pub fn run_bg_stdout_stream_unsupported_miri() -> Diagnostic {
    Diagnostic::new(
        "proc.run.bg_stdout_stream_unsupported_miri",
        "stdout streaming not supported for background command under Miri".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Stderr streaming for a background command under Miri isolation.
/// Parameterless: the wording is fixed.
pub fn run_bg_stderr_stream_unsupported_miri() -> Diagnostic {
    Diagnostic::new(
        "proc.run.bg_stderr_stream_unsupported_miri",
        "stderr streaming not supported for background command under Miri".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Native spawn under the Miri synthetic backend. Parameterless: the
/// wording is fixed.
pub fn run_spawn_unsupported_miri() -> Diagnostic {
    Diagnostic::new(
        "proc.run.spawn_unsupported_miri",
        "spawn is not supported under Miri synthetic process backend".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Unguarded write under Miri isolation. Parameterless: the wording
/// is fixed.
pub fn run_unguarded_writes_unsupported_miri() -> Diagnostic {
    Diagnostic::new(
        "proc.run.unguarded_writes_unsupported_miri",
        "unguarded writes not supported in Miri".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Default `run_argv` with no override. Takes the argv; `Debug`
/// rendering lives here.
pub fn run_argv_unimplemented(argv: &[String]) -> Diagnostic {
    let argv_text = format!("{argv:?}");
    let message = format!("run_argv not implemented for argv {argv_text}");
    Diagnostic::new(
        "proc.run.argv_unimplemented",
        message,
        Diagnostic::make_fields(&[(FIELD_ARGV, argv_text.as_str())]),
    )
}

/// Spawn without `PATH` in the script environment. Takes the program;
/// the `PATH` spelling and the `INHERIT_ENV` fix derive from the
/// declared env contract, never literals.
pub fn spawn_requires_path(program: &str) -> Diagnostic {
    let message = format!(
        "cannot resolve '{program}' without {} in the script environment; declare {KEYWORD_INHERIT_ENV} [{}]",
        oxdock_env::os::PATH,
        oxdock_env::os::PATH,
    );
    Diagnostic::new(
        "proc.run.spawn_requires_path",
        message,
        Diagnostic::make_fields(&[(FIELD_PROGRAM, program)]),
    )
}

/// Windows spawn without `SystemRoot` in the script environment. The
/// name spelling and the `INHERIT_ENV` fix derive from the declared
/// env contract, never literals.
pub fn spawn_requires_system_root() -> Diagnostic {
    let message = format!(
        "Windows process execution requires {} in the script environment; declare {KEYWORD_INHERIT_ENV} [{}]",
        oxdock_env::os::windows::SYSTEM_ROOT,
        oxdock_env::os::windows::SYSTEM_ROOT,
    );
    Diagnostic::new(
        "proc.run.spawn_requires_system_root",
        message,
        Diagnostic::make_fields(&[]),
    )
}

/// Mock stdin snapshot losing the lock race. Parameterless: the
/// wording is fixed.
pub fn run_stdin_lock_failed() -> Diagnostic {
    Diagnostic::new(
        "proc.run.stdin_lock_failed",
        "failed to lock stdin".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Capture buffer losing the lock race. Parameterless: the wording
/// is fixed.
pub fn run_capture_lock_poisoned() -> Diagnostic {
    Diagnostic::new(
        "proc.run.capture_lock_poisoned",
        "capture stdout poisoned".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxdock_sys_test_utils::exit_status_from_code as exit_status;

    #[test]
    fn each_failure_has_a_distinct_key() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "x");
        let keys = [
            undefined_env_var("FOO", false).key(),
            undefined_script_var("x", None).key(),
            missing_step_override("n", None).key(),
            invalid_placeholder_format("k").key(),
            call_without_registry("M", "F").key(),
            call_arg_too_deep(8).key(),
            call_arg_empty_var().key(),
            call_arg_invalid("s").key(),
            call_arg_nested_calls().key(),
            map_entry_invalid("e").key(),
            map_duplicate_key("k").key(),
            key_path_missing_property("r", "s").key(),
            key_path_bad_index("r", "s").key(),
            key_path_index_out_of_bounds("r", 5, 2).key(),
            key_path_on_primitive("r", "s").key(),
            render_pipe().key(),
            render_semaphore().key(),
            render_permit().key(),
            run_interpreter_unsupported_miri().key(),
            run_exec_requires_argv().key(),
            run_exec_requires_program().key(),
            shell_exited_with_status(&exit_status(1)).key(),
            run_interactive_unsupported().key(),
            run_bg_capture_unsupported().key(),
            run_bg_capture_unsupported_miri().key(),
            run_bg_stdout_stream_unsupported_miri().key(),
            run_bg_stderr_stream_unsupported_miri().key(),
            run_spawn_unsupported_miri().key(),
            run_unguarded_writes_unsupported_miri().key(),
            run_argv_unimplemented(&["RUN".to_string()]).key(),
            spawn_requires_path("prog").key(),
            spawn_requires_system_root().key(),
            run_stdin_lock_failed().key(),
            run_capture_lock_poisoned().key(),
            run_current_exe_failed(&io_err).key(),
            run_failed(&"echo hi", &io_err).key(),
            spawn_failed(&"echo hi", &io_err).key(),
            wait_failed(&io_err).key(),
            job_assign_failed(&anyhow::anyhow!("boom")).key(),
            synthetic_failed(&anyhow::anyhow!("boom")).key(),
            spawn_pipe_half_missing(&"echo hi", "stdin").key(),
            spawn_interactive_window_failed(&anyhow::anyhow!("boom")).key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }

        // `run_command_failed` renders any `Debug` command description;
        // one key for every description shape.
        assert_eq!(
            run_command_failed(&"echo hi", &exit_status(1)).key(),
            run_command_failed(&vec!["echo".to_string()], &exit_status(1)).key()
        );
    }

    #[test]
    fn hinted_and_bare_share_one_key() {
        assert_eq!(
            undefined_env_var("FOO", false).key(),
            undefined_env_var("FOO", true).key()
        );
        assert_eq!(
            undefined_script_var("x", None).key(),
            undefined_script_var("x", Some(ScriptSuggest::Env)).key()
        );
        assert_eq!(
            missing_step_override("n", None).key(),
            missing_step_override("n", Some(OverrideSuggest::EnvVar)).key()
        );
    }

    #[test]
    fn messages_match_canonical_wording() {
        assert_eq!(
            undefined_env_var("FOO", false).message(),
            "undefined environment variable: 'FOO'"
        );
        assert_eq!(
            undefined_env_var("FOO", true).message(),
            "undefined environment variable: 'FOO'; did you mean '$FOO' (script variable)?"
        );
        assert_eq!(
            undefined_env_var("", false).message(),
            "undefined environment variable: ''"
        );
        assert_eq!(
            undefined_script_var("WHO", Some(ScriptSuggest::Env)).message(),
            "undefined script variable: '$WHO'; did you mean 'env:WHO' (environment variable)?"
        );
        assert_eq!(
            missing_step_override("PORT", Some(OverrideSuggest::ScriptVar)).message(),
            "missing required step override argument: 'PORT'; did you mean '$PORT' (script variable)?"
        );
        assert_eq!(
            missing_step_override("PORT", Some(OverrideSuggest::EnvVar)).message(),
            "missing required step override argument: 'PORT'; did you mean 'env:PORT' (environment variable)?"
        );
        assert_eq!(
            key_path_index_out_of_bounds("r", 5, 2).message(),
            "index 5 out of bounds for list '$r' (len: 2)"
        );
        assert_eq!(
            call_arg_too_deep(8).message(),
            "placeholder call argument nests too deep (limit 8)"
        );
        // `ExitStatus` Display is platform-specific (`exit status: N`
        // on Unix, `exit code: N` on Windows): interpolate the value
        // itself so the template wording is pinned without assuming
        // either rendering.
        let failed_status = exit_status(3);
        assert_eq!(
            run_command_failed(&"echo hi", &failed_status).message(),
            format!("command \"echo hi\" failed with status {failed_status}")
        );
        let exited_status = exit_status(1);
        assert_eq!(
            shell_exited_with_status(&exited_status).message(),
            format!("shell exited with status {exited_status}")
        );
        assert_eq!(
            run_argv_unimplemented(&["RUN".to_string()]).message(),
            "run_argv not implemented for argv [\"RUN\"]"
        );
        assert_eq!(
            spawn_requires_path("prog").message(),
            "cannot resolve 'prog' without PATH in the script environment; declare INHERIT_ENV [PATH]"
        );
        assert_eq!(
            spawn_requires_system_root().message(),
            "Windows process execution requires SystemRoot in the script environment; declare INHERIT_ENV [SystemRoot]"
        );
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
        assert_eq!(
            run_current_exe_failed(&io_err).message(),
            "cannot determine current executable: nope"
        );
        assert_eq!(
            run_failed(&"echo hi", &io_err).message(),
            "failed to run \"echo hi\": nope"
        );
        assert_eq!(
            spawn_failed(&"echo hi", &io_err).message(),
            "failed to spawn \"echo hi\": nope"
        );
        assert_eq!(
            wait_failed(&io_err).message(),
            "failed to wait for child process: nope"
        );
        assert_eq!(
            job_assign_failed(&anyhow::anyhow!("boom")).message(),
            "boom"
        );
        assert_eq!(synthetic_failed(&anyhow::anyhow!("boom")).message(), "boom");
        assert_eq!(
            spawn_pipe_half_missing(&"echo hi", "stdin").message(),
            "spawned process has no piped stdin: \"echo hi\""
        );
        assert_eq!(
            spawn_interactive_window_failed(&anyhow::anyhow!("boom")).message(),
            "failed to start interactive shell window: boom"
        );
    }

    #[test]
    fn from_anyhow_reclaims_keyed_payloads() {
        let keyed = run_exec_requires_argv();
        let key = keyed.key();
        let reclaimed = from_anyhow(anyhow::Error::new(keyed));
        assert_eq!(reclaimed.key(), key);
        assert_eq!(
            reclaimed.message(),
            "RUN exec form requires at least one argument"
        );
        let wrapped = from_anyhow(anyhow::anyhow!("boom"));
        assert_eq!(wrapped.key(), "proc.run.synthetic_failed");
        assert_eq!(wrapped.message(), "boom");
    }

    #[test]
    fn io_ctors_preserve_typed_source_chain() {
        use std::error::Error as _;
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
        for diag in [
            run_current_exe_failed(&err),
            run_failed(&"echo hi", &err),
            spawn_failed(&"echo hi", &err),
            wait_failed(&err),
        ] {
            let source = diag.source().expect("io ctor keeps a source");
            let io_source = source
                .downcast_ref::<std::io::Error>()
                .expect("source is the typed io error");
            assert_eq!(io_source.kind(), std::io::ErrorKind::NotFound);
            assert_eq!(io_source.to_string(), "nope");
        }
    }
}
