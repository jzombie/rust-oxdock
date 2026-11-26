//! DSL spelling diagnostics, owned by `oxdock-parser`.
//!
//! One constructor per diagnostic at the owning function. Each carries
//! a stable `dsl.*` key, structured fields, and a whole-sentence
//! template. Call sites pass decoded values, never `format!` strings
//! and never raw mid-parse text containing `WS_SENTINEL`; the
//! transport sanitizes any residual marker so untrusted input cannot
//! crash release.

use oxdock_diagnostics::Diagnostic;

use crate::constants::{
    KEYWORD_APPEND, KEYWORD_APPEND_LINE, KEYWORD_ARCHIVE, KEYWORD_ASSERT_CONTAINS, KEYWORD_ASYNC,
    KEYWORD_AWAIT, KEYWORD_CANCEL, KEYWORD_CONST, KEYWORD_COPY, KEYWORD_COPY_GIT, KEYWORD_ENV,
    KEYWORD_EXIT, KEYWORD_EXPAND, KEYWORD_EXPORT, KEYWORD_FUNC, KEYWORD_INHERIT_ENV,
    KEYWORD_INSTALL, KEYWORD_LET, KEYWORD_LIST_APPEND, KEYWORD_MKDIR, KEYWORD_PROMPT,
    KEYWORD_READ_LINE, KEYWORD_REMOTE, KEYWORD_REMOVE, KEYWORD_RUN, KEYWORD_SET_EXECUTABLE,
    KEYWORD_SLEEP, KEYWORD_SYMLINK, KEYWORD_TYPE, KEYWORD_UNARCHIVE, KEYWORD_WITH_IO,
    KEYWORD_WORKDIR, KEYWORD_WORKSPACE, KEYWORD_WRITE, SCRIPT_MODULE_NAME,
};
use crate::tag::{
    BOOL_TYPE_NAME, FLOAT_TYPE_NAME, HANDLE_TYPE_NAME, INT_TYPE_NAME, LIST_TYPE_NAME,
    STRING_TYPE_NAME,
};

/// Structured field keys. Single spelling for the whole module:
/// constructors reference these, never literals, so a key rename is
/// one edit. Keys used once or twice stay inline at their single use.
const FIELD_COMMAND: &str = "command";
const FIELD_RULE: &str = "rule";
const FIELD_GOT: &str = "got";
const FIELD_FOUND: &str = "found";
const FIELD_REASON: &str = "reason";
const FIELD_ALIAS: &str = "alias";
const FIELD_ARGUMENT: &str = "argument";
const FIELD_FIELD: &str = "field";
const FIELD_FIRST: &str = "first";
const FIELD_FLAG: &str = "flag";
const FIELD_MAX: &str = "max";
const FIELD_SECOND: &str = "second";
const FIELD_SPELLING: &str = "spelling";
const FIELD_LINE: &str = "line";
const FIELD_KEY: &str = "key";

/// Decode a mid-parse fragment for user output: the transient
/// `WS_SENTINEL` boundary becomes a space. Single owner for the decode
/// rule in this crate.
pub fn decode_spelling_fragment(raw: &str) -> String {
    raw.replace(crate::tag::WS_SENTINEL, " ")
}

/// Second `ALIAS` on the same field.
pub fn alias_duplicate_on_field(field: &str, first: &str, second: &str) -> Diagnostic {
    let message = format!(
        "field `{field}` already has alias `{first}`, found second alias `{second}`: only one `ALIAS` per field"
    );
    Diagnostic::new(
        "dsl.alias.duplicate_on_field",
        message,
        Diagnostic::make_fields(&[
            (FIELD_FIELD, field),
            (FIELD_FIRST, first),
            (FIELD_SECOND, second),
        ]),
    )
}

/// `ALIAS` without a named field.
pub fn alias_needs_field(got: &str) -> Diagnostic {
    let message = format!("`ALIAS` needs a named field, found bare {got:?}");
    Diagnostic::new(
        "dsl.alias.needs_field",
        message,
        Diagnostic::make_fields(&[(FIELD_GOT, got)]),
    )
}

