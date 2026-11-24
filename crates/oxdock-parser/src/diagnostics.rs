//! DSL spelling diagnostics, owned by `oxdock-parser`.
//!
//! One constructor per diagnostic at the owning function. Each carries
//! a stable `dsl.*` key, structured fields, and a whole-sentence
//! template. Call sites pass decoded values, never `format!` strings
//! and never raw mid-parse text containing `WS_SENTINEL`; the
//! transport sanitizes any residual marker so untrusted input cannot
//! crash release.

use oxdock_diagnostics::Diagnostic;

/// Decode a mid-parse fragment for user output: the transient
/// `WS_SENTINEL` boundary becomes a space. Single owner for the decode
/// rule in this crate.
pub fn decode_spelling_fragment(raw: &str) -> String {
    raw.replace(crate::tag::WS_SENTINEL, " ")
}

/// Second `ALIAS` on the same field.
pub fn alias_duplicate_on_field(field: &str, first: &str, second: &str) -> Diagnostic {
    let message = format!(
        "field '{field}' already has alias '{first}', found second alias '{second}': only one ALIAS per field"
    );
    Diagnostic::new(
        "dsl.alias.duplicate_on_field",
        message,
        Diagnostic::make_fields(&[("field", field), ("first", first), ("second", second)]),
    )
}

/// `ALIAS` without a named field.
pub fn alias_needs_field(got: &str) -> Diagnostic {
    let message = format!("ALIAS needs a named field, found bare {got:?}");
    Diagnostic::new(
        "dsl.alias.needs_field",
        message,
        Diagnostic::make_fields(&[("got", got)]),
    )
}

/// `ALIAS` without a lowercase alias name.
pub fn alias_needs_name() -> Diagnostic {
    Diagnostic::new(
        "dsl.alias.needs_name",
        "ALIAS needs a lowercase alias name".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `ALIAS` name must start lowercase. Takes the decoded offending
/// character and the decoded spelling; the template owns quoting.
pub fn alias_name_lowercase(found: &str, spelling: &str) -> Diagnostic {
    let message = format!("ALIAS name must start lowercase, found {found:?} in {spelling:?}");
    Diagnostic::new(
        "dsl.alias.name_lowercase",
        message,
        Diagnostic::make_fields(&[("found", found), ("spelling", spelling)]),
    )
}

/// Unexpected trailing text after a complete spelling.
pub fn spelling_trailing_text(got: &str) -> Diagnostic {
    let message = format!("unexpected trailing text in type spelling: {got:?}");
    Diagnostic::new(
        "dsl.spelling.trailing_text",
        message,
        Diagnostic::make_fields(&[("found", got)]),
    )
}

/// Generic nesting deeper than the cap. Takes the numeric cap so no
/// `format!` leaks to the call site.
pub fn spelling_max_depth(max: usize, got: &str) -> Diagnostic {
    let message = format!("type spelling exceeds maximum generic depth of {max}: {got:?}");
    Diagnostic::new(
        "dsl.spelling.max_depth",
        message,
        Diagnostic::make_fields(&[("max", &max.to_string()), ("found", got)]),
    )
}

/// Expected a type name but found end of input.
pub fn spelling_expected_type_name() -> Diagnostic {
    Diagnostic::new(
        "dsl.spelling.expected_type_name",
        "expected type name, found end of spelling".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Expected an uppercase type name. Takes the decoded character.
pub fn spelling_expected_uppercase(found: &str) -> Diagnostic {
    let message = format!("expected uppercase type name, found {found:?}");
    Diagnostic::new(
        "dsl.spelling.expected_uppercase",
        message,
        Diagnostic::make_fields(&[("found", found)]),
    )
}

/// Expected `,` or `>` to continue a generic spelling.
pub fn spelling_expected_delimiter(got: &str) -> Diagnostic {
    let message = format!("expected ',' or '>' in type spelling, found {got:?}");
    Diagnostic::new(
        "dsl.spelling.expected_delimiter",
        message,
        Diagnostic::make_fields(&[("found", got)]),
    )
}

/// Unterminated quoted string default.
pub fn spelling_unterminated_string(got: &str) -> Diagnostic {
    let message = format!("unterminated string default in type spelling: {got:?}");
    Diagnostic::new(
        "dsl.spelling.unterminated_string",
        message,
        Diagnostic::make_fields(&[("found", got)]),
    )
}

/// Expected a default literal.
pub fn spelling_expected_literal(got: &str) -> Diagnostic {
    let message = format!(
        "expected default literal (INT, FLOAT, BOOL, or quoted STRING) in type spelling, found {got:?}"
    );
    Diagnostic::new(
        "dsl.spelling.expected_literal",
        message,
        Diagnostic::make_fields(&[("found", got)]),
    )
}

/// Float default that does not parse as finite.
pub fn spelling_invalid_float(got: &str) -> Diagnostic {
    let message = format!("bad float default in type spelling: {got:?}");
    Diagnostic::new(
        "dsl.spelling.invalid_float",
        message,
        Diagnostic::make_fields(&[("found", got)]),
    )
}

/// Integer default that does not parse.
pub fn spelling_invalid_int(got: &str) -> Diagnostic {
    let message = format!("bad integer default in type spelling: {got:?}");
    Diagnostic::new(
        "dsl.spelling.invalid_int",
        message,
        Diagnostic::make_fields(&[("found", got)]),
    )
}

/// Flag that requires a value but got none. Preserves the legacy
/// `invalid syntax for command ...` prefix so `expect_error`
/// assertions keep matching.
pub fn flag_missing_value(command: &str, flag: &str) -> Diagnostic {
    let message = format!("invalid syntax for command {command}: {flag} requires a value");
    Diagnostic::new(
        "dsl.validation.flag_missing_value",
        message,
        Diagnostic::make_fields(&[("command", command), ("flag", flag)]),
    )
}

/// Unknown flag for a known command.
pub fn unknown_flag(command: &str, flag: &str) -> Diagnostic {
    let message = format!("invalid syntax for command {command}: unknown flag {flag}");
    Diagnostic::new(
        "dsl.validation.unknown_flag",
        message,
        Diagnostic::make_fields(&[("command", command), ("flag", flag)]),
    )
}

/// More positionals than the command spec allows.
pub fn too_many_positionals(command: &str, max: usize, got: usize) -> Diagnostic {
    let message = format!(
        "invalid syntax for command {command}: expects at most {max} positional argument(s), got {got}"
    );
    Diagnostic::new(
        "dsl.validation.too_many_positionals",
        message,
        Diagnostic::make_fields(&[
            ("command", command),
            ("max", &max.to_string()),
            ("got", &got.to_string()),
        ]),
    )
}

/// Missing required positional argument.
pub fn missing_argument(command: &str, name: &str) -> Diagnostic {
    let message = format!("invalid syntax for command {command}: requires argument `{name}`");
    Diagnostic::new(
        "dsl.validation.missing_argument",
        message,
        Diagnostic::make_fields(&[("command", command), ("argument", name)]),
    )
}

/// Statically-known type violation for one positional. Takes the
/// underlying failure; `{reason:#}` (full chain) formatting lives
/// here so call sites stay `format!`-free.
pub fn arg_type_mismatch(
    command: &str,
    name: &str,
    got: &str,
    reason: &anyhow::Error,
) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message =
        format!("invalid syntax for command {command}: argument `{name}` got {got} — {reason_text}");
    Diagnostic::new(
        "dsl.validation.arg_type_mismatch",
        message,
        Diagnostic::make_fields(&[
            ("command", command),
            ("argument", name),
            ("got", got),
            ("reason", &reason_text),
        ]),
    )
}

/// Command invoked with no value to join. Shared helper, so the
/// command rides as a field and every caller shares one key.
pub fn needs_value(command: &str) -> Diagnostic {
    let message = format!("{command} requires at least one argument");
    Diagnostic::new(
        "dsl.validation.needs_value",
        message,
        Diagnostic::make_fields(&[("command", command)]),
    )
}

/// `ENV` without any `KEY=value` assignment.
pub fn env_requires_assignment() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.env_requires_assignment",
        "ENV requires KEY=value".to_string(),
        Diagnostic::make_fields(&[("command", "ENV")]),
    )
}

/// `ENV` assignment that is not `KEY=value` shaped.
pub fn env_bad_format() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.env_bad_format",
        "ENV requires KEY=value format".to_string(),
        Diagnostic::make_fields(&[("command", "ENV")]),
    )
}