/// `ALIAS` without a lowercase alias name.
pub fn alias_needs_name() -> Diagnostic {
    Diagnostic::new(
        "dsl.alias.needs_name",
        "`ALIAS` needs a lowercase alias name".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `ALIAS` name must start lowercase. Takes the decoded offending
/// character and the decoded spelling; the template owns quoting.
pub fn alias_name_lowercase(found: &str, spelling: &str) -> Diagnostic {
    let message = format!("`ALIAS` name must start lowercase, found {found:?} in {spelling:?}");
    Diagnostic::new(
        "dsl.alias.name_lowercase",
        message,
        Diagnostic::make_fields(&[(FIELD_FOUND, found), (FIELD_SPELLING, spelling)]),
    )
}

/// Unexpected trailing text after a complete spelling.
pub fn spelling_trailing_text(got: &str) -> Diagnostic {
    let message = format!("unexpected trailing text in type spelling: {got:?}");
    Diagnostic::new(
        "dsl.spelling.trailing_text",
        message,
        Diagnostic::make_fields(&[(FIELD_FOUND, got)]),
    )
}

/// Generic nesting deeper than the cap. Takes the numeric cap so no
/// `format!` leaks to the call site.
pub fn spelling_max_depth(max: usize, got: &str) -> Diagnostic {
    let message = format!("type spelling exceeds maximum generic depth of {max}: {got:?}");
    Diagnostic::new(
        "dsl.spelling.max_depth",
        message,
        Diagnostic::make_fields(&[(FIELD_MAX, &max.to_string()), (FIELD_FOUND, got)]),
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
        Diagnostic::make_fields(&[(FIELD_FOUND, found)]),
    )
}

/// Expected `,` or `>` to continue a generic spelling.
pub fn spelling_expected_delimiter(got: &str) -> Diagnostic {
    let message = format!("expected ',' or '>' in type spelling, found {got:?}");
    Diagnostic::new(
        "dsl.spelling.expected_delimiter",
        message,
        Diagnostic::make_fields(&[(FIELD_FOUND, got)]),
    )
}

/// Unterminated quoted string default.
pub fn spelling_unterminated_string(got: &str) -> Diagnostic {
    let message = format!("unterminated string default in type spelling: {got:?}");
    Diagnostic::new(
        "dsl.spelling.unterminated_string",
        message,
        Diagnostic::make_fields(&[(FIELD_FOUND, got)]),
    )
}

/// Expected a default literal.
pub fn spelling_expected_literal(got: &str) -> Diagnostic {
    let message = format!(
        "expected default literal ({INT_TYPE_NAME}, {FLOAT_TYPE_NAME}, {BOOL_TYPE_NAME}, or quoted {STRING_TYPE_NAME}) in type spelling, found {got:?}"
    );
    Diagnostic::new(
        "dsl.spelling.expected_literal",
        message,
        Diagnostic::make_fields(&[(FIELD_FOUND, got)]),
    )
}

/// Float default that does not parse as finite.
pub fn spelling_invalid_float(got: &str) -> Diagnostic {
    let message = format!("bad float default in type spelling: {got:?}");
    Diagnostic::new(
        "dsl.spelling.invalid_float",
        message,
        Diagnostic::make_fields(&[(FIELD_FOUND, got)]),
    )
}

/// Integer default that does not parse.
pub fn spelling_invalid_int(got: &str) -> Diagnostic {
    let message = format!("bad integer default in type spelling: {got:?}");
    Diagnostic::new(
        "dsl.spelling.invalid_int",
        message,
        Diagnostic::make_fields(&[(FIELD_FOUND, got)]),
    )
}

/// Comment inside an `IMPORT`/`EXPORT` bracket name list. Takes the
/// invoking command; one key for both lists since the rule is
/// shared. Call sites pass the `KEYWORD_*` constant, never a
/// literal.
pub fn spelling_bracket_list_comment(command: &str) -> Diagnostic {
    let message = format!(
        "{command} lists do not allow comments inside the brackets: move the comment above the statement"
    );
    Diagnostic::new(
        "dsl.spelling.bracket_list_comment",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, command)]),
    )
}

/// Flag that requires a value but got none. `COMMAND`-lead per the
/// unified contract; the flag is a DSL token in backticks.
pub fn flag_missing_value(command: &str, flag: &str) -> Diagnostic {
    let message = format!("{command} `{flag}` requires a value");
    Diagnostic::new(
        "dsl.validation.flag_missing_value",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, command), (FIELD_FLAG, flag)]),
    )
}

/// Unknown flag for a known command.
pub fn unknown_flag(command: &str, flag: &str) -> Diagnostic {
    let message = format!("{command} unknown flag `{flag}`");
    Diagnostic::new(
        "dsl.validation.unknown_flag",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, command), (FIELD_FLAG, flag)]),
    )
}

/// More positionals than the command spec allows.
pub fn too_many_positionals(command: &str, max: usize, got: usize) -> Diagnostic {
    let message = format!("{command} expects at most {max} positional arguments, got {got}");
    Diagnostic::new(
        "dsl.validation.too_many_positionals",
        message,
        Diagnostic::make_fields(&[
            (FIELD_COMMAND, command),
            (FIELD_MAX, &max.to_string()),
            (FIELD_GOT, &got.to_string()),
        ]),
    )
}

/// Missing required positional argument.
pub fn missing_argument(command: &str, name: &str) -> Diagnostic {
    let message = format!("{command} requires argument `{name}`");
    Diagnostic::new(
        "dsl.validation.missing_argument",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, command), (FIELD_ARGUMENT, name)]),
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
    let message = format!("{command} argument `{name}` got {got:?}: {reason_text}");
    Diagnostic::new(
        "dsl.validation.arg_type_mismatch",
        message,
        Diagnostic::make_fields(&[
            (FIELD_COMMAND, command),
            (FIELD_ARGUMENT, name),
            (FIELD_GOT, got),
            (FIELD_REASON, &reason_text),
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
        Diagnostic::make_fields(&[(FIELD_COMMAND, command)]),
    )
}

/// `ENV` without any `KEY=value` assignment.
pub fn env_requires_assignment() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.env_requires_assignment",
        format!("{KEYWORD_ENV} requires KEY=value"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_ENV)]),
    )
}

/// `ENV` assignment that is not `KEY=value` shaped.
pub fn env_bad_format() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.env_bad_format",
        format!("{KEYWORD_ENV} requires KEY=value format"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_ENV)]),
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
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_ENV), (FIELD_REASON, &reason_text)]),
    )
}

/// `EXPAND` override that is not `KEY=value` shaped.
pub fn expand_bad_override() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.expand_bad_override",
        format!("{KEYWORD_EXPAND} requires KEY=value format for overrides"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_EXPAND)]),
    )
}

/// `EXPAND` split failure passthrough. Takes the underlying failure;
/// `{reason:#}` formatting lives here.
pub fn expand_assignment_error(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.validation.expand_assignment_error",
        reason_text.clone(),
        Diagnostic::make_fields(&[
            (FIELD_COMMAND, KEYWORD_EXPAND),
            (FIELD_REASON, &reason_text),
        ]),
    )
}

/// `EXPAND` path validation failure passthrough.
pub fn expand_path_error(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.validation.expand_path_error",
        reason_text.clone(),
        Diagnostic::make_fields(&[
            (FIELD_COMMAND, KEYWORD_EXPAND),
            (FIELD_REASON, &reason_text),
        ]),
    )
}

/// `EXPAND` with more than one path positional.
pub fn expand_extra_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.expand_extra_path",
        format!("{KEYWORD_EXPAND} accepts at most one path"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_EXPAND)]),
    )
}

/// `WORKDIR` without a path.
pub fn workdir_requires_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.workdir_requires_path",
        format!("{KEYWORD_WORKDIR} requires a path"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_WORKDIR)]),
    )
}

/// `WORKSPACE` without a target.
pub fn workspace_requires_target() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.workspace_requires_target",
        format!("{KEYWORD_WORKSPACE} requires a target"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_WORKSPACE)]),
    )
}

/// `WORKSPACE --local` without `CACHE`.
pub fn workspace_local_needs_cache() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.workspace_local_needs_cache",
        format!("{KEYWORD_WORKSPACE} --local requires CACHE"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_WORKSPACE)]),
    )
}

/// Unknown `WORKSPACE` target. Takes the offending value; quoting
/// lives here.
pub fn workspace_unknown_target(other: &str) -> Diagnostic {
    let message = format!("unknown workspace target {other:?}");
    Diagnostic::new(
        "dsl.validation.workspace_unknown_target",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_WORKSPACE), (FIELD_GOT, other)]),
    )
}

/// Unknown `--from-workspace` source. Shared by `COPY` and `SYMLINK`:
/// same failure mode, one key. Takes the invoking command for the
/// structured `command` field; the message is command-free.
pub fn unknown_workspace_source(command: &str, other: &str) -> Diagnostic {
    let message = format!("unknown workspace source {other:?}");
    Diagnostic::new(
        "dsl.validation.unknown_workspace_source",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, command), (FIELD_GOT, other)]),
    )
}

/// `COPY` without a source positional.
pub fn copy_requires_source() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.copy_requires_source",
        format!("{KEYWORD_COPY} requires a source"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_COPY)]),
    )
}

/// `COPY` without a destination positional.
pub fn copy_requires_destination() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.copy_requires_destination",
        format!("{KEYWORD_COPY} requires a destination"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_COPY)]),
    )
}

/// `COPY` with both host-transfer flags.
pub fn copy_host_exclusion() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.copy_host_exclusion",
        format!("{KEYWORD_COPY} --from-host and --to-host are mutually exclusive"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_COPY)]),
    )
}

/// `COPY_GIT` without a revision positional.
pub fn copy_git_requires_revision() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.copy_git_requires_revision",
        format!("{KEYWORD_COPY_GIT} requires a revision"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_COPY_GIT)]),
    )
}

/// `COPY_GIT` without a source positional.
pub fn copy_git_requires_source() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.copy_git_requires_source",
        format!("{KEYWORD_COPY_GIT} requires a source"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_COPY_GIT)]),
    )
}

/// `COPY_GIT` without a destination positional.
pub fn copy_git_requires_destination() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.copy_git_requires_destination",
        format!("{KEYWORD_COPY_GIT} requires a destination"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_COPY_GIT)]),
    )
}

/// `SYMLINK` without a source positional.
pub fn symlink_requires_source() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.symlink_requires_source",
        format!("{KEYWORD_SYMLINK} requires a source"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_SYMLINK)]),
    )
}

/// `SYMLINK` without a target positional.
pub fn symlink_requires_target() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.symlink_requires_target",
        format!("{KEYWORD_SYMLINK} requires a target"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_SYMLINK)]),
    )
}

/// `MKDIR` without a path.
pub fn mkdir_requires_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.mkdir_requires_path",
        format!("{KEYWORD_MKDIR} requires a path"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_MKDIR)]),
    )
}

/// `INSTALL` without a source positional.
pub fn install_requires_source() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.install_requires_source",
        format!("{KEYWORD_INSTALL} requires a source"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_INSTALL)]),
    )
}

/// `INSTALL` without a destination positional.
pub fn install_requires_destination() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.install_requires_destination",
        format!("{KEYWORD_INSTALL} requires a destination"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_INSTALL)]),
    )
}

/// `SET_EXECUTABLE` without a path.
pub fn set_executable_requires_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.set_executable_requires_path",
        format!("{KEYWORD_SET_EXECUTABLE} requires a path"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_SET_EXECUTABLE)]),
    )
}

/// `ARCHIVE` without a source positional.
pub fn archive_requires_source() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.archive_requires_source",
        format!("{KEYWORD_ARCHIVE} requires a source"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_ARCHIVE)]),
    )
}

/// `ARCHIVE` without a destination positional.
pub fn archive_requires_destination() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.archive_requires_destination",
        format!("{KEYWORD_ARCHIVE} requires a destination"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_ARCHIVE)]),
    )
}

/// `UNARCHIVE` without an archive positional.
pub fn unarchive_requires_archive() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.unarchive_requires_archive",
        format!("{KEYWORD_UNARCHIVE} requires an archive"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_UNARCHIVE)]),
    )
}

/// `UNARCHIVE` without a destination positional.
pub fn unarchive_requires_destination() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.unarchive_requires_destination",
        format!("{KEYWORD_UNARCHIVE} requires a destination"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_UNARCHIVE)]),
    )
}

/// `UNARCHIVE --max-bytes` with an unparseable byte count. Takes the
/// underlying failure; `{reason:#}` formatting lives here.
pub fn unarchive_bad_max_bytes(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.validation.unarchive_bad_max_bytes",
        format!("{KEYWORD_UNARCHIVE} --max-bytes {reason_text}"),
        Diagnostic::make_fields(&[
            (FIELD_COMMAND, KEYWORD_UNARCHIVE),
            (FIELD_REASON, &reason_text),
        ]),
    )
}

/// `EXPORT` naming a function with no top-level `FUNC` or known module
/// member. Takes the exported name; quoting lives here.
pub fn export_unknown_func(name: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_EXPORT} `{name}` does not match any top-level {KEYWORD_FUNC} or known module member"
    );
    Diagnostic::new(
        "dsl.validation.export_unknown_func",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_EXPORT), (FIELD_GOT, name)]),
    )
}

/// `EXPORT TYPE` with a qualified `ALIAS::TYPE` name, which no
/// downstream path serves. Takes the exported name.
pub fn export_type_qualified(name: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_EXPORT} {KEYWORD_TYPE} `{name}` must be bare: qualified ALIAS::{KEYWORD_TYPE} re-exports are not served; import the file and re-export the bare name"
    );
    Diagnostic::new(
        "dsl.validation.export_type_qualified",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_EXPORT), (FIELD_GOT, name)]),
    )
}

/// `EXPORT TYPE` naming no top-level `TYPE` (and no imports to defer
/// to). Takes the exported name.
pub fn export_unknown_type(name: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_EXPORT} {KEYWORD_TYPE} `{name}` does not match any top-level {KEYWORD_TYPE}"
    );
    Diagnostic::new(
        "dsl.validation.export_unknown_type",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_EXPORT), (FIELD_GOT, name)]),
    )
}