/// `ENV` split failure passthrough. Takes the underlying failure;
/// `{reason:#}` formatting lives here so call sites stay
/// `format!`-free.
pub fn env_assignment_error(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.validation.env_assignment_error",
        reason_text.clone(),
        Diagnostic::make_fields(&[("command", "ENV"), ("reason", &reason_text)]),
    )
}

/// `EXPAND` override that is not `KEY=value` shaped.
pub fn expand_bad_override() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.expand_bad_override",
        "EXPAND requires KEY=value format for overrides".to_string(),
        Diagnostic::make_fields(&[("command", "EXPAND")]),
    )
}

/// `EXPAND` split failure passthrough. Takes the underlying failure;
/// `{reason:#}` formatting lives here.
pub fn expand_assignment_error(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.validation.expand_assignment_error",
        reason_text.clone(),
        Diagnostic::make_fields(&[("command", "EXPAND"), ("reason", &reason_text)]),
    )
}

/// `EXPAND` path validation failure passthrough.
pub fn expand_path_error(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.validation.expand_path_error",
        reason_text.clone(),
        Diagnostic::make_fields(&[("command", "EXPAND"), ("reason", &reason_text)]),
    )
}

/// `EXPAND` with more than one path positional.
pub fn expand_extra_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.expand_extra_path",
        "EXPAND accepts at most one path".to_string(),
        Diagnostic::make_fields(&[("command", "EXPAND")]),
    )
}

/// `WORKDIR` without a path.
pub fn workdir_requires_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.workdir_requires_path",
        "WORKDIR requires a path".to_string(),
        Diagnostic::make_fields(&[("command", "WORKDIR")]),
    )
}

/// `WORKSPACE` without a target.
pub fn workspace_requires_target() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.workspace_requires_target",
        "WORKSPACE requires a target".to_string(),
        Diagnostic::make_fields(&[("command", "WORKSPACE")]),
    )
}

/// `WORKSPACE --local` without `CACHE`.
pub fn workspace_local_needs_cache() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.workspace_local_needs_cache",
        "WORKSPACE --local requires CACHE".to_string(),
        Diagnostic::make_fields(&[("command", "WORKSPACE")]),
    )
}

/// Unknown `WORKSPACE` target. Takes the offending value; quoting
/// lives here.
pub fn workspace_unknown_target(other: &str) -> Diagnostic {
    let message = format!("unknown workspace target: {other}");
    Diagnostic::new(
        "dsl.validation.workspace_unknown_target",
        message,
        Diagnostic::make_fields(&[("command", "WORKSPACE"), ("got", other)]),
    )
}