/// `EXPORT CONST` through the `SCRIPT::` namespace, which never names
/// a `CONST`. Takes the exported name.
pub fn export_const_script_namespace(name: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_EXPORT} {KEYWORD_CONST} `{name}` must name a top-level {KEYWORD_CONST} (`$NAME`) or `$ALIAS::NAME` for a visible import: `{SCRIPT_MODULE_NAME}::` is not a {KEYWORD_CONST} namespace"
    );
    Diagnostic::new(
        "dsl.validation.export_const_script_namespace",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_EXPORT), (FIELD_GOT, name)]),
    )
}

/// `EXPORT CONST` forwarding through an alias with no visible import.
/// Takes the exported name and the unknown alias.
pub fn export_const_unknown_alias(name: &str, module: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_EXPORT} {KEYWORD_CONST} `{name}` names unknown import alias `{module}`: import a file as `{module}` to re-export its CONSTs"
    );
    Diagnostic::new(
        "dsl.validation.export_const_unknown_alias",
        message,
        Diagnostic::make_fields(&[
            (FIELD_COMMAND, KEYWORD_EXPORT),
            (FIELD_GOT, name),
            (FIELD_ALIAS, module),
        ]),
    )
}

/// `EXPORT CONST` naming no top-level `CONST`. Takes the exported name.
pub fn export_const_unknown(name: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_EXPORT} {KEYWORD_CONST} `{name}` does not match any top-level {KEYWORD_CONST} ({KEYWORD_LET} bindings are never exportable)"
    );
    Diagnostic::new(
        "dsl.validation.export_const_unknown",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_EXPORT), (FIELD_GOT, name)]),
    )
}

/// Known command with malformed arguments (the lowering fallback).
/// Takes the invoking command, the rendered args (`nothing` when
/// empty), and an optional structural hint. One key: hinted and bare
/// are the same failure mode, the hint is ancillary. Canonical form
/// is `COMMAND`-lead with no legacy prefix and no trailing period;
/// a hinted message is the hint itself (which already leads with the
/// command), a bare one names what arrived.
pub fn invalid_syntax(command: &str, got: &str, hint: Option<&str>) -> Diagnostic {
    let message = match hint {
        Some(hint) => hint.trim_end_matches('.').to_string(),
        None => format!("{command} got {got}"),
    };
    Diagnostic::new(
        "dsl.invalid_syntax.malformed",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, command), (FIELD_GOT, got)]),
    )
}

/// Truly unknown command name. Takes the attempted name and an
/// optional hint (structural or case hint). Canonical form quotes the
/// found name with `{name:?}` and keeps the hint on its own line with
/// no trailing period.
pub fn unknown_command(name: &str, hint: Option<&str>) -> Diagnostic {
    let message = match hint {
        Some(hint) => format!("unknown command {name:?}\n{}", hint.trim_end_matches('.')),
        None => format!("unknown command {name:?}"),
    };
    Diagnostic::new(
        "dsl.unknown_command.unknown",
        message,
        Diagnostic::make_fields(&[(FIELD_FOUND, name)]),
    )
}

/// `LET` capture running `ASYNC`/`AWAIT`/`CANCEL` inline. The capture
/// sink cannot stage tasks; hoist first, then await.
pub fn let_capture_async() -> Diagnostic {
    let rule = KEYWORD_LET.to_lowercase();
    Diagnostic::new(
        "dsl.structural.let_capture_async",
        format!(
            "{KEYWORD_LET} capture cannot run {KEYWORD_ASYNC}/{KEYWORD_AWAIT}/{KEYWORD_CANCEL} inline; use {KEYWORD_LET} $t: {HANDLE_TYPE_NAME}<T> = {KEYWORD_ASYNC} ... then {KEYWORD_LET} $o: {STRING_TYPE_NAME} = {KEYWORD_AWAIT} $t"
        ),
        Diagnostic::make_fields(&[(FIELD_RULE, rule.as_str())]),
    )
}

/// `LET` capture using `WITH_IO [stdout=$var]`. The capture sink owns
/// stdout.
pub fn let_capture_pipe_stdout() -> Diagnostic {
    let rule = KEYWORD_LET.to_lowercase();
    Diagnostic::new(
        "dsl.structural.let_capture_pipe_stdout",
        format!(
            "{KEYWORD_LET} capture cannot use {KEYWORD_WITH_IO} [stdout=$var]; the capture sink owns stdout"
        ),
        Diagnostic::make_fields(&[(FIELD_RULE, rule.as_str())]),
    )
}

/// `INHERIT_ENV` nested inside another command. The prelude directive
/// only reads at statement level.
pub fn nested_inherit_env() -> Diagnostic {
    let rule = KEYWORD_INHERIT_ENV.to_lowercase();
    Diagnostic::new(
        "dsl.structural.nested_inherit_env",
        format!("{KEYWORD_INHERIT_ENV} cannot be nested inside other commands"),
        Diagnostic::make_fields(&[(FIELD_RULE, rule.as_str())]),
    )
}

/// `WITH_IO [rows]`/`[cols]` without `[tty]`. Geometry without a
/// terminal has nothing to size.
pub fn tty_geometry_without_tty() -> Diagnostic {
    let rule = KEYWORD_WITH_IO.to_lowercase();
    Diagnostic::new(
        "dsl.structural.tty_geometry_without_tty",
        format!(
            "{KEYWORD_WITH_IO} [rows]/[cols] need [tty] on the same step: geometry without a terminal has nothing to size"
        ),
        Diagnostic::make_fields(&[(FIELD_RULE, rule.as_str())]),
    )
}

/// `WITH_IO [tty]` binding stderr. The pty master interleaves both
/// streams, so they cannot demultiplex.
pub fn tty_with_stderr() -> Diagnostic {
    let rule = KEYWORD_WITH_IO.to_lowercase();
    Diagnostic::new(
        "dsl.structural.tty_with_stderr",
        format!(
            "{KEYWORD_WITH_IO} [tty] cannot bind stderr: the pty master carries stdout and stderr interleaved, so the two streams cannot demultiplex"
        ),
        Diagnostic::make_fields(&[(FIELD_RULE, rule.as_str())]),
    )
}

/// `WITH_IO [tty]` wrapping a non-`RUN` step. Terminal-unaware
/// commands cannot run under the pty master.
pub fn tty_wraps_non_run() -> Diagnostic {
    let rule = KEYWORD_WITH_IO.to_lowercase();
    Diagnostic::new(
        "dsl.structural.tty_wraps_non_run",
        format!(
            "{KEYWORD_WITH_IO} [tty] only wraps a {KEYWORD_RUN} step: the pty master is one interleaved child stream, so terminal-unaware commands cannot run under it"
        ),
        Diagnostic::make_fields(&[(FIELD_RULE, rule.as_str())]),
    )
}

/// `WITH_IO` placed inside an `ASYNC` body. Pipe wiring belongs
/// outside the task.
pub fn with_io_in_async() -> Diagnostic {
    let rule = KEYWORD_ASYNC.to_lowercase();
    Diagnostic::new(
        "dsl.structural.with_io_in_async",
        format!(
            "{KEYWORD_WITH_IO} cannot be placed inside {KEYWORD_ASYNC}. Place {KEYWORD_WITH_IO} outside {KEYWORD_ASYNC} instead (e.g. {KEYWORD_WITH_IO} [...] {KEYWORD_ASYNC} {KEYWORD_RUN} ...)"
        ),
        Diagnostic::make_fields(&[(FIELD_RULE, rule.as_str())]),
    )
}

/// `COPY --from-host` / `--to-host` outside a `REMOTE` block. Transfer
/// declarations only mean something where a session hoists them.
pub fn copy_host_outside_remote() -> Diagnostic {
    let rule = KEYWORD_COPY.to_lowercase();
    Diagnostic::new(
        "dsl.structural.copy_host_outside_remote",
        format!(
            "{KEYWORD_COPY} --from-host and --to-host are only valid inside a {KEYWORD_REMOTE} block"
        ),
        Diagnostic::make_fields(&[(FIELD_RULE, rule.as_str())]),
    )
}

/// Staged `UnsignedIntBoundary` for `2^63` where unary `-` cannot
/// consume it. Fixed value, parameterless. The `expr` rule is a
/// parser-phase name, not a DSL command, so no `KEYWORD_*` applies.
pub fn int_boundary_overflow() -> Diagnostic {
    Diagnostic::new(
        "dsl.structural.int_boundary_overflow",
        "integer overflow: 9223372036854775808 exceeds i64::MAX".to_string(),
        Diagnostic::make_fields(&[(FIELD_RULE, "expr")]),
    )
}

/// Integer literal exceeding `i64::MAX` under unary `-`. Takes the
/// offending value; quoting lives here.
pub fn int_overflow(n: u64) -> Diagnostic {
    let message = format!("integer overflow: {n} exceeds i64::MAX");
    Diagnostic::new(
        "dsl.structural.int_overflow",
        message,
        Diagnostic::make_fields(&[(FIELD_RULE, "expr"), (FIELD_GOT, &n.to_string())]),
    )
}

/// `WRITE` without a path.
pub fn write_requires_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.write_requires_path",
        format!("{KEYWORD_WRITE} requires a path"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_WRITE)]),
    )
}

/// `APPEND` without a path.
pub fn append_requires_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.append_requires_path",
        format!("{KEYWORD_APPEND} requires a path"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_APPEND)]),
    )
}

/// `APPEND_LINE` without a path.
pub fn append_line_requires_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.append_line_requires_path",
        format!("{KEYWORD_APPEND_LINE} requires a path"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_APPEND_LINE)]),
    )
}

/// `REMOVE` without a path.
pub fn remove_requires_path() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.remove_requires_path",
        format!("{KEYWORD_REMOVE} requires a path"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_REMOVE)]),
    )
}

/// `READ_LINE` without a variable.
pub fn read_line_requires_variable() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.read_line_requires_variable",
        format!("{KEYWORD_READ_LINE} requires a variable"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_READ_LINE)]),
    )
}

/// `READ_LINE` target without a `$` sigil. Takes the rendered
/// argument; quoting lives here. (Renders the value, never the
/// internal `Arg` debug shape.)
pub fn read_line_requires_sigil(got: &str) -> Diagnostic {
    let message = format!("{KEYWORD_READ_LINE} requires a $variable, found {got:?}");
    Diagnostic::new(
        "dsl.validation.read_line_requires_sigil",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_READ_LINE), (FIELD_GOT, got)]),
    )
}

/// `PROMPT` without a variable.
pub fn prompt_requires_variable() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.prompt_requires_variable",
        format!("{KEYWORD_PROMPT} requires a variable"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_PROMPT)]),
    )
}

/// `PROMPT` target without a `$` sigil. Takes the rendered argument.
pub fn prompt_requires_sigil(got: &str) -> Diagnostic {
    let message = format!("{KEYWORD_PROMPT} requires a $variable, found {got:?}");
    Diagnostic::new(
        "dsl.validation.prompt_requires_sigil",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_PROMPT), (FIELD_GOT, got)]),
    )
}

/// `PROMPT` without a message.
pub fn prompt_requires_message() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.prompt_requires_message",
        format!("{KEYWORD_PROMPT} requires a message"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_PROMPT)]),
    )
}

/// `ASSERT_CONTAINS` without a value.
pub fn assert_contains_requires_value() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.assert_contains_requires_value",
        format!("{KEYWORD_ASSERT_CONTAINS} requires a value"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_ASSERT_CONTAINS)]),
    )
}

/// `ASSERT_CONTAINS` without a needle.
pub fn assert_contains_requires_needle() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.assert_contains_requires_needle",
        format!("{KEYWORD_ASSERT_CONTAINS} requires a needle"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_ASSERT_CONTAINS)]),
    )
}

/// `EXIT` without a code.
pub fn exit_requires_code() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.exit_requires_code",
        format!("{KEYWORD_EXIT} requires a code"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_EXIT)]),
    )
}