/// Unknown `--from-workspace` source. Shared by `COPY` and `SYMLINK`:
/// same failure mode, one key. Takes the invoking command for the
/// structured `command` field; the message is command-free by legacy.
pub fn unknown_workspace_source(command: &str, other: &str) -> Diagnostic {
    let message = format!("unknown workspace source: {other}");
    Diagnostic::new(
        "dsl.validation.unknown_workspace_source",
        message,
        Diagnostic::make_fields(&[("command", command), ("got", other)]),
    )
}

/// `COPY` without a source positional.
pub fn copy_requires_source() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.copy_requires_source",
        "COPY requires a source".to_string(),
        Diagnostic::make_fields(&[("command", "COPY")]),
    )
}

/// `COPY` without a destination positional.
pub fn copy_requires_destination() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.copy_requires_destination",
        "COPY requires a destination".to_string(),
        Diagnostic::make_fields(&[("command", "COPY")]),
    )
}

/// `COPY` with both host-transfer flags.
pub fn copy_host_exclusion() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.copy_host_exclusion",
        "COPY --from-host and --to-host are mutually exclusive".to_string(),
        Diagnostic::make_fields(&[("command", "COPY")]),
    )
}

/// `COPY_GIT` without a revision positional.
pub fn copy_git_requires_revision() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.copy_git_requires_revision",
        "COPY_GIT requires a revision".to_string(),
        Diagnostic::make_fields(&[("command", "COPY_GIT")]),
    )
}

/// `COPY_GIT` without a source positional.
pub fn copy_git_requires_source() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.copy_git_requires_source",
        "COPY_GIT requires a source".to_string(),
        Diagnostic::make_fields(&[("command", "COPY_GIT")]),
    )
}

/// `COPY_GIT` without a destination positional.
pub fn copy_git_requires_destination() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.copy_git_requires_destination",
        "COPY_GIT requires a destination".to_string(),
        Diagnostic::make_fields(&[("command", "COPY_GIT")]),
    )
}

/// `SYMLINK` without a source positional.
pub fn symlink_requires_source() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.symlink_requires_source",
        "SYMLINK requires a source".to_string(),
        Diagnostic::make_fields(&[("command", "SYMLINK")]),
    )
}

/// `SYMLINK` without a target positional.
pub fn symlink_requires_target() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.symlink_requires_target",
        "SYMLINK requires a target".to_string(),
        Diagnostic::make_fields(&[("command", "SYMLINK")]),
    )
}

/// `MKDIR` without a path.
pub fn mkdir_requires_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.mkdir_requires_path",
        "MKDIR requires a path".to_string(),
        Diagnostic::make_fields(&[("command", "MKDIR")]),
    )
}

/// `INSTALL` without a source positional.
pub fn install_requires_source() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.install_requires_source",
        "INSTALL requires a source".to_string(),
        Diagnostic::make_fields(&[("command", "INSTALL")]),
    )
}

/// `INSTALL` without a destination positional.
pub fn install_requires_destination() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.install_requires_destination",
        "INSTALL requires a destination".to_string(),
        Diagnostic::make_fields(&[("command", "INSTALL")]),
    )
}

/// `SET_EXECUTABLE` without a path.
pub fn set_executable_requires_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.set_executable_requires_path",
        "SET_EXECUTABLE requires a path".to_string(),
        Diagnostic::make_fields(&[("command", "SET_EXECUTABLE")]),
    )
}

/// `ARCHIVE` without a source positional.
pub fn archive_requires_source() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.archive_requires_source",
        "ARCHIVE requires a source".to_string(),
        Diagnostic::make_fields(&[("command", "ARCHIVE")]),
    )
}

/// `ARCHIVE` without a destination positional.
pub fn archive_requires_destination() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.archive_requires_destination",
        "ARCHIVE requires a destination".to_string(),
        Diagnostic::make_fields(&[("command", "ARCHIVE")]),
    )
}

/// `UNARCHIVE` without an archive positional.
pub fn unarchive_requires_archive() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.unarchive_requires_archive",
        "UNARCHIVE requires an archive".to_string(),
        Diagnostic::make_fields(&[("command", "UNARCHIVE")]),
    )
}

/// `UNARCHIVE` without a destination positional.
pub fn unarchive_requires_destination() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.unarchive_requires_destination",
        "UNARCHIVE requires a destination".to_string(),
        Diagnostic::make_fields(&[("command", "UNARCHIVE")]),
    )
}

/// `UNARCHIVE --max-bytes` with an unparseable byte count. Takes the
/// underlying failure; `{reason:#}` formatting lives here.
pub fn unarchive_bad_max_bytes(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.validation.unarchive_bad_max_bytes",
        format!("UNARCHIVE --max-bytes {reason_text}"),
        Diagnostic::make_fields(&[("command", "UNARCHIVE"), ("reason", &reason_text)]),
    )
}

/// `EXPORT` naming a function with no top-level `FUNC` or known module
/// member. Takes the exported name; quoting lives here.
pub fn export_unknown_func(name: &str) -> Diagnostic {
    let message =
        format!("EXPORT `{name}` does not match any top-level FUNC or known module member");
    Diagnostic::new(
        "dsl.validation.export_unknown_func",
        message,
        Diagnostic::make_fields(&[("command", "EXPORT"), ("got", name)]),
    )
}

/// `EXPORT TYPE` with a qualified `ALIAS::TYPE` name, which no
/// downstream path serves. Takes the exported name.
pub fn export_type_qualified(name: &str) -> Diagnostic {
    let message = format!(
        "EXPORT TYPE `{name}` must be bare: qualified ALIAS::TYPE re-exports are not served; import the file and re-export the bare name"
    );
    Diagnostic::new(
        "dsl.validation.export_type_qualified",
        message,
        Diagnostic::make_fields(&[("command", "EXPORT"), ("got", name)]),
    )
}