/// `SLEEP` without a duration.
pub fn sleep_requires_duration() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.sleep_requires_duration",
        format!("{KEYWORD_SLEEP} requires a duration (e.g. {KEYWORD_SLEEP} 500ms)"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_SLEEP)]),
    )
}

/// `SLEEP` with more than one argument.
pub fn sleep_takes_one() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.sleep_takes_one",
        format!("{KEYWORD_SLEEP} takes exactly one duration argument"),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_SLEEP)]),
    )
}

/// `LIST_APPEND` without a LIST variable.
pub fn list_append_requires_list() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.list_append_requires_list",
        format!(
            "{KEYWORD_LIST_APPEND} requires a {LIST_TYPE_NAME} variable (e.g. {KEYWORD_LIST_APPEND} $items $x)"
        ),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_LIST_APPEND)]),
    )
}

/// `LIST_APPEND` target without a `$` sigil. Takes the rendered
/// argument; quoting lives here.
pub fn list_append_requires_sigil(got: &str) -> Diagnostic {
    let message = format!("{KEYWORD_LIST_APPEND} requires a $variable, found {got:?}");
    Diagnostic::new(
        "dsl.validation.list_append_requires_sigil",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_LIST_APPEND), (FIELD_GOT, got)]),
    )
}

/// `LIST_APPEND` without an item.
pub fn list_append_requires_item() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.list_append_requires_item",
        format!(
            "{KEYWORD_LIST_APPEND} requires an item to append (e.g. {KEYWORD_LIST_APPEND} $items $x)"
        ),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_LIST_APPEND)]),
    )
}

/// `LIST_APPEND` with other than two arguments.
pub fn list_append_takes_two() -> Diagnostic {
    Diagnostic::new(
        "dsl.validation.list_append_takes_two",
        format!(
            "{KEYWORD_LIST_APPEND} takes exactly two arguments: {KEYWORD_LIST_APPEND} $list <item>"
        ),
        Diagnostic::make_fields(&[(FIELD_COMMAND, KEYWORD_LIST_APPEND)]),
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
        Diagnostic::make_fields(&[(FIELD_COMMAND, command)]),
    )
}

/// `ASSERT_EQ` / `ASSERT_NE` without an expected value (and no `--hash`).
/// Shared key; invoking command in the field.
pub fn assert_equality_requires_expected(command: &str) -> Diagnostic {
    let message = format!("{command} requires an expected value");
    Diagnostic::new(
        "dsl.validation.assert_equality_requires_expected",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, command)]),
    )
}

/// `ASSERT_EQ` / `ASSERT_NE` combining `--hash` with a positional
/// expected value. Shared key; invoking command in the field.
pub fn assert_equality_hash_with_expected(command: &str) -> Diagnostic {
    let message = format!("{command} --hash cannot be combined with an expected value");
    Diagnostic::new(
        "dsl.validation.assert_equality_hash_with_expected",
        message,
        Diagnostic::make_fields(&[(FIELD_COMMAND, command)]),
    )
}

/// Markdown code fence that never closes. Takes the 1-based line
/// number where the fence opens; the `markdown:{line}` prefix lives
/// here so call sites pass the line only.
pub fn markdown_unclosed_fence(line: usize) -> Diagnostic {
    let message = format!("markdown:{line}: code fence is never closed");
    Diagnostic::new(
        "dsl.markdown.unclosed_fence",
        message,
        Diagnostic::make_fields(&[(FIELD_LINE, &line.to_string())]),
    )
}

/// Unexpected character in fence metadata. Takes the 1-based line
/// number and the character; quoting lives here.
pub fn markdown_metadata_unexpected_char(line: usize, ch: char) -> Diagnostic {
    let message = format!("markdown:{line}: unexpected character `{ch}` in fence metadata");
    Diagnostic::new(
        "dsl.markdown.metadata_unexpected_char",
        message,
        Diagnostic::make_fields(&[
            (FIELD_LINE, &line.to_string()),
            (FIELD_GOT, &ch.to_string()),
        ]),
    )
}

/// Fence metadata item missing `:` after its key. Takes the 1-based
/// line number and the key; quoting lives here.
pub fn markdown_metadata_missing_colon(line: usize, key: &str) -> Diagnostic {
    let message =
        format!("markdown:{line}: fence metadata item `{key}` is missing ':' after its key");
    Diagnostic::new(
        "dsl.markdown.metadata_missing_colon",
        message,
        Diagnostic::make_fields(&[(FIELD_LINE, &line.to_string()), (FIELD_KEY, key)]),
    )
}

/// Fence metadata item using `=` instead of `:`. Takes the 1-based
/// line number and the key; quoting lives here.
pub fn markdown_metadata_equals_syntax(line: usize, key: &str) -> Diagnostic {
    let message =
        format!("markdown:{line}: fence metadata item `{key}` must use `key:value` syntax");
    Diagnostic::new(
        "dsl.markdown.metadata_equals_syntax",
        message,
        Diagnostic::make_fields(&[(FIELD_LINE, &line.to_string()), (FIELD_KEY, key)]),
    )
}

/// Unterminated double quote in fence metadata. Takes the 1-based
/// line number.
pub fn markdown_metadata_unterminated_quote(line: usize) -> Diagnostic {
    let message = format!("markdown:{line}: unterminated double quote in fence metadata");
    Diagnostic::new(
        "dsl.markdown.metadata_unterminated_quote",
        message,
        Diagnostic::make_fields(&[(FIELD_LINE, &line.to_string())]),
    )
}

/// `env` metadata without `KEY=VALUE` shape. Takes the 1-based line
/// number.
pub fn markdown_metadata_env_shape(line: usize) -> Diagnostic {
    let message = format!("markdown:{line}: env metadata must be `env:KEY=VALUE`");
    Diagnostic::new(
        "dsl.markdown.metadata_env_shape",
        message,
        Diagnostic::make_fields(&[(FIELD_LINE, &line.to_string())]),
    )
}

/// Unsupported `roots` metadata value. Takes the 1-based line number
/// and the value; quoting lives here.
pub fn markdown_metadata_roots_unsupported(line: usize, value: &str) -> Diagnostic {
    let message = format!(
        "markdown:{line}: unsupported roots metadata `{value}` (only `unified` is defined)"
    );
    Diagnostic::new(
        "dsl.markdown.metadata_roots_unsupported",
        message,
        Diagnostic::make_fields(&[(FIELD_LINE, &line.to_string()), (FIELD_GOT, value)]),
    )
}

/// Duplicate `roots` metadata. Takes the 1-based line number.
pub fn markdown_metadata_roots_duplicate(line: usize) -> Diagnostic {
    let message = format!("markdown:{line}: roots metadata specified more than once");
    Diagnostic::new(
        "dsl.markdown.metadata_roots_duplicate",
        message,
        Diagnostic::make_fields(&[(FIELD_LINE, &line.to_string())]),
    )
}

/// Duplicate `expect_error` metadata. Takes the 1-based line number.
pub fn markdown_metadata_expect_error_duplicate(line: usize) -> Diagnostic {
    let message = format!("markdown:{line}: expect_error metadata specified more than once");
    Diagnostic::new(
        "dsl.markdown.metadata_expect_error_duplicate",
        message,
        Diagnostic::make_fields(&[(FIELD_LINE, &line.to_string())]),
    )
}

/// Unknown fence metadata key. Takes the 1-based line number and the
/// key; the supported-key list lives here.
pub fn markdown_metadata_unknown_key(line: usize, key: &str) -> Diagnostic {
    let message = format!(
        "markdown:{line}: unknown fence metadata key `{key}` (supported: env, roots, expect_error, mock_remote)"
    );
    Diagnostic::new(
        "dsl.markdown.metadata_unknown_key",
        message,
        Diagnostic::make_fields(&[(FIELD_LINE, &line.to_string()), (FIELD_KEY, key)]),
    )
}

/// Text after a quoted string literal's closer. Takes the source;
/// `Debug` rendering lives here.
pub fn template_string_trailing_text(src: &str) -> Diagnostic {
    let message = format!("unexpected text after string literal: {src:?}");
    Diagnostic::new(
        "dsl.template.string_trailing_text",
        message,
        Diagnostic::make_fields(&[(FIELD_GOT, src)]),
    )
}

/// Quoted string literal with no closer. Takes the source; `Debug`
/// rendering lives here.
pub fn template_string_unterminated(src: &str) -> Diagnostic {
    let message = format!("unterminated string literal: {src:?}");
    Diagnostic::new(
        "dsl.template.string_unterminated",
        message,
        Diagnostic::make_fields(&[(FIELD_GOT, src)]),
    )
}

/// Text after a block string literal's closer. Takes the source;
/// `Debug` rendering lives here.
pub fn template_block_trailing_text(src: &str) -> Diagnostic {
    let message = format!("unexpected text after block string literal: {src:?}");
    Diagnostic::new(
        "dsl.template.block_trailing_text",
        message,
        Diagnostic::make_fields(&[(FIELD_GOT, src)]),
    )
}

/// Block string literal with no closer triple. Takes the source;
/// `Debug` rendering lives here.
pub fn template_block_unterminated(src: &str) -> Diagnostic {
    let message = format!("unterminated block string literal: {src:?}");
    Diagnostic::new(
        "dsl.template.block_unterminated",
        message,
        Diagnostic::make_fields(&[(FIELD_GOT, src)]),
    )
}