/// `EXPORT TYPE` naming no top-level `TYPE` (and no imports to defer
/// to). Takes the exported name.
pub fn export_unknown_type(name: &str) -> Diagnostic {
    let message = format!("EXPORT TYPE `{name}` does not match any top-level TYPE");
    Diagnostic::new(
        "dsl.validation.export_unknown_type",
        message,
        Diagnostic::make_fields(&[("command", "EXPORT"), ("got", name)]),
    )
}

/// `EXPORT CONST` through the `SCRIPT::` namespace, which never names
/// a `CONST`. Takes the exported name.
pub fn export_const_script_namespace(name: &str) -> Diagnostic {
    let message = format!(
        "EXPORT CONST `{name}` must name a top-level CONST (`$NAME`) or `$ALIAS::NAME` for a visible import: `SCRIPT::` is not a CONST namespace"
    );
    Diagnostic::new(
        "dsl.validation.export_const_script_namespace",
        message,
        Diagnostic::make_fields(&[("command", "EXPORT"), ("got", name)]),
    )
}

/// `EXPORT CONST` forwarding through an alias with no visible import.
/// Takes the exported name and the unknown alias.
pub fn export_const_unknown_alias(name: &str, module: &str) -> Diagnostic {
    let message = format!(
        "EXPORT CONST `{name}` names unknown import alias `{module}`: import a file as `{module}` to re-export its CONSTs"
    );
    Diagnostic::new(
        "dsl.validation.export_const_unknown_alias",
        message,
        Diagnostic::make_fields(&[
            ("command", "EXPORT"),
            ("got", name),
            ("alias", module),
        ]),
    )
}

/// `EXPORT CONST` naming no top-level `CONST`. Takes the exported name.
pub fn export_const_unknown(name: &str) -> Diagnostic {
    let message = format!(
        "EXPORT CONST `{name}` does not match any top-level CONST (LET bindings are never exportable)"
    );
    Diagnostic::new(
        "dsl.validation.export_const_unknown",
        message,
        Diagnostic::make_fields(&[("command", "EXPORT"), ("got", name)]),
    )
}

/// `LET` capture running `ASYNC`/`AWAIT`/`CANCEL` inline. The capture
/// sink cannot stage tasks; hoist first, then await.
pub fn let_capture_async() -> Diagnostic {
    Diagnostic::new(
        "dsl.structural.let_capture_async",
        "LET capture cannot run ASYNC/AWAIT/CANCEL inline; use LET $t: HANDLE<T> = ASYNC ... then LET $o: STRING = AWAIT $t".to_string(),
        Diagnostic::make_fields(&[("rule", "let")]),
    )
}

/// `LET` capture using `WITH_IO [stdout=$var]`. The capture sink owns
/// stdout.
pub fn let_capture_pipe_stdout() -> Diagnostic {
    Diagnostic::new(
        "dsl.structural.let_capture_pipe_stdout",
        "LET capture cannot use WITH_IO [stdout=$var]; the capture sink owns stdout".to_string(),
        Diagnostic::make_fields(&[("rule", "let")]),
    )
}

/// `INHERIT_ENV` nested inside another command. The prelude directive
/// only reads at statement level.
pub fn nested_inherit_env() -> Diagnostic {
    Diagnostic::new(
        "dsl.structural.nested_inherit_env",
        "INHERIT_ENV cannot be nested inside other commands".to_string(),
        Diagnostic::make_fields(&[("rule", "inherit_env")]),
    )
}

/// `WITH_IO [rows]`/`[cols]` without `[tty]`. Geometry without a
/// terminal has nothing to size.
pub fn tty_geometry_without_tty() -> Diagnostic {
    Diagnostic::new(
        "dsl.structural.tty_geometry_without_tty",
        "WITH_IO [rows]/[cols] need [tty] on the same step: geometry without a terminal has nothing to size".to_string(),
        Diagnostic::make_fields(&[("rule", "with_io")]),
    )
}

/// `WITH_IO [tty]` binding stderr. The pty master interleaves both
/// streams, so they cannot demultiplex.
pub fn tty_with_stderr() -> Diagnostic {
    Diagnostic::new(
        "dsl.structural.tty_with_stderr",
        "WITH_IO [tty] cannot bind stderr: the pty master carries stdout and stderr interleaved, so the two streams cannot demultiplex".to_string(),
        Diagnostic::make_fields(&[("rule", "with_io")]),
    )
}

/// `WITH_IO [tty]` wrapping a non-`RUN` step. Terminal-unaware
/// commands cannot run under the pty master.
pub fn tty_wraps_non_run() -> Diagnostic {
    Diagnostic::new(
        "dsl.structural.tty_wraps_non_run",
        "WITH_IO [tty] only wraps a RUN step: the pty master is one interleaved child stream, so terminal-unaware commands cannot run under it".to_string(),
        Diagnostic::make_fields(&[("rule", "with_io")]),
    )
}

/// `WITH_IO` placed inside an `ASYNC` body. Pipe wiring belongs
/// outside the task.
pub fn with_io_in_async() -> Diagnostic {
    Diagnostic::new(
        "dsl.structural.with_io_in_async",
        "WITH_IO cannot be placed inside ASYNC. Place WITH_IO outside ASYNC instead (e.g. WITH_IO [...] ASYNC RUN ...)".to_string(),
        Diagnostic::make_fields(&[("rule", "async")]),
    )
}

/// `COPY --from-host` / `--to-host` outside a `REMOTE` block. Transfer
/// declarations only mean something where a session hoists them.
pub fn copy_host_outside_remote() -> Diagnostic {
    Diagnostic::new(
        "dsl.structural.copy_host_outside_remote",
        "COPY --from-host and --to-host are only valid inside a REMOTE block".to_string(),
        Diagnostic::make_fields(&[("rule", "copy")]),
    )
}

/// Staged `UnsignedIntBoundary` for `2^63` where unary `-` cannot
/// consume it. Fixed value, parameterless.
pub fn int_boundary_overflow() -> Diagnostic {
    Diagnostic::new(
        "dsl.structural.int_boundary_overflow",
        "integer overflow: 9223372036854775808 exceeds i64::MAX".to_string(),
        Diagnostic::make_fields(&[("rule", "expr")]),
    )
}

/// Integer literal exceeding `i64::MAX` under unary `-`. Takes the
/// offending value; quoting lives here.
pub fn int_overflow(n: u64) -> Diagnostic {
    let message = format!("integer overflow: {n} exceeds i64::MAX");
    Diagnostic::new(
        "dsl.structural.int_overflow",
        message,
        Diagnostic::make_fields(&[("rule", "expr"), ("got", &n.to_string())]),
    )
}

/// `WRITE` without a path.
pub fn write_requires_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.write_requires_path",
        "WRITE requires a path".to_string(),
        Diagnostic::make_fields(&[("command", "WRITE")]),
    )
}

/// `APPEND` without a path.
pub fn append_requires_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.append_requires_path",
        "APPEND requires a path".to_string(),
        Diagnostic::make_fields(&[("command", "APPEND")]),
    )
}

/// `APPEND_LINE` without a path.
pub fn append_line_requires_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.append_line_requires_path",
        "APPEND_LINE requires a path".to_string(),
        Diagnostic::make_fields(&[("command", "APPEND_LINE")]),
    )
}

/// `REMOVE` without a path.
pub fn remove_requires_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.remove_requires_path",
        "REMOVE requires a path".to_string(),
        Diagnostic::make_fields(&[("command", "REMOVE")]),
    )
}

/// `READ_LINE` without a variable.
pub fn read_line_requires_variable() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.read_line_requires_variable",
        "READ_LINE requires a variable".to_string(),
        Diagnostic::make_fields(&[("command", "READ_LINE")]),
    )
}

/// `READ_LINE` target without a `$` sigil. Takes the rendered
/// argument; quoting lives here. (Renders the value, never the
/// internal `Arg` debug shape.)
pub fn read_line_requires_sigil(got: &str) -> Diagnostic {
    let message = format!("READ_LINE requires a $variable, found {got:?}");
    Diagnostic::new(
        "dsl.validation.read_line_requires_sigil",
        message,
        Diagnostic::make_fields(&[("command", "READ_LINE"), ("got", got)]),
    )
}

/// `PROMPT` without a variable.
pub fn prompt_requires_variable() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.prompt_requires_variable",
        "PROMPT requires a variable".to_string(),
        Diagnostic::make_fields(&[("command", "PROMPT")]),
    )
}

/// `PROMPT` target without a `$` sigil. Takes the rendered argument.
pub fn prompt_requires_sigil(got: &str) -> Diagnostic {
    let message = format!("PROMPT requires a $variable, found {got:?}");
    Diagnostic::new(
        "dsl.validation.prompt_requires_sigil",
        message,
        Diagnostic::make_fields(&[("command", "PROMPT"), ("got", got)]),
    )
}

/// `PROMPT` without a message.
pub fn prompt_requires_message() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.prompt_requires_message",
        "PROMPT requires a message".to_string(),
        Diagnostic::make_fields(&[("command", "PROMPT")]),
    )
}

/// `ASSERT_CONTAINS` without a value.
pub fn assert_contains_requires_value() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.assert_contains_requires_value",
        "ASSERT_CONTAINS requires a value".to_string(),
        Diagnostic::make_fields(&[("command", "ASSERT_CONTAINS")]),
    )
}

/// `ASSERT_CONTAINS` without a needle.
pub fn assert_contains_requires_needle() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.assert_contains_requires_needle",
        "ASSERT_CONTAINS requires a needle".to_string(),
        Diagnostic::make_fields(&[("command", "ASSERT_CONTAINS")]),
    )
}

/// `EXIT` without a code.
pub fn exit_requires_code() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.exit_requires_code",
        "EXIT requires a code".to_string(),
        Diagnostic::make_fields(&[("command", "EXIT")]),
    )
}

/// `SLEEP` without a duration.
pub fn sleep_requires_duration() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.sleep_requires_duration",
        "SLEEP requires a duration (e.g. SLEEP 500ms)".to_string(),
        Diagnostic::make_fields(&[("command", "SLEEP")]),
    )
}

/// `SLEEP` with more than one argument.
pub fn sleep_takes_one() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.sleep_takes_one",
        "SLEEP takes exactly one duration argument".to_string(),
        Diagnostic::make_fields(&[("command", "SLEEP")]),
    )
}

/// `LIST_APPEND` without a LIST variable.
pub fn list_append_requires_list() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.list_append_requires_list",
        "LIST_APPEND requires a LIST variable (e.g. LIST_APPEND $items $x)".to_string(),
        Diagnostic::make_fields(&[("command", "LIST_APPEND")]),
    )
}