/// Placeholder string literal ending in a backslash. Parameterless:
/// the wording is fixed.
pub fn template_string_unterminated_escape() -> Diagnostic {
    Diagnostic::new(
        "dsl.template.string_unterminated_escape",
        "unterminated escape in placeholder string literal".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Map literal key that is neither a bare name nor a string spelling.
/// Takes the source; quoting lives here.
pub fn template_map_key_invalid(src: &str) -> Diagnostic {
    let message =
        format!("invalid map literal key `{src}`: use a bare name or any string spelling");
    Diagnostic::new(
        "dsl.template.map_key_invalid",
        message,
        Diagnostic::make_fields(&[(FIELD_GOT, src)]),
    )
}

/// `modules:` prefix holding a non-module token. Takes the rendered
/// token; quoting lives here.
pub fn macro_input_modules_prefix_other(other: &str) -> Diagnostic {
    let message = format!("modules: prefix holds module names, found `{other}`");
    Diagnostic::new(
        "dsl.macro_input.modules_prefix_other",
        message,
        Diagnostic::make_fields(&[(FIELD_GOT, other)]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_names_both_spellings() {
        let d = alias_duplicate_on_field("h", "help", "h2");
        assert_eq!(d.key(), "dsl.alias.duplicate_on_field");
        assert!(d.message().contains("`h`"));
        assert!(d.message().contains("`help`"));
        assert!(d.message().contains("`h2`"));
    }

    #[test]
    fn trailing_text_is_unexpected_not_expected() {
        let d = spelling_trailing_text("foo");
        assert_eq!(d.key(), "dsl.spelling.trailing_text");
        assert!(d.message().starts_with("unexpected trailing text"));
    }

    #[test]
    fn bracket_list_comment_shares_one_key_across_invokers() {
        let export = spelling_bracket_list_comment("EXPORT");
        let import = spelling_bracket_list_comment("IMPORT");
        assert_eq!(export.key(), "dsl.spelling.bracket_list_comment");
        assert_eq!(import.key(), export.key());
        assert_eq!(
            export.message(),
            "EXPORT lists do not allow comments inside the brackets: move the comment above the statement"
        );
        assert_eq!(
            import.message(),
            "IMPORT lists do not allow comments inside the brackets: move the comment above the statement"
        );
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
            spelling_bracket_list_comment("EXPORT").key(),
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
            invalid_syntax("WRITE", "nothing", None).key(),
            unknown_command("NOPE", None).key(),
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
            markdown_unclosed_fence(3).key(),
            markdown_metadata_unexpected_char(3, '%').key(),
            markdown_metadata_missing_colon(3, "k").key(),
            markdown_metadata_equals_syntax(3, "k").key(),
            markdown_metadata_unterminated_quote(3).key(),
            markdown_metadata_env_shape(3).key(),
            markdown_metadata_roots_unsupported(3, "v").key(),
            markdown_metadata_roots_duplicate(3).key(),
            markdown_metadata_expect_error_duplicate(3).key(),
            markdown_metadata_unknown_key(3, "k").key(),
            template_string_trailing_text("x").key(),
            template_string_unterminated("x").key(),
            template_block_trailing_text("x").key(),
            template_block_unterminated("x").key(),
            template_string_unterminated_escape().key(),
            template_map_key_invalid("x").key(),
            macro_input_modules_prefix_other("x").key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn markdown_template_macro_messages_match_canonical_wording() {
        assert_eq!(
            markdown_unclosed_fence(3).message(),
            "markdown:3: code fence is never closed"
        );
        assert_eq!(
            markdown_unclosed_fence(3).key(),
            "dsl.markdown.unclosed_fence"
        );
        assert_eq!(
            markdown_metadata_unknown_key(3, "bogus").message(),
            "markdown:3: unknown fence metadata key `bogus` (supported: env, roots, expect_error, mock_remote)"
        );
        assert_eq!(
            markdown_metadata_roots_unsupported(3, "squashed").message(),
            "markdown:3: unsupported roots metadata `squashed` (only `unified` is defined)"
        );
        assert_eq!(
            markdown_metadata_env_shape(3).message(),
            "markdown:3: env metadata must be `env:KEY=VALUE`"
        );
        assert_eq!(
            template_string_unterminated("abc").message(),
            "unterminated string literal: \"abc\""
        );
        assert_eq!(
            template_string_trailing_text("abc").message(),
            "unexpected text after string literal: \"abc\""
        );
        assert_eq!(
            template_map_key_invalid("a b").message(),
            "invalid map literal key `a b`: use a bare name or any string spelling"
        );
        assert_eq!(
            template_string_unterminated_escape().message(),
            "unterminated escape in placeholder string literal"
        );
        assert_eq!(
            macro_input_modules_prefix_other("foo").message(),
            "modules: prefix holds module names, found `foo`"
        );
    }

    #[test]
    fn validation_messages_are_command_lead() {
        let d = unknown_flag("WRITE", "--nope");
        assert_eq!(d.key(), "dsl.validation.unknown_flag");
        assert_eq!(d.message(), "WRITE unknown flag `--nope`");
        let d = flag_missing_value("WRITE", "--path");
        assert_eq!(d.message(), "WRITE `--path` requires a value");
        let d = missing_argument("WRITE", "path");
        assert!(d.message().contains("requires argument `path`"));
        assert!(d.message().starts_with("WRITE"));
        let d = too_many_positionals("CMD", 1, 3);
        assert_eq!(
            d.message(),
            "CMD expects at most 1 positional arguments, got 3"
        );
        let err = anyhow::anyhow!("expected PATH");
        let d = arg_type_mismatch("CMD", "path", "3", &err);
        assert_eq!(d.message(), "CMD argument `path` got \"3\": expected PATH");
    }

    #[test]
    fn env_expand_messages_keep_legacy_wording() {
        assert_eq!(
            env_requires_assignment().message(),
            "ENV requires KEY=value"
        );
        assert_eq!(env_bad_format().message(), "ENV requires KEY=value format");
        assert_eq!(
            expand_bad_override().message(),
            "EXPAND requires KEY=value format for overrides"
        );
        assert_eq!(
            expand_extra_path().message(),
            "EXPAND accepts at most one path"
        );
        assert_eq!(
            needs_value("ECHO").message(),
            "ECHO requires at least one argument"
        );
        let inner = anyhow::anyhow!("assignment requires KEY=value format");
        assert_eq!(
            env_assignment_error(&inner).message(),
            "assignment requires KEY=value format"
        );
    }

    #[test]
    fn workspace_copy_messages_keep_legacy_wording() {
        assert_eq!(workdir_requires_path().message(), "WORKDIR requires a path");
        assert_eq!(
            workspace_requires_target().message(),
            "WORKSPACE requires a target"
        );
        assert_eq!(
            workspace_local_needs_cache().message(),
            "WORKSPACE --local requires CACHE"
        );
        assert_eq!(
            workspace_unknown_target("NOPE").message(),
            "unknown workspace target \"NOPE\""
        );
        assert_eq!(
            unknown_workspace_source("COPY", "NOPE").message(),
            "unknown workspace source \"NOPE\""
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
        assert_eq!(
            symlink_requires_source().message(),
            "SYMLINK requires a source"
        );
        assert_eq!(
            symlink_requires_target().message(),
            "SYMLINK requires a target"
        );
    }

    #[test]
    fn fs_path_messages_keep_legacy_wording() {
        assert_eq!(mkdir_requires_path().message(), "MKDIR requires a path");
        assert_eq!(
            install_requires_source().message(),
            "INSTALL requires a source"
        );
        assert_eq!(
            install_requires_destination().message(),
            "INSTALL requires a destination"
        );
        assert_eq!(
            set_executable_requires_path().message(),
            "SET_EXECUTABLE requires a path"
        );
        assert_eq!(
            archive_requires_source().message(),
            "ARCHIVE requires a source"
        );
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
    fn invalid_syntax_and_unknown_command_messages() {
        assert_eq!(
            invalid_syntax("WRITE", "nothing", None).message(),
            "WRITE got nothing"
        );
        assert_eq!(
            invalid_syntax("WRITE", "`x`", Some("use WRITE <path>.")).message(),
            "use WRITE <path>"
        );
        assert_eq!(
            unknown_command("NOPE", None).message(),
            "unknown command \"NOPE\""
        );
        assert_eq!(
            unknown_command("WRIT", Some("did you mean WRITE?.")).message(),
            "unknown command \"WRIT\"\ndid you mean WRITE?"
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
        assert_eq!(
            prompt_requires_message().message(),
            "PROMPT requires a message"
        );
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
        assert!(
            d.message().contains("static check failed"),
            "{}",
            d.message()
        );
    }
}