/// `LIST_APPEND` target without a `$` sigil. Takes the rendered
/// argument; quoting lives here.
pub fn list_append_requires_sigil(got: &str) -> Diagnostic {
    let message = format!("LIST_APPEND requires a $variable, found {got:?}");
    Diagnostic::new(
        "dsl.validation.list_append_requires_sigil",
        message,
        Diagnostic::make_fields(&[("command", "LIST_APPEND"), ("got", got)]),
    )
}

/// `LIST_APPEND` without an item.
pub fn list_append_requires_item() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.list_append_requires_item",
        "LIST_APPEND requires an item to append (e.g. LIST_APPEND $items $x)".to_string(),
        Diagnostic::make_fields(&[("command", "LIST_APPEND")]),
    )
}

/// `LIST_APPEND` with other than two arguments.
pub fn list_append_takes_two() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.list_append_takes_two",
        "LIST_APPEND takes exactly two arguments: LIST_APPEND $list <item>".to_string(),
        Diagnostic::make_fields(&[("command", "LIST_APPEND")]),
    )
}

/// `ASSERT_EQ` / `ASSERT_NE` without an actual value. Shared by both
/// commands: same failure mode, one key; the invoking command rides in
/// the structured `command` field and leads the message.
pub fn assert_equality_requires_value(command: &str) -> Diagnostic {
    let message = format!("{command} requires a value");
    Diagnostic::new(
        "dsl.validation.assert_equality_requires_value",
        message,
        Diagnostic::make_fields(&[("command", command)]),
    )
}

/// `ASSERT_EQ` / `ASSERT_NE` without an expected value (and no `--hash`).
/// Shared key; invoking command in the field.
pub fn assert_equality_requires_expected(command: &str) -> Diagnostic {
    let message = format!("{command} requires an expected value");
    Diagnostic::new(
        "dsl.validation.assert_equality_requires_expected",
        message,
        Diagnostic::make_fields(&[("command", command)]),
    )
}

/// `ASSERT_EQ` / `ASSERT_NE` combining `--hash` with a positional
/// expected value. Shared key; invoking command in the field.
pub fn assert_equality_hash_with_expected(command: &str) -> Diagnostic {
    let message = format!("{command} --hash cannot be combined with an expected value");
    Diagnostic::new(
        "dsl.validation.assert_equality_hash_with_expected",
        message,
        Diagnostic::make_fields(&[("command", command)]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_names_both_spellings() {
        let d = alias_duplicate_on_field("h", "help", "h2");
        assert_eq!(d.key(), "dsl.alias.duplicate_on_field");
        assert!(d.message().contains("'h'"));
        assert!(d.message().contains("'help'"));
        assert!(d.message().contains("'h2'"));
    }

    #[test]
    fn trailing_text_is_unexpected_not_expected() {
        let d = spelling_trailing_text("foo");
        assert_eq!(d.key(), "dsl.spelling.trailing_text");
        assert!(d.message().starts_with("unexpected trailing text"));
    }

    #[test]
    fn each_failure_has_a_distinct_key() {
        let keys = [
            spelling_trailing_text("x").key(),
            spelling_max_depth(8, "x").key(),
            spelling_expected_type_name().key(),
            spelling_expected_uppercase("a").key(),
            spelling_expected_delimiter("x").key(),
            spelling_unterminated_string("x").key(),
            spelling_expected_literal("x").key(),
            spelling_invalid_float("x").key(),
            spelling_invalid_int("x").key(),
            alias_needs_field("x").key(),
            alias_needs_name().key(),
            alias_name_lowercase("B", "B").key(),
            alias_duplicate_on_field("h", "a", "b").key(),
            flag_missing_value("CMD", "--flag").key(),
            unknown_flag("CMD", "--nope").key(),
            too_many_positionals("CMD", 1, 2).key(),
            missing_argument("CMD", "path").key(),
            arg_type_mismatch("CMD", "path", "3", &anyhow::anyhow!("expected PATH")).key(),
            needs_value("ECHO").key(),
            env_requires_assignment().key(),
            env_bad_format().key(),
            env_assignment_error(&anyhow::anyhow!("bad split")).key(),
            expand_bad_override().key(),
            expand_assignment_error(&anyhow::anyhow!("bad split")).key(),
            expand_path_error(&anyhow::anyhow!("bad path")).key(),
            expand_extra_path().key(),
            workdir_requires_path().key(),
            workspace_requires_target().key(),
            workspace_local_needs_cache().key(),
            workspace_unknown_target("NOPE").key(),
            unknown_workspace_source("COPY", "NOPE").key(),
            copy_requires_source().key(),
            copy_requires_destination().key(),
            copy_host_exclusion().key(),
            copy_git_requires_revision().key(),
            copy_git_requires_source().key(),
            copy_git_requires_destination().key(),
            symlink_requires_source().key(),
            symlink_requires_target().key(),
            mkdir_requires_path().key(),
            install_requires_source().key(),
            install_requires_destination().key(),
            set_executable_requires_path().key(),
            archive_requires_source().key(),
            archive_requires_destination().key(),
            unarchive_requires_archive().key(),
            unarchive_requires_destination().key(),
            unarchive_bad_max_bytes(&anyhow::anyhow!("bad count")).key(),
            export_unknown_func("NOPE").key(),
            export_type_qualified("LIB::T").key(),
            export_unknown_type("NOPE").key(),
            export_const_script_namespace("SCRIPT::N").key(),
            export_const_unknown_alias("$LIB::N", "LIB").key(),
            export_const_unknown("$N").key(),
            let_capture_async().key(),
            let_capture_pipe_stdout().key(),
            nested_inherit_env().key(),
            tty_geometry_without_tty().key(),
            tty_with_stderr().key(),
            tty_wraps_non_run().key(),
            with_io_in_async().key(),
            copy_host_outside_remote().key(),
            int_boundary_overflow().key(),
            int_overflow(42).key(),
            write_requires_path().key(),
            append_requires_path().key(),
            append_line_requires_path().key(),
            remove_requires_path().key(),
            read_line_requires_variable().key(),
            read_line_requires_sigil("reply").key(),
            prompt_requires_variable().key(),
            prompt_requires_sigil("reply").key(),
            prompt_requires_message().key(),
            assert_contains_requires_value().key(),
            assert_contains_requires_needle().key(),
            exit_requires_code().key(),
            sleep_requires_duration().key(),
            sleep_takes_one().key(),
            list_append_requires_list().key(),
            list_append_requires_sigil("x").key(),
            list_append_requires_item().key(),
            list_append_takes_two().key(),
            assert_equality_requires_value("ASSERT_EQ").key(),
            assert_equality_requires_expected("ASSERT_EQ").key(),
            assert_equality_hash_with_expected("ASSERT_EQ").key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn validation_messages_keep_legacy_prefix() {
        let d = unknown_flag("WRITE", "--nope");
        assert_eq!(d.key(), "dsl.validation.unknown_flag");
        assert!(d.message().contains("invalid syntax for command WRITE"));
        assert!(d.message().contains("unknown flag --nope"));
        let d = missing_argument("WRITE", "path");
        assert!(d.message().contains("requires argument `path`"));
        let d = too_many_positionals("CMD", 1, 3);
        assert!(d.message().contains("expects at most 1 positional argument(s), got 3"));
    }

    #[test]
    fn env_expand_messages_keep_legacy_wording() {
        assert_eq!(env_requires_assignment().message(), "ENV requires KEY=value");
        assert_eq!(env_bad_format().message(), "ENV requires KEY=value format");
        assert_eq!(
            expand_bad_override().message(),
            "EXPAND requires KEY=value format for overrides"
        );
        assert_eq!(expand_extra_path().message(), "EXPAND accepts at most one path");
        assert_eq!(needs_value("ECHO").message(), "ECHO requires at least one argument");
        let inner = anyhow::anyhow!("assignment requires KEY=value format");
        assert_eq!(
            env_assignment_error(&inner).message(),
            "assignment requires KEY=value format"
        );
    }

    #[test]
    fn workspace_copy_messages_keep_legacy_wording() {
        assert_eq!(workdir_requires_path().message(), "WORKDIR requires a path");
        assert_eq!(workspace_requires_target().message(), "WORKSPACE requires a target");
        assert_eq!(
            workspace_local_needs_cache().message(),
            "WORKSPACE --local requires CACHE"
        );
        assert_eq!(
            workspace_unknown_target("NOPE").message(),
            "unknown workspace target: NOPE"
        );
        assert_eq!(
            unknown_workspace_source("COPY", "NOPE").message(),
            "unknown workspace source: NOPE"
        );
        assert_eq!(copy_requires_source().message(), "COPY requires a source");
        assert_eq!(
            copy_requires_destination().message(),
            "COPY requires a destination"
        );
        assert_eq!(
            copy_host_exclusion().message(),
            "COPY --from-host and --to-host are mutually exclusive"
        );
        assert_eq!(
            copy_git_requires_revision().message(),
            "COPY_GIT requires a revision"
        );
        assert_eq!(
            copy_git_requires_source().message(),
            "COPY_GIT requires a source"
        );
        assert_eq!(
            copy_git_requires_destination().message(),
            "COPY_GIT requires a destination"
        );
        assert_eq!(symlink_requires_source().message(), "SYMLINK requires a source");
        assert_eq!(symlink_requires_target().message(), "SYMLINK requires a target");
    }

    #[test]
    fn fs_path_messages_keep_legacy_wording() {
        assert_eq!(mkdir_requires_path().message(), "MKDIR requires a path");
        assert_eq!(install_requires_source().message(), "INSTALL requires a source");
        assert_eq!(
            install_requires_destination().message(),
            "INSTALL requires a destination"
        );
        assert_eq!(
            set_executable_requires_path().message(),
            "SET_EXECUTABLE requires a path"
        );
        assert_eq!(archive_requires_source().message(), "ARCHIVE requires a source");
        assert_eq!(
            archive_requires_destination().message(),
            "ARCHIVE requires a destination"
        );
        assert_eq!(
            unarchive_requires_archive().message(),
            "UNARCHIVE requires an archive"
        );
        assert_eq!(
            unarchive_requires_destination().message(),
            "UNARCHIVE requires a destination"
        );
        let inner = anyhow::anyhow!("bad count");
        assert_eq!(
            unarchive_bad_max_bytes(&inner).message(),
            "UNARCHIVE --max-bytes bad count"
        );
        assert_eq!(write_requires_path().message(), "WRITE requires a path");
        assert_eq!(append_requires_path().message(), "APPEND requires a path");
        assert_eq!(
            append_line_requires_path().message(),
            "APPEND_LINE requires a path"
        );
        assert_eq!(remove_requires_path().message(), "REMOVE requires a path");
    }

    #[test]
    fn export_messages() {
        assert_eq!(
            export_unknown_func("NOPE").message(),
            "EXPORT `NOPE` does not match any top-level FUNC or known module member"
        );
        assert_eq!(
            export_type_qualified("LIB::T").message(),
            "EXPORT TYPE `LIB::T` must be bare: qualified ALIAS::TYPE re-exports are not served; import the file and re-export the bare name"
        );
        assert_eq!(
            export_unknown_type("NOPE").message(),
            "EXPORT TYPE `NOPE` does not match any top-level TYPE"
        );
        assert_eq!(
            export_const_script_namespace("SCRIPT::N").message(),
            "EXPORT CONST `SCRIPT::N` must name a top-level CONST (`$NAME`) or `$ALIAS::NAME` for a visible import: `SCRIPT::` is not a CONST namespace"
        );
        assert_eq!(
            export_const_unknown_alias("$LIB::N", "LIB").message(),
            "EXPORT CONST `$LIB::N` names unknown import alias `LIB`: import a file as `LIB` to re-export its CONSTs"
        );
        assert_eq!(
            export_const_unknown("$N").message(),
            "EXPORT CONST `$N` does not match any top-level CONST (LET bindings are never exportable)"
        );
    }

    #[test]
    fn structural_messages() {
        assert_eq!(
            let_capture_async().message(),
            "LET capture cannot run ASYNC/AWAIT/CANCEL inline; use LET $t: HANDLE<T> = ASYNC ... then LET $o: STRING = AWAIT $t"
        );
        assert_eq!(
            let_capture_pipe_stdout().message(),
            "LET capture cannot use WITH_IO [stdout=$var]; the capture sink owns stdout"
        );
        assert_eq!(
            nested_inherit_env().message(),
            "INHERIT_ENV cannot be nested inside other commands"
        );
        assert_eq!(
            tty_geometry_without_tty().message(),
            "WITH_IO [rows]/[cols] need [tty] on the same step: geometry without a terminal has nothing to size"
        );
        assert_eq!(
            tty_with_stderr().message(),
            "WITH_IO [tty] cannot bind stderr: the pty master carries stdout and stderr interleaved, so the two streams cannot demultiplex"
        );
        assert_eq!(
            tty_wraps_non_run().message(),
            "WITH_IO [tty] only wraps a RUN step: the pty master is one interleaved child stream, so terminal-unaware commands cannot run under it"
        );
        assert_eq!(
            with_io_in_async().message(),
            "WITH_IO cannot be placed inside ASYNC. Place WITH_IO outside ASYNC instead (e.g. WITH_IO [...] ASYNC RUN ...)"
        );
        assert_eq!(
            copy_host_outside_remote().message(),
            "COPY --from-host and --to-host are only valid inside a REMOTE block"
        );
        assert_eq!(
            int_boundary_overflow().message(),
            "integer overflow: 9223372036854775808 exceeds i64::MAX"
        );
        assert_eq!(
            int_overflow(42).message(),
            "integer overflow: 42 exceeds i64::MAX"
        );
    }

    #[test]
    fn read_line_prompt_messages_keep_legacy_shape() {
        assert_eq!(
            read_line_requires_variable().message(),
            "READ_LINE requires a variable"
        );
        assert_eq!(
            read_line_requires_sigil("reply").message(),
            "READ_LINE requires a $variable, found \"reply\""
        );
        assert_eq!(
            prompt_requires_variable().message(),
            "PROMPT requires a variable"
        );
        assert_eq!(
            prompt_requires_sigil("reply").message(),
            "PROMPT requires a $variable, found \"reply\""
        );
        assert_eq!(prompt_requires_message().message(), "PROMPT requires a message");
    }

    #[test]
    fn assert_exit_sleep_list_append_messages() {
        assert_eq!(
            assert_contains_requires_value().message(),
            "ASSERT_CONTAINS requires a value"
        );
        assert_eq!(
            assert_contains_requires_needle().message(),
            "ASSERT_CONTAINS requires a needle"
        );
        assert_eq!(exit_requires_code().message(), "EXIT requires a code");
        assert_eq!(
            sleep_requires_duration().message(),
            "SLEEP requires a duration (e.g. SLEEP 500ms)"
        );
        assert_eq!(
            sleep_takes_one().message(),
            "SLEEP takes exactly one duration argument"
        );
        assert_eq!(
            list_append_requires_list().message(),
            "LIST_APPEND requires a LIST variable (e.g. LIST_APPEND $items $x)"
        );
        assert_eq!(
            list_append_requires_sigil("x").message(),
            "LIST_APPEND requires a $variable, found \"x\""
        );
        assert_eq!(
            list_append_requires_item().message(),
            "LIST_APPEND requires an item to append (e.g. LIST_APPEND $items $x)"
        );
        assert_eq!(
            list_append_takes_two().message(),
            "LIST_APPEND takes exactly two arguments: LIST_APPEND $list <item>"
        );
    }

    #[test]
    fn assert_equality_messages() {
        assert_eq!(
            assert_equality_requires_value("ASSERT_EQ").message(),
            "ASSERT_EQ requires a value"
        );
        assert_eq!(
            assert_equality_requires_value("ASSERT_NE").message(),
            "ASSERT_NE requires a value"
        );
        assert_eq!(
            assert_equality_requires_expected("ASSERT_EQ").message(),
            "ASSERT_EQ requires an expected value"
        );
        assert_eq!(
            assert_equality_hash_with_expected("ASSERT_NE").message(),
            "ASSERT_NE --hash cannot be combined with an expected value"
        );
    }

    #[test]
    fn arg_mismatch_renders_full_chain() {
        let err = anyhow::anyhow!("expected PATH").context("static check failed");
        let d = arg_type_mismatch("CMD", "path", "3", &err);
        assert!(d.message().contains("expected PATH"), "{}", d.message());
        assert!(d.message().contains("static check failed"), "{}", d.message());
    }
}
