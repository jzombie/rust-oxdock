//! DSL typing diagnostics, owned by `oxdock-core`.
//!
//! One constructor per diagnostic at the owning function (`typing.rs`
//! resolution and `collect_type_aliases`). Stable `dsl.*` keys,
//! structured fields, and whole-sentence templates. Call sites pass
//! decoded values, never `format!` strings; joining and quoting live
//! here.

use oxdock_diagnostics::Diagnostic;
use oxdock_parser::template::{bare_sigil_error, qualified_keypath_error};
use oxdock_parser::{
    ANY_TYPE_NAME, BOOL_TYPE_NAME, Expr, FLOAT_TYPE_NAME, Field, HANDLE_TYPE_NAME, INT_TYPE_NAME,
    KEYWORD_APPEND, KEYWORD_ASSERT_CONTAINS, KEYWORD_ASYNC, KEYWORD_AWAIT, KEYWORD_BREAK,
    KEYWORD_CONST, KEYWORD_CONTINUE, KEYWORD_COPY, KEYWORD_COPY_GIT, KEYWORD_ENV, KEYWORD_EXPORT,
    KEYWORD_FUNC, KEYWORD_IMPORT, KEYWORD_INHERIT_ENV, KEYWORD_ISOLATED, KEYWORD_LET,
    KEYWORD_LIST_APPEND, KEYWORD_LS, KEYWORD_MKDIR, KEYWORD_READ, KEYWORD_READ_LINE,
    KEYWORD_REMOTE, KEYWORD_RETURN, KEYWORD_RUN, KEYWORD_SET_EXECUTABLE, KEYWORD_SLEEP,
    KEYWORD_SWITCH, KEYWORD_SYMLINK, KEYWORD_TYPE, KEYWORD_WITH_IO, KEYWORD_WORKDIR, KEYWORD_WRITE,
    LIST_TYPE_NAME, MAP_TYPE_NAME, PERMIT_TYPE_NAME, PIPE_TYPE_NAME, SEMAPHORE_TYPE_NAME,
    STRING_TYPE_NAME, Spelled, TypeTag, VOID_TYPE_NAME, render_structural,
};

use super::function_names::{
    FUNC_BASE64_DECODE, FUNC_BASE64_ENCODE, FUNC_BOOL, FUNC_CONTAINS, FUNC_FLOAT,
    FUNC_FORMAT_BYTE_COUNT, FUNC_FORMAT_DURATION, FUNC_GLOB, FUNC_HAS_KEY, FUNC_INT, FUNC_IS_EXEC,
    FUNC_IS_TERMINAL, FUNC_LEN, FUNC_LOAD_JSON, FUNC_LOAD_TOML, FUNC_MAP_SET, FUNC_MERGE_MAPS,
    FUNC_PARSE_ARGS, FUNC_PARSE_BYTE_COUNT, FUNC_PARSE_DURATION, FUNC_PARSE_FLAGS, FUNC_PARSE_JSON,
    FUNC_PARSE_POSITIONALS, FUNC_PARSE_TOML, FUNC_PATH_BASE, FUNC_PATH_REL, FUNC_PATH_TYPE,
    FUNC_PATH_WRITABLE, FUNC_SEMAPHORE_NEW, FUNC_SHELL_SPLIT_ARGS, FUNC_TO_JSON,
};

/// Duplicate field name inside one MAP spelling.
pub fn map_duplicate_field(field: &str) -> Diagnostic {
    let message = format!("duplicate field `{field}` in `{MAP_TYPE_NAME}` spelling");
    Diagnostic::new(
        "dsl.map.duplicate_field",
        message,
        Diagnostic::make_fields(&[("field", field)]),
    )
}

/// Alias duplicates its own field name.
pub fn alias_self(field: &str, alias: &str) -> Diagnostic {
    let message = format!("alias `{alias}` duplicates its own field `{field}`");
    Diagnostic::new(
        "dsl.alias.self",
        message,
        Diagnostic::make_fields(&[("field", field), ("alias", alias)]),
    )
}

/// Alias clashes with another field or alias. Takes the alias, the
/// field that declares it, and the owner it clashes with.
pub fn alias_clash(alias: &str, field: &str, owner: &str) -> Diagnostic {
    let message = format!("alias `{alias}` for field `{field}` clashes with `{owner}`");
    Diagnostic::new(
        "dsl.alias.clash",
        message,
        Diagnostic::make_fields(&[("alias", alias), ("field", field), ("owner", owner)]),
    )
}

/// Generic nesting deeper than the cap, resolving a bare name.
pub fn typing_max_depth(max: usize, name: &str) -> Diagnostic {
    let message = format!("type spelling exceeds maximum generic depth of {max}: `{name}`");
    Diagnostic::new(
        "dsl.typing.max_depth",
        message,
        Diagnostic::make_fields(&[("max", &max.to_string()), ("spelling", name)]),
    )
}

/// Generic nesting deeper than the cap, building a parsed spelling.
/// Same key as [`typing_max_depth`]: one rule, two entry shapes. The
/// `Debug` rendering lives here so call sites pass the parsed value.
pub fn typing_max_depth_spelled(max: usize, spelled: &Spelled) -> Diagnostic {
    let rendered = format!("{spelled:?}");
    let message = format!("type spelling exceeds maximum generic depth of {max}: `{rendered}`");
    Diagnostic::new(
        "dsl.typing.max_depth",
        message,
        Diagnostic::make_fields(&[("max", &max.to_string()), ("spelling", &rendered)]),
    )
}

/// Bare `MAP` without arguments.
pub fn typing_bare_map() -> Diagnostic {
    let message = format!(
        "bare `{MAP_TYPE_NAME}` is not a declaration type: write a shape (`{MAP_TYPE_NAME}<name: {KEYWORD_TYPE}, ...>`) or `{MAP_TYPE_NAME}<{ANY_TYPE_NAME}>` for maps with unknown keys"
    );
    Diagnostic::new(
        "dsl.typing.bare_map",
        message,
        Diagnostic::make_fields(&[("type", MAP_TYPE_NAME)]),
    )
}

/// Bare `LIST` without arguments.
pub fn typing_bare_list() -> Diagnostic {
    let message = format!(
        "bare `{LIST_TYPE_NAME}` is not a declaration type: write a shape (`{LIST_TYPE_NAME}<{KEYWORD_TYPE}>`) or `{LIST_TYPE_NAME}<{ANY_TYPE_NAME}>` for lists with unknown elements"
    );
    Diagnostic::new(
        "dsl.typing.bare_list",
        message,
        Diagnostic::make_fields(&[("type", LIST_TYPE_NAME)]),
    )
}

/// Bare `HANDLE` without arguments.
pub fn typing_bare_handle() -> Diagnostic {
    let message = format!(
        "bare `{HANDLE_TYPE_NAME}` is not a declaration type: write `{HANDLE_TYPE_NAME}<T>` for a known payload or `{HANDLE_TYPE_NAME}<{ANY_TYPE_NAME}>` for an unknown one"
    );
    Diagnostic::new(
        "dsl.typing.bare_handle",
        message,
        Diagnostic::make_fields(&[("type", HANDLE_TYPE_NAME)]),
    )
}

/// Alias resolution cycle. Takes the resolution chain including the
/// repeated name; joining lives here.
pub fn alias_cycle(chain: &[String]) -> Diagnostic {
    let joined = chain.join(" -> ");
    let message = format!("type alias cycle: {joined}");
    Diagnostic::new(
        "dsl.alias.cycle",
        message,
        Diagnostic::make_fields(&[("chain", &joined)]),
    )
}

/// Unknown type name. Takes the name plus the directory listings;
/// joining lives here so call sites pass slices.
pub fn typing_unknown(
    name: &str,
    types: &[String],
    schemas: &[String],
    aliases: &[String],
) -> Diagnostic {
    let types = types.join(", ");
    let schemas = schemas.join(", ");
    let aliases = aliases.join(", ");
    let message = format!(
        "unknown type `{name}`; known types: {types} and schemas: {schemas} and aliases: {aliases}"
    );
    Diagnostic::new(
        "dsl.typing.unknown",
        message,
        Diagnostic::make_fields(&[
            ("name", name),
            ("types", &types),
            ("schemas", &schemas),
            ("aliases", &aliases),
        ]),
    )
}

/// Declaration names a custom type nobody registered. Takes the
/// use site, the name, and the known-type directory; joining lives
/// here so call sites pass the slice.
pub fn typing_unregistered(what: &str, name: &str, types: &[String]) -> Diagnostic {
    let known = types.join(", ");
    let message = format!("{what} declares unregistered type '{name}'; known types: {known}");
    Diagnostic::new(
        "dsl.typing.unregistered",
        message,
        Diagnostic::make_fields(&[("ctx", what), ("name", name), ("types", &known)]),
    )
}

/// `VOID` outside a return annotation. Takes the use site; the rule
/// (bare `VOID` is a return type, or a `HANDLE` payload) is fixed
/// wording.
pub fn typing_void_position(what: &str) -> Diagnostic {
    let message = format!(
        "{what} declares {VOID_TYPE_NAME} outside a return annotation: {VOID_TYPE_NAME} is only a return type (or {HANDLE_TYPE_NAME}<{VOID_TYPE_NAME}> payload)"
    );
    Diagnostic::new(
        "dsl.typing.void_position",
        message,
        Diagnostic::make_fields(&[("ctx", what)]),
    )
}

/// Unknown options-map key. Takes the function name, the offending
/// key, and the declared option names; joining lives here so call
/// sites pass slices. Owned by `check_options` in `typing.rs`.
pub fn check_unknown_option(func: &str, key: &str, known: &[&str]) -> Diagnostic {
    let expected = known.join(", ");
    let message = format!("{func}() unknown option '{key}' (expected: {expected})");
    Diagnostic::new(
        "dsl.check.unknown_option",
        message,
        Diagnostic::make_fields(&[("function", func), ("key", key), ("expected", &expected)]),
    )
}

/// Options-map value failing its declared shape. Takes the
/// underlying shape failure verbatim: its path prefix already names
/// func, key, expected, and actual, so no wrapper is added.
/// Owned by `check_options` in `typing.rs`.
pub fn check_option_value(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.check.option_value",
        reason_text.clone(),
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// Type arguments on a head that takes none.
pub fn typing_no_args(name: &str) -> Diagnostic {
    let message = format!("`{name}` takes no type arguments");
    Diagnostic::new(
        "dsl.typing.no_args",
        message,
        Diagnostic::make_fields(&[("type", name)]),
    )
}

/// `HANDLE` with anything but one bare argument.
pub fn typing_handle_arity() -> Diagnostic {
    let message = format!("`{HANDLE_TYPE_NAME}` takes exactly one bare type argument");
    Diagnostic::new(
        "dsl.typing.handle_arity",
        message,
        Diagnostic::make_fields(&[("type", HANDLE_TYPE_NAME)]),
    )
}

/// `LIST` with anything but one bare argument.
pub fn typing_list_arity() -> Diagnostic {
    let message = format!("`{LIST_TYPE_NAME}` takes exactly one bare type argument");
    Diagnostic::new(
        "dsl.typing.list_arity",
        message,
        Diagnostic::make_fields(&[("type", LIST_TYPE_NAME)]),
    )
}

/// Bare argument where `MAP` requires `name: TYPE`. Takes the head
/// name for the trailing clause.
pub fn typing_map_bare(head: &str) -> Diagnostic {
    let message = format!(
        "`{MAP_TYPE_NAME}` fields require `name: {KEYWORD_TYPE}`; `{head}` takes no bare arguments"
    );
    Diagnostic::new(
        "dsl.typing.map_bare",
        message,
        Diagnostic::make_fields(&[("head", head)]),
    )
}

/// Default literal that does not check against its field type. Takes
/// the field name and the rendered expected type.
pub fn typing_default_mismatch(field: &str, expected: &str) -> Diagnostic {
    let message = format!("default for field `{field}` does not match {expected}");
    Diagnostic::new(
        "dsl.typing.default_mismatch",
        message,
        Diagnostic::make_fields(&[("field", field), ("expected", expected)]),
    )
}

/// Union with no member shapes.
pub fn typing_empty_union() -> Diagnostic {
    Diagnostic::new(
        "dsl.typing.empty_union",
        "empty union: no member shapes to admit".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Alias name already defined as an alias.
pub fn alias_already_defined(name: &str) -> Diagnostic {
    let message = format!("type alias `{name}` is already defined");
    Diagnostic::new(
        "dsl.alias.already_defined",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// Alias name collides with a builtin type.
pub fn alias_builtin_collision(name: &str) -> Diagnostic {
    let message = format!("type alias `{name}` collides with a builtin type");
    Diagnostic::new(
        "dsl.alias.builtin_collision",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// Alias name collides with a registered host type.
pub fn alias_host_collision(name: &str) -> Diagnostic {
    let message = format!("type alias `{name}` collides with registered host type `{name}`");
    Diagnostic::new(
        "dsl.alias.host_collision",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// Alias name collides with a registered record schema.
pub fn alias_schema_collision(name: &str) -> Diagnostic {
    let message = format!("type alias `{name}` collides with registered record schema `{name}`");
    Diagnostic::new(
        "dsl.alias.schema_collision",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// Alias name collides with a top-level function.
pub fn alias_func_collision(name: &str) -> Diagnostic {
    let message = format!("type alias `{name}` collides with function `{name}`");
    Diagnostic::new(
        "dsl.alias.func_collision",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// Last `n` chars of `s` on a char boundary. Error messages carry
/// tails, never whole buffers. Single owner: the MCP call-failure
/// template below is the only user.
pub fn tail_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// MCP init `EXPORT TYPE` naming neither a local `TYPE` nor an
/// imported file's export. Takes the exported name; quoting lives
/// here.
pub fn mcp_export_type_unknown(name: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_EXPORT} {KEYWORD_TYPE} `{name}` matches no local {KEYWORD_TYPE} and no imported file exports it"
    );
    Diagnostic::new(
        "dsl.mcp.export_type_unknown",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// MCP linear init hit `BREAK` outside any loop. Takes the 1-based
/// step number, matching the core runtime `step N` convention.
pub fn mcp_break_outside_loop(step: usize) -> Diagnostic {
    let message = format!("step {step}: {KEYWORD_BREAK} outside loop");
    Diagnostic::new(
        "dsl.mcp.break_outside_loop",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string())]),
    )
}

/// MCP linear init hit `CONTINUE` outside any loop.
pub fn mcp_continue_outside_loop(step: usize) -> Diagnostic {
    let message = format!("step {step}: {KEYWORD_CONTINUE} outside loop");
    Diagnostic::new(
        "dsl.mcp.continue_outside_loop",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string())]),
    )
}

/// MCP linear init hit `RETURN` outside any function, task, or block.
pub fn mcp_return_outside_function(step: usize) -> Diagnostic {
    let message = format!(
        "step {step}: {KEYWORD_RETURN} outside function, {KEYWORD_ASYNC} task, or {KEYWORD_LET} block"
    );
    Diagnostic::new(
        "dsl.mcp.return_outside_function",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string())]),
    )
}

/// MCP dispatch naming no exported tool. Takes the base name.
pub fn mcp_unknown_tool(base: &str) -> Diagnostic {
    let message = format!("unknown MCP tool `{base}`");
    Diagnostic::new(
        "dsl.mcp.unknown_tool",
        message,
        Diagnostic::make_fields(&[("tool", base)]),
    )
}

/// MCP tool call failure with drained capture tails. Takes the
/// underlying failure plus the drained stdout/stderr tails; section
/// headers and the 4096-char tail bound live here so call sites pass
/// structured data only. The tails ride the message (human
/// diagnosis), the structured field carries the reason chain.
pub fn mcp_call_failed(reason: &anyhow::Error, stdout: &str, stderr: &str) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let mut message = reason_text.clone();
    if !stdout.is_empty() {
        message.push_str("\n--- captured stdout ---\n");
        message.push_str(&tail_chars(stdout, 4096));
    }
    if !stderr.is_empty() {
        message.push_str("\n--- captured stderr ---\n");
        message.push_str(&tail_chars(stderr, 4096));
    }
    Diagnostic::new(
        "dsl.mcp.call_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// IMPORT cycle through already-loading files. Takes the resolution
/// chain including the repeated key; joining lives here.
pub fn import_cycle(chain: &[String]) -> Diagnostic {
    let joined = chain.join(" -> ");
    let message = format!("{KEYWORD_IMPORT} cycle detected: {joined}");
    Diagnostic::new(
        "dsl.import.cycle",
        message,
        Diagnostic::make_fields(&[("chain", &joined)]),
    )
}

/// IMPORT nesting deeper than the cap. Takes the cap and the path
/// that exceeded it.
pub fn import_depth_limit(max: usize, path: &str) -> Diagnostic {
    let message = format!("{KEYWORD_IMPORT} depth limit exceeded ({max}): `{path}`");
    Diagnostic::new(
        "dsl.import.depth_limit",
        message,
        Diagnostic::make_fields(&[("max", &max.to_string()), ("path", path)]),
    )
}

/// IMPORT naming a file that does not exist at run start.
pub fn import_not_found(path: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_IMPORT} `{path}` not found: the file must exist when the run starts (generated imports are not supported)"
    );
    Diagnostic::new(
        "dsl.import.not_found",
        message,
        Diagnostic::make_fields(&[("path", path)]),
    )
}

/// IMPORT path that cannot be resolved. Takes the underlying
/// failure; `{reason:#}` formatting lives here so call sites stay
/// `format!`-free.
pub fn import_cannot_resolve(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_IMPORT} `{path}` cannot be resolved: {reason_text}");
    Diagnostic::new(
        "dsl.import.cannot_resolve",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// IMPORT file that cannot be read.
pub fn import_cannot_read(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_IMPORT} `{path}` cannot be read: {reason_text}");
    Diagnostic::new(
        "dsl.import.cannot_read",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// IMPORT file that is not valid UTF-8.
pub fn import_bad_utf8(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_IMPORT} `{path}` is not valid UTF-8: {reason_text}");
    Diagnostic::new(
        "dsl.import.bad_utf8",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// IMPORT file that fails to parse.
pub fn import_parse_failed(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_IMPORT} `{path}` failed to parse: {reason_text}");
    Diagnostic::new(
        "dsl.import.parse_failed",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// IMPORT file not exporting a name. Takes the export kind
/// (`FUNC`/`TYPE`/`CONST`); the template owns the kind word (`FUNC`
/// exports read bare, others spell their kind).
pub fn import_missing_export(path: &str, kind: &str, name: &str) -> Diagnostic {
    let what = if kind == KEYWORD_FUNC {
        String::new()
    } else {
        format!("{kind} ")
    };
    let message = format!("{KEYWORD_IMPORT} `{path}` does not export {what}`{name}`");
    Diagnostic::new(
        "dsl.import.missing_export",
        message,
        Diagnostic::make_fields(&[("path", path), ("kind", kind), ("name", name)]),
    )
}

/// IMPORT file with no such member. Takes the noun as rendered
/// (`function`/`TYPE`/`CONST`).
pub fn import_unknown_member(path: &str, noun: &str, name: &str) -> Diagnostic {
    let message = format!("{KEYWORD_IMPORT} `{path}` has no such {noun} `{name}`");
    Diagnostic::new(
        "dsl.import.unknown_member",
        message,
        Diagnostic::make_fields(&[("path", path), ("noun", noun), ("name", name)]),
    )
}

/// IMPORT file with no such `TYPE`. Takes the optional resolution
/// failure; chained and bare are the same failure mode.
pub fn import_unknown_type(path: &str, name: &str, reason: Option<&anyhow::Error>) -> Diagnostic {
    let message = match reason {
        Some(reason) => {
            format!("{KEYWORD_IMPORT} `{path}` has no such {KEYWORD_TYPE} `{name}`: {reason:#}")
        }
        None => format!("{KEYWORD_IMPORT} `{path}` has no such {KEYWORD_TYPE} `{name}`"),
    };
    Diagnostic::new(
        "dsl.import.unknown_type",
        message,
        Diagnostic::make_fields(&[("path", path), ("name", name)]),
    )
}

/// IMPORT body failing validation. Takes the underlying failure.
pub fn import_failed_validation(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_IMPORT} `{path}` failed validation: {reason_text}");
    Diagnostic::new(
        "dsl.import.failed_validation",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// IMPORT harvesting a stateful function (the file body never runs,
/// so there is no initialized state to observe).
pub fn import_stateful_harvest(path: &str, offender: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_IMPORT} `{path}` cannot harvest stateful function `{offender}`: import the file"
    );
    Diagnostic::new(
        "dsl.import.stateful_harvest",
        message,
        Diagnostic::make_fields(&[("path", path), ("offender", offender)]),
    )
}

/// IMPORT file with no such function.
pub fn import_unknown_function(path: &str, name: &str) -> Diagnostic {
    let message = format!("{KEYWORD_IMPORT} `{path}` has no such function `{name}`");
    Diagnostic::new(
        "dsl.import.unknown_function",
        message,
        Diagnostic::make_fields(&[("path", path), ("name", name)]),
    )
}

/// IMPORT type alias colliding with a different target. Takes the
/// previous and new targets; both ride the message so the two files
/// are named by their targets.
pub fn import_type_collision(path: &str, name: &str, existing: &str, target: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_IMPORT} `{path}` {KEYWORD_TYPE} `{name}` collides: already defined with target `{existing}`, new target `{target}`"
    );
    Diagnostic::new(
        "dsl.import.type_collision",
        message,
        Diagnostic::make_fields(&[
            ("path", path),
            ("name", name),
            ("existing", existing),
            ("target", target),
        ]),
    )
}

/// `unknown CONST` with no import alias exporting the name.
pub fn import_const_no_alias(qualified: &str, alias: &str) -> Diagnostic {
    let message = format!(
        "unknown {KEYWORD_CONST} `{qualified}`: no import alias `{alias}` exports that name"
    );
    Diagnostic::new(
        "dsl.import.const_no_alias",
        message,
        Diagnostic::make_fields(&[("name", qualified), ("alias", alias)]),
    )
}

/// Ambiguous `CONST` alias across files. Takes the alias, the name,
/// and the holder files; joining and the `function`->`CONST` wording
/// live here with the single owner in `native`.
pub fn import_const_ambiguous(
    alias: &str,
    name: &str,
    files: &std::collections::HashSet<String>,
) -> Diagnostic {
    let message = super::native::ambiguous_alias_message(alias, name, files).replacen(
        "function",
        KEYWORD_CONST,
        1,
    );
    Diagnostic::new(
        "dsl.import.const_ambiguous",
        message,
        Diagnostic::make_fields(&[("alias", alias), ("name", name)]),
    )
}

/// `unknown CONST` with a resolution hint (unexported `CONST` or
/// private `LET` under the alias).
pub fn import_const_hinted(qualified: &str, hint: &str) -> Diagnostic {
    let message = format!("unknown {KEYWORD_CONST} `{qualified}`: {hint}");
    Diagnostic::new(
        "dsl.import.const_hinted",
        message,
        Diagnostic::make_fields(&[("name", qualified), ("hint", hint)]),
    )
}

/// Bare `unknown CONST` (no holder, no terminal, no record).
pub fn import_const_unknown(qualified: &str) -> Diagnostic {
    let message = format!("unknown {KEYWORD_CONST} `{qualified}`");
    Diagnostic::new(
        "dsl.import.const_unknown",
        message,
        Diagnostic::make_fields(&[("name", qualified)]),
    )
}

/// `CONST` harvested but never initialized (module read without
/// import).
pub fn import_const_uninitialized(qualified: &str) -> Diagnostic {
    let message = format!(
        "`{qualified}` cannot be read: the module was harvested but never initialized; import the file"
    );
    Diagnostic::new(
        "dsl.import.const_uninitialized",
        message,
        Diagnostic::make_fields(&[("name", qualified)]),
    )
}

/// `LET` binding read as `CONST` (only `CONST` crosses the module
/// boundary).
pub fn import_const_is_let(qualified: &str) -> Diagnostic {
    let message = format!(
        "`{qualified}` is a {KEYWORD_LET} binding and cannot be exported: only {KEYWORD_CONST} bindings cross the module boundary"
    );
    Diagnostic::new(
        "dsl.import.const_is_let",
        message,
        Diagnostic::make_fields(&[("name", qualified)]),
    )
}

/// `CONST` forward-chain failure. Takes the chain error verbatim: the
/// `ForwardError` `Display` in `native` owns the wording
/// (unknown/broken/split/cycle/ambiguous hops), so no wrapper is
/// added and no source is attached.
pub fn import_const_forward_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.import.const_forward_failed",
        reason_text.clone(),
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// Control flow escaping an import boundary during file init. Takes
/// the flow kind (`BREAK`/`CONTINUE`), the 1-based step number, and
/// the importing path.
pub fn import_flow_crosses_boundary(flow: &str, step: usize, path: &str) -> Diagnostic {
    let message =
        format!("step {step}: {flow} cannot cross import boundary (in {KEYWORD_IMPORT} `{path}`)");
    Diagnostic::new(
        "dsl.import.flow_crosses_boundary",
        message,
        Diagnostic::make_fields(&[("flow", flow), ("step", &step.to_string()), ("path", path)]),
    )
}

/// Function registration needing a zero value where none exists.
/// Takes the rendered structural spelling; quoting lives here.
pub fn func_zero_value_missing(rendered: &str) -> Diagnostic {
    let message =
        format!("no zero value for {rendered}; omit the `?` or pass the argument explicitly");
    Diagnostic::new(
        "dsl.func.zero_value_missing",
        message,
        Diagnostic::make_fields(&[("spelling", rendered)]),
    )
}

/// Function parameter type resolution failure. Takes the function
/// name and the underlying failure.
pub fn func_param_type(func: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("function `{func}`: {reason_text}");
    Diagnostic::new(
        "dsl.func.param_type",
        message,
        Diagnostic::make_fields(&[("function", func), ("reason", &reason_text)]),
    )
}

/// Optional function parameter without a zero value. Takes the
/// function and parameter names plus the underlying failure.
pub fn func_optional_param(func: &str, param: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("function `{func}` optional param `${param}`: {reason_text}");
    Diagnostic::new(
        "dsl.func.optional_param",
        message,
        Diagnostic::make_fields(&[
            ("function", func),
            ("param", param),
            ("reason", &reason_text),
        ]),
    )
}

/// Shadowing a reserved (native/host) function.
pub fn func_shadow_reserved(name: &str) -> Diagnostic {
    let message = format!("cannot shadow reserved function `{name}`");
    Diagnostic::new(
        "dsl.func.shadow_reserved",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// Duplicate function definition in the same scope.
pub fn func_duplicate_in_scope(name: &str) -> Diagnostic {
    let message = format!("duplicate function `{name}` in same scope");
    Diagnostic::new(
        "dsl.func.duplicate_in_scope",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// Host parameter declaring a closed value set without a type. The
/// set only means something alongside the type check that enforces
/// it, so the declaration fails naming the parameter and function.
pub fn func_param_values_without_type(param: &str, func: &str) -> Diagnostic {
    let message = format!("param '{param}' of '{func}' declares values without a type");
    Diagnostic::new(
        "dsl.func.param_values_without_type",
        message,
        Diagnostic::make_fields(&[("function", func), ("param", param)]),
    )
}

/// Optional parameter before a required one. Omission fills trailing
/// position only, so the declaration fails naming the offender.
pub fn func_optional_before_required(param: &str, func: &str) -> Diagnostic {
    let message = format!("param '{param}' of '{func}' is optional before a required param");
    Diagnostic::new(
        "dsl.func.optional_before_required",
        message,
        Diagnostic::make_fields(&[("function", func), ("param", param)]),
    )
}

/// Optional parameter whose type does not admit an empty MAP.
/// Omission fills an empty MAP, so the declaration fails naming the
/// parameter and function.
pub fn func_optional_without_map(param: &str, func: &str) -> Diagnostic {
    let message = format!("param '{param}' of '{func}' is optional without a {MAP_TYPE_NAME} type");
    Diagnostic::new(
        "dsl.func.optional_without_map",
        message,
        Diagnostic::make_fields(&[("function", func), ("param", param)]),
    )
}

/// Ambiguous `ALIAS::FUNC` resolution. Takes the alias, the base
/// name, and the holder files; joining lives here with the single
/// owner in `super::native`.
pub fn func_ambiguous_alias(
    alias: &str,
    base: &str,
    files: &std::collections::HashSet<String>,
) -> Diagnostic {
    let message = super::native::ambiguous_alias_message(alias, base, files);
    Diagnostic::new(
        "dsl.func.ambiguous_alias",
        message,
        Diagnostic::make_fields(&[("alias", alias), ("name", base)]),
    )
}

/// `EXPORT` naming no defined script function and no known host
/// function.
pub fn func_export_unknown(name: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_EXPORT} `{name}` matches no defined script function or known host function"
    );
    Diagnostic::new(
        "dsl.func.export_unknown",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// Unknown placeholder function. Takes the module and function names
/// plus the known functions; sorting and joining live here.
pub fn func_placeholder_unknown(module: &str, func: &str, known: &[String]) -> Diagnostic {
    let mut known: Vec<&str> = known.iter().map(|s| s.as_str()).collect();
    known.sort();
    let joined = known.join(", ");
    let message =
        format!("unknown placeholder function `{module}::{func}`; known functions: {joined}");
    Diagnostic::new(
        "dsl.func.placeholder_unknown",
        message,
        Diagnostic::make_fields(&[("module", module), ("name", func), ("known", &joined)]),
    )
}

/// `DESCRIBE` with an unqualified name.
pub fn func_describe_unqualified(name: &str) -> Diagnostic {
    let message = format!(
        "unknown function `{name}`: DESCRIBE requires a qualified name (e.g. `STD::{name}`)"
    );
    Diagnostic::new(
        "dsl.func.describe_unqualified",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// Unknown function name.
pub fn func_unknown(name: &str) -> Diagnostic {
    let message = format!("unknown function `{name}`");
    Diagnostic::new(
        "dsl.func.unknown",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// Unknown type name (bare builtin lookup).
pub fn std_unknown_type(name: &str) -> Diagnostic {
    let message = format!("unknown type `{name}`");
    Diagnostic::new(
        "dsl.std.unknown_type",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// `CONTAINS` with a non-string needle for a string haystack. Takes
/// the actual needle type name.
pub fn std_contains_needle_type(actual: &str) -> Diagnostic {
    let message = format!(
        "{FUNC_CONTAINS} needs a {STRING_TYPE_NAME} needle for a {STRING_TYPE_NAME} haystack, found `{actual}`"
    );
    Diagnostic::new(
        "dsl.std.contains_needle_type",
        message,
        Diagnostic::make_fields(&[("actual", actual)]),
    )
}

/// `CONTAINS` with a non-string, non-list haystack.
pub fn std_contains_haystack_type(actual: &str) -> Diagnostic {
    let message = format!(
        "{FUNC_CONTAINS} needs a {STRING_TYPE_NAME} or {LIST_TYPE_NAME} haystack, found `{actual}`"
    );
    Diagnostic::new(
        "dsl.std.contains_haystack_type",
        message,
        Diagnostic::make_fields(&[("actual", actual)]),
    )
}

/// `LEN` with a non-string, non-list, non-map value.
pub fn std_len_unexpected_type(actual: &str) -> Diagnostic {
    let message = format!(
        "{FUNC_LEN} needs a {STRING_TYPE_NAME}, {LIST_TYPE_NAME}, or {MAP_TYPE_NAME} value, got `{actual}`"
    );
    Diagnostic::new(
        "dsl.std.len_unexpected_type",
        message,
        Diagnostic::make_fields(&[("actual", actual)]),
    )
}

/// `IS_TERMINAL` with an unexpected stream name.
pub fn std_is_terminal_expected(stream: &str) -> Diagnostic {
    let message =
        format!("{FUNC_IS_TERMINAL} expects \"stdin\", \"stdout\", or \"stderr\", got {stream:?}");
    Diagnostic::new(
        "dsl.std.is_terminal_expected",
        message,
        Diagnostic::make_fields(&[("got", stream)]),
    )
}

/// `SEMAPHORE_NEW` with a non-positive max.
pub fn std_semaphore_needs_positive(max: i64) -> Diagnostic {
    let message = format!("{FUNC_SEMAPHORE_NEW}() requires a positive max, got {max}");
    Diagnostic::new(
        "dsl.std.semaphore_needs_positive",
        message,
        Diagnostic::make_fields(&[("max", &max.to_string())]),
    )
}

/// `BOOL` with a non-bool, non-string value. Takes the value;
/// `Debug` rendering lives here.
pub fn std_bool_needs_bool_or_string(got: &super::Value) -> Diagnostic {
    let message = format!("{FUNC_BOOL} needs a Bool or String, found {got:?}");
    Diagnostic::new(
        "dsl.std.bool_needs_bool_or_string",
        message,
        Diagnostic::make_fields(&[("got", &format!("{got:?}"))]),
    )
}

/// `BOOL`/`BOOL_OR` with an uncoercible string. Takes the invoking
/// command (same failure mode, one key).
pub fn std_bool_cannot_coerce(command: &str, got: &str) -> Diagnostic {
    let message = format!("{command} cannot coerce {got:?} to {BOOL_TYPE_NAME}");
    Diagnostic::new(
        "dsl.std.bool_cannot_coerce",
        message,
        Diagnostic::make_fields(&[("command", command), ("got", got)]),
    )
}

/// `PATH_BASE` failure. Takes the underlying failure.
pub fn std_path_base_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{FUNC_PATH_BASE}: {reason_text}");
    Diagnostic::new(
        "dsl.std.path_base_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// `PATH_REL` failure. Takes the underlying failure.
pub fn std_path_rel_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{FUNC_PATH_REL}: {reason_text}");
    Diagnostic::new(
        "dsl.std.path_rel_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// `SHELL_SPLIT_ARGS` failure. Takes the underlying failure.
pub fn std_shell_split_failed(reason: &shell_words::ParseError) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = format!("{FUNC_SHELL_SPLIT_ARGS}: {reason_text}");
    Diagnostic::new(
        "dsl.std.shell_split_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// `PARSE_ARGS` with other than one argument.
pub fn std_parse_args_arity(got: usize) -> Diagnostic {
    let message = format!("{FUNC_PARSE_ARGS}() expects 1 argument, got {got}");
    Diagnostic::new(
        "dsl.std.parse_args_arity",
        message,
        Diagnostic::make_fields(&[("got", &got.to_string())]),
    )
}

/// `PARSE_ARGS` BOOL flag with an unparseable string. Takes the flag
/// and the rendered value; quoting lives here.
pub fn std_parse_args_bool_flag(flag: &str, got: &str) -> Diagnostic {
    let message = format!(
        "{FUNC_PARSE_ARGS} flag --{flag} expects {BOOL_TYPE_NAME} (true/false, 1/0, yes/no, on/off), got {got:?}"
    );
    Diagnostic::new(
        "dsl.std.parse_args_bool_flag",
        message,
        Diagnostic::make_fields(&[("flag", flag), ("got", got)]),
    )
}

/// `PARSE_ARGS` BOOL flag with a non-string value. Takes the flag and
/// the actual type name.
pub fn std_parse_args_bool_type(flag: &str, actual: &str) -> Diagnostic {
    let message =
        format!("{FUNC_PARSE_ARGS} flag --{flag} expects {BOOL_TYPE_NAME}, got `{actual}`");
    Diagnostic::new(
        "dsl.std.parse_args_bool_type",
        message,
        Diagnostic::make_fields(&[("flag", flag), ("actual", actual)]),
    )
}

/// `PARSE_ARGS` flag without a value.
pub fn std_parse_args_needs_value(flag: &str) -> Diagnostic {
    let message = format!("{FUNC_PARSE_ARGS} flag --{flag} needs a value: use --{flag}=<value>");
    Diagnostic::new(
        "dsl.std.parse_args_needs_value",
        message,
        Diagnostic::make_fields(&[("flag", flag)]),
    )
}

/// `PARSE_ARGS` INT flag with an unparseable value.
pub fn std_parse_args_expects_int(flag: &str) -> Diagnostic {
    let message = format!("{FUNC_PARSE_ARGS} flag --{flag} expects {INT_TYPE_NAME}");
    Diagnostic::new(
        "dsl.std.parse_args_expects_int",
        message,
        Diagnostic::make_fields(&[("flag", flag)]),
    )
}

/// `PARSE_ARGS` FLOAT flag with an unparseable value.
pub fn std_parse_args_expects_float(flag: &str) -> Diagnostic {
    let message = format!("{FUNC_PARSE_ARGS} flag --{flag} expects {FLOAT_TYPE_NAME}");
    Diagnostic::new(
        "dsl.std.parse_args_expects_float",
        message,
        Diagnostic::make_fields(&[("flag", flag)]),
    )
}

/// `PARSE_ARGS` with a non-UTF-8 argument.
pub fn std_parse_args_not_utf8() -> Diagnostic {
    Diagnostic::new(
        "dsl.std.parse_args_not_utf8",
        format!("{FUNC_PARSE_ARGS}: argument must be UTF-8"),
        Diagnostic::make_fields(&[]),
    )
}

/// `PARSE_ARGS` lexing failure. Takes the underlying failure.
pub fn std_parse_args_lex_error(reason: &lexopt::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{FUNC_PARSE_ARGS}: {reason_text}");
    Diagnostic::new(
        "dsl.std.parse_args_lex_error",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// `PARSE_ARGS` schema with an empty flag name.
pub fn std_parse_args_schema_flag_empty() -> Diagnostic {
    Diagnostic::new(
        "dsl.std.parse_args_schema_flag_empty",
        format!("{FUNC_PARSE_ARGS} schema flag names must be non-empty"),
        Diagnostic::make_fields(&[]),
    )
}

/// `PARSE_ARGS` flag given twice (canonical or alias). Takes the flag
/// and its optional alias; the display name joins both spellings
/// here.
pub fn std_parse_args_duplicate_flag(flag: &str, alias: Option<&str>) -> Diagnostic {
    let display = match alias {
        Some(alias) => format!("--{flag}/--{alias}"),
        None => format!("--{flag}"),
    };
    let message = format!("{FUNC_PARSE_ARGS} duplicate flag {display}");
    Diagnostic::new(
        "dsl.std.parse_args_duplicate_flag",
        message,
        Diagnostic::make_fields(&[("flag", flag), ("display", &display)]),
    )
}

/// `PARSE_ARGS` missing a required flag.
pub fn std_parse_args_missing_flag(flag: &str, alias: Option<&str>) -> Diagnostic {
    let display = match alias {
        Some(alias) => format!("--{flag}/--{alias}"),
        None => format!("--{flag}"),
    };
    let message = format!("{FUNC_PARSE_ARGS} missing required flag {display}");
    Diagnostic::new(
        "dsl.std.parse_args_missing_flag",
        message,
        Diagnostic::make_fields(&[("flag", flag), ("display", &display)]),
    )
}

/// `PARSE_ARGS` with an undeclared flag.
pub fn std_parse_args_unknown_flag(flag: &str) -> Diagnostic {
    let message = format!("{FUNC_PARSE_ARGS} unknown flag --{flag}");
    Diagnostic::new(
        "dsl.std.parse_args_unknown_flag",
        message,
        Diagnostic::make_fields(&[("flag", flag)]),
    )
}

/// `PARSE_ARGS` schema type resolution failure. Takes the schema name
/// and the underlying failure.
pub fn std_parse_args_schema_type(name: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{FUNC_PARSE_ARGS} schema type {name:?}: {reason_text}");
    Diagnostic::new(
        "dsl.std.parse_args_schema_type",
        message,
        Diagnostic::make_fields(&[("name", name), ("reason", &reason_text)]),
    )
}

/// `PARSE_ARGS` schema type that is not a MAP of scalars.
pub fn std_parse_args_schema_shape(name: &str) -> Diagnostic {
    let message = format!(
        "{FUNC_PARSE_ARGS} schema type {name:?} must be a {MAP_TYPE_NAME} shape with scalar fields"
    );
    Diagnostic::new(
        "dsl.std.parse_args_schema_shape",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// `PARSE_ARGS` schema field with a non-scalar type. Takes the field
/// name and the actual type name.
pub fn std_parse_args_schema_field_type(field: &str, actual: &str) -> Diagnostic {
    let message = format!(
        "{FUNC_PARSE_ARGS} schema for --{field}: type must be one of {STRING_TYPE_NAME}, {BOOL_TYPE_NAME}, {INT_TYPE_NAME}, or {FLOAT_TYPE_NAME}, got {actual}"
    );
    Diagnostic::new(
        "dsl.std.parse_args_schema_field_type",
        message,
        Diagnostic::make_fields(&[("field", field), ("actual", actual)]),
    )
}

/// `PARSE_ARGS` trio with a non-`LIST<STRING>` argv value. Takes the
/// underlying shape failure verbatim: its path prefix already names
/// the entry point and argument, so no wrapper is added.
pub fn std_parse_args_arg_shape(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.std.parse_args_arg_shape",
        reason_text.clone(),
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// `PARSE_FLAGS` with other than one argument.
pub fn std_parse_flags_arity(got: usize) -> Diagnostic {
    let message = format!("{FUNC_PARSE_FLAGS}() expects 1 argument, got {got}");
    Diagnostic::new(
        "dsl.std.parse_flags_arity",
        message,
        Diagnostic::make_fields(&[("got", &got.to_string())]),
    )
}

/// `PARSE_FLAGS` with positional arguments. Takes the positionals;
/// joining lives here.
pub fn std_parse_flags_positional(items: &[String]) -> Diagnostic {
    let joined = items.join(", ");
    let message =
        format!("{FUNC_PARSE_FLAGS}() does not accept positional arguments; found {joined}");
    Diagnostic::new(
        "dsl.std.parse_flags_positional",
        message,
        Diagnostic::make_fields(&[("found", &joined)]),
    )
}

/// `PARSE_POSITIONALS` with other than one argument.
pub fn std_parse_positionals_arity(got: usize) -> Diagnostic {
    let message = format!("{FUNC_PARSE_POSITIONALS}() expects 1 argument, got {got}");
    Diagnostic::new(
        "dsl.std.parse_positionals_arity",
        message,
        Diagnostic::make_fields(&[("got", &got.to_string())]),
    )
}

/// `PARSE_BYTE_COUNT` with a negative count.
pub fn std_parse_byte_count_negative(n: i64) -> Diagnostic {
    let message = format!("{FUNC_PARSE_BYTE_COUNT} needs a non-negative byte count, got {n}");
    Diagnostic::new(
        "dsl.std.parse_byte_count_negative",
        message,
        Diagnostic::make_fields(&[("got", &n.to_string())]),
    )
}

/// `PARSE_BYTE_COUNT` with a non-integral float.
pub fn std_parse_byte_count_integral(got: f64) -> Diagnostic {
    let message = format!("{FUNC_PARSE_BYTE_COUNT} needs an integral byte count, got {got:?}");
    Diagnostic::new(
        "dsl.std.parse_byte_count_integral",
        message,
        Diagnostic::make_fields(&[("got", &format!("{got:?}"))]),
    )
}

/// `PARSE_BYTE_COUNT` with neither a token nor a number. Takes
/// the value; `Debug` rendering lives here.
pub fn std_parse_byte_count_shape(got: &super::Value) -> Diagnostic {
    let message = format!("{FUNC_PARSE_BYTE_COUNT} needs a token or number, found {got:?}");
    Diagnostic::new(
        "dsl.std.parse_byte_count_shape",
        message,
        Diagnostic::make_fields(&[("got", &format!("{got:?}"))]),
    )
}

/// `PARSE_BYTE_COUNT` out of `INT` range.
pub fn std_parse_byte_count_range(s: &str) -> Diagnostic {
    let message = format!("{FUNC_PARSE_BYTE_COUNT} out of {INT_TYPE_NAME} range: {s:?}");
    Diagnostic::new(
        "dsl.std.parse_byte_count_range",
        message,
        Diagnostic::make_fields(&[("got", s)]),
    )
}

/// `INT()` with a non-integral `FLOAT`. Takes the value; `Debug`
/// rendering lives here.
pub fn std_int_non_integer_float(got: f64) -> Diagnostic {
    let message =
        format!("{FUNC_INT}() requires an integer value, found {FLOAT_TYPE_NAME} ({got:?})");
    Diagnostic::new(
        "dsl.std.int_non_integer_float",
        message,
        Diagnostic::make_fields(&[("got", &format!("{got:?}"))]),
    )
}

/// `INT()` with an unparsable string. Takes the raw spelling.
pub fn std_int_bad_string(s: &str) -> Diagnostic {
    let message = format!("{FUNC_INT}() requires an integer string, found {s:?}");
    Diagnostic::new(
        "dsl.std.int_bad_string",
        message,
        Diagnostic::make_fields(&[("got", s)]),
    )
}

/// `INT()` with a non-numeric type. Takes the value; `Debug`
/// rendering lives here.
pub fn std_int_bad_type(got: &super::Value) -> Diagnostic {
    let message = format!("{FUNC_INT}() requires an Int, Float, Bool, or String, found {got:?}");
    Diagnostic::new(
        "dsl.std.int_bad_type",
        message,
        Diagnostic::make_fields(&[("got", &format!("{got:?}"))]),
    )
}

/// `FLOAT()` with a non-finite `FLOAT`. Takes the value; `Debug`
/// rendering lives here.
pub fn std_float_non_finite_float(got: f64) -> Diagnostic {
    let message =
        format!("{FUNC_FLOAT}() requires a finite value, found {FLOAT_TYPE_NAME} ({got:?})");
    Diagnostic::new(
        "dsl.std.float_non_finite_float",
        message,
        Diagnostic::make_fields(&[("got", &format!("{got:?}"))]),
    )
}

/// `FLOAT()` with an unparsable string. Takes the raw spelling.
pub fn std_float_bad_string(s: &str) -> Diagnostic {
    let message = format!("{FUNC_FLOAT}() requires a numeric string, found {s:?}");
    Diagnostic::new(
        "dsl.std.float_bad_string",
        message,
        Diagnostic::make_fields(&[("got", s)]),
    )
}

/// `FLOAT()` with a string parsing to a non-finite value. Takes the
/// raw spelling.
pub fn std_float_non_finite_string(s: &str) -> Diagnostic {
    let message = format!("{FUNC_FLOAT}() requires a finite value, found {s:?}");
    Diagnostic::new(
        "dsl.std.float_non_finite_string",
        message,
        Diagnostic::make_fields(&[("got", s)]),
    )
}

/// `FLOAT()` with a non-numeric type. Takes the value; `Debug`
/// rendering lives here.
pub fn std_float_bad_type(got: &super::Value) -> Diagnostic {
    let message = format!("{FUNC_FLOAT}() requires an Int, Float, or String, found {got:?}");
    Diagnostic::new(
        "dsl.std.float_bad_type",
        message,
        Diagnostic::make_fields(&[("got", &format!("{got:?}"))]),
    )
}

/// Compiled-math stack underflow at runtime. Parameterless: the RPN
/// shape already names the site, so no operand is attached.
pub fn eval_math_stack_underflow() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.math_stack_underflow",
        "compiled math stack underflow".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Compiled-math stack imbalance at finish: the RPN left other than
/// exactly one value. Takes the leftover count; rendering lives here.
pub fn eval_math_stack_len(len: usize) -> Diagnostic {
    let message = format!("compiled math left {len} values on the stack");
    Diagnostic::new(
        "dsl.eval.math_stack_len",
        message,
        Diagnostic::make_fields(&[("count", &len.to_string())]),
    )
}

/// Integer arithmetic overflow or division by zero. Parameterless:
/// the operands ride the enclosing expression, so no value is attached.
pub fn eval_arith_overflow_div_zero() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.arith_overflow_div_zero",
        "arithmetic error: integer overflow or division by zero".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Non-numeric operands to a binary arithmetic operator. Takes both
/// operands; `Debug` rendering lives here.
pub fn eval_arith_needs_numeric(left: &super::Value, right: &super::Value) -> Diagnostic {
    let message =
        format!("Type Error: arithmetic requires Int or Float, found {left:?} and {right:?}");
    Diagnostic::new(
        "dsl.eval.arith_needs_numeric",
        message,
        Diagnostic::make_fields(&[
            ("left", &format!("{left:?}")),
            ("right", &format!("{right:?}")),
        ]),
    )
}

/// Float division by zero. Parameterless: the divisor rides the
/// enclosing expression.
pub fn eval_float_div_zero() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.float_div_zero",
        "arithmetic error: float division by zero".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Non-finite float result from arithmetic or negation. Parameterless:
/// shared by both paths so one key owns the failure mode.
pub fn eval_non_finite_result() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.non_finite_result",
        "arithmetic error: non-finite float result".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Integer overflow on unary negation. Parameterless: the operand
/// rides the enclosing expression.
pub fn eval_neg_overflow() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.neg_overflow",
        "arithmetic error: integer overflow".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Non-numeric operand to unary negation. Takes the value; `Debug`
/// rendering lives here.
pub fn eval_neg_needs_numeric(got: &super::Value) -> Diagnostic {
    let message = format!("Type Error: unary '-' requires Int or Float, found {got:?}");
    Diagnostic::new(
        "dsl.eval.neg_needs_numeric",
        message,
        Diagnostic::make_fields(&[("got", &format!("{got:?}"))]),
    )
}

/// Non-`BOOL` condition value. Takes the value; `Debug` rendering
/// lives here.
pub fn eval_condition_needs_bool(got: &super::Value) -> Diagnostic {
    let message = format!("Type Error: condition must be a Bool, found {got:?}");
    Diagnostic::new(
        "dsl.eval.condition_needs_bool",
        message,
        Diagnostic::make_fields(&[("got", &format!("{got:?}"))]),
    )
}

/// `int`-typed argument with a non-integer spelling. Takes the
/// resolved text; `Debug` rendering lives here.
pub fn eval_expected_int(got: &str) -> Diagnostic {
    let message = format!("expected int, got {got:?}");
    Diagnostic::new(
        "dsl.eval.expected_int",
        message,
        Diagnostic::make_fields(&[("got", got)]),
    )
}

/// Dispatch into a not-yet-migrated `anyhow` layer (placeholder
/// calls, block bodies, statement/RPN call entries) that returned an
/// error carrying no keyed diagnostic. Takes the failure verbatim:
/// keyed diagnostics keep their keys through `lift_dispatch_error`
/// downcast and never reach here, so this is the rare unkeyed tail.
pub fn eval_dispatch_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.eval.dispatch_failed",
        reason_text.clone(),
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// Pipe content peek behind a pipe assertion. Takes the underlying
/// failure verbatim: it already names the handle and the cause, so
/// no wrapper is added.
pub fn eval_pipe_assertion_read_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("step pipe assertion cannot read pipe: {reason_text}");
    Diagnostic::new(
        "dsl.eval.pipe_assertion_read_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// `WITH_IO` block reaching the leaf pipeline. Parameterless: the
/// parser expands every block before execution, so arrival here is an
/// internal invariant, never a script spelling.
pub fn eval_with_io_unexpanded() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.with_io_unexpanded",
        format!("{KEYWORD_WITH_IO} block should have been expanded during parsing"),
        Diagnostic::make_fields(&[]),
    )
}

/// `ASYNC` scope observing cancellation. Parameterless: the wording
/// names the surface; no operand varies.
pub fn eval_async_cancelled() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.async_cancelled",
        format!("{KEYWORD_ASYNC} task cancelled"),
        Diagnostic::make_fields(&[]),
    )
}

/// Background task exiting nonzero. Takes the named task id when the
/// task is named (`None` for anonymous pipeline handles) and the
/// platform exit status; the two surfaces share one key. The status
/// rendering stays platform-native, so tests pin the prefix only.
pub fn eval_async_exit_status(task: Option<u64>, status: &std::process::ExitStatus) -> Diagnostic {
    let status_text = format!("{status}");
    let message = match task {
        Some(id) => format!("named {KEYWORD_ASYNC} task {id} exited with status {status_text}"),
        None => format!("{KEYWORD_ASYNC} process exited with status {status_text}"),
    };
    let task_text = task.map(|id| id.to_string());
    let mut fields: Vec<(&str, &str)> = Vec::with_capacity(2);
    if let Some(task) = task_text.as_deref() {
        fields.push(("task", task));
    }
    fields.push(("status", &status_text));
    Diagnostic::new(
        "dsl.eval.async_exit_status",
        message,
        Diagnostic::make_fields(&fields),
    )
}

/// Runtime flow word crossing a `REMOTE` boundary. Takes the 1-based
/// step number and the word; mirrors the static async/func/block
/// family, with the runtime article preserved verbatim.
pub fn eval_flow_crosses_remote(step: usize, word: &str) -> Diagnostic {
    let message = format!("step {step}: {word} cannot cross a {KEYWORD_REMOTE} boundary");
    Diagnostic::new(
        "dsl.eval.flow_crosses_remote",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("flow", word)]),
    )
}

/// Stdin stream lock poisoned (a worker thread panicked while holding
/// it). Takes the invoking verb; every site reports the same shape,
/// so one key covers READ, READ_LINE, WRITE, APPEND, EXPAND, and the
/// assert drain paths. The poison flag carries no detail, so no
/// source rides along.
pub fn eval_stdin_lock_failed(func: &str) -> Diagnostic {
    let message = format!("failed to lock stdin for {func}");
    Diagnostic::new(
        "dsl.eval.stdin_lock_failed",
        message,
        Diagnostic::make_fields(&[("func", func)]),
    )
}

/// Stdin chunk read failure. Takes the invoking verb when the message
/// names one (`None` for the bare READ/READ_LINE/EXPAND paths) and
/// the underlying I/O failure; the text is inlined, so no source
/// rides along. One key covers both shapes, mirroring
/// `eval_async_exit_status` and its optional task field.
pub fn eval_stdin_read_failed(func: Option<&str>, reason: &std::io::Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = match func {
        Some(verb) => format!("failed to read from stdin for {verb}: {reason_text}"),
        None => format!("failed to read from stdin: {reason_text}"),
    };
    let mut fields: Vec<(&str, &str)> = Vec::with_capacity(2);
    if let Some(verb) = func {
        fields.push(("func", verb));
    }
    fields.push(("reason", &reason_text));
    Diagnostic::new(
        "dsl.eval.stdin_read_failed",
        message,
        Diagnostic::make_fields(&fields),
    )
}

/// Exact-match stdin drain exceeding the byte cap. Takes the invoking
/// verb; the cap and the remediation are fixed, so the verb is the
/// only field. No source rides along.
pub fn eval_assert_stdin_overflow(verb: &str) -> Diagnostic {
    let message = format!(
        "{verb} stdin exceeded the 8 MiB exact buffer; assert via pipe targets or harness expect.stdout"
    );
    Diagnostic::new(
        "dsl.eval.assert_stdin_overflow",
        message,
        Diagnostic::make_fields(&[("verb", verb)]),
    )
}

/// Trial-cumulative exact stdout already overflowed. Takes the invoking
/// verb; the cap and the remediation are fixed, so the verb is the
/// only field. No source rides along.
pub fn eval_assert_stdout_overflow(verb: &str) -> Diagnostic {
    let message = format!(
        "{verb} stdout overflowed the 8 MiB exact buffer; assert via pipe targets or harness expect.stdout"
    );
    Diagnostic::new(
        "dsl.eval.assert_stdout_overflow",
        message,
        Diagnostic::make_fields(&[("verb", verb)]),
    )
}

/// Exact stdout read with no capture registered for the generation.
/// Takes the invoking verb; no source rides along.
pub fn eval_assert_no_exact_capture(verb: &str) -> Diagnostic {
    let message = format!("{verb} stdout has no exact capture registered");
    Diagnostic::new(
        "dsl.eval.assert_no_exact_capture",
        message,
        Diagnostic::make_fields(&[("verb", verb)]),
    )
}

/// Assert `--hash` digest mismatch. Takes the invoking verb, the
/// expected hex digest, and the computed hex digest; `Debug`
/// rendering is not needed, both sides are hex text. No source
/// rides along.
pub fn eval_assert_hash_mismatch(verb: &str, expected: &str, computed: &str) -> Diagnostic {
    let message = format!("{verb} --hash mismatch: expected {expected}, computed {computed}");
    Diagnostic::new(
        "dsl.eval.assert_hash_mismatch",
        message,
        Diagnostic::make_fields(&[
            ("verb", verb),
            ("expected", expected),
            ("computed", computed),
        ]),
    )
}

/// Assert `--hash` unexpected match under `ASSERT_NE`. Takes the
/// invoking verb and the matching digest. No source rides along.
pub fn eval_assert_hash_match(verb: &str, sha: &str) -> Diagnostic {
    let message = format!("{verb} --hash match: content matches {sha}");
    Diagnostic::new(
        "dsl.eval.assert_hash_match",
        message,
        Diagnostic::make_fields(&[("verb", verb), ("sha", sha)]),
    )
}

/// Assert `--hash` with a non-string actual. Takes the invoking verb
/// and the offending value; `Debug` rendering lives here.
pub fn eval_assert_hash_needs_string(verb: &str, actual: &super::Value) -> Diagnostic {
    let message = format!("{verb} --hash needs a string actual, found {actual:?}");
    Diagnostic::new(
        "dsl.eval.assert_hash_needs_string",
        message,
        Diagnostic::make_fields(&[("verb", verb), ("actual", &format!("{actual:?}"))]),
    )
}

/// Assert stream mismatch when the expected side is an optional
/// value (missing or non-string). Takes the invoking verb, the
/// optional expected value, and the actual text; `Debug` rendering
/// lives here for both sides.
pub fn eval_assert_stream_mismatch(
    verb: &str,
    expected: Option<&super::Value>,
    actual: &str,
) -> Diagnostic {
    let message = format!("{verb} stream mismatch\nexpected: {expected:?}\nactual:   {actual:?}");
    Diagnostic::new(
        "dsl.eval.assert_stream_mismatch",
        message,
        Diagnostic::make_fields(&[
            ("verb", verb),
            ("expected", &format!("{expected:?}")),
            ("actual", actual),
        ]),
    )
}

/// Assert stream mismatch when both sides are text. Takes the
/// invoking verb, the expected text, and the actual text; `Debug`
/// rendering lives here.
pub fn eval_assert_stream_mismatch_text(verb: &str, expected: &str, actual: &str) -> Diagnostic {
    let message = format!("{verb} stream mismatch\nexpected: {expected:?}\nactual:   {actual:?}");
    Diagnostic::new(
        "dsl.eval.assert_stream_mismatch_text",
        message,
        Diagnostic::make_fields(&[("verb", verb), ("expected", expected), ("actual", actual)]),
    )
}

/// Assert stream unexpectedly equal under `ASSERT_NE`. Takes the
/// invoking verb and the duplicated text; `Debug` rendering lives
/// here.
pub fn eval_assert_stream_unexpected_equal(verb: &str, expected: &str) -> Diagnostic {
    let message = format!("{verb} stream unexpectedly equal: {expected:?}");
    Diagnostic::new(
        "dsl.eval.assert_stream_unexpected_equal",
        message,
        Diagnostic::make_fields(&[("verb", verb), ("expected", expected)]),
    )
}

/// Assert over `stderr`, which exact equality does not support.
/// Takes the invoking verb; the remediation names the shared
/// `ASSERT_CONTAINS` keyword constant so bytes stay identical.
pub fn eval_assert_stderr_unsupported(verb: &str) -> Diagnostic {
    let message =
        format!("{verb} over stderr is not supported; use {KEYWORD_ASSERT_CONTAINS} stderr ...");
    Diagnostic::new(
        "dsl.eval.assert_stderr_unsupported",
        message,
        Diagnostic::make_fields(&[("verb", verb)]),
    )
}

/// Assert without an expected value. Takes the invoking verb; no
/// source rides along.
pub fn eval_assert_requires_expected(verb: &str) -> Diagnostic {
    let message = format!("{verb} requires an expected value");
    Diagnostic::new(
        "dsl.eval.assert_requires_expected",
        message,
        Diagnostic::make_fields(&[("verb", verb)]),
    )
}

/// Assert value mismatch. Takes the invoking verb and both values;
/// `Debug` rendering lives here.
pub fn eval_assert_value_mismatch(
    verb: &str,
    expected: &super::Value,
    actual: &super::Value,
) -> Diagnostic {
    let message = format!("{verb} mismatch\nexpected: {expected:?}\nactual:   {actual:?}");
    Diagnostic::new(
        "dsl.eval.assert_value_mismatch",
        message,
        Diagnostic::make_fields(&[
            ("verb", verb),
            ("expected", &format!("{expected:?}")),
            ("actual", &format!("{actual:?}")),
        ]),
    )
}

/// Assert values unexpectedly equal under `ASSERT_NE`. Takes the
/// invoking verb and both values; `Debug` rendering lives here.
pub fn eval_assert_value_unexpected_equal(
    verb: &str,
    expected: &super::Value,
    actual: &super::Value,
) -> Diagnostic {
    let message =
        format!("{verb} mismatch: values are equal\nexpected: {expected:?}\nactual:   {actual:?}");
    Diagnostic::new(
        "dsl.eval.assert_value_unexpected_equal",
        message,
        Diagnostic::make_fields(&[
            ("verb", verb),
            ("expected", &format!("{expected:?}")),
            ("actual", &format!("{actual:?}")),
        ]),
    )
}

/// Assert pipe mismatch when the expected side is an optional
/// value (missing or non-string). Takes the invoking verb, the
/// optional expected value, and the actual text; `Debug` rendering
/// lives here for the expected side.
pub fn eval_assert_pipe_mismatch(
    verb: &str,
    expected: Option<&super::Value>,
    actual: &str,
) -> Diagnostic {
    let message = format!("{verb} pipe mismatch\nexpected: {expected:?}\nactual:   {actual:?}");
    Diagnostic::new(
        "dsl.eval.assert_pipe_mismatch",
        message,
        Diagnostic::make_fields(&[
            ("verb", verb),
            ("expected", &format!("{expected:?}")),
            ("actual", actual),
        ]),
    )
}

/// Assert pipe mismatch when both sides are text. Takes the invoking
/// verb, the expected text, and the actual text; `Debug` rendering
/// lives here.
pub fn eval_assert_pipe_mismatch_text(verb: &str, expected: &str, actual: &str) -> Diagnostic {
    let message = format!("{verb} pipe mismatch\nexpected: {expected:?}\nactual:   {actual:?}");
    Diagnostic::new(
        "dsl.eval.assert_pipe_mismatch_text",
        message,
        Diagnostic::make_fields(&[("verb", verb), ("expected", expected), ("actual", actual)]),
    )
}

/// Assert pipe unexpectedly equal under `ASSERT_NE`. Takes the
/// invoking verb and the duplicated text; `Debug` rendering lives
/// here.
pub fn eval_assert_pipe_unexpected_equal(verb: &str, expected: &str) -> Diagnostic {
    let message = format!("{verb} pipe unexpectedly equal: {expected:?}");
    Diagnostic::new(
        "dsl.eval.assert_pipe_unexpected_equal",
        message,
        Diagnostic::make_fields(&[("verb", verb), ("expected", expected)]),
    )
}

/// Containment mismatch over a string haystack. Takes the invoking
/// verb, the needle, and the haystack text; `Debug` rendering of
/// the haystack lives here.
pub fn eval_assert_contains_str_mismatch(verb: &str, needle: &str, actual: &str) -> Diagnostic {
    let message = format!("{verb} did not contain '{needle}'; actual: {actual:?}");
    Diagnostic::new(
        "dsl.eval.assert_contains_str_mismatch",
        message,
        Diagnostic::make_fields(&[("verb", verb), ("needle", needle), ("actual", actual)]),
    )
}

/// Containment mismatch over a non-string value (list or the
/// whole value). Takes the invoking verb, the needle, and the
/// offending value; `Debug` rendering of the value lives here.
pub fn eval_assert_contains_value_mismatch(
    verb: &str,
    needle: &str,
    actual: &super::Value,
) -> Diagnostic {
    let message = format!("{verb} did not contain '{needle}'; actual: {actual:?}");
    Diagnostic::new(
        "dsl.eval.assert_contains_value_mismatch",
        message,
        Diagnostic::make_fields(&[
            ("verb", verb),
            ("needle", needle),
            ("actual", &format!("{actual:?}")),
        ]),
    )
}

/// Containment miss with no emitted detail (map key miss, missing
/// window). Takes the invoking verb and the needle.
pub fn eval_assert_contains_missing(verb: &str, needle: &str) -> Diagnostic {
    let message = format!("{verb} did not contain '{needle}'");
    Diagnostic::new(
        "dsl.eval.assert_contains_missing",
        message,
        Diagnostic::make_fields(&[("verb", verb), ("needle", needle)]),
    )
}

/// Containment over a non-string/list/map haystack. Takes the
/// invoking verb and the offending value; `Debug` rendering lives
/// here.
pub fn eval_assert_contains_needs_haystack(verb: &str, actual: &super::Value) -> Diagnostic {
    let message = format!("{verb} needs a string, list, or map haystack, found {actual:?}");
    Diagnostic::new(
        "dsl.eval.assert_contains_needs_haystack",
        message,
        Diagnostic::make_fields(&[("verb", verb), ("actual", &format!("{actual:?}"))]),
    )
}

/// Containment mismatch with emitted window detail. Takes the
/// invoking verb, the needle, and the emitted buffer; trimming
/// lives here so all three call sites share one wording.
pub fn eval_assert_contains_emitted_mismatch(
    verb: &str,
    needle: &str,
    emitted: &str,
) -> Diagnostic {
    let trimmed = emitted.trim_end();
    let message = format!("{verb} did not contain '{needle}'; emitted:\n{trimmed}");
    Diagnostic::new(
        "dsl.eval.assert_contains_emitted_mismatch",
        message,
        Diagnostic::make_fields(&[("verb", verb), ("needle", needle), ("emitted", trimmed)]),
    )
}

/// Containment mismatch over a pipe buffer. Takes the invoking verb,
/// the needle, and the pipe text; `Debug` rendering of the pipe
/// text lives here.
pub fn eval_assert_contains_pipe_mismatch(verb: &str, needle: &str, hay: &str) -> Diagnostic {
    let message = format!("{verb} did not contain '{needle}'; pipe held {hay:?}");
    Diagnostic::new(
        "dsl.eval.assert_contains_pipe_mismatch",
        message,
        Diagnostic::make_fields(&[("verb", verb), ("needle", needle), ("hay", hay)]),
    )
}

/// Bare `BREAK` outside any loop. Parameterless: the keyword is
/// fixed, so no fields vary. No source rides along.
pub fn eval_break_outside_loop() -> Diagnostic {
    let message = format!("{KEYWORD_BREAK} outside loop");
    Diagnostic::new(
        "dsl.eval.break_outside_loop",
        message,
        Diagnostic::make_fields(&[]),
    )
}

/// Bare `CONTINUE` outside any loop. Parameterless: the keyword is
/// fixed, so no fields vary. No source rides along.
pub fn eval_continue_outside_loop() -> Diagnostic {
    let message = format!("{KEYWORD_CONTINUE} outside loop");
    Diagnostic::new(
        "dsl.eval.continue_outside_loop",
        message,
        Diagnostic::make_fields(&[]),
    )
}

/// Flow word escaping a capture boundary. Takes the 1-based step
/// number and the word (`BREAK`/`CONTINUE`); quoting lives here.
/// One key covers both words, mirroring `check_flow_outside_loop`.
pub fn eval_capture_flow_outside_loop(step: usize, word: &str) -> Diagnostic {
    let message = format!("step {step}: {word} outside loop (cannot be captured)");
    Diagnostic::new(
        "dsl.eval.capture_flow_outside_loop",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("flow", word)]),
    )
}

/// `RETURN` escaping a capture boundary. Takes the 1-based step
/// number. No source rides along.
pub fn eval_capture_return_outside_function(step: usize) -> Diagnostic {
    let message = format!("step {step}: {KEYWORD_RETURN} outside function (cannot be captured)");
    Diagnostic::new(
        "dsl.eval.capture_return_outside_function",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string())]),
    )
}

/// `RUN` interrupted by cancellation. Takes the rendered command
/// (shell text or pty `what`); one key covers both, the template
/// is identical. No source rides along.
pub fn eval_run_cancelled(command: &str) -> Diagnostic {
    let message = format!("{KEYWORD_RUN} {command} interrupted by cancellation");
    Diagnostic::new(
        "dsl.eval.run_cancelled",
        message,
        Diagnostic::make_fields(&[("command", command)]),
    )
}

/// `RUN` argv form interrupted by cancellation. Takes the argv
/// slice; `Debug` rendering lives here. No source rides along.
pub fn eval_run_argv_cancelled(argv: &[String]) -> Diagnostic {
    let message = format!("{KEYWORD_RUN} {argv:?} interrupted by cancellation");
    Diagnostic::new(
        "dsl.eval.run_argv_cancelled",
        message,
        Diagnostic::make_fields(&[("argv", &format!("{argv:?}"))]),
    )
}

/// `RUN` unexpectedly captured output (background contract
/// breach). Takes the rendered shell command. No source rides
/// along.
pub fn eval_run_unexpected_capture(command: &str) -> Diagnostic {
    let message = format!("{KEYWORD_RUN} {command} unexpectedly captured output");
    Diagnostic::new(
        "dsl.eval.run_unexpected_capture",
        message,
        Diagnostic::make_fields(&[("command", command)]),
    )
}

/// `RUN` argv form unexpectedly captured output. Takes the argv
/// slice; `Debug` rendering lives here. No source rides along.
pub fn eval_run_argv_unexpected_capture(argv: &[String]) -> Diagnostic {
    let message = format!("{KEYWORD_RUN} {argv:?} unexpectedly captured output");
    Diagnostic::new(
        "dsl.eval.run_argv_unexpected_capture",
        message,
        Diagnostic::make_fields(&[("argv", &format!("{argv:?}"))]),
    )
}

/// `RUN` shell form exited non-zero. Takes the rendered command and
/// the platform exit status; `Display` rendering of the status
/// lives here (platform-specific, so wording pins use
/// `starts_with`). No source rides along.
pub fn eval_run_exit_status(command: &str, status: &std::process::ExitStatus) -> Diagnostic {
    let message = format!("{KEYWORD_RUN} {command} exited with status {status}");
    Diagnostic::new(
        "dsl.eval.run_exit_status",
        message,
        Diagnostic::make_fields(&[("command", command), ("status", &format!("{status}"))]),
    )
}

/// `RUN` argv form exited non-zero. Takes the argv slice and the
/// platform exit status; `Debug`/`Display` rendering lives here.
/// No source rides along.
pub fn eval_run_argv_exit_status(argv: &[String], status: &std::process::ExitStatus) -> Diagnostic {
    let message = format!("{KEYWORD_RUN} {argv:?} exited with status {status}");
    Diagnostic::new(
        "dsl.eval.run_argv_exit_status",
        message,
        Diagnostic::make_fields(&[
            ("argv", &format!("{argv:?}")),
            ("status", &format!("{status}")),
        ]),
    )
}

/// `RUN` pty form asked to spawn with no program. Takes the rendered
/// `what` (shell argv debug or shell text). No source rides along.
pub fn eval_run_needs_program(command: &str) -> Diagnostic {
    let message = format!("{KEYWORD_RUN} {command} needs at least a program");
    Diagnostic::new(
        "dsl.eval.run_needs_program",
        message,
        Diagnostic::make_fields(&[("command", command)]),
    )
}

/// `RUN` pty form exited non-zero. Takes the rendered `what` and
/// the numeric code. No source rides along.
pub fn eval_run_pty_exit_status(command: &str, code: i64) -> Diagnostic {
    let message = format!("{KEYWORD_RUN} {command} exited with status {code}");
    Diagnostic::new(
        "dsl.eval.run_pty_exit_status",
        message,
        Diagnostic::make_fields(&[("command", command), ("code", &code.to_string())]),
    )
}

/// `RUN` exec form with no arguments. Parameterless: the keyword
/// and the requirement are fixed. No source rides along.
pub fn eval_run_exec_requires_arg() -> Diagnostic {
    let message = format!("{KEYWORD_RUN} exec form requires at least one argument");
    Diagnostic::new(
        "dsl.eval.run_exec_requires_arg",
        message,
        Diagnostic::make_fields(&[]),
    )
}

/// Pipe buffer that is not valid UTF-8. Takes the invoking verb and
/// the conversion failure; `Display` rendering of the failure lives
/// here. One key covers both `ASSERT` equality and `ASSERT_CONTAINS`,
/// mirroring `check_flow_outside_loop`: the verb rides as a
/// structured field. The text is inlined, so no source rides along.
pub fn eval_pipe_not_utf8(verb: &str, reason: &std::string::FromUtf8Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = format!("{verb} pipe content is not UTF-8: {reason_text}");
    Diagnostic::new(
        "dsl.eval.pipe_not_utf8",
        message,
        Diagnostic::make_fields(&[("verb", verb), ("reason", &reason_text)]),
    )
}

/// `RUN` shell or pty spawn failure. Takes the rendered command
/// (shell text or pty `what`) and the underlying failure verbatim:
/// its message already names the cause, so the wrapper only adds the
/// `RUN` prefix. Renders `{reason:#}` inline with no `with_source`,
/// per the passthrough contract. One key covers both, mirroring
/// `eval_run_cancelled`.
pub fn eval_run_spawn_failed(command: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_RUN} {command}: {reason_text}");
    Diagnostic::new(
        "dsl.eval.run_spawn_failed",
        message,
        Diagnostic::make_fields(&[("command", command), ("reason", &reason_text)]),
    )
}

/// `RUN` argv spawn failure. Takes the argv slice and the underlying
/// failure verbatim; `Debug` rendering of the argv lives here and
/// `{reason:#}` is inlined with no `with_source`.
pub fn eval_run_argv_spawn_failed(argv: &[String], reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_RUN} {argv:?}: {reason_text}");
    Diagnostic::new(
        "dsl.eval.run_argv_spawn_failed",
        message,
        Diagnostic::make_fields(&[("argv", &format!("{argv:?}")), ("reason", &reason_text)]),
    )
}

/// Pipe handle in `RUN` argv position. Parameterless: the type name,
/// the `LET` capture recipe, and the `STRING` target are fixed, so no
/// fields vary. No source rides along.
pub fn eval_run_exec_pipe_arg() -> Diagnostic {
    let message = format!(
        "cannot pass {PIPE_TYPE_NAME} as a command argument: pipes carry streams between steps, not text. Capture the producing step's output with {KEYWORD_LET} $var: {STRING_TYPE_NAME} = ... instead."
    );
    Diagnostic::new(
        "dsl.eval.run_exec_pipe_arg",
        message,
        Diagnostic::make_fields(&[]),
    )
}

/// Semaphore in `RUN` argv position. Parameterless: the wording is
/// fixed. No source rides along.
pub fn eval_run_exec_semaphore_arg() -> Diagnostic {
    let message = format!(
        "cannot pass {SEMAPHORE_TYPE_NAME} as a command argument: semaphores coordinate task access between steps, not text."
    );
    Diagnostic::new(
        "dsl.eval.run_exec_semaphore_arg",
        message,
        Diagnostic::make_fields(&[]),
    )
}

/// Permit in `RUN` argv position. Parameterless: the wording is
/// fixed. No source rides along.
pub fn eval_run_exec_permit_arg() -> Diagnostic {
    let message = format!(
        "cannot pass {PERMIT_TYPE_NAME} as a command argument: permits grant scoped task access, not text."
    );
    Diagnostic::new(
        "dsl.eval.run_exec_permit_arg",
        message,
        Diagnostic::make_fields(&[]),
    )
}

/// `RUN` exec-form element that is not stringifiable. Takes the
/// rendered `got` (`map` or `task handle task#N`); one key covers
/// both shapes, mirroring `check_flow_outside_loop`. No source rides
/// along.
pub fn eval_run_exec_element_needs_string(got: &str) -> Diagnostic {
    let message = format!("{KEYWORD_RUN} exec form element must be a string, got {got}");
    Diagnostic::new(
        "dsl.eval.run_exec_element_needs_string",
        message,
        Diagnostic::make_fields(&[("got", got)]),
    )
}

/// `SLEEP` interrupted by cancellation. Parameterless: the keyword
/// is fixed, so no fields vary. No source rides along.
pub fn eval_sleep_cancelled() -> Diagnostic {
    let message = format!("{KEYWORD_SLEEP} interrupted by cancellation");
    Diagnostic::new(
        "dsl.eval.sleep_cancelled",
        message,
        Diagnostic::make_fields(&[]),
    )
}

/// `COPY` transport failure (resolve, place, or copy). Takes the
/// source, destination, and the underlying failure verbatim; one key
/// covers all five call sites, the `from`/`to` pair rides as
/// structured fields. Renders `{reason:#}` inline with no
/// `with_source`, per the passthrough contract.
pub fn eval_copy_failed(from: &str, to: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_COPY} {from} {to}: {reason_text}");
    Diagnostic::new(
        "dsl.eval.copy_failed",
        message,
        Diagnostic::make_fields(&[("from", from), ("to", to), ("reason", &reason_text)]),
    )
}

/// `COPY_GIT` transport failure (resolve or fetch). Takes the
/// revision, source, destination, and the underlying failure
/// verbatim; one key covers both call sites. Renders `{reason:#}`
/// inline with no `with_source`.
pub fn eval_copy_git_failed(rev: &str, from: &str, to: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_COPY_GIT} {rev} {from} {to}: {reason_text}");
    Diagnostic::new(
        "dsl.eval.copy_git_failed",
        message,
        Diagnostic::make_fields(&[
            ("rev", rev),
            ("from", from),
            ("to", to),
            ("reason", &reason_text),
        ]),
    )
}

/// `COPY` source with no file name. Takes the display path; quoting
/// lives here. No source rides along.
pub fn eval_copy_no_file_name(display: &str) -> Diagnostic {
    let message = format!("{KEYWORD_COPY} source has no file name: {display}");
    Diagnostic::new(
        "dsl.eval.copy_no_file_name",
        message,
        Diagnostic::make_fields(&[("path", display)]),
    )
}

/// `COPY` destination escaping the root. Takes the display path
/// and the underlying join failure verbatim; one key covers the
/// single call site. Renders `{reason:#}` inline with no
/// `with_source`.
pub fn eval_copy_destination_escapes(display: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_COPY} destination escapes root: {display}: {reason_text}");
    Diagnostic::new(
        "dsl.eval.copy_destination_escapes",
        message,
        Diagnostic::make_fields(&[("path", display), ("reason", &reason_text)]),
    )
}

/// `SYMLINK` transport failure (resolve, place, or link). Takes the
/// source, destination, and the underlying failure; one key covers all
/// six call sites, mirroring `eval_copy_failed`. The message stays the
/// bare outer context and the failure rides as the chained source, so
/// `err.chain()` keeps its layers and the snapshot renderer keeps
/// printing `step N (SYMLINK from to)` with the cause under `causes:`
/// (inlining would flatten the chain and move the paren). `anyhow`
/// carries no `std::error::Error` impl, so the full-chain text rides an
/// `Other`-kind io error, collapsing deeper layers the same way the
/// process io ctors collapse to kind plus text. Per the chain contract:
/// no interpolation, source attached.
pub fn eval_symlink_failed(from: &str, to: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_SYMLINK} {from} {to}");
    Diagnostic::new(
        "dsl.eval.symlink_failed",
        message,
        Diagnostic::make_fields(&[("from", from), ("to", to), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `SYMLINK` source with no file name. Takes the script spelling;
/// quoting lives here. No source rides along (`Option` context, no
/// underlying error).
pub fn eval_symlink_no_file_name(from: &str) -> Diagnostic {
    let message = format!("{KEYWORD_SYMLINK} source has no file name: {from}");
    Diagnostic::new(
        "dsl.eval.symlink_no_file_name",
        message,
        Diagnostic::make_fields(&[("from", from)]),
    )
}

/// `WORKDIR` resolution failure. Takes the script spelling and the
/// underlying failure. The message stays the bare outer context and
/// the failure rides as the chained source, so `err.chain()` keeps
/// its layers for the snapshot renderer. Per the chain contract:
/// no interpolation, source attached.
pub fn eval_workdir_failed(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_WORKDIR} {path}");
    Diagnostic::new(
        "dsl.eval.workdir_failed",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `MKDIR` destination-resolution failure. Takes the script spelling
/// and the underlying failure. Bare outer context plus chained
/// source, mirroring `eval_symlink_failed`.
pub fn eval_mkdir_resolve_failed(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_MKDIR} {path}");
    Diagnostic::new(
        "dsl.eval.mkdir_resolve_failed",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `MKDIR` creation failure. Takes the resolved display and the
/// underlying failure. The `failed to create dir` wording is kept
/// byte-identical; only the key plus fields are new.
pub fn eval_mkdir_create_failed(display: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to create dir {display}");
    Diagnostic::new(
        "dsl.eval.mkdir_create_failed",
        message,
        Diagnostic::make_fields(&[("path", display), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `SET_EXECUTABLE` transport failure (resolve or chmod). Takes the
/// script spelling and the underlying failure; one key covers both
/// sites since the outer text is identical.
pub fn eval_set_executable_failed(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_SET_EXECUTABLE} {path}");
    Diagnostic::new(
        "dsl.eval.set_executable_failed",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `LS` resolution failure. Takes the optional script spelling (`None`
/// for the bare `LS` form) and the underlying failure. One key covers
/// both resolve sites; the bare form renders without a path.
pub fn eval_ls_resolve_failed(path: Option<&str>, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = match path {
        Some(p) => format!("{KEYWORD_LS} {p}"),
        None => KEYWORD_LS.to_string(),
    };
    let mut fields: Vec<(&str, &str)> = Vec::with_capacity(2);
    if let Some(p) = path {
        fields.push(("path", p));
    }
    fields.push(("reason", &reason_text));
    Diagnostic::new(
        "dsl.eval.ls_resolve_failed",
        message,
        Diagnostic::make_fields(&fields),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `LS` directory-read failure. Takes the resolved display and the
/// underlying failure. Bare outer context plus chained source.
pub fn eval_ls_read_failed(display: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_LS} {display}");
    Diagnostic::new(
        "dsl.eval.ls_read_failed",
        message,
        Diagnostic::make_fields(&[("path", display), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `READ` destination-resolution failure. Takes the script spelling
/// and the underlying failure. Bare outer context plus chained
/// source, mirroring `eval_symlink_failed`.
pub fn eval_read_resolve_failed(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_READ} {path}");
    Diagnostic::new(
        "dsl.eval.read_resolve_failed",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `READ` file-open failure. Takes the resolved display and the
/// underlying failure. The `failed to open` wording is kept
/// byte-identical; the cause rides as the chained source so the
/// snapshot renderer keeps every layer (the missing-file chain
/// test pins the leaf under `causes:`).
pub fn eval_read_open_failed(display: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to open {display}");
    Diagnostic::new(
        "dsl.eval.read_open_failed",
        message,
        Diagnostic::make_fields(&[("path", display), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `READ` chunk-read failure. Takes the underlying I/O failure; the
/// static wording lives here so call sites pass no prose. Bare outer
/// context plus chained source.
pub fn eval_read_file_failed(reason: &std::io::Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    Diagnostic::new(
        "dsl.eval.read_file_failed",
        "failed to read from file".to_string(),
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `READ` stdout-write failure. Takes the underlying I/O failure.
/// One key covers both the file and stdin branches since the outer
/// text is identical.
pub fn eval_read_write_failed(reason: &std::io::Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    Diagnostic::new(
        "dsl.eval.read_write_failed",
        "failed to write to output".to_string(),
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `READ` without a stdin stream. Parameterless: the remediation
/// names the shared keyword constants, so no value is passed in.
pub fn eval_read_needs_stdin() -> Diagnostic {
    let message =
        format!("{KEYWORD_READ} requires stdin (use {KEYWORD_WITH_IO} [stdin=...] {KEYWORD_READ})");
    Diagnostic::new(
        "dsl.eval.read_needs_stdin",
        message,
        Diagnostic::make_fields(&[]),
    )
}

/// `READ_LINE` without a stdin stream. Parameterless; the `$var`
/// suffix is a literal usage hint, not the variable name.
pub fn eval_read_line_needs_stdin() -> Diagnostic {
    let message = format!(
        "{KEYWORD_READ_LINE} requires stdin (use {KEYWORD_WITH_IO} [stdin=...] {KEYWORD_READ_LINE} $var)"
    );
    Diagnostic::new(
        "dsl.eval.read_line_needs_stdin",
        message,
        Diagnostic::make_fields(&[]),
    )
}

/// `READ_LINE` decoding failure. Takes the UTF-8 error; the message
/// stays the bare outer context and the detail rides as the chained
/// source. No interpolation, source attached.
pub fn eval_read_line_not_utf8(reason: &std::string::FromUtf8Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = format!("{KEYWORD_READ_LINE} received non-UTF8 bytes");
    Diagnostic::new(
        "dsl.eval.read_line_not_utf8",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `WRITE` destination-resolution failure. Takes the script spelling
/// and the underlying failure. Bare outer context plus chained
/// source.
pub fn eval_write_resolve_failed(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_WRITE} {path}");
    Diagnostic::new(
        "dsl.eval.write_resolve_failed",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// Parent-directory creation failure. Takes the resolved display and
/// the underlying failure. One key covers `WRITE`, `APPEND`, and
/// `APPEND_LINE`: the outer text names no keyword, so the invoker
/// needs no field.
pub fn eval_ensure_parent_failed(display: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to create parent for {display}");
    Diagnostic::new(
        "dsl.eval.ensure_parent_failed",
        message,
        Diagnostic::make_fields(&[("path", display), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `WRITE` whole-file failure. Takes the resolved display and the
/// underlying failure. Bare outer context plus chained source.
pub fn eval_write_file_failed(display: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to write {display}");
    Diagnostic::new(
        "dsl.eval.write_file_failed",
        message,
        Diagnostic::make_fields(&[("path", display), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `WRITE` without a stdin stream. Takes the script spelling; the
/// remediation lives here. No underlying error, so no source.
pub fn eval_write_needs_stdin(path: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_WRITE} {path} requires stdin (use {KEYWORD_WITH_IO} [stdin=...] {KEYWORD_WRITE})"
    );
    Diagnostic::new(
        "dsl.eval.write_needs_stdin",
        message,
        Diagnostic::make_fields(&[("path", path)]),
    )
}

/// `WRITE` stream-open failure. Takes the resolved display and the
/// underlying failure. Bare outer context plus chained source.
pub fn eval_write_open_failed(display: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to open {display} for writing");
    Diagnostic::new(
        "dsl.eval.write_open_failed",
        message,
        Diagnostic::make_fields(&[("path", display), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `WRITE` streamed-chunk failure. Takes the resolved display and the
/// underlying failure. Bare outer context plus chained source.
pub fn eval_write_stream_failed(display: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to write to {display}");
    Diagnostic::new(
        "dsl.eval.write_stream_failed",
        message,
        Diagnostic::make_fields(&[("path", display), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `APPEND` destination-resolution failure. Takes the script spelling
/// and the underlying failure. Bare outer context plus chained
/// source.
pub fn eval_append_resolve_failed(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{KEYWORD_APPEND} {path}");
    Diagnostic::new(
        "dsl.eval.append_resolve_failed",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `APPEND` write failure (whole-file or streamed chunk). Takes the
/// resolved display and the underlying failure; one key covers both
/// sites since the outer text is identical, mirroring
/// `eval_set_executable_failed`.
pub fn eval_append_failed(display: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to append to {display}");
    Diagnostic::new(
        "dsl.eval.append_failed",
        message,
        Diagnostic::make_fields(&[("path", display), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `APPEND` without a stdin stream. Takes the script spelling; the
/// remediation lives here. No underlying error, so no source.
pub fn eval_append_needs_stdin(path: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_APPEND} {path} requires stdin (use {KEYWORD_WITH_IO} [stdin=...] {KEYWORD_APPEND})"
    );
    Diagnostic::new(
        "dsl.eval.append_needs_stdin",
        message,
        Diagnostic::make_fields(&[("path", path)]),
    )
}

/// `APPEND` stream-open failure. Takes the resolved display and the
/// underlying failure. Bare outer context plus chained source.
pub fn eval_append_open_failed(display: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to open {display} for appending");
    Diagnostic::new(
        "dsl.eval.append_open_failed",
        message,
        Diagnostic::make_fields(&[("path", display), ("reason", &reason_text)]),
    )
    .with_source(std::sync::Arc::new(std::io::Error::other(reason_text)))
}

/// `int`-typed argument whose resolution failed. Takes the underlying
/// failure verbatim: its message already names the argument and the
/// cause, so no wrapper is added.
pub fn eval_int_arg_resolve(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.eval.int_arg_resolve",
        reason_text.clone(),
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// `Arg::Expr` resolved without a `StepCtx`. Takes the expression;
/// `Debug` rendering lives here.
pub fn eval_arg_expr_needs_step_ctx(expr: &Expr) -> Diagnostic {
    let rendered = format!("{expr:?}");
    let message = format!("Arg::Expr cannot be resolved without StepCtx: {rendered}");
    Diagnostic::new(
        "dsl.eval.arg_expr_needs_step_ctx",
        message,
        Diagnostic::make_fields(&[("expr", &rendered)]),
    )
}

/// String expansion failure during `Arg` resolution. Takes the
/// underlying failure verbatim: it already names the variable and the
/// cause, so no wrapper is added.
pub fn eval_arg_expand_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.eval.arg_expand_failed",
        reason_text.clone(),
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// Coercion mismatch: a value word refused at a declared slot. Takes
/// the call-site context, the expected tag, and the offending value;
/// structural rendering and the type word live here.
pub fn coerce_type_mismatch(ctx: &str, expected: &TypeTag, value: &super::Value) -> Diagnostic {
    let expected_text = render_structural(expected);
    let actual = value.type_name();
    let message = format!("{ctx}: TypeMismatch: expected {expected_text}, got {actual}");
    Diagnostic::new(
        "dsl.coerce.type_mismatch",
        message,
        Diagnostic::make_fields(&[
            ("ctx", ctx),
            ("expected", &expected_text),
            ("actual", actual),
        ]),
    )
}

/// Plain string refused at a `PIPE` slot: handles are created
/// explicitly via `LET`, never coerced. Takes the offered string;
/// `Debug` rendering lives here.
pub fn coerce_string_to_pipe(s: &str) -> Diagnostic {
    let message = format!(
        "TypeMismatch: expected {PIPE_TYPE_NAME}, got {STRING_TYPE_NAME} ({s:?}); declare {KEYWORD_LET} $x: {PIPE_TYPE_NAME} and pass $x"
    );
    Diagnostic::new(
        "dsl.coerce.string_to_pipe",
        message,
        Diagnostic::make_fields(&[("got", s)]),
    )
}

/// Pipe handle refused in a string slot: streams are captured via
/// `LET`, never rendered. Parameterless: the wording names the rule,
/// and the operands ride the enclosing call.
pub fn coerce_pipe_to_string() -> Diagnostic {
    let message = format!(
        "coercion: TypeMismatch: expected {STRING_TYPE_NAME}, got {PIPE_TYPE_NAME}: pipes carry streams between steps, not text. Capture the producing step's output with {KEYWORD_LET} $var: {STRING_TYPE_NAME} = ... instead."
    );
    Diagnostic::new(
        "dsl.coerce.pipe_to_string",
        message,
        Diagnostic::make_fields(&[]),
    )
}

/// Shaped-type shape failure inside coercion. Takes the underlying
/// parser failure verbatim: its indexed path already names the
/// expected tag and the offending position, so no wrapper is added.
pub fn coerce_check_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.coerce.check_failed",
        reason_text.clone(),
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// Host return value failing its declared shape. Takes the underlying
/// shape failure verbatim: the caller-supplied context already names
/// the function, so no wrapper is added.
pub fn eval_host_return_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.eval.host_return_failed",
        reason_text.clone(),
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// Scope stack empty on pop: an internal invariant, never a user
/// spelling. Parameterless: no operand varies.
pub fn eval_scope_underflow() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.scope_underflow",
        "scope stack underflow during pop".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `LET` redeclaration in one scope. Takes the key; quoting lives
/// here.
pub fn eval_redeclare_var(key: &str) -> Diagnostic {
    let message = format!(
        "redeclaration error: ${key} already declared in this scope; use ${key} = ... to mutate"
    );
    Diagnostic::new(
        "dsl.eval.redeclare_var",
        message,
        Diagnostic::make_fields(&[("key", key)]),
    )
}

/// `CONST` redeclaration in one scope. Takes the key; the keyword
/// renders from the shared constant.
pub fn eval_redeclare_const(key: &str) -> Diagnostic {
    let message = format!(
        "redeclaration error: {key} already declared in this scope; {} bindings are immutable and cannot redeclare it",
        oxdock_parser::constants::KEYWORD_CONST,
    );
    Diagnostic::new(
        "dsl.eval.redeclare_const",
        message,
        Diagnostic::make_fields(&[("key", key)]),
    )
}

/// Declaration with no variable scope on the stack: an internal
/// invariant, never a user spelling. Parameterless.
pub fn eval_no_var_scope() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.no_var_scope",
        "no variable scope for declaration".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Refused `CONST` write. Takes the offending binding description;
/// the keyword renders from the shared constant. Single wording
/// owner for every runtime `CONST` refusal; the `anyhow` helper in
/// `state.rs` delegates here so macro-generated wrappers share it.
pub fn eval_const_mutation(what: &str) -> Diagnostic {
    let message = format!(
        "cannot mutate {} {what}: {} bindings are deeply immutable",
        oxdock_parser::constants::KEYWORD_CONST,
        oxdock_parser::constants::KEYWORD_CONST,
    );
    Diagnostic::new(
        "dsl.eval.const_mutation",
        message,
        Diagnostic::make_fields(&[("what", what)]),
    )
}

/// Undeclared variable with a declare-first hint. Takes the key and
/// the expected declaration shape (`TYPE` for mutation,
/// `LIST<...>` for append); quoting lives here.
pub fn eval_undeclared_declare(key: &str, expected: &str) -> Diagnostic {
    let message = format!(
        "undeclared variable ${key}: declare it first with {KEYWORD_LET} ${key}: {expected} = ..."
    );
    Diagnostic::new(
        "dsl.eval.undeclared_declare",
        message,
        Diagnostic::make_fields(&[("key", key), ("expected", expected)]),
    )
}

/// Undeclared variable with no hint. Takes the key; quoting lives
/// here.
pub fn eval_undeclared(key: &str) -> Diagnostic {
    let message = format!("undeclared variable ${key}");
    Diagnostic::new(
        "dsl.eval.undeclared",
        message,
        Diagnostic::make_fields(&[("key", key)]),
    )
}

/// `LIST_APPEND` on a non-LIST binding. Takes the key and the actual
/// type name; quoting lives here.
pub fn eval_list_not_list(key: &str, actual: &str) -> Diagnostic {
    let message =
        format!("{KEYWORD_LIST_APPEND} ${key}: variable is {actual}, not {LIST_TYPE_NAME}");
    Diagnostic::new(
        "dsl.eval.list_not_list",
        message,
        Diagnostic::make_fields(&[("key", key), ("actual", actual)]),
    )
}

/// `LIST_APPEND` on a binding whose value is not a list buffer.
/// Takes the key; quoting lives here.
pub fn eval_list_value_not_list(key: &str) -> Diagnostic {
    let message = format!("{KEYWORD_LIST_APPEND} ${key}: variable is not a {LIST_TYPE_NAME} value");
    Diagnostic::new(
        "dsl.eval.list_value_not_list",
        message,
        Diagnostic::make_fields(&[("key", key)]),
    )
}

/// Shaped-list element failing its element tag. Takes the underlying
/// shape failure verbatim: its indexed path already names the
/// position, so no wrapper is added.
pub fn eval_list_element_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.eval.list_element_failed",
        reason_text.clone(),
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// Key path on `$ALIAS::NAME` in a placeholder call arg. Parameterless:
/// the parser owns the wording through its template helper, so every
/// phase agrees word for word; this constructor only adds the key.
pub fn eval_const_arg_keypath() -> Diagnostic {
    let message = qualified_keypath_error();
    Diagnostic::new(
        "dsl.eval.const_arg_keypath",
        message,
        Diagnostic::make_fields(&[]),
    )
}

/// Bare word in a placeholder call arg where only `$` reads a binding.
/// Takes the trimmed source; the parser owns the wording through its
/// template helper, so every phase agrees word for word.
pub fn eval_const_arg_bare_sigil(src: &str) -> Diagnostic {
    let message = bare_sigil_error(src);
    Diagnostic::new(
        "dsl.eval.const_arg_bare_sigil",
        message,
        Diagnostic::make_fields(&[("src", src)]),
    )
}

/// Unresolvable bare key in template expansion. Takes the key and
/// whether the name bridges from the host environment: the hint names
/// the `env:` form when bridged, else the `$` sigil form. Both call
/// sites (bare keys and bare call heads) share this key.
pub fn eval_template_missing_key(key: &str, env_hint: bool) -> Diagnostic {
    let suggestion = if env_hint {
        format!("env:{key}")
    } else {
        format!("${key}")
    };
    let message = if env_hint {
        format!(
            "undefined script variable: '{key}'; did you mean 'env:{key}' (environment variable)?"
        )
    } else {
        format!("undefined script variable: '{key}'; did you mean '${key}'?")
    };
    Diagnostic::new(
        "dsl.eval.template_missing_key",
        message,
        Diagnostic::make_fields(&[("key", key), ("suggestion", &suggestion)]),
    )
}

/// Empty `{{ }}` placeholder. Parameterless: the wording names the
/// rule.
pub fn eval_template_empty_placeholder() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.template_empty_placeholder",
        "empty template placeholder".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Malformed `$` placeholder key. Takes the rest after `$`; `Debug`
/// rendering is unnecessary here since the key is already displayable.
pub fn eval_template_invalid_key(rest: &str) -> Diagnostic {
    let message = format!("invalid placeholder key: '${rest}'");
    Diagnostic::new(
        "dsl.eval.template_invalid_key",
        message,
        Diagnostic::make_fields(&[("key", rest)]),
    )
}

/// Empty `env:` / `script_env:` name. Parameterless: both prefixes
/// share this key, and the wording names the empty spelling.
pub fn eval_template_empty_env() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.template_empty_env",
        "undefined environment variable: ''".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Key path on `$ALIAS::NAME` in template expansion. Parameterless:
/// the parser owns the wording through its template helper, so every
/// phase agrees word for word; this constructor only adds the key.
pub fn eval_template_keypath() -> Diagnostic {
    let message = qualified_keypath_error();
    Diagnostic::new(
        "dsl.eval.template_keypath",
        message,
        Diagnostic::make_fields(&[]),
    )
}

/// Bare word in template expansion where only `$` reads a binding.
/// Takes the trimmed source; the parser owns the wording through its
/// template helper, so every phase agrees word for word.
pub fn eval_template_bare_sigil(src: &str) -> Diagnostic {
    let message = bare_sigil_error(src);
    Diagnostic::new(
        "dsl.eval.template_bare_sigil",
        message,
        Diagnostic::make_fields(&[("src", src)]),
    )
}

/// Placeholder miss classified lazily at runtime. Takes the classified
/// text verbatim: it already names the module, name, and miss kind,
/// so no wrapper is added and no source is attached.
pub fn eval_template_placeholder_miss(text: &str) -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.template_placeholder_miss",
        text.to_string(),
        Diagnostic::make_fields(&[("reason", text)]),
    )
}

/// Pipe input read failure. Takes the invoking function and the
/// underlying failure.
pub fn std_pipe_read_failed(func: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{func} cannot read {PIPE_TYPE_NAME} input: {reason_text}");
    Diagnostic::new(
        "dsl.std.pipe_read_failed",
        message,
        Diagnostic::make_fields(&[("func", func), ("reason", &reason_text)]),
    )
}

/// Pipe output creation failure. Takes the invoking function and the
/// underlying failure.
pub fn std_pipe_create_failed(func: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{func} cannot create {PIPE_TYPE_NAME} output: {reason_text}");
    Diagnostic::new(
        "dsl.std.pipe_create_failed",
        message,
        Diagnostic::make_fields(&[("func", func), ("reason", &reason_text)]),
    )
}

/// Pump thread spawn failure. Takes the invoking function and the
/// underlying I/O failure.
pub fn std_pipe_spawn_failed(func: &str, reason: &std::io::Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = format!("{func} cannot spawn pump thread: {reason_text}");
    Diagnostic::new(
        "dsl.std.pipe_spawn_failed",
        message,
        Diagnostic::make_fields(&[("func", func), ("reason", &reason_text)]),
    )
}

/// Pipe reader lock poisoned during an eager read. Takes the invoking
/// function; the poison flag carries no detail.
pub fn std_pipe_reader_lock_poisoned(func: &str) -> Diagnostic {
    let message = format!("{func} pipe reader lock poisoned");
    Diagnostic::new(
        "dsl.std.pipe_reader_lock_poisoned",
        message,
        Diagnostic::make_fields(&[("func", func)]),
    )
}

/// Pipe writer lock poisoned during a completed-pipe write. Takes the
/// invoking function; the poison flag carries no detail.
pub fn std_pipe_writer_lock_poisoned(func: &str) -> Diagnostic {
    let message = format!("{func} pipe writer lock poisoned");
    Diagnostic::new(
        "dsl.std.pipe_writer_lock_poisoned",
        message,
        Diagnostic::make_fields(&[("func", func)]),
    )
}

/// Take-twice on an OS pipe half: the descriptor already moved to a
/// previous binding. Parameterless: every site reports the identical
/// remedy, so the wording lives here once.
pub fn std_pipe_handle_spent() -> Diagnostic {
    Diagnostic::new(
        "dsl.std.pipe_handle_spent",
        format!(
            "OS pipe handle has already been consumed by another binding; declare a fresh {KEYWORD_LET} $x: {PIPE_TYPE_NAME} for a new session"
        ),
        Diagnostic::make_fields(&[]),
    )
}

/// Host pipe accessor handed a non-PIPE word. Takes the invoking
/// function and the offending type; quoting lives here.
pub fn std_pipe_needs_pipe(func: &str, actual: &str) -> Diagnostic {
    let message = format!("host {func} needs a {PIPE_TYPE_NAME} value, got {actual}");
    Diagnostic::new(
        "dsl.std.pipe_needs_pipe",
        message,
        Diagnostic::make_fields(&[("func", func), ("actual", actual)]),
    )
}

/// Force-close on a pipe without a script backend: unbound and OS
/// handles close by dropping their taken halves. Parameterless: the
/// single call site names the function in the fixed text.
pub fn std_pipe_close_needs_script() -> Diagnostic {
    Diagnostic::new(
        "dsl.std.pipe_close_needs_script",
        "host close_pipe needs a script-materialized pipe (unbound and OS handles cannot be force-closed)".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Assertion-observer lock poisoned (a worker thread panicked while
/// holding it). Takes the lock name; every site reports the same
/// shape, so one key covers all three locks.
pub fn std_assert_lock_poisoned(lock: &str) -> Diagnostic {
    let message = format!("{lock} poisoned");
    Diagnostic::new(
        "dsl.std.assert_lock_poisoned",
        message,
        Diagnostic::make_fields(&[("lock", lock)]),
    )
}

/// Chunk read failure inside the eager-read loop. Takes the invoking
/// function and the underlying I/O failure with `Display` rendering:
/// the loop reports `{e}`, while the outer open reports `{e:#}`, so
/// this stays a separate key to hold the wording byte-identical.
pub fn std_pipe_chunk_read_failed(func: &str, reason: &std::io::Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = format!("{func} cannot read {PIPE_TYPE_NAME} input: {reason_text}");
    Diagnostic::new(
        "dsl.std.pipe_chunk_read_failed",
        message,
        Diagnostic::make_fields(&[("func", func), ("reason", &reason_text)]),
    )
}

/// Pipe input that is not valid UTF-8. Takes the invoking function;
/// the offending bytes carry no displayable detail, so no reason
/// field rides along.
pub fn std_pipe_input_not_utf8(func: &str) -> Diagnostic {
    let message = format!("{func} {PIPE_TYPE_NAME} input is not valid UTF-8");
    Diagnostic::new(
        "dsl.std.pipe_input_not_utf8",
        message,
        Diagnostic::make_fields(&[("func", func)]),
    )
}

/// Spill backlog overflow while staging eager pipe input. Takes the
/// invoking function and the underlying I/O failure.
pub fn std_pipe_spill_failed(func: &str, reason: &std::io::Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = format!("{func} {PIPE_TYPE_NAME} input exceeds the spill backlog: {reason_text}");
    Diagnostic::new(
        "dsl.std.pipe_spill_failed",
        message,
        Diagnostic::make_fields(&[("func", func), ("reason", &reason_text)]),
    )
}

/// Completed-pipe write failure. Takes the invoking function and the
/// underlying I/O failure; `write_all` and `flush` share this key.
pub fn std_pipe_write_failed(func: &str, reason: &std::io::Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = format!("{func} cannot write {PIPE_TYPE_NAME} output: {reason_text}");
    Diagnostic::new(
        "dsl.std.pipe_write_failed",
        message,
        Diagnostic::make_fields(&[("func", func), ("reason", &reason_text)]),
    )
}

/// Unreachable `==`/`!=` shape in the strict-equality fallback.
/// Parameterless: the operands ride the enclosing expression.
pub fn eval_compare_equality_shape() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.compare_equality_shape",
        "internal error: equality shape".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Ordering comparison on non-numeric operands. Takes both operands;
/// `Debug` rendering lives here.
pub fn eval_ordering_needs_numeric(left: &super::Value, right: &super::Value) -> Diagnostic {
    let message = format!(
        "Type Error: ordering comparison requires Int or Float, found {left:?} and {right:?}"
    );
    Diagnostic::new(
        "dsl.eval.ordering_needs_numeric",
        message,
        Diagnostic::make_fields(&[
            ("left", &format!("{left:?}")),
            ("right", &format!("{right:?}")),
        ]),
    )
}

/// Unstaged `2^63` boundary literal reaching evaluation. Takes the raw
/// value; the parser stages negation, so arrival here is internal.
pub fn eval_unstaged_int_boundary(n: u64) -> Diagnostic {
    let message = format!("internal error: unstaged integer boundary {n}");
    Diagnostic::new(
        "dsl.eval.unstaged_int_boundary",
        message,
        Diagnostic::make_fields(&[("got", &n.to_string())]),
    )
}

/// Unbound variable read at runtime. Takes the name; the `$` sigil is
/// part of the template, not the field.
pub fn eval_undefined_var(name: &str) -> Diagnostic {
    let message = format!("undefined variable ${name}");
    Diagnostic::new(
        "dsl.eval.undefined_var",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// Missing key on a map read. Takes the key; quoting lives here.
pub fn eval_key_not_found(key: &str) -> Diagnostic {
    let message = format!("Key '{key}' not found in map");
    Diagnostic::new(
        "dsl.eval.key_not_found",
        message,
        Diagnostic::make_fields(&[("key", key)]),
    )
}

/// Non-numeric index on a list read. Takes the key; quoting lives here.
pub fn eval_invalid_array_index(key: &str) -> Diagnostic {
    let message = format!("Invalid array index '{key}'");
    Diagnostic::new(
        "dsl.eval.invalid_array_index",
        message,
        Diagnostic::make_fields(&[("key", key)]),
    )
}

/// Out-of-range index on a list read. Takes the parsed index.
pub fn eval_index_out_of_bounds(idx: usize) -> Diagnostic {
    let message = format!("Index {idx} out of bounds");
    Diagnostic::new(
        "dsl.eval.index_out_of_bounds",
        message,
        Diagnostic::make_fields(&[("index", &idx.to_string())]),
    )
}

/// Key-path step into a scalar. Takes the key; quoting lives here.
pub fn eval_traverse_scalar(key: &str) -> Diagnostic {
    let message = format!("Cannot traverse into scalar value at key '{key}'");
    Diagnostic::new(
        "dsl.eval.traverse_scalar",
        message,
        Diagnostic::make_fields(&[("key", key)]),
    )
}

/// `PATH_TYPE` without its path argument. Parameterless: the arity
/// shape already names the call, so no operand is attached.
pub fn eval_path_type_needs_arg() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.path_type_needs_arg",
        format!("{FUNC_PATH_TYPE} requires a path argument"),
        Diagnostic::make_fields(&[]),
    )
}

/// `PATH_TYPE` with a non-string path. Parameterless: the value rides
/// the enclosing call, so no operand is attached.
pub fn eval_path_type_needs_string() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.path_type_needs_string",
        format!("{FUNC_PATH_TYPE} path argument must evaluate to a string"),
        Diagnostic::make_fields(&[]),
    )
}

/// `IS_EXEC` without its name argument. Parameterless.
pub fn eval_is_exec_needs_arg() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.is_exec_needs_arg",
        format!("{FUNC_IS_EXEC} requires a name argument"),
        Diagnostic::make_fields(&[]),
    )
}

/// `IS_EXEC` with a non-string name. Parameterless.
pub fn eval_is_exec_needs_string() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.is_exec_needs_string",
        format!("{FUNC_IS_EXEC} name argument must evaluate to a string"),
        Diagnostic::make_fields(&[]),
    )
}

/// `PATH_WRITABLE` without its path argument. Parameterless.
pub fn eval_path_writable_needs_arg() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.path_writable_needs_arg",
        format!("{FUNC_PATH_WRITABLE} requires a path argument"),
        Diagnostic::make_fields(&[]),
    )
}

/// `PATH_WRITABLE` with a non-string path. Parameterless.
pub fn eval_path_writable_needs_string() -> Diagnostic {
    Diagnostic::new(
        "dsl.eval.path_writable_needs_string",
        format!("{FUNC_PATH_WRITABLE} path argument must evaluate to a string"),
        Diagnostic::make_fields(&[]),
    )
}

/// Path resolution failure behind the path predicates. Takes the
/// spelling and the underlying failure; shared by `PATH_TYPE` and
/// `PATH_WRITABLE`, whose texts are identical.
pub fn eval_path_resolve_failed(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to resolve path '{path}': {reason_text}");
    Diagnostic::new(
        "dsl.eval.path_resolve_failed",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// Working-directory resolution failure behind `PATH_WRITABLE`'s
/// upward probe. Takes the underlying failure.
pub fn eval_workdir_resolve_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to resolve working directory: {reason_text}");
    Diagnostic::new(
        "dsl.eval.workdir_resolve_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// Session scratch hierarchy failing to materialize (root plus
/// `tmp` for seeded `HOME`/`TMPDIR`). Takes the underlying failure
/// verbatim; every spawn choke point shares this one key.
pub fn eval_scratch_materialize_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.eval.scratch_materialize_failed",
        reason_text.clone(),
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// Live-task cap refusing a spawn. Takes the configured maximum;
/// the call site passes the shared constant, so no number is
/// hardcoded here.
pub fn eval_task_cap_exceeded(max: u64) -> Diagnostic {
    let message =
        format!("concurrency limit exceeded: maximum of {max} running background tasks allowed");
    Diagnostic::new(
        "dsl.eval.task_cap_exceeded",
        message,
        Diagnostic::make_fields(&[("max", &max.to_string())]),
    )
}

/// `FORMAT_BYTE_COUNT` with a negative count.
pub fn std_format_byte_count_negative(bytes: i64) -> Diagnostic {
    let message = format!("{FUNC_FORMAT_BYTE_COUNT} needs a non-negative byte count, got {bytes}");
    Diagnostic::new(
        "dsl.std.format_byte_count_negative",
        message,
        Diagnostic::make_fields(&[("got", &bytes.to_string())]),
    )
}

/// `FORMAT_DURATION` with negative milliseconds.
pub fn std_format_duration_negative(millis: i64) -> Diagnostic {
    let message = format!("{FUNC_FORMAT_DURATION} needs non-negative milliseconds, got {millis}");
    Diagnostic::new(
        "dsl.std.format_duration_negative",
        message,
        Diagnostic::make_fields(&[("got", &millis.to_string())]),
    )
}

/// `PARSE_DURATION` out of `INT` range.
pub fn std_parse_duration_range(text: &str) -> Diagnostic {
    let message = format!("{FUNC_PARSE_DURATION} out of {INT_TYPE_NAME} range: {text:?}");
    Diagnostic::new(
        "dsl.std.parse_duration_range",
        message,
        Diagnostic::make_fields(&[("got", text)]),
    )
}

/// `TO_JSON` on a non-finite float. Takes the float; rendering lives
/// here.
pub fn std_to_json_non_finite_float(f: f64) -> Diagnostic {
    let message = format!("{FUNC_TO_JSON} cannot encode non-finite float {f}");
    Diagnostic::new(
        "dsl.std.to_json_non_finite_float",
        message,
        Diagnostic::make_fields(&[("got", &f.to_string())]),
    )
}

/// `TO_JSON` on a value outside the template-safe set. Takes the type
/// name; the admit list lives here.
pub fn std_to_json_unencodable(actual: &str) -> Diagnostic {
    let message = format!(
        "{FUNC_TO_JSON} cannot encode {actual} values; use {STRING_TYPE_NAME}, {INT_TYPE_NAME}, {FLOAT_TYPE_NAME}, {BOOL_TYPE_NAME}, {LIST_TYPE_NAME}, or {MAP_TYPE_NAME}"
    );
    Diagnostic::new(
        "dsl.std.to_json_unencodable",
        message,
        Diagnostic::make_fields(&[("actual", actual)]),
    )
}

/// `TO_JSON` serialization failure. Takes the underlying failure.
pub fn std_to_json_encode_failed(reason: &serde_json::Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = format!("encode JSON: {reason_text}");
    Diagnostic::new(
        "dsl.std.to_json_encode_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// `BASE64_ENCODE` with a non-string value. Takes the actual type
/// name.
pub fn std_base64_encode_needs_string(actual: &str) -> Diagnostic {
    let message = format!("{FUNC_BASE64_ENCODE} expects a {STRING_TYPE_NAME}, got {actual}");
    Diagnostic::new(
        "dsl.std.base64_encode_needs_string",
        message,
        Diagnostic::make_fields(&[("actual", actual)]),
    )
}

/// `BASE64_DECODE` with a non-string value. Takes the actual type
/// name.
pub fn std_base64_decode_needs_string(actual: &str) -> Diagnostic {
    let message = format!("{FUNC_BASE64_DECODE} expects a {STRING_TYPE_NAME}, got {actual}");
    Diagnostic::new(
        "dsl.std.base64_decode_needs_string",
        message,
        Diagnostic::make_fields(&[("actual", actual)]),
    )
}

/// `BASE64_DECODE` with an undecodable token. Takes the raw input
/// (`Debug` rendering lives here) and the underlying failure.
pub fn std_base64_decode_failed(input: &str, reason: &base64::DecodeError) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = format!("{FUNC_BASE64_DECODE} cannot decode {input:?}: {reason_text}");
    Diagnostic::new(
        "dsl.std.base64_decode_failed",
        message,
        Diagnostic::make_fields(&[("input", input), ("reason", &reason_text)]),
    )
}

/// `BASE64_DECODE` with a payload that is not UTF-8. Parameterless:
/// the input rides the enclosing call.
pub fn std_base64_decode_not_utf8() -> Diagnostic {
    Diagnostic::new(
        "dsl.std.base64_decode_not_utf8",
        format!("{FUNC_BASE64_DECODE} decoded bytes are not UTF-8"),
        Diagnostic::make_fields(&[]),
    )
}

/// `LOAD_TOML` without its path argument. Parameterless: the arity
/// shape already names the call, so no operand is attached.
pub fn std_load_toml_needs_arg() -> Diagnostic {
    Diagnostic::new(
        "dsl.std.load_toml_needs_arg",
        format!("{FUNC_LOAD_TOML} requires a path argument"),
        Diagnostic::make_fields(&[]),
    )
}

/// `LOAD_TOML` with a non-string path. Parameterless: the value rides
/// the enclosing call, so no operand is attached.
pub fn std_load_toml_needs_string() -> Diagnostic {
    Diagnostic::new(
        "dsl.std.load_toml_needs_string",
        format!("{FUNC_LOAD_TOML} path argument must evaluate to a string"),
        Diagnostic::make_fields(&[]),
    )
}

/// `LOAD_TOML` path resolution failure. Takes the spelling and the
/// underlying failure.
pub fn std_load_toml_resolve_failed(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to resolve TOML path '{path}': {reason_text}");
    Diagnostic::new(
        "dsl.std.load_toml_resolve_failed",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// `LOAD_TOML` file read failure. Takes the spelling and the
/// underlying failure.
pub fn std_load_toml_read_failed(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to read TOML file '{path}': {reason_text}");
    Diagnostic::new(
        "dsl.std.load_toml_read_failed",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// `LOAD_TOML` file with invalid UTF-8. Takes the spelling and the
/// underlying failure.
pub fn std_load_toml_not_utf8(path: &str, reason: &std::str::Utf8Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = format!("invalid UTF-8 in TOML file '{path}': {reason_text}");
    Diagnostic::new(
        "dsl.std.load_toml_not_utf8",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// TOML content that fails to parse. Takes the underlying failure.
pub fn std_toml_parse_failed(reason: &toml::de::Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = format!("TOML parse error: {reason_text}");
    Diagnostic::new(
        "dsl.std.toml_parse_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// `PARSE_TOML` with a non-string value. Takes the actual type name.
pub fn std_parse_toml_needs_string(actual: &str) -> Diagnostic {
    let message = format!("{FUNC_PARSE_TOML} expects a {STRING_TYPE_NAME}, got {actual}");
    Diagnostic::new(
        "dsl.std.parse_toml_needs_string",
        message,
        Diagnostic::make_fields(&[("actual", actual)]),
    )
}

/// `LOAD_JSON` without its path argument. Parameterless: the arity
/// shape already names the call, so no operand is attached.
pub fn std_load_json_needs_arg() -> Diagnostic {
    Diagnostic::new(
        "dsl.std.load_json_needs_arg",
        format!("{FUNC_LOAD_JSON} requires a path argument"),
        Diagnostic::make_fields(&[]),
    )
}

/// `LOAD_JSON` with a non-string path. Parameterless: the value rides
/// the enclosing call, so no operand is attached.
pub fn std_load_json_needs_string() -> Diagnostic {
    Diagnostic::new(
        "dsl.std.load_json_needs_string",
        format!("{FUNC_LOAD_JSON} path argument must evaluate to a string"),
        Diagnostic::make_fields(&[]),
    )
}

/// `LOAD_JSON` path resolution failure. Takes the spelling and the
/// underlying failure.
pub fn std_load_json_resolve_failed(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to resolve JSON path '{path}': {reason_text}");
    Diagnostic::new(
        "dsl.std.load_json_resolve_failed",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// `LOAD_JSON` file read failure. Takes the spelling and the
/// underlying failure.
pub fn std_load_json_read_failed(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to read JSON file '{path}': {reason_text}");
    Diagnostic::new(
        "dsl.std.load_json_read_failed",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// `LOAD_JSON` file with invalid UTF-8. Takes the spelling and the
/// underlying failure.
pub fn std_load_json_not_utf8(path: &str, reason: &std::str::Utf8Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = format!("invalid UTF-8 in JSON file '{path}': {reason_text}");
    Diagnostic::new(
        "dsl.std.load_json_not_utf8",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// JSON content that fails to parse. Takes the underlying failure.
pub fn std_json_parse_failed(reason: &serde_json::Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = format!("JSON parse error: {reason_text}");
    Diagnostic::new(
        "dsl.std.json_parse_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// `PARSE_JSON` with a non-string value. Takes the actual type name.
pub fn std_parse_json_needs_string(actual: &str) -> Diagnostic {
    let message = format!("{FUNC_PARSE_JSON} expects a {STRING_TYPE_NAME}, got {actual}");
    Diagnostic::new(
        "dsl.std.parse_json_needs_string",
        message,
        Diagnostic::make_fields(&[("actual", actual)]),
    )
}

/// `GLOB` without its pattern argument. Parameterless: the arity
/// shape already names the call, so no operand is attached.
pub fn std_glob_needs_arg() -> Diagnostic {
    Diagnostic::new(
        "dsl.std.glob_needs_arg",
        format!("{FUNC_GLOB} requires a pattern argument"),
        Diagnostic::make_fields(&[]),
    )
}

/// `GLOB` with a non-string pattern. Parameterless: the value rides
/// the enclosing call, so no operand is attached.
pub fn std_glob_needs_string() -> Diagnostic {
    Diagnostic::new(
        "dsl.std.glob_needs_string",
        format!("{FUNC_GLOB} pattern argument must evaluate to a string"),
        Diagnostic::make_fields(&[]),
    )
}

/// `GLOB` root-resolution failure. Takes the underlying failure
/// verbatim: it already names the root and the cause, so no wrapper
/// is added.
pub fn std_glob_resolve_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    Diagnostic::new(
        "dsl.std.glob_resolve_failed",
        reason_text.clone(),
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// `GLOB` listing failure. Takes the underlying I/O failure verbatim:
/// the bare propagation it replaces carried no `GLOB` prefix, so none
/// is added.
pub fn std_glob_list_failed(reason: &std::io::Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    Diagnostic::new(
        "dsl.std.glob_list_failed",
        reason_text.clone(),
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// `HAS_KEY` with a non-map first argument. Takes the actual type name.
pub fn std_has_key_needs_map(actual: &str) -> Diagnostic {
    let message = format!(
        "{FUNC_HAS_KEY} expects a {} as its first argument, got {actual}",
        MAP_TYPE_NAME
    );
    Diagnostic::new(
        "dsl.std.has_key_needs_map",
        message,
        Diagnostic::make_fields(&[("actual", actual)]),
    )
}

/// `MAP_SET` with a non-map first argument. Takes the actual type name.
pub fn std_map_set_needs_map(actual: &str) -> Diagnostic {
    let message = format!(
        "{FUNC_MAP_SET} expects a {} as its first argument, got {actual}",
        MAP_TYPE_NAME
    );
    Diagnostic::new(
        "dsl.std.map_set_needs_map",
        message,
        Diagnostic::make_fields(&[("actual", actual)]),
    )
}

/// `MAP_SET` on a duplicate key. Takes the rejected key.
pub fn std_map_set_duplicate(key: &str) -> Diagnostic {
    let message = format!("{FUNC_MAP_SET} duplicate key '{key}'");
    Diagnostic::new(
        "dsl.std.map_set_duplicate",
        message,
        Diagnostic::make_fields(&[("key", key)]),
    )
}

/// `MERGE_MAPS` with a non-list argument. Takes the actual type name.
pub fn std_merge_maps_needs_list(actual: &str) -> Diagnostic {
    let message = format!(
        "{FUNC_MERGE_MAPS} expects a {} of {}s, got {actual}",
        LIST_TYPE_NAME, MAP_TYPE_NAME
    );
    Diagnostic::new(
        "dsl.std.merge_maps_needs_list",
        message,
        Diagnostic::make_fields(&[("actual", actual)]),
    )
}

/// `MERGE_MAPS` element that is not a map. Takes the position and the
/// actual type name.
pub fn std_merge_maps_needs_map(index: usize, actual: &str) -> Diagnostic {
    let message = format!("{FUNC_MERGE_MAPS} element {index} is {actual}, not {MAP_TYPE_NAME}");
    Diagnostic::new(
        "dsl.std.merge_maps_needs_map",
        message,
        Diagnostic::make_fields(&[("index", &index.to_string()), ("actual", actual)]),
    )
}

/// `MERGE_MAPS` duplicate under `fail_on_duplicate`. Takes the
/// repeated key.
pub fn std_merge_maps_duplicate(key: &str) -> Diagnostic {
    let message = format!(
        "duplicate values key '{key}'; later files must not repeat it under fail_on_duplicate"
    );
    Diagnostic::new(
        "dsl.std.merge_maps_duplicate",
        message,
        Diagnostic::make_fields(&[("key", key)]),
    )
}

/// Invalid regex pattern. Takes the invoking function, the pattern,
/// and the underlying failure; `Debug` rendering of the pattern and
/// `Display` rendering of the failure live here.
pub fn std_text_invalid_pattern(func: &str, pattern: &str, reason: &regex::Error) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = format!("{func} invalid pattern {pattern:?}: {reason_text}");
    Diagnostic::new(
        "dsl.std.text_invalid_pattern",
        message,
        Diagnostic::make_fields(&[
            ("func", func),
            ("pattern", pattern),
            ("reason", &reason_text),
        ]),
    )
}

/// Literal replacement with an empty search string. Takes the
/// invoking function.
pub fn std_text_needs_search(func: &str) -> Diagnostic {
    let message = format!("{func} needs a non-empty search string");
    Diagnostic::new(
        "dsl.std.text_needs_search",
        message,
        Diagnostic::make_fields(&[("func", func)]),
    )
}

/// Literal counting with an empty needle. Takes the invoking function.
pub fn std_text_needs_needle(func: &str) -> Diagnostic {
    let message = format!("{func} needs a non-empty needle");
    Diagnostic::new(
        "dsl.std.text_needs_needle",
        message,
        Diagnostic::make_fields(&[("func", func)]),
    )
}

/// Line slicing with a zero or negative start. Takes the invoking
/// function and the rejected start.
pub fn std_text_needs_one_based_start(func: &str, from: i64) -> Diagnostic {
    let message = format!("{func} needs a 1-based start line, got {from}");
    Diagnostic::new(
        "dsl.std.text_needs_one_based_start",
        message,
        Diagnostic::make_fields(&[("func", func), ("from", &from.to_string())]),
    )
}

/// Line slicing with the start past the end. Takes the invoking
/// function and the rejected range.
pub fn std_text_needs_ordered_range(func: &str, from: i64, to: i64) -> Diagnostic {
    let message = format!("{func} needs start <= end, got {from}..={to}");
    Diagnostic::new(
        "dsl.std.text_needs_ordered_range",
        message,
        Diagnostic::make_fields(&[
            ("func", func),
            ("from", &from.to_string()),
            ("to", &to.to_string()),
        ]),
    )
}

/// Refusal for pipe words reaching a template projection: one message
/// for every mirror op, so the boundary cannot drift per function.
/// Takes the invoking function.
pub fn std_pipe_needs_step_io(func: &str) -> Diagnostic {
    let message = format!(
        "{func} on {PIPE_TYPE_NAME} input needs step IO, unavailable to placeholder calls; pass a {STRING_TYPE_NAME} there"
    );
    Diagnostic::new(
        "dsl.std.pipe_needs_step_io",
        message,
        Diagnostic::make_fields(&[("func", func)]),
    )
}

/// `HANDLE` typed JSON decode on a non-object. Takes the offending
/// JSON value; `Display` rendering lives here.
pub fn json_handle_needs_object(other: &serde_json::Value) -> Diagnostic {
    let message = format!(
        "{HANDLE_TYPE_NAME} argument must be an object like {{\"task_id\": 7}}, got {other}"
    );
    Diagnostic::new(
        "dsl.json.handle_needs_object",
        message,
        Diagnostic::make_fields(&[("got", &other.to_string())]),
    )
}

/// `HANDLE` typed JSON decode on a wrong-shaped object. Parameterless:
/// the exact `{"task_id": N}` spelling lives here.
pub fn json_handle_exact_shape() -> Diagnostic {
    Diagnostic::new(
        "dsl.json.handle_exact_shape",
        format!(
            "{HANDLE_TYPE_NAME} argument must be exactly {{\"task_id\": N}} with no other keys"
        ),
        Diagnostic::make_fields(&[]),
    )
}

/// `HANDLE` typed JSON decode on a non-positive `task_id`.
/// Parameterless: the backticked spelling lives here.
pub fn json_handle_task_id_positive() -> Diagnostic {
    Diagnostic::new(
        "dsl.json.handle_task_id_positive",
        format!("{HANDLE_TYPE_NAME} argument `task_id` must be a positive integer"),
        Diagnostic::make_fields(&[]),
    )
}

/// Typed JSON encode of a non-handle as `HANDLE`. Takes the type name;
/// quoting lives here.
pub fn json_handle_encode_needs_handle(actual: &str) -> Diagnostic {
    let message = format!("cannot encode {actual} as {HANDLE_TYPE_NAME}; expected a task handle");
    Diagnostic::new(
        "dsl.json.handle_encode_needs_handle",
        message,
        Diagnostic::make_fields(&[("actual", actual)]),
    )
}

/// IMPORT naming a file outside the static import closure. Takes the
/// path; the closure wording lives here.
pub fn import_not_in_closure(path: &str) -> Diagnostic {
    let message = format!("{KEYWORD_IMPORT} `{path}` cannot be read: not in the import closure");
    Diagnostic::new(
        "dsl.import.not_in_closure",
        message,
        Diagnostic::make_fields(&[("path", path)]),
    )
}

/// IMPORT `EXPORT TYPE` naming no top-level `TYPE` in that file.
pub fn import_export_type_mismatch(path: &str, name: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_IMPORT} `{path}` {KEYWORD_EXPORT} {KEYWORD_TYPE} `{name}` does not match any top-level {KEYWORD_TYPE} in that file"
    );
    Diagnostic::new(
        "dsl.import.export_type_mismatch",
        message,
        Diagnostic::make_fields(&[("path", path), ("name", name)]),
    )
}

/// Static variable declaration type resolution failure. Takes the
/// variable name and the underlying failure.
pub fn check_var_tag(var: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("variable `${var}`: {reason_text}");
    Diagnostic::new(
        "dsl.check.var_tag",
        message,
        Diagnostic::make_fields(&[("var", var), ("reason", &reason_text)]),
    )
}

/// Static function tag resolution failure. Takes the function name
/// and the underlying failure.
pub fn check_func_tag(name: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("function `{name}`: {reason_text}");
    Diagnostic::new(
        "dsl.check.func_tag",
        message,
        Diagnostic::make_fields(&[("function", name), ("reason", &reason_text)]),
    )
}

/// Static step-attributed passthrough. Takes the 1-based step number
/// and the underlying failure; the `step N` prefix lives here.
pub fn check_step_wrap(step: usize, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("step {step}: {reason_text}");
    Diagnostic::new(
        "dsl.check.step_wrap",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("reason", &reason_text)]),
    )
}

/// Static context-attributed passthrough. Takes the context prefix
/// (a file key, `at()`, `step N, line L, col C`) and the underlying
/// failure; joining lives here.
pub fn check_ctx_wrap(ctx: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{ctx}: {reason_text}");
    Diagnostic::new(
        "dsl.check.ctx_wrap",
        message,
        Diagnostic::make_fields(&[("ctx", ctx), ("reason", &reason_text)]),
    )
}

/// Static exact-shape mismatch. Takes the context and both sides;
/// structural rendering and quoting live here.
pub fn check_exact_mismatch(ctx: &str, expected: &TypeTag, actual: &TypeTag) -> Diagnostic {
    let exp = render_structural(expected);
    let act = render_structural(actual);
    let message = format!("{ctx}: TypeMismatch: expected `{exp}`, got `{act}`");
    Diagnostic::new(
        "dsl.check.exact_mismatch",
        message,
        Diagnostic::make_fields(&[("ctx", ctx), ("expected", &exp), ("actual", &act)]),
    )
}

/// Static unification failure. Takes the context and both sides;
/// structural rendering and quoting live here.
pub fn check_cannot_unify(ctx: &str, left: &TypeTag, right: &TypeTag) -> Diagnostic {
    let l = render_structural(left);
    let r = render_structural(right);
    let message = format!("{ctx}: cannot unify `{l}` and `{r}`");
    Diagnostic::new(
        "dsl.check.cannot_unify",
        message,
        Diagnostic::make_fields(&[("ctx", ctx), ("left", &l), ("right", &r)]),
    )
}

/// Static undeclared variable read. Takes the location prefix
/// (`step N: ` or empty in RPN contexts) and the key; the `$` sigil
/// and quoting live here.
pub fn check_undefined_var(prefix: &str, key: &str) -> Diagnostic {
    let message = format!("{prefix}undefined variable `${key}`");
    Diagnostic::new(
        "dsl.check.undefined_var",
        message,
        Diagnostic::make_fields(&[("prefix", prefix), ("key", key)]),
    )
}

/// Static definite-unbound read: the name binds on no reachable path.
/// Takes the key; quoting lives here.
pub fn check_var_not_guaranteed(key: &str) -> Diagnostic {
    let message = format!(
        "variable `${key}` is not guaranteed to be defined on all reachable paths for this step"
    );
    Diagnostic::new(
        "dsl.check.var_not_guaranteed",
        message,
        Diagnostic::make_fields(&[("key", key)]),
    )
}

/// Static key-path field miss. Takes the location prefix, the part,
/// and the record fields; the expected-names join lives here.
pub fn check_unknown_field(prefix: &str, part: &str, fields: &[Field]) -> Diagnostic {
    let expected = fields
        .iter()
        .map(|f| f.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let message = format!("{prefix}unknown field `{part}`; expected fields: {expected}");
    Diagnostic::new(
        "dsl.check.unknown_field",
        message,
        Diagnostic::make_fields(&[("prefix", prefix), ("part", part), ("expected", &expected)]),
    )
}

/// Static key-path index miss. Takes the location prefix and the
/// part; quoting lives here.
pub fn check_invalid_array_index(prefix: &str, part: &str) -> Diagnostic {
    let message = format!("{prefix}invalid array index `{part}`");
    Diagnostic::new(
        "dsl.check.invalid_array_index",
        message,
        Diagnostic::make_fields(&[("prefix", prefix), ("part", part)]),
    )
}

/// Static key-path traversal through a scalar. Takes the location
/// prefix and the part; quoting lives here.
pub fn check_cannot_traverse(prefix: &str, part: &str) -> Diagnostic {
    let message = format!("{prefix}cannot traverse into scalar value at key `{part}`");
    Diagnostic::new(
        "dsl.check.cannot_traverse",
        message,
        Diagnostic::make_fields(&[("prefix", prefix), ("part", part)]),
    )
}

/// Static duplicate function across imports. Takes the key and the
/// display path; quoting lives here.
pub fn check_duplicate_func(key: &str, display: &str) -> Diagnostic {
    let message = format!("duplicate function `{key}` from `{display}` collides across imports");
    Diagnostic::new(
        "dsl.check.duplicate_func",
        message,
        Diagnostic::make_fields(&[("key", key), ("display", display)]),
    )
}

/// Static `CONST` redeclaration. Takes the 1-based step number and the
/// key; quoting lives here.
pub fn check_redeclare_const(step: usize, key: &str) -> Diagnostic {
    let message = format!(
        "step {step}: redeclaration error: `{key}` is a {KEYWORD_CONST} binding and cannot be redeclared; {KEYWORD_CONST} and {KEYWORD_LET} share one namespace"
    );
    Diagnostic::new(
        "dsl.check.redeclare_const",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("key", key)]),
    )
}

/// Static `LET` redeclaration. Takes the 1-based step number and the
/// key; the `$` sigil and quoting live here.
pub fn check_redeclare_let(step: usize, key: &str) -> Diagnostic {
    let message = format!(
        "step {step}: redeclaration error: `${key}` already declared in this scope; use `${key}` = ... to mutate"
    );
    Diagnostic::new(
        "dsl.check.redeclare_let",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("key", key)]),
    )
}

/// Static undeclared environment read. Takes the step position, the
/// scope label, the source prefix (`env:`/`script_env:`), and the
/// name; quoting lives here.
pub fn check_undeclared_env(pos: &str, label: &str, prefix: &str, name: &str) -> Diagnostic {
    let message = format!(
        "{pos} in {label}: uses undeclared environment variable `{prefix}{name}`: declare it first with {KEYWORD_ENV} or {KEYWORD_INHERIT_ENV}"
    );
    Diagnostic::new(
        "dsl.check.undeclared_env",
        message,
        Diagnostic::make_fields(&[
            ("pos", pos),
            ("scope", label),
            ("prefix", prefix),
            ("name", name),
        ]),
    )
}

/// Static `$ALIAS::NAME` key path. Qualified reads never traverse;
/// the template owner wording lives here so no call site restates it.
pub fn check_placeholder_keypath() -> Diagnostic {
    let message = oxdock_parser::template::qualified_keypath_error();
    Diagnostic::new(
        "dsl.check.placeholder_keypath",
        message,
        Diagnostic::make_fields(&[]),
    )
}

/// Static empty placeholder variable. Parameterless: the wording is
/// fixed.
pub fn check_empty_placeholder_var() -> Diagnostic {
    Diagnostic::new(
        "dsl.check.empty_placeholder_var",
        "invalid placeholder variable: empty variable reference".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Static bare-sigil read. Takes the shaped source; the hint wording
/// lives with the template owner.
pub fn check_bare_sigil(src: &str) -> Diagnostic {
    let message = oxdock_parser::template::bare_sigil_error(src);
    Diagnostic::new(
        "dsl.check.bare_sigil",
        message,
        Diagnostic::make_fields(&[("src", src)]),
    )
}

/// Static bare template head. Takes the key; quoting lives here.
pub fn check_undefined_script_var(key: &str) -> Diagnostic {
    let message = format!("undefined script variable: `{key}`");
    Diagnostic::new(
        "dsl.check.undefined_script_var",
        message,
        Diagnostic::make_fields(&[("key", key)]),
    )
}

/// Static unknown placeholder call. Takes the qualified name; quoting
/// lives here. The runtime path lists known functions; the static
/// gate cannot, so this is a distinct key.
pub fn check_placeholder_unknown(qualified: &str) -> Diagnostic {
    let message = format!("unknown placeholder function `{qualified}`");
    Diagnostic::new(
        "dsl.check.placeholder_unknown",
        message,
        Diagnostic::make_fields(&[("name", qualified)]),
    )
}

/// Static disjoint platform-alias miss. Takes the alias, base, holder
/// files, host flag, and script probe; the miss wording lives with
/// the single owner in `super::native`.
pub fn check_disjoint_alias_miss(
    module: &str,
    name: &str,
    base: &str,
    files: &std::collections::HashSet<String>,
    has_host: bool,
    is_script: &dyn Fn(&str) -> bool,
) -> Diagnostic {
    let message = super::native::disjoint_alias_placeholder_miss(
        module, name, base, files, has_host, is_script,
    );
    Diagnostic::new(
        "dsl.check.disjoint_alias_miss",
        message,
        Diagnostic::make_fields(&[("alias", module), ("name", name), ("base", base)]),
    )
}

/// Static described placeholder miss. Takes the module, name, and the
/// three visibility flags; the wording lives with the single owner
/// in `super::native`. Returns `None` when the miss is undescribed
/// (the caller falls through to [`check_placeholder_unknown`]).
pub fn check_placeholder_miss(
    module: &str,
    name: &str,
    is_script: bool,
    is_stateful: bool,
    alias_known: bool,
) -> Option<Diagnostic> {
    let text = super::native::describe_placeholder_miss(
        module,
        name,
        is_script,
        is_stateful,
        alias_known,
    )?;
    Some(Diagnostic::new(
        "dsl.check.placeholder_miss",
        text,
        Diagnostic::make_fields(&[("alias", module), ("name", name)]),
    ))
}

/// Static template call arity (exact). Takes the step position, scope
/// label, qualified name, expected total, and count.
pub fn check_template_arity_exact(
    pos: &str,
    label: &str,
    qualified: &str,
    total: usize,
    got: usize,
) -> Diagnostic {
    let message = format!("{pos} in {label}: {qualified}() expects {total} argument(s), got {got}");
    Diagnostic::new(
        "dsl.check.template_arity_exact",
        message,
        Diagnostic::make_fields(&[
            ("pos", pos),
            ("scope", label),
            ("function", qualified),
            ("expected", &total.to_string()),
            ("got", &got.to_string()),
        ]),
    )
}

/// Static template call arity (range). Takes the step position, scope
/// label, qualified name, min, max, and count.
pub fn check_template_arity_range(
    pos: &str,
    label: &str,
    qualified: &str,
    min: usize,
    max: usize,
    got: usize,
) -> Diagnostic {
    let message =
        format!("{pos} in {label}: {qualified}() expects {min} to {max} argument(s), got {got}");
    Diagnostic::new(
        "dsl.check.template_arity_range",
        message,
        Diagnostic::make_fields(&[
            ("pos", pos),
            ("scope", label),
            ("function", qualified),
            ("min", &min.to_string()),
            ("max", &max.to_string()),
            ("got", &got.to_string()),
        ]),
    )
}

/// Static template call closed-set miss. Takes the step position,
/// scope label, qualified and parameter names, the allowed values,
/// and the rendered literal; joining lives here.
pub fn check_template_arg_allowed(
    pos: &str,
    label: &str,
    qualified: &str,
    param: &str,
    allowed: &[&str],
    got: &str,
) -> Diagnostic {
    let joined = allowed.join(", ");
    let message = format!(
        "{pos} in {label}: {qualified}() argument `${param}` must be one of: {joined}, got {got:?}"
    );
    Diagnostic::new(
        "dsl.check.template_arg_allowed",
        message,
        Diagnostic::make_fields(&[
            ("pos", pos),
            ("scope", label),
            ("function", qualified),
            ("param", param),
            ("allowed", &joined),
            ("got", got),
        ]),
    )
}

/// Static template call argument nesting past the literal cap.
/// Parameterless: the cap renders from the shared constant.
pub fn check_template_arg_depth() -> Diagnostic {
    let max = oxdock_parser::template::MAX_TEMPLATE_ARG_DEPTH;
    let message = format!("placeholder call argument nests too deep (limit {max})");
    Diagnostic::new(
        "dsl.check.template_arg_depth",
        message,
        Diagnostic::make_fields(&[("max", &max.to_string())]),
    )
}

/// Static empty template call argument. Parameterless: fixed wording.
pub fn check_empty_placeholder_arg() -> Diagnostic {
    Diagnostic::new(
        "dsl.check.empty_placeholder_arg",
        "invalid placeholder call argument: empty variable reference".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Static analysis nesting past the cap inside a function.
/// Takes the function key.
pub fn check_depth_function(key: &str) -> Diagnostic {
    let message = format!("static analysis depth exceeded in function `{key}`");
    Diagnostic::new(
        "dsl.check.depth_function",
        message,
        Diagnostic::make_fields(&[("function", key)]),
    )
}

/// Static analysis nesting past the cap inside a scope. Takes the
/// scope label.
pub fn check_depth_scope(label: &str) -> Diagnostic {
    let message = format!("static analysis depth exceeded in `{label}`");
    Diagnostic::new(
        "dsl.check.depth_scope",
        message,
        Diagnostic::make_fields(&[("scope", label)]),
    )
}
/// Static malformed placeholder call argument. Takes the source;
/// quoting lives here.
pub fn check_invalid_placeholder_arg(src: &str) -> Diagnostic {
    let message = format!("invalid placeholder call argument `{src}`");
    Diagnostic::new(
        "dsl.check.invalid_placeholder_arg",
        message,
        Diagnostic::make_fields(&[("src", src)]),
    )
}

/// Static malformed map literal entry. Takes the entry; quoting lives
/// here.
pub fn check_invalid_map_entry(entry: &str) -> Diagnostic {
    let message = format!("invalid map literal entry `{entry}`: use key: value");
    Diagnostic::new(
        "dsl.check.invalid_map_entry",
        message,
        Diagnostic::make_fields(&[("entry", entry)]),
    )
}

/// Static duplicate map literal key. Takes the key; quoting lives
/// here.
pub fn check_duplicate_map_key(key: &str) -> Diagnostic {
    let message = format!("duplicate key `{key}` in map literal");
    Diagnostic::new(
        "dsl.check.duplicate_map_key",
        message,
        Diagnostic::make_fields(&[("key", key)]),
    )
}

/// Static `VOID` block bound to a variable. Takes the 1-based step
/// number and the variable; quoting lives here.
pub fn check_void_block_bind(step: usize, var: &str) -> Diagnostic {
    let message = format!("step {step}: {VOID_TYPE_NAME} block has no value to bind to `${var}`");
    Diagnostic::new(
        "dsl.check.void_block_bind",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("var", var)]),
    )
}

/// Static capture yield mismatch. Takes the 1-based step number, the
/// keyword (`LET`/`CONST`), the variable, and both sides; rendering
/// and quoting live here.
pub fn check_capture_mismatch(
    step: usize,
    kw: &str,
    var: &str,
    yield_tag: &TypeTag,
    tag: &TypeTag,
) -> Diagnostic {
    let y = render_structural(yield_tag);
    let t = render_structural(tag);
    let message = format!("step {step}: {kw} `${var}` captures {y} but declares {t}");
    Diagnostic::new(
        "dsl.check.capture_mismatch",
        message,
        Diagnostic::make_fields(&[
            ("step", &step.to_string()),
            ("command", kw),
            ("var", var),
            ("yield", &y),
            ("declares", &t),
        ]),
    )
}

/// Static `AWAIT` on a non-handle. Takes the 1-based step number, the
/// task variable, and the actual tag; rendering lives here.
pub fn check_await_not_handle(step: usize, task_var: &str, actual: &TypeTag) -> Diagnostic {
    let a = render_structural(actual);
    let message =
        format!("step {step}: {KEYWORD_AWAIT} `${task_var}` is {a}, not {HANDLE_TYPE_NAME}");
    Diagnostic::new(
        "dsl.check.await_not_handle",
        message,
        Diagnostic::make_fields(&[
            ("step", &step.to_string()),
            ("task", task_var),
            ("actual", &a),
        ]),
    )
}

/// Static `AWAIT` of a `HANDLE<VOID>` with an output binding. Takes
/// the 1-based step number and both variables.
pub fn check_await_void_payload(step: usize, task_var: &str, out_var: &str) -> Diagnostic {
    let message = format!(
        "step {step}: {KEYWORD_AWAIT} `${task_var}` is {HANDLE_TYPE_NAME}<{VOID_TYPE_NAME}>: no value to bind to `${out_var}`; use bare {KEYWORD_AWAIT} for join"
    );
    Diagnostic::new(
        "dsl.check.await_void_payload",
        message,
        Diagnostic::make_fields(&[
            ("step", &step.to_string()),
            ("task", task_var),
            ("var", out_var),
        ]),
    )
}

/// Static `AWAIT` yield mismatch. Takes the 1-based step number, the
/// task and output variables, and both sides.
pub fn check_await_yield_mismatch(
    step: usize,
    task_var: &str,
    payload: &TypeTag,
    out_var: &str,
    tag: &TypeTag,
) -> Diagnostic {
    let p = render_structural(payload);
    let t = render_structural(tag);
    let message = format!(
        "step {step}: {KEYWORD_AWAIT} `${task_var}` yields {p} but `${out_var}` declares {t}"
    );
    Diagnostic::new(
        "dsl.check.await_yield_mismatch",
        message,
        Diagnostic::make_fields(&[
            ("step", &step.to_string()),
            ("task", task_var),
            ("yield", &p),
            ("var", out_var),
            ("declares", &t),
        ]),
    )
}

/// Static `ASYNC` binding with a non-`HANDLE` declaration. Takes the
/// 1-based step number, the variable, and the tag.
pub fn check_async_not_handle(step: usize, var: &str, tag: &TypeTag) -> Diagnostic {
    let t = render_structural(tag);
    let message = format!(
        "step {step}: {KEYWORD_ASYNC} `${var}` declares {t}, not {HANDLE_TYPE_NAME}; every {KEYWORD_ASYNC} position is {HANDLE_TYPE_NAME}-headed"
    );
    Diagnostic::new(
        "dsl.check.async_not_handle",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("var", var), ("declares", &t)]),
    )
}

/// Static `ASYNC` task with a bare `RETURN` under a valued payload.
/// Takes the 1-based step number, the variable, and the tag.
pub fn check_async_bare_return(step: usize, var: &str, tag: &TypeTag) -> Diagnostic {
    let t = render_structural(tag);
    let message = format!(
        "step {step}: {KEYWORD_ASYNC} `${var}` declares {t} but task has a bare {KEYWORD_RETURN}; bare {KEYWORD_RETURN} is only a void exit"
    );
    Diagnostic::new(
        "dsl.check.async_bare_return",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("var", var), ("declares", &t)]),
    )
}

/// Static `HANDLE<VOID>` task with a value `RETURN`. Takes the 1-based
/// step number, the return line, and the yielded tag.
pub fn check_async_void_yield(step: usize, line: usize, ty: &TypeTag) -> Diagnostic {
    let t = render_structural(ty);
    let message = format!(
        "step {step}: {HANDLE_TYPE_NAME}<{VOID_TYPE_NAME}> task {KEYWORD_RETURN} at line {line} yields {t}"
    );
    Diagnostic::new(
        "dsl.check.async_void_yield",
        message,
        Diagnostic::make_fields(&[
            ("step", &step.to_string()),
            ("line", &line.to_string()),
            ("yield", &t),
        ]),
    )
}

/// Static `ASYNC` task with no value `RETURN` under a valued payload.
/// Takes the 1-based step number, the variable, and the tag.
pub fn check_async_no_return(step: usize, var: &str, tag: &TypeTag) -> Diagnostic {
    let t = render_structural(tag);
    let message = format!(
        "step {step}: {KEYWORD_ASYNC} `${var}` declares {t} but task has no value {KEYWORD_RETURN}"
    );
    Diagnostic::new(
        "dsl.check.async_no_return",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("var", var), ("declares", &t)]),
    )
}

/// Static `ASYNC` task `RETURN` mismatching the payload. Takes the
/// 1-based step number, the variable, the declared tag, the return
/// line, and the yielded tag.
pub fn check_async_return_mismatch(
    step: usize,
    var: &str,
    tag: &TypeTag,
    line: usize,
    ty: &TypeTag,
) -> Diagnostic {
    let t = render_structural(tag);
    let y = render_structural(ty);
    let message = format!(
        "step {step}: {KEYWORD_ASYNC} `${var}` declares {t} but task {KEYWORD_RETURN} at line {line} yields {y}"
    );
    Diagnostic::new(
        "dsl.check.async_return_mismatch",
        message,
        Diagnostic::make_fields(&[
            ("step", &step.to_string()),
            ("var", var),
            ("declares", &t),
            ("line", &line.to_string()),
            ("yield", &y),
        ]),
    )
}

/// Static `RETURN` outside any function, task, or block. Takes the
/// 1-based step number.
pub fn check_return_outside(step: usize) -> Diagnostic {
    let message = format!(
        "step {step}: {KEYWORD_RETURN} outside function, {KEYWORD_ASYNC} task, or {KEYWORD_LET} block"
    );
    Diagnostic::new(
        "dsl.check.return_outside",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string())]),
    )
}

/// Static `RETURN` crossing an `ASYNC` statement boundary. Takes the
/// 1-based step number.
pub fn check_return_crosses_async(step: usize) -> Diagnostic {
    let message = format!("step {step}: {KEYWORD_RETURN} cannot cross {KEYWORD_ASYNC} boundary");
    Diagnostic::new(
        "dsl.check.return_crosses_async",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string())]),
    )
}

/// Static mutation of an undeclared variable. Takes the 1-based step
/// number and the key; quoting lives here.
pub fn check_undeclared_set(step: usize, key: &str) -> Diagnostic {
    let message = format!(
        "step {step}: undeclared variable `${key}`: declare it first with {KEYWORD_LET} `${key}`: {KEYWORD_TYPE} = ..."
    );
    Diagnostic::new(
        "dsl.check.undeclared_set",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("key", key)]),
    )
}

/// Static mutation of a `CONST` binding. Takes the 1-based step number
/// and the key; quoting lives here.
pub fn check_mutate_const(step: usize, key: &str) -> Diagnostic {
    let message = format!(
        "step {step}: cannot mutate {KEYWORD_CONST} `{key}`: {KEYWORD_CONST} bindings are deeply immutable"
    );
    Diagnostic::new(
        "dsl.check.mutate_const",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("key", key)]),
    )
}

/// Static isolated allowlist outside `ISOLATED`. Takes the 1-based
/// step number.
pub fn check_isolated_allowlist(step: usize) -> Diagnostic {
    let message = format!("step {step}: isolated allowlist outside {KEYWORD_ISOLATED}");
    Diagnostic::new(
        "dsl.check.isolated_allowlist",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string())]),
    )
}

/// Static `REMOTE` use of a variable unknown outside. Takes the target
/// and the key; quoting lives here.
pub fn check_remote_unknown_var(target: &str, key: &str) -> Diagnostic {
    let message = format!("{KEYWORD_REMOTE} `{target}` uses unknown variable `${key}`");
    Diagnostic::new(
        "dsl.check.remote_unknown_var",
        message,
        Diagnostic::make_fields(&[("target", target), ("key", key)]),
    )
}

/// Static `REMOTE` shipment of an undeclared environment variable.
/// Takes the step position, scope label, target, and name.
pub fn check_remote_undeclared_env(pos: &str, label: &str, target: &str, name: &str) -> Diagnostic {
    let message = format!(
        "{pos} in {label}: {KEYWORD_REMOTE} `{target}` ships undeclared environment variable `env:{name}`: declare it first with {KEYWORD_ENV} or {KEYWORD_INHERIT_ENV}"
    );
    Diagnostic::new(
        "dsl.check.remote_undeclared_env",
        message,
        Diagnostic::make_fields(&[
            ("pos", pos),
            ("scope", label),
            ("target", target),
            ("name", name),
        ]),
    )
}

/// Static `LIST_APPEND` on a non-list. Takes the 1-based step number,
/// the list variable, and the actual tag.
pub fn check_list_not_list(step: usize, list: &str, actual: &TypeTag) -> Diagnostic {
    let a = render_structural(actual);
    let message = format!(
        "step {step}: {KEYWORD_LIST_APPEND} `${list}`: variable is {a}, not {LIST_TYPE_NAME}"
    );
    Diagnostic::new(
        "dsl.check.list_not_list",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("list", list), ("actual", &a)]),
    )
}

/// Static `LIST_APPEND` of a `VOID` item. Takes the 1-based step
/// number and the list variable.
pub fn check_list_void_push(step: usize, list: &str) -> Diagnostic {
    let message = format!(
        "step {step}: {KEYWORD_LIST_APPEND} `${list}`: {VOID_TYPE_NAME} has no value to push"
    );
    Diagnostic::new(
        "dsl.check.list_void_push",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("list", list)]),
    )
}

/// Static pipe binding to a non-`PIPE`. Takes the 1-based step number
/// and the actual tag name.
pub fn check_pipe_mismatch(step: usize, actual: &str) -> Diagnostic {
    let message = format!("step {step}: TypeMismatch: expected `{PIPE_TYPE_NAME}`, got `{actual}`");
    Diagnostic::new(
        "dsl.check.pipe_mismatch",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("actual", actual)]),
    )
}

/// Static record shape missing a required field. Takes the context
/// and the field name; quoting lives here.
pub fn check_missing_field(ctx: &str, name: &str) -> Diagnostic {
    let message = format!("{ctx}: missing field `{name}`");
    Diagnostic::new(
        "dsl.check.missing_field",
        message,
        Diagnostic::make_fields(&[("ctx", ctx), ("field", name)]),
    )
}

/// Static record shape with an unknown field. Takes the context and
/// the field name; quoting lives here.
pub fn check_record_unknown_field(ctx: &str, name: &str) -> Diagnostic {
    let message = format!("{ctx}: unknown field `{name}`");
    Diagnostic::new(
        "dsl.check.record_unknown_field",
        message,
        Diagnostic::make_fields(&[("ctx", ctx), ("field", name)]),
    )
}

/// Static `BREAK` outside any loop or `SWITCH`. Takes the 1-based
/// step number.
pub fn check_break_outside_switch(step: usize) -> Diagnostic {
    let message = format!("step {step}: {KEYWORD_BREAK} outside loop or {KEYWORD_SWITCH}");
    Diagnostic::new(
        "dsl.check.break_outside_switch",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string())]),
    )
}

/// Static flow word outside any loop. Takes the 1-based step number
/// and the word (`BREAK`/`CONTINUE`).
pub fn check_flow_outside_loop(step: usize, word: &str) -> Diagnostic {
    let message = format!("step {step}: {word} outside loop");
    Diagnostic::new(
        "dsl.check.flow_outside_loop",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("flow", word)]),
    )
}

/// Static flow word crossing a function boundary. Takes the 1-based
/// step number, the word, and the function name.
pub fn check_flow_crosses_func(step: usize, word: &str, name: &str) -> Diagnostic {
    let message = format!("step {step}: {word} cannot cross function boundary (in {name}())");
    Diagnostic::new(
        "dsl.check.flow_crosses_func",
        message,
        Diagnostic::make_fields(&[
            ("step", &step.to_string()),
            ("flow", word),
            ("function", name),
        ]),
    )
}

/// Static flow word crossing a block boundary. Takes the 1-based step
/// number and the word.
pub fn check_flow_crosses_block(step: usize, word: &str) -> Diagnostic {
    let message = format!("step {step}: {word} cannot cross block boundary");
    Diagnostic::new(
        "dsl.check.flow_crosses_block",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("flow", word)]),
    )
}

/// Static flow word crossing an `ASYNC` boundary. Takes the 1-based
/// step number and the word.
pub fn check_flow_crosses_async(step: usize, word: &str) -> Diagnostic {
    let message = format!("step {step}: {word} cannot cross {KEYWORD_ASYNC} boundary");
    Diagnostic::new(
        "dsl.check.flow_crosses_async",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string()), ("flow", word)]),
    )
}

/// Static `VOID` argument where a value is required. Takes the
/// context; the `VOID` wording lives here.
pub fn check_arg_void(ctx: &str) -> Diagnostic {
    let message = format!("{ctx}: {VOID_TYPE_NAME} has no value to pass");
    Diagnostic::new(
        "dsl.check.arg_void",
        message,
        Diagnostic::make_fields(&[("ctx", ctx)]),
    )
}

/// Static untyped `ANY` flowing into a concrete parameter. Takes the
/// context and the expected tag; rendering lives here.
pub fn check_any_into_concrete(ctx: &str, expected: &TypeTag) -> Diagnostic {
    let exp = render_structural(expected);
    let message = format!(
        "{ctx}: untyped ANY value flows into `{exp}` parameter; bind through {KEYWORD_LET} `$x`: `{exp}` first"
    );
    Diagnostic::new(
        "dsl.check.any_into_concrete",
        message,
        Diagnostic::make_fields(&[("ctx", ctx), ("expected", &exp)]),
    )
}

/// Static `ASYNC FUNC` without a `HANDLE` declaration. Takes the
/// function key and the declared tag.
pub fn check_async_decl_not_handle(key: &str, declared: &TypeTag) -> Diagnostic {
    let d = render_structural(declared);
    let message = format!(
        "function `{key}` declares {d}, but {KEYWORD_ASYNC} {KEYWORD_FUNC} must declare {HANDLE_TYPE_NAME} or {HANDLE_TYPE_NAME}<T>"
    );
    Diagnostic::new(
        "dsl.check.async_decl_not_handle",
        message,
        Diagnostic::make_fields(&[("function", key), ("declares", &d)]),
    )
}

/// Static function falling through with no `RETURN` on some path.
/// Takes the function key and the declared tag.
pub fn check_func_fallthrough(key: &str, declared: &TypeTag) -> Diagnostic {
    let d = render_structural(declared);
    let message = format!(
        "function `{key}` declares {d} but may fall through with no {KEYWORD_RETURN} on some path"
    );
    Diagnostic::new(
        "dsl.check.func_fallthrough",
        message,
        Diagnostic::make_fields(&[("function", key), ("declares", &d)]),
    )
}

/// Static function with no `RETURN` on any path. Takes the function
/// key and the declared tag.
pub fn check_func_no_return(key: &str, declared: &TypeTag) -> Diagnostic {
    let d = render_structural(declared);
    let message = format!("function `{key}` declares {d} but has no {KEYWORD_RETURN} on any path");
    Diagnostic::new(
        "dsl.check.func_no_return",
        message,
        Diagnostic::make_fields(&[("function", key), ("declares", &d)]),
    )
}

/// Static function with a bare `RETURN` under a valued declaration.
/// Takes the function key and the declared tag.
pub fn check_func_bare_return(key: &str, declared: &TypeTag) -> Diagnostic {
    let d = render_structural(declared);
    let message = format!(
        "function `{key}` declares {d} but has a bare {KEYWORD_RETURN} on some path; bare {KEYWORD_RETURN} is only a void exit"
    );
    Diagnostic::new(
        "dsl.check.func_bare_return",
        message,
        Diagnostic::make_fields(&[("function", key), ("declares", &d)]),
    )
}

/// Static `VOID` function with a value `RETURN`. Takes the function
/// key, the return line, and the yielded tag.
pub fn check_func_void_yield(key: &str, line: usize, ty: &TypeTag) -> Diagnostic {
    let t = render_structural(ty);
    let message = format!(
        "function `{key}` declares {VOID_TYPE_NAME} but {KEYWORD_RETURN} at line {line} yields {t}"
    );
    Diagnostic::new(
        "dsl.check.func_void_yield",
        message,
        Diagnostic::make_fields(&[
            ("function", key),
            ("line", &line.to_string()),
            ("yield", &t),
        ]),
    )
}

/// Static function with no value `RETURN` on any path. Takes the
/// function key and the declared tag.
pub fn check_func_no_value_return(key: &str, declared: &TypeTag) -> Diagnostic {
    let d = render_structural(declared);
    let message =
        format!("function `{key}` declares {d} but has no value {KEYWORD_RETURN} on any path");
    Diagnostic::new(
        "dsl.check.func_no_value_return",
        message,
        Diagnostic::make_fields(&[("function", key), ("declares", &d)]),
    )
}

/// Static function `RETURN` mismatching the declaration. Takes the
/// function key, the declared tag, the return line, and the yielded
/// tag.
pub fn check_func_return_mismatch(
    key: &str,
    declared: &TypeTag,
    line: usize,
    ty: &TypeTag,
) -> Diagnostic {
    let d = render_structural(declared);
    let t = render_structural(ty);
    let message =
        format!("function `{key}` declares {d} but {KEYWORD_RETURN} at line {line} yields {t}");
    Diagnostic::new(
        "dsl.check.func_return_mismatch",
        message,
        Diagnostic::make_fields(&[
            ("function", key),
            ("declares", &d),
            ("line", &line.to_string()),
            ("yield", &t),
        ]),
    )
}

/// Static unknown function with an import hint suffix. Takes the name
/// and the hint (empty when no file exports the base); joining lives
/// here.
pub fn check_unknown_function(name: &str, hint: &str) -> Diagnostic {
    let message = format!("unknown function `{name}`{hint}");
    Diagnostic::new(
        "dsl.check.unknown_function",
        message,
        Diagnostic::make_fields(&[("name", name), ("hint", hint)]),
    )
}

/// Static `CONST` read with no import alias exporting the name. Takes
/// the step position, the qualified name, and the alias.
pub fn check_const_no_alias_at(pos: &str, qualified: &str, alias: &str) -> Diagnostic {
    let message = format!(
        "{pos}: unknown {KEYWORD_CONST} `{qualified}`: no import alias `{alias}` exports that name"
    );
    Diagnostic::new(
        "dsl.check.const_no_alias_at",
        message,
        Diagnostic::make_fields(&[("pos", pos), ("name", qualified), ("alias", alias)]),
    )
}

/// Static ambiguous `CONST` alias. Takes the step position, alias,
/// name, and holder files; the `function`->`CONST` wording lives here
/// with the single owner in `super::native`.
pub fn check_const_ambiguous_at(
    pos: &str,
    alias: &str,
    name: &str,
    files: &std::collections::HashSet<String>,
) -> Diagnostic {
    let message = format!(
        "{pos}: {}",
        super::native::ambiguous_alias_message(alias, name, files).replacen(
            "function",
            KEYWORD_CONST,
            1
        )
    );
    Diagnostic::new(
        "dsl.check.const_ambiguous_at",
        message,
        Diagnostic::make_fields(&[("pos", pos), ("alias", alias), ("name", name)]),
    )
}

/// Static `LET` binding read as `CONST`. Takes the step position and
/// the qualified name.
pub fn check_const_is_let_at(pos: &str, qualified: &str) -> Diagnostic {
    let message = format!(
        "{pos}: `{qualified}` is a {KEYWORD_LET} binding and cannot be exported: only {KEYWORD_CONST} bindings cross the module boundary"
    );
    Diagnostic::new(
        "dsl.check.const_is_let_at",
        message,
        Diagnostic::make_fields(&[("pos", pos), ("name", qualified)]),
    )
}

/// Static unknown `CONST` with a resolution hint. Takes the step
/// position, the qualified name, and the hint.
pub fn check_const_hinted_at(pos: &str, qualified: &str, hint: &str) -> Diagnostic {
    let message = format!("{pos}: unknown {KEYWORD_CONST} `{qualified}`: {hint}");
    Diagnostic::new(
        "dsl.check.const_hinted_at",
        message,
        Diagnostic::make_fields(&[("pos", pos), ("name", qualified), ("hint", hint)]),
    )
}

/// Static call arity (exact). Takes the 1-based step number, the base
/// name, the expected total, and the count.
pub fn check_call_arity_exact(step: usize, display: &str, total: usize, got: usize) -> Diagnostic {
    let message = format!("step {step}: {display}() expects {total} argument(s), got {got}");
    Diagnostic::new(
        "dsl.check.call_arity_exact",
        message,
        Diagnostic::make_fields(&[
            ("step", &step.to_string()),
            ("function", display),
            ("expected", &total.to_string()),
            ("got", &got.to_string()),
        ]),
    )
}

/// Static call arity (range). Takes the 1-based step number, the base
/// name, min, max, and the count.
pub fn check_call_arity_range(
    step: usize,
    display: &str,
    min: usize,
    max: usize,
    got: usize,
) -> Diagnostic {
    let message = format!("step {step}: {display}() expects {min} to {max} argument(s), got {got}");
    Diagnostic::new(
        "dsl.check.call_arity_range",
        message,
        Diagnostic::make_fields(&[
            ("step", &step.to_string()),
            ("function", display),
            ("min", &min.to_string()),
            ("max", &max.to_string()),
            ("got", &got.to_string()),
        ]),
    )
}

/// Static checked-call arity (exact) with an optional RPN position.
/// Takes the location prefix, the base name, the expected total, and
/// the count.
pub fn check_apply_arity_exact(
    prefix: &str,
    display: &str,
    total: usize,
    got: usize,
) -> Diagnostic {
    let message = format!("{prefix}{display}() expects {total} argument(s), got {got}");
    Diagnostic::new(
        "dsl.check.apply_arity_exact",
        message,
        Diagnostic::make_fields(&[
            ("prefix", prefix),
            ("function", display),
            ("expected", &total.to_string()),
            ("got", &got.to_string()),
        ]),
    )
}

/// Static checked-call arity (range) with an optional RPN position.
/// Takes the location prefix, the base name, min, max, and the count.
pub fn check_apply_arity_range(
    prefix: &str,
    display: &str,
    min: usize,
    max: usize,
    got: usize,
) -> Diagnostic {
    let message = format!("{prefix}{display}() expects {min} to {max} argument(s), got {got}");
    Diagnostic::new(
        "dsl.check.apply_arity_range",
        message,
        Diagnostic::make_fields(&[
            ("prefix", prefix),
            ("function", display),
            ("min", &min.to_string()),
            ("max", &max.to_string()),
            ("got", &got.to_string()),
        ]),
    )
}

/// Static checked-call closed-set miss. Takes the argument context,
/// the allowed values, and the rendered literal; joining lives here.
pub fn check_call_arg_allowed(pat: &str, allowed: &[&str], text: &str) -> Diagnostic {
    let joined = allowed.join(", ");
    let message = format!("{pat} must be one of: {joined}, got {text:?}");
    Diagnostic::new(
        "dsl.check.call_arg_allowed",
        message,
        Diagnostic::make_fields(&[("ctx", pat), ("allowed", &joined), ("got", text)]),
    )
}

/// Static `VOID` in arithmetic. Takes the location prefix (possibly
/// empty in RPN contexts).
pub fn check_void_in_arithmetic(prefix: &str) -> Diagnostic {
    let message = format!("{prefix}{VOID_TYPE_NAME} has no value in arithmetic");
    Diagnostic::new(
        "dsl.check.void_in_arithmetic",
        message,
        Diagnostic::make_fields(&[("prefix", prefix)]),
    )
}

/// Static non-numeric arithmetic. Takes the location prefix.
pub fn check_arithmetic_type(prefix: &str) -> Diagnostic {
    let message = format!("{prefix}Type Error: arithmetic requires Int or Float");
    Diagnostic::new(
        "dsl.check.arithmetic_type",
        message,
        Diagnostic::make_fields(&[("prefix", prefix)]),
    )
}

/// Static `VOID` under unary negation. Takes the location prefix.
pub fn check_void_to_negate(prefix: &str) -> Diagnostic {
    let message = format!("{prefix}{VOID_TYPE_NAME} has no value to negate");
    Diagnostic::new(
        "dsl.check.void_to_negate",
        message,
        Diagnostic::make_fields(&[("prefix", prefix)]),
    )
}

/// Static non-numeric unary negation. Takes the location prefix and
/// the actual type name.
pub fn check_negate_type(prefix: &str, actual: &str) -> Diagnostic {
    let message = format!("{prefix}Type Error: unary '-' requires Int or Float, got {actual}");
    Diagnostic::new(
        "dsl.check.negate_type",
        message,
        Diagnostic::make_fields(&[("prefix", prefix), ("actual", actual)]),
    )
}

/// Static `VOID` in an ordering comparison. Takes the location
/// prefix.
pub fn check_void_to_compare(prefix: &str) -> Diagnostic {
    let message = format!("{prefix}{VOID_TYPE_NAME} has no value to compare");
    Diagnostic::new(
        "dsl.check.void_to_compare",
        message,
        Diagnostic::make_fields(&[("prefix", prefix)]),
    )
}

/// Static non-numeric ordering comparison. Takes the location prefix.
pub fn check_ordering_type(prefix: &str) -> Diagnostic {
    let message = format!("{prefix}Type Error: ordering comparison requires Int or Float");
    Diagnostic::new(
        "dsl.check.ordering_type",
        message,
        Diagnostic::make_fields(&[("prefix", prefix)]),
    )
}

/// Static RPN stack underflow. Parameterless: an internal invariant,
/// never user input.
pub fn check_rpn_underflow() -> Diagnostic {
    Diagnostic::new(
        "dsl.check.rpn_underflow",
        "internal error: RPN stack underflow".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Static RPN stack with leftover values. Takes the count.
pub fn check_rpn_leftover(count: usize) -> Diagnostic {
    let message = format!("internal error: RPN stack left {count} values");
    Diagnostic::new(
        "dsl.check.rpn_leftover",
        message,
        Diagnostic::make_fields(&[("count", &count.to_string())]),
    )
}

/// Static block falling through with no `RETURN` on some path. Takes
/// the step position.
pub fn check_block_fallthrough(pos: &str) -> Diagnostic {
    let message = format!("{pos}: block may fall through with no {KEYWORD_RETURN} on some path");
    Diagnostic::new(
        "dsl.check.block_fallthrough",
        message,
        Diagnostic::make_fields(&[("pos", pos)]),
    )
}

/// Static `VOID` where a template value is required. Parameterless:
/// fixed wording, shared by map and list literals.
pub fn check_void_no_value() -> Diagnostic {
    Diagnostic::new(
        "dsl.check.void_no_value",
        "VOID has no value to pass".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Utf8Error` sample built at runtime: a literal invalid
    /// sequence would trip the `invalid_from_utf8` lint, so the
    /// bytes go through a `Vec` first.
    fn utf8_err_sample() -> std::str::Utf8Error {
        String::from_utf8(vec![0xff]).unwrap_err().utf8_error()
    }

    /// A `regex::Error` sample built at runtime: a literal invalid
    /// pattern would trip the `invalid_regex` lint, so the pattern
    /// goes through a `String` first.
    fn regex_err_sample() -> regex::Error {
        let pattern = "[".to_owned();
        regex::Regex::new(&pattern).unwrap_err()
    }

    /// An `ExitStatus` sample built through the shared test helper:
    /// the status rendering is platform-native, so tests pin the
    /// prefix only, never the full text.
    fn exit_status_sample() -> std::process::ExitStatus {
        oxdock_sys_test_utils::exit_status_from_code(1)
    }

    #[test]
    fn clash_names_both_sides() {
        let d = alias_clash("x", "y", "z");
        assert_eq!(d.key(), "dsl.alias.clash");
        assert_eq!(d.message(), "alias `x` for field `y` clashes with `z`");
    }

    #[test]
    fn self_and_duplicate_have_distinct_keys() {
        assert_eq!(alias_self("a", "a").key(), "dsl.alias.self");
        assert_eq!(map_duplicate_field("a").key(), "dsl.map.duplicate_field");
    }

    #[test]
    fn every_resolution_error_has_a_distinct_key() {
        let types = vec!["STRING".to_string()];
        let empty: Vec<String> = vec![];
        let keys = [
            typing_max_depth(8, "INT").key(),
            typing_bare_map().key(),
            typing_bare_list().key(),
            typing_bare_handle().key(),
            alias_cycle(&["A".to_string(), "A".to_string()]).key(),
            typing_unknown("NOPE", &types, &empty, &empty).key(),
            typing_unregistered("LET $x", "NOPE", &types).key(),
            typing_void_position("LET $x").key(),
            check_unknown_option("F", "k", &["a", "b"]).key(),
            check_option_value(&anyhow::anyhow!("x")).key(),
            typing_no_args("STRING").key(),
            typing_handle_arity().key(),
            typing_list_arity().key(),
            typing_map_bare("MAP").key(),
            typing_default_mismatch("f", "INT").key(),
            typing_empty_union().key(),
            alias_already_defined("A").key(),
            alias_builtin_collision("A").key(),
            alias_host_collision("A").key(),
            alias_schema_collision("A").key(),
            alias_func_collision("A").key(),
            mcp_export_type_unknown("T").key(),
            mcp_break_outside_loop(1).key(),
            mcp_continue_outside_loop(1).key(),
            mcp_return_outside_function(1).key(),
            mcp_unknown_tool("FOO").key(),
            mcp_call_failed(&anyhow::anyhow!("boom"), "", "").key(),
            import_cycle(&["a".to_string(), "b".to_string()]).key(),
            import_depth_limit(8, "f").key(),
            import_not_found("f").key(),
            import_cannot_resolve("f", &anyhow::anyhow!("x")).key(),
            import_cannot_read("f", &anyhow::anyhow!("x")).key(),
            import_bad_utf8("f", &anyhow::anyhow!("x")).key(),
            import_parse_failed("f", &anyhow::anyhow!("x")).key(),
            import_missing_export("f", "FUNC", "g").key(),
            import_unknown_member("f", "function", "g").key(),
            import_unknown_type("f", "T", None).key(),
            import_failed_validation("f", &anyhow::anyhow!("x")).key(),
            import_stateful_harvest("f", "g").key(),
            import_unknown_function("f", "g").key(),
            import_type_collision("f", "T", "a", "b").key(),
            import_const_no_alias("q", "A").key(),
            import_const_ambiguous("A", "N", &std::collections::HashSet::new()).key(),
            import_const_hinted("q", "h").key(),
            import_const_unknown("q").key(),
            import_const_uninitialized("q").key(),
            import_const_is_let("q").key(),
            import_const_forward_failed(&anyhow::anyhow!("x")).key(),
            import_not_in_closure("f").key(),
            import_export_type_mismatch("f", "T").key(),
            check_var_tag("v", &anyhow::anyhow!("x")).key(),
            check_func_tag("f", &anyhow::anyhow!("x")).key(),
            check_step_wrap(3, &anyhow::anyhow!("x")).key(),
            check_ctx_wrap("ctx", &anyhow::anyhow!("x")).key(),
            check_exact_mismatch("ctx", &TypeTag::Int, &TypeTag::String).key(),
            check_cannot_unify("ctx", &TypeTag::Int, &TypeTag::String).key(),
            check_undefined_var("step 1: ", "x").key(),
            check_var_not_guaranteed("x").key(),
            check_unknown_field("step 1: ", "a", &[]).key(),
            check_invalid_array_index("", "0").key(),
            check_cannot_traverse("", "k").key(),
            check_duplicate_func("F", "f.oxdock").key(),
            check_redeclare_const(1, "x").key(),
            check_redeclare_let(1, "x").key(),
            check_undeclared_env("step 1", "top", "env:", "K").key(),
            check_placeholder_keypath().key(),
            check_empty_placeholder_var().key(),
            check_bare_sigil("$x").key(),
            check_undefined_script_var("k").key(),
            check_placeholder_unknown("A::F").key(),
            check_disjoint_alias_miss(
                "A",
                "F",
                "F",
                &std::collections::HashSet::new(),
                false,
                &|_| false,
            )
            .key(),
            check_placeholder_miss("M", "F", true, false, false)
                .expect("described")
                .key(),
            check_template_arity_exact("step 1", "top", "F", 1, 2).key(),
            check_template_arity_range("step 1", "top", "F", 1, 2, 3).key(),
            check_template_arg_allowed("step 1", "top", "F", "p", &["a"], "b").key(),
            check_template_arg_depth().key(),
            check_empty_placeholder_arg().key(),
            check_invalid_placeholder_arg("x").key(),
            check_invalid_map_entry("e").key(),
            check_duplicate_map_key("k").key(),
            check_void_no_value().key(),
            check_depth_function("F").key(),
            check_depth_scope("top").key(),
            check_void_block_bind(1, "x").key(),
            check_capture_mismatch(1, "LET", "x", &TypeTag::String, &TypeTag::Int).key(),
            check_await_not_handle(1, "t", &TypeTag::String).key(),
            check_await_void_payload(1, "t", "x").key(),
            check_await_yield_mismatch(1, "t", &TypeTag::String, "x", &TypeTag::Int).key(),
            check_async_not_handle(1, "x", &TypeTag::Int).key(),
            check_async_bare_return(1, "x", &TypeTag::Int).key(),
            check_async_void_yield(1, 3, &TypeTag::Int).key(),
            check_async_no_return(1, "x", &TypeTag::Int).key(),
            check_async_return_mismatch(1, "x", &TypeTag::Int, 3, &TypeTag::String).key(),
            check_return_outside(1).key(),
            check_return_crosses_async(1).key(),
            check_undeclared_set(1, "x").key(),
            check_mutate_const(1, "x").key(),
            check_isolated_allowlist(1).key(),
            check_remote_unknown_var("t", "x").key(),
            check_remote_undeclared_env("step 1", "top", "t", "K").key(),
            check_list_not_list(1, "l", &TypeTag::String).key(),
            check_list_void_push(1, "l").key(),
            check_pipe_mismatch(1, "STRING").key(),
            check_missing_field("ctx", "f").key(),
            check_record_unknown_field("ctx", "f").key(),
            check_break_outside_switch(1).key(),
            check_flow_outside_loop(1, "CONTINUE").key(),
            check_flow_crosses_func(1, "BREAK", "F").key(),
            check_flow_crosses_block(1, "BREAK").key(),
            check_flow_crosses_async(1, "BREAK").key(),
            check_arg_void("ctx").key(),
            check_any_into_concrete("ctx", &TypeTag::String).key(),
            check_async_decl_not_handle("F", &TypeTag::Int).key(),
            check_func_fallthrough("F", &TypeTag::Int).key(),
            check_func_no_return("F", &TypeTag::Int).key(),
            check_func_bare_return("F", &TypeTag::Int).key(),
            check_func_void_yield("F", 3, &TypeTag::Int).key(),
            check_func_no_value_return("F", &TypeTag::Int).key(),
            check_func_return_mismatch("F", &TypeTag::Int, 3, &TypeTag::String).key(),
            check_unknown_function("F", "").key(),
            check_const_no_alias_at("step 1", "A::C", "A").key(),
            check_const_ambiguous_at("step 1", "A", "C", &std::collections::HashSet::new()).key(),
            check_const_is_let_at("step 1", "A::C").key(),
            check_const_hinted_at("step 1", "A::C", "h").key(),
            check_call_arity_exact(1, "F", 1, 2).key(),
            check_call_arity_range(1, "F", 1, 2, 3).key(),
            check_apply_arity_exact("", "F", 1, 2).key(),
            check_apply_arity_range("", "F", 1, 2, 3).key(),
            check_call_arg_allowed("ctx", &["a"], "b").key(),
            check_void_in_arithmetic("").key(),
            check_arithmetic_type("").key(),
            check_void_to_negate("").key(),
            check_negate_type("", "STRING").key(),
            check_void_to_compare("").key(),
            check_ordering_type("").key(),
            check_rpn_underflow().key(),
            check_rpn_leftover(2).key(),
            check_block_fallthrough("step 1").key(),
            import_flow_crosses_boundary("BREAK", 3, "f").key(),
            func_zero_value_missing("MAP").key(),
            func_param_type("F", &anyhow::anyhow!("x")).key(),
            func_optional_param("F", "p", &anyhow::anyhow!("x")).key(),
            func_shadow_reserved("F").key(),
            func_duplicate_in_scope("F").key(),
            func_param_values_without_type("p", "F").key(),
            func_optional_before_required("p", "F").key(),
            func_optional_without_map("p", "F").key(),
            func_ambiguous_alias("A", "F", &std::collections::HashSet::new()).key(),
            func_export_unknown("F").key(),
            func_placeholder_unknown("M", "F", &["M::F".to_string()]).key(),
            func_describe_unqualified("F").key(),
            func_unknown("F").key(),
            std_unknown_type("T").key(),
            std_contains_needle_type("INT").key(),
            std_contains_haystack_type("INT").key(),
            std_len_unexpected_type("INT").key(),
            std_is_terminal_expected("nope").key(),
            std_semaphore_needs_positive(0).key(),
            std_bool_needs_bool_or_string(&crate::Value::string("x".to_string())).key(),
            std_bool_cannot_coerce("BOOL", "x").key(),
            std_shell_split_failed(&shell_words::ParseError).key(),
            std_path_base_failed(&anyhow::anyhow!("x")).key(),
            std_path_rel_failed(&anyhow::anyhow!("x")).key(),
            std_parse_args_arity(2).key(),
            std_parse_args_bool_flag("f", "x").key(),
            std_parse_args_bool_type("f", "INT").key(),
            std_parse_args_needs_value("f").key(),
            std_parse_args_expects_int("f").key(),
            std_parse_args_expects_float("f").key(),
            std_parse_args_not_utf8().key(),
            std_parse_args_lex_error(&lexopt::Error::UnexpectedOption("--x".to_string())).key(),
            std_parse_args_schema_flag_empty().key(),
            std_parse_args_duplicate_flag("f", None).key(),
            std_parse_args_missing_flag("f", None).key(),
            std_parse_args_unknown_flag("f").key(),
            std_parse_args_schema_type("S", &anyhow::anyhow!("x")).key(),
            std_parse_args_schema_shape("S").key(),
            std_parse_args_schema_field_type("f", "INT").key(),
            std_parse_args_arg_shape(&anyhow::anyhow!("x")).key(),
            std_parse_flags_arity(2).key(),
            std_parse_flags_positional(&["a".to_string()]).key(),
            std_parse_positionals_arity(2).key(),
            std_parse_byte_count_negative(-1).key(),
            std_parse_byte_count_integral(1.5).key(),
            std_parse_byte_count_shape(&crate::Value::string("x".to_string())).key(),
            std_parse_byte_count_range("x").key(),
            std_int_non_integer_float(1.5).key(),
            std_int_bad_string("x").key(),
            std_int_bad_type(&crate::Value::string("x".to_string())).key(),
            std_float_non_finite_float(f64::INFINITY).key(),
            std_float_bad_string("x").key(),
            std_float_non_finite_string("inf").key(),
            std_float_bad_type(&crate::Value::string("x".to_string())).key(),
            eval_math_stack_underflow().key(),
            eval_math_stack_len(2).key(),
            eval_arith_overflow_div_zero().key(),
            eval_arith_needs_numeric(
                &crate::Value::string("x".to_string()),
                &crate::Value::string("y".to_string()),
            )
            .key(),
            eval_float_div_zero().key(),
            eval_non_finite_result().key(),
            eval_neg_overflow().key(),
            eval_neg_needs_numeric(&crate::Value::string("x".to_string())).key(),
            eval_condition_needs_bool(&crate::Value::string("x".to_string())).key(),
            eval_expected_int("x").key(),
            eval_int_arg_resolve(&anyhow::anyhow!("x")).key(),
            eval_dispatch_failed(&anyhow::anyhow!("x")).key(),
            eval_pipe_assertion_read_failed(&anyhow::anyhow!("x")).key(),
            eval_with_io_unexpanded().key(),
            eval_async_cancelled().key(),
            eval_flow_crosses_remote(1, "BREAK").key(),
            eval_stdin_lock_failed("READ").key(),
            eval_stdin_read_failed(None, &std::io::Error::other("x")).key(),
            eval_assert_stdin_overflow("ASSERT_EQ").key(),
            eval_assert_stdout_overflow("ASSERT_EQ").key(),
            eval_assert_no_exact_capture("ASSERT_EQ").key(),
            eval_assert_hash_mismatch("ASSERT_EQ", "a", "b").key(),
            eval_assert_hash_match("ASSERT_EQ", "a").key(),
            eval_assert_hash_needs_string("ASSERT_EQ", &crate::Value::string("x".to_string()))
                .key(),
            eval_assert_stream_mismatch("ASSERT_EQ", None, "a").key(),
            eval_assert_stream_mismatch_text("ASSERT_EQ", "a", "b").key(),
            eval_assert_stream_unexpected_equal("ASSERT_NE", "a").key(),
            eval_assert_stderr_unsupported("ASSERT_EQ").key(),
            eval_assert_requires_expected("ASSERT_EQ").key(),
            eval_assert_value_mismatch(
                "ASSERT_EQ",
                &crate::Value::string("a".to_string()),
                &crate::Value::string("b".to_string()),
            )
            .key(),
            eval_assert_value_unexpected_equal(
                "ASSERT_NE",
                &crate::Value::string("a".to_string()),
                &crate::Value::string("a".to_string()),
            )
            .key(),
            eval_assert_pipe_mismatch("ASSERT_EQ", None, "a").key(),
            eval_assert_pipe_mismatch_text("ASSERT_EQ", "a", "b").key(),
            eval_assert_pipe_unexpected_equal("ASSERT_NE", "a").key(),
            eval_assert_contains_str_mismatch("ASSERT_CONTAINS", "n", "h").key(),
            eval_assert_contains_value_mismatch(
                "ASSERT_CONTAINS",
                "n",
                &crate::Value::string("x".to_string()),
            )
            .key(),
            eval_assert_contains_missing("ASSERT_CONTAINS", "n").key(),
            eval_assert_contains_needs_haystack(
                "ASSERT_CONTAINS",
                &crate::Value::string("x".to_string()),
            )
            .key(),
            eval_assert_contains_emitted_mismatch("ASSERT_CONTAINS", "n", "e").key(),
            eval_assert_contains_pipe_mismatch("ASSERT_CONTAINS", "n", "h").key(),
            eval_break_outside_loop().key(),
            eval_continue_outside_loop().key(),
            eval_capture_flow_outside_loop(1, "BREAK").key(),
            eval_capture_return_outside_function(1).key(),
            eval_run_cancelled("echo hi").key(),
            eval_run_argv_cancelled(&["echo".to_string()]).key(),
            eval_run_unexpected_capture("echo hi").key(),
            eval_run_argv_unexpected_capture(&["echo".to_string()]).key(),
            eval_run_exit_status("echo hi", &exit_status_sample()).key(),
            eval_run_argv_exit_status(&["echo".to_string()], &exit_status_sample()).key(),
            eval_run_needs_program("[]").key(),
            eval_run_pty_exit_status("echo hi", 1).key(),
            eval_run_exec_requires_arg().key(),
            eval_pipe_not_utf8("ASSERT", &String::from_utf8(vec![0xff]).unwrap_err()).key(),
            eval_run_spawn_failed("echo hi", &anyhow::anyhow!("x")).key(),
            eval_run_argv_spawn_failed(&["echo".to_string()], &anyhow::anyhow!("x")).key(),
            eval_run_exec_pipe_arg().key(),
            eval_run_exec_semaphore_arg().key(),
            eval_run_exec_permit_arg().key(),
            eval_run_exec_element_needs_string("map").key(),
            eval_sleep_cancelled().key(),
            eval_copy_failed("a", "b", &anyhow::anyhow!("x")).key(),
            eval_copy_git_failed("r", "a", "b", &anyhow::anyhow!("x")).key(),
            eval_copy_no_file_name("p").key(),
            eval_copy_destination_escapes("p", &anyhow::anyhow!("x")).key(),
            eval_symlink_failed("a", "b", &anyhow::anyhow!("x")).key(),
            eval_symlink_no_file_name("a").key(),
            eval_workdir_failed("p", &anyhow::anyhow!("x")).key(),
            eval_mkdir_resolve_failed("p", &anyhow::anyhow!("x")).key(),
            eval_mkdir_create_failed("p", &anyhow::anyhow!("x")).key(),
            eval_set_executable_failed("p", &anyhow::anyhow!("x")).key(),
            eval_ls_resolve_failed(Some("p"), &anyhow::anyhow!("x")).key(),
            eval_ls_read_failed("p", &anyhow::anyhow!("x")).key(),
            eval_read_resolve_failed("p", &anyhow::anyhow!("x")).key(),
            eval_read_open_failed("p", &anyhow::anyhow!("x")).key(),
            eval_read_file_failed(&std::io::Error::other("x")).key(),
            eval_read_write_failed(&std::io::Error::other("x")).key(),
            eval_read_needs_stdin().key(),
            eval_read_line_needs_stdin().key(),
            eval_read_line_not_utf8(&String::from_utf8(vec![0xff]).unwrap_err()).key(),
            eval_write_resolve_failed("p", &anyhow::anyhow!("x")).key(),
            eval_ensure_parent_failed("p", &anyhow::anyhow!("x")).key(),
            eval_write_file_failed("p", &anyhow::anyhow!("x")).key(),
            eval_write_needs_stdin("p").key(),
            eval_write_open_failed("p", &anyhow::anyhow!("x")).key(),
            eval_write_stream_failed("p", &anyhow::anyhow!("x")).key(),
            eval_append_resolve_failed("p", &anyhow::anyhow!("x")).key(),
            eval_append_failed("p", &anyhow::anyhow!("x")).key(),
            eval_append_needs_stdin("p").key(),
            eval_append_open_failed("p", &anyhow::anyhow!("x")).key(),
            eval_async_exit_status(None, &exit_status_sample()).key(),
            eval_arg_expr_needs_step_ctx(&Expr::Env("X".to_string())).key(),
            eval_arg_expand_failed(&anyhow::anyhow!("x")).key(),
            coerce_type_mismatch(
                "coercion",
                &TypeTag::Int,
                &crate::Value::string("x".to_string()),
            )
            .key(),
            coerce_string_to_pipe("x").key(),
            coerce_pipe_to_string().key(),
            coerce_check_failed(&anyhow::anyhow!("x")).key(),
            eval_host_return_failed(&anyhow::anyhow!("x")).key(),
            eval_scope_underflow().key(),
            eval_redeclare_var("x").key(),
            eval_redeclare_const("x").key(),
            eval_no_var_scope().key(),
            eval_const_mutation("x").key(),
            eval_undeclared_declare("x", "TYPE").key(),
            eval_undeclared("x").key(),
            eval_list_not_list("x", "STRING").key(),
            eval_list_value_not_list("x").key(),
            eval_list_element_failed(&anyhow::anyhow!("x")).key(),
            eval_scratch_materialize_failed(&anyhow::anyhow!("x")).key(),
            eval_task_cap_exceeded(64).key(),
            eval_const_arg_keypath().key(),
            eval_const_arg_bare_sigil("foo").key(),
            eval_template_missing_key("foo", false).key(),
            eval_template_empty_placeholder().key(),
            eval_template_invalid_key("foo").key(),
            eval_template_empty_env().key(),
            eval_template_keypath().key(),
            eval_template_bare_sigil("foo").key(),
            eval_template_placeholder_miss("unknown placeholder").key(),
            std_pipe_read_failed("TEXT_REPLACE", &anyhow::anyhow!("x")).key(),
            std_pipe_create_failed("TEXT_REPLACE", &anyhow::anyhow!("x")).key(),
            std_pipe_spawn_failed("TEXT_REPLACE", &std::io::Error::other("x")).key(),
            std_pipe_reader_lock_poisoned("TEXT_REPLACE").key(),
            std_pipe_writer_lock_poisoned("TEXT_REPLACE").key(),
            std_pipe_handle_spent().key(),
            std_pipe_needs_pipe("TEXT_REPLACE", "STRING").key(),
            std_pipe_close_needs_script().key(),
            std_assert_lock_poisoned("TEXT_REPLACE").key(),
            std_pipe_chunk_read_failed("TEXT_REPLACE", &std::io::Error::other("x")).key(),
            std_pipe_input_not_utf8("TEXT_REPLACE").key(),
            std_pipe_spill_failed("TEXT_REPLACE", &std::io::Error::other("x")).key(),
            std_pipe_write_failed("TEXT_REPLACE", &std::io::Error::other("x")).key(),
            eval_compare_equality_shape().key(),
            eval_ordering_needs_numeric(
                &crate::Value::string("x".to_string()),
                &crate::Value::string("y".to_string()),
            )
            .key(),
            eval_unstaged_int_boundary(9223372036854775808).key(),
            eval_undefined_var("x").key(),
            eval_key_not_found("k").key(),
            eval_invalid_array_index("k").key(),
            eval_index_out_of_bounds(7).key(),
            eval_traverse_scalar("k").key(),
            eval_path_type_needs_arg().key(),
            eval_path_type_needs_string().key(),
            eval_is_exec_needs_arg().key(),
            eval_is_exec_needs_string().key(),
            eval_path_writable_needs_arg().key(),
            eval_path_writable_needs_string().key(),
            eval_path_resolve_failed("p", &anyhow::anyhow!("x")).key(),
            eval_workdir_resolve_failed(&anyhow::anyhow!("x")).key(),
            std_text_invalid_pattern("TEXT_MATCH", "[", &regex_err_sample()).key(),
            std_text_needs_search("TEXT_REPLACE").key(),
            std_text_needs_needle("TEXT_COUNT").key(),
            std_text_needs_one_based_start("TEXT_LINES_RANGE", 0).key(),
            std_text_needs_ordered_range("TEXT_LINES_RANGE", 3, 2).key(),
            std_pipe_needs_step_io("TEXT_REPLACE").key(),
            std_format_byte_count_negative(-1).key(),
            std_format_duration_negative(-1).key(),
            std_parse_duration_range("x").key(),
            std_to_json_non_finite_float(f64::NAN).key(),
            std_to_json_unencodable("VOID").key(),
            std_to_json_encode_failed(
                &serde_json::from_str::<serde_json::Value>("{bad").unwrap_err(),
            )
            .key(),
            std_load_toml_needs_arg().key(),
            std_load_toml_needs_string().key(),
            std_load_toml_resolve_failed("p", &anyhow::anyhow!("x")).key(),
            std_load_toml_read_failed("p", &anyhow::anyhow!("x")).key(),
            std_load_toml_not_utf8("p", &utf8_err_sample()).key(),
            std_toml_parse_failed(&toml::from_str::<serde_json::Value>("a = ").unwrap_err()).key(),
            std_parse_toml_needs_string("INT").key(),
            std_load_json_needs_arg().key(),
            std_load_json_needs_string().key(),
            std_load_json_resolve_failed("p", &anyhow::anyhow!("x")).key(),
            std_load_json_read_failed("p", &anyhow::anyhow!("x")).key(),
            std_load_json_not_utf8("p", &utf8_err_sample()).key(),
            std_json_parse_failed(&serde_json::from_str::<serde_json::Value>("{bad").unwrap_err())
                .key(),
            std_parse_json_needs_string("INT").key(),
            std_glob_needs_arg().key(),
            std_glob_needs_string().key(),
            std_glob_resolve_failed(&anyhow::anyhow!("x")).key(),
            std_glob_list_failed(&std::io::Error::other("x")).key(),
            std_has_key_needs_map("INT").key(),
            std_map_set_needs_map("INT").key(),
            std_map_set_duplicate("k").key(),
            std_merge_maps_needs_list("INT").key(),
            std_merge_maps_needs_map(0, "INT").key(),
            std_merge_maps_duplicate("k").key(),
            std_base64_encode_needs_string("INT").key(),
            std_base64_decode_needs_string("INT").key(),
            std_base64_decode_failed("!!!", &base64::DecodeError::InvalidByte(0, b'!')).key(),
            std_base64_decode_not_utf8().key(),
            json_handle_needs_object(&serde_json::json!(7)).key(),
            json_handle_exact_shape().key(),
            json_handle_task_id_positive().key(),
            json_handle_encode_needs_handle("STRING").key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn messages_match_canonical_wording() {
        assert_eq!(
            typing_bare_map().message(),
            "bare `MAP` is not a declaration type: write a shape (`MAP<name: TYPE, ...>`) or `MAP<ANY>` for maps with unknown keys"
        );
        assert_eq!(
            typing_bare_list().message(),
            "bare `LIST` is not a declaration type: write a shape (`LIST<TYPE>`) or `LIST<ANY>` for lists with unknown elements"
        );
        assert_eq!(
            typing_bare_handle().message(),
            "bare `HANDLE` is not a declaration type: write `HANDLE<T>` for a known payload or `HANDLE<ANY>` for an unknown one"
        );
        assert_eq!(
            typing_no_args("STRING").message(),
            "`STRING` takes no type arguments"
        );
        assert_eq!(
            typing_handle_arity().message(),
            "`HANDLE` takes exactly one bare type argument"
        );
        assert_eq!(
            typing_list_arity().message(),
            "`LIST` takes exactly one bare type argument"
        );
        assert_eq!(
            typing_empty_union().message(),
            "empty union: no member shapes to admit"
        );
        assert_eq!(
            typing_unregistered("LET $x", "NOPE", &["STRING".to_string()]).message(),
            "LET $x declares unregistered type 'NOPE'; known types: STRING"
        );
        assert_eq!(
            typing_void_position("LET $x").message(),
            "LET $x declares VOID outside a return annotation: VOID is only a return type (or HANDLE<VOID> payload)"
        );
        assert_eq!(
            check_unknown_option("F", "k", &["a", "b"]).message(),
            "F() unknown option 'k' (expected: a, b)"
        );
        assert_eq!(
            check_option_value(&anyhow::anyhow!("F() option 'k': expected INT, got STRING"))
                .message(),
            "F() option 'k': expected INT, got STRING"
        );
        assert_eq!(
            alias_cycle(&["A".to_string(), "B".to_string(), "A".to_string()]).message(),
            "type alias cycle: A -> B -> A"
        );
    }

    #[test]
    fn func_and_std_messages_match_canonical_wording() {
        assert_eq!(
            func_export_unknown("F").message(),
            "EXPORT `F` matches no defined script function or known host function"
        );
        assert!(
            func_placeholder_unknown("M", "F", &["M::F".to_string()])
                .message()
                .contains("`M::F`")
        );
        assert_eq!(func_unknown("F").message(), "unknown function `F`");
        assert_eq!(
            func_param_values_without_type("p", "F").message(),
            "param 'p' of 'F' declares values without a type"
        );
        assert_eq!(
            func_optional_before_required("p", "F").message(),
            "param 'p' of 'F' is optional before a required param"
        );
        assert_eq!(
            func_optional_without_map("p", "F").message(),
            "param 'p' of 'F' is optional without a MAP type"
        );
        assert_eq!(std_unknown_type("T").message(), "unknown type `T`");
        assert_eq!(
            std_parse_args_arity(2).message(),
            "PARSE_ARGS() expects 1 argument, got 2"
        );
        assert_eq!(
            std_parse_args_needs_value("f").message(),
            "PARSE_ARGS flag --f needs a value: use --f=<value>"
        );
        assert_eq!(
            std_parse_args_duplicate_flag("f", Some("g")).message(),
            "PARSE_ARGS duplicate flag --f/--g"
        );
        assert_eq!(
            std_parse_args_missing_flag("f", None).message(),
            "PARSE_ARGS missing required flag --f"
        );
        assert_eq!(
            std_parse_args_arg_shape(&anyhow::anyhow!(
                "PARSE_FLAGS() argument `$args`: expected LIST<STRING>, got INT"
            ))
            .message(),
            "PARSE_FLAGS() argument `$args`: expected LIST<STRING>, got INT"
        );
        assert_eq!(
            std_int_non_integer_float(1.5).message(),
            "INT() requires an integer value, found FLOAT (1.5)"
        );
        assert_eq!(
            std_int_bad_string("x").message(),
            "INT() requires an integer string, found \"x\""
        );
        assert_eq!(
            std_int_bad_type(&crate::Value::string("x".to_string())).message(),
            "INT() requires an Int, Float, Bool, or String, found STRING(\"x\")"
        );
        assert_eq!(
            std_float_non_finite_float(f64::INFINITY).message(),
            "FLOAT() requires a finite value, found FLOAT (inf)"
        );
        assert_eq!(
            std_float_bad_string("x").message(),
            "FLOAT() requires a numeric string, found \"x\""
        );
        assert_eq!(
            std_float_non_finite_string("inf").message(),
            "FLOAT() requires a finite value, found \"inf\""
        );
        assert_eq!(
            std_float_bad_type(&crate::Value::string("x".to_string())).message(),
            "FLOAT() requires an Int, Float, or String, found STRING(\"x\")"
        );
        assert_eq!(
            eval_math_stack_underflow().message(),
            "compiled math stack underflow"
        );
        assert_eq!(
            eval_math_stack_len(2).message(),
            "compiled math left 2 values on the stack"
        );
        assert_eq!(
            eval_arith_overflow_div_zero().message(),
            "arithmetic error: integer overflow or division by zero"
        );
        assert_eq!(
            eval_arith_needs_numeric(
                &crate::Value::string("x".to_string()),
                &crate::Value::string("y".to_string()),
            )
            .message(),
            "Type Error: arithmetic requires Int or Float, found STRING(\"x\") and STRING(\"y\")"
        );
        assert_eq!(
            eval_float_div_zero().message(),
            "arithmetic error: float division by zero"
        );
        assert_eq!(
            eval_non_finite_result().message(),
            "arithmetic error: non-finite float result"
        );
        assert_eq!(
            eval_neg_overflow().message(),
            "arithmetic error: integer overflow"
        );
        assert_eq!(
            eval_neg_needs_numeric(&crate::Value::string("x".to_string())).message(),
            "Type Error: unary '-' requires Int or Float, found STRING(\"x\")"
        );
        assert_eq!(
            eval_condition_needs_bool(&crate::Value::string("x".to_string())).message(),
            "Type Error: condition must be a Bool, found STRING(\"x\")"
        );
        assert_eq!(
            eval_expected_int("12x").message(),
            "expected int, got \"12x\""
        );
        assert_eq!(
            eval_int_arg_resolve(&anyhow::anyhow!("undefined variable `$x`")).message(),
            "undefined variable `$x`"
        );
        assert_eq!(
            eval_dispatch_failed(&anyhow::anyhow!("boom")).message(),
            "boom"
        );
        assert_eq!(
            eval_pipe_assertion_read_failed(&anyhow::anyhow!("boom")).message(),
            "step pipe assertion cannot read pipe: boom"
        );
        assert_eq!(
            eval_with_io_unexpanded().message(),
            "WITH_IO block should have been expanded during parsing"
        );
        assert_eq!(eval_async_cancelled().message(), "ASYNC task cancelled");
        assert_eq!(
            eval_flow_crosses_remote(3, "BREAK").message(),
            "step 3: BREAK cannot cross a REMOTE boundary"
        );
        assert_eq!(
            eval_stdin_lock_failed("READ").message(),
            "failed to lock stdin for READ"
        );
        assert_eq!(
            eval_stdin_read_failed(None, &std::io::Error::other("boom")).message(),
            "failed to read from stdin: boom"
        );
        assert_eq!(
            eval_stdin_read_failed(Some("WRITE"), &std::io::Error::other("boom")).message(),
            "failed to read from stdin for WRITE: boom"
        );
        assert_eq!(
            eval_assert_stdin_overflow("ASSERT_EQ").message(),
            "ASSERT_EQ stdin exceeded the 8 MiB exact buffer; assert via pipe targets or harness expect.stdout"
        );
        assert_eq!(
            eval_assert_stdout_overflow("ASSERT_EQ").message(),
            "ASSERT_EQ stdout overflowed the 8 MiB exact buffer; assert via pipe targets or harness expect.stdout"
        );
        assert_eq!(
            eval_assert_no_exact_capture("ASSERT_EQ").message(),
            "ASSERT_EQ stdout has no exact capture registered"
        );
        assert_eq!(
            eval_assert_hash_mismatch("ASSERT_EQ", "a", "b").message(),
            "ASSERT_EQ --hash mismatch: expected a, computed b"
        );
        assert_eq!(
            eval_assert_hash_match("ASSERT_NE", "a").message(),
            "ASSERT_NE --hash match: content matches a"
        );
        assert_eq!(
            eval_assert_stream_mismatch("ASSERT_EQ", None, "a").message(),
            "ASSERT_EQ stream mismatch\nexpected: None\nactual:   \"a\""
        );
        assert_eq!(
            eval_assert_stream_mismatch_text("ASSERT_EQ", "a", "b").message(),
            "ASSERT_EQ stream mismatch\nexpected: \"a\"\nactual:   \"b\""
        );
        assert_eq!(
            eval_assert_stream_unexpected_equal("ASSERT_NE", "a").message(),
            "ASSERT_NE stream unexpectedly equal: \"a\""
        );
        assert_eq!(
            eval_assert_stderr_unsupported("ASSERT_EQ").message(),
            "ASSERT_EQ over stderr is not supported; use ASSERT_CONTAINS stderr ..."
        );
        assert_eq!(
            eval_assert_requires_expected("ASSERT_EQ").message(),
            "ASSERT_EQ requires an expected value"
        );
        assert_eq!(
            eval_assert_pipe_mismatch_text("ASSERT_EQ", "a", "b").message(),
            "ASSERT_EQ pipe mismatch\nexpected: \"a\"\nactual:   \"b\""
        );
        assert_eq!(
            eval_assert_pipe_unexpected_equal("ASSERT_NE", "a").message(),
            "ASSERT_NE pipe unexpectedly equal: \"a\""
        );
        assert_eq!(
            eval_assert_contains_str_mismatch("ASSERT_CONTAINS", "n", "h").message(),
            "ASSERT_CONTAINS did not contain 'n'; actual: \"h\""
        );
        assert_eq!(
            eval_assert_contains_missing("ASSERT_CONTAINS", "n").message(),
            "ASSERT_CONTAINS did not contain 'n'"
        );
        assert_eq!(
            eval_assert_contains_emitted_mismatch("ASSERT_CONTAINS", "n", "e  ").message(),
            "ASSERT_CONTAINS did not contain 'n'; emitted:\ne"
        );
        assert_eq!(
            eval_assert_contains_pipe_mismatch("ASSERT_CONTAINS", "n", "h").message(),
            "ASSERT_CONTAINS did not contain 'n'; pipe held \"h\""
        );
        assert_eq!(eval_break_outside_loop().message(), "BREAK outside loop");
        assert_eq!(
            eval_continue_outside_loop().message(),
            "CONTINUE outside loop"
        );
        assert_eq!(
            eval_capture_flow_outside_loop(1, "BREAK").message(),
            "step 1: BREAK outside loop (cannot be captured)"
        );
        assert_eq!(
            eval_capture_return_outside_function(1).message(),
            "step 1: RETURN outside function (cannot be captured)"
        );
        assert_eq!(
            eval_run_cancelled("echo hi").message(),
            "RUN echo hi interrupted by cancellation"
        );
        assert_eq!(
            eval_run_argv_cancelled(&["echo".to_string()]).message(),
            "RUN [\"echo\"] interrupted by cancellation"
        );
        assert_eq!(
            eval_run_unexpected_capture("echo hi").message(),
            "RUN echo hi unexpectedly captured output"
        );
        assert_eq!(
            eval_run_needs_program("[]").message(),
            "RUN [] needs at least a program"
        );
        assert_eq!(
            eval_run_pty_exit_status("echo hi", 1).message(),
            "RUN echo hi exited with status 1"
        );
        assert_eq!(
            eval_run_exec_requires_arg().message(),
            "RUN exec form requires at least one argument"
        );
        assert!(
            eval_pipe_not_utf8("ASSERT", &String::from_utf8(vec![0xff]).unwrap_err())
                .message()
                .starts_with("ASSERT pipe content is not UTF-8: ")
        );
        assert!(
            eval_run_spawn_failed("echo hi", &anyhow::anyhow!("boom"))
                .message()
                .starts_with("RUN echo hi: ")
        );
        assert!(
            eval_run_argv_spawn_failed(&["echo".to_string()], &anyhow::anyhow!("boom"))
                .message()
                .starts_with("RUN [\"echo\"]: ")
        );
        assert_eq!(
            eval_run_exec_pipe_arg().message(),
            "cannot pass PIPE as a command argument: pipes carry streams between steps, not text. Capture the producing step's output with LET $var: STRING = ... instead."
        );
        assert_eq!(
            eval_run_exec_semaphore_arg().message(),
            "cannot pass SEMAPHORE as a command argument: semaphores coordinate task access between steps, not text."
        );
        assert_eq!(
            eval_run_exec_permit_arg().message(),
            "cannot pass PERMIT as a command argument: permits grant scoped task access, not text."
        );
        assert_eq!(
            eval_run_exec_element_needs_string("map").message(),
            "RUN exec form element must be a string, got map"
        );
        assert_eq!(
            eval_run_exec_element_needs_string("task handle task#7").message(),
            "RUN exec form element must be a string, got task handle task#7"
        );
        assert_eq!(
            eval_sleep_cancelled().message(),
            "SLEEP interrupted by cancellation"
        );
        assert!(
            eval_copy_failed("a", "b", &anyhow::anyhow!("boom"))
                .message()
                .starts_with("COPY a b: ")
        );
        assert!(
            eval_copy_git_failed("r", "a", "b", &anyhow::anyhow!("boom"))
                .message()
                .starts_with("COPY_GIT r a b: ")
        );
        assert_eq!(
            eval_copy_no_file_name("/x").message(),
            "COPY source has no file name: /x"
        );
        assert!(
            eval_copy_destination_escapes("/x", &anyhow::anyhow!("boom"))
                .message()
                .starts_with("COPY destination escapes root: /x: ")
        );
        assert_eq!(
            eval_symlink_failed("a", "b", &anyhow::anyhow!("boom")).message(),
            "SYMLINK a b"
        );
        assert_eq!(
            eval_symlink_no_file_name("a").message(),
            "SYMLINK source has no file name: a"
        );
        assert_eq!(
            eval_workdir_failed("sub", &anyhow::anyhow!("boom")).message(),
            "WORKDIR sub"
        );
        assert_eq!(
            eval_mkdir_resolve_failed("d", &anyhow::anyhow!("boom")).message(),
            "MKDIR d"
        );
        assert_eq!(
            eval_mkdir_create_failed("/x", &anyhow::anyhow!("boom")).message(),
            "failed to create dir /x"
        );
        assert_eq!(
            eval_set_executable_failed("bin", &anyhow::anyhow!("boom")).message(),
            "SET_EXECUTABLE bin"
        );
        assert_eq!(
            eval_ls_resolve_failed(Some("dir"), &anyhow::anyhow!("boom")).message(),
            "LS dir"
        );
        assert_eq!(
            eval_ls_resolve_failed(None, &anyhow::anyhow!("boom")).message(),
            "LS"
        );
        assert_eq!(
            eval_ls_read_failed("/x", &anyhow::anyhow!("boom")).message(),
            "LS /x"
        );
        assert_eq!(
            eval_read_resolve_failed("f", &anyhow::anyhow!("boom")).message(),
            "READ f"
        );
        assert_eq!(
            eval_read_open_failed("/x", &anyhow::anyhow!("boom")).message(),
            "failed to open /x"
        );
        assert_eq!(
            eval_read_file_failed(&std::io::Error::other("boom")).message(),
            "failed to read from file"
        );
        assert_eq!(
            eval_read_write_failed(&std::io::Error::other("boom")).message(),
            "failed to write to output"
        );
        assert_eq!(
            eval_read_needs_stdin().message(),
            "READ requires stdin (use WITH_IO [stdin=...] READ)"
        );
        assert_eq!(
            eval_read_line_needs_stdin().message(),
            "READ_LINE requires stdin (use WITH_IO [stdin=...] READ_LINE $var)"
        );
        assert_eq!(
            eval_read_line_not_utf8(&String::from_utf8(vec![0xff]).unwrap_err()).message(),
            "READ_LINE received non-UTF8 bytes"
        );
        assert_eq!(
            eval_write_resolve_failed("f", &anyhow::anyhow!("boom")).message(),
            "WRITE f"
        );
        assert_eq!(
            eval_ensure_parent_failed("/x", &anyhow::anyhow!("boom")).message(),
            "failed to create parent for /x"
        );
        assert_eq!(
            eval_write_file_failed("/x", &anyhow::anyhow!("boom")).message(),
            "failed to write /x"
        );
        assert_eq!(
            eval_write_needs_stdin("f").message(),
            "WRITE f requires stdin (use WITH_IO [stdin=...] WRITE)"
        );
        assert_eq!(
            eval_write_open_failed("/x", &anyhow::anyhow!("boom")).message(),
            "failed to open /x for writing"
        );
        assert_eq!(
            eval_write_stream_failed("/x", &anyhow::anyhow!("boom")).message(),
            "failed to write to /x"
        );
        assert_eq!(
            eval_append_resolve_failed("f", &anyhow::anyhow!("boom")).message(),
            "APPEND f"
        );
        assert_eq!(
            eval_append_failed("/x", &anyhow::anyhow!("boom")).message(),
            "failed to append to /x"
        );
        assert_eq!(
            eval_append_needs_stdin("f").message(),
            "APPEND f requires stdin (use WITH_IO [stdin=...] APPEND)"
        );
        assert_eq!(
            eval_append_open_failed("/x", &anyhow::anyhow!("boom")).message(),
            "failed to open /x for appending"
        );
        assert!(
            eval_run_exit_status("echo hi", &exit_status_sample())
                .message()
                .starts_with("RUN echo hi exited with status ")
        );
        assert!(
            eval_run_argv_exit_status(&["echo".to_string()], &exit_status_sample())
                .message()
                .starts_with("RUN [\"echo\"] exited with status ")
        );
        assert!(
            eval_async_exit_status(None, &exit_status_sample())
                .message()
                .starts_with("ASYNC process exited with status ")
        );
        assert!(
            eval_async_exit_status(Some(7), &exit_status_sample())
                .message()
                .starts_with("named ASYNC task 7 exited with status ")
        );
        assert_eq!(
            eval_arg_expr_needs_step_ctx(&Expr::Env("X".to_string())).message(),
            "Arg::Expr cannot be resolved without StepCtx: Env(\"X\")"
        );
        assert_eq!(
            eval_arg_expand_failed(&anyhow::anyhow!("undefined variable `$x`")).message(),
            "undefined variable `$x`"
        );
        assert_eq!(
            coerce_type_mismatch(
                "coercion",
                &TypeTag::Int,
                &crate::Value::string("x".to_string()),
            )
            .message(),
            "coercion: TypeMismatch: expected INT, got STRING"
        );
        assert_eq!(
            coerce_string_to_pipe("x").message(),
            "TypeMismatch: expected PIPE, got STRING (\"x\"); declare LET $x: PIPE and pass $x"
        );
        assert_eq!(
            coerce_pipe_to_string().message(),
            "coercion: TypeMismatch: expected STRING, got PIPE: pipes carry streams between steps, not text. Capture the producing step's output with LET $var: STRING = ... instead."
        );
        assert_eq!(coerce_check_failed(&anyhow::anyhow!("x")).message(), "x");
        assert_eq!(
            eval_host_return_failed(&anyhow::anyhow!("x")).message(),
            "x"
        );
        assert_eq!(
            eval_scope_underflow().message(),
            "scope stack underflow during pop"
        );
        assert_eq!(
            eval_redeclare_var("x").message(),
            "redeclaration error: $x already declared in this scope; use $x = ... to mutate"
        );
        assert_eq!(
            eval_redeclare_const("x").message(),
            "redeclaration error: x already declared in this scope; CONST bindings are immutable and cannot redeclare it"
        );
        assert_eq!(
            eval_no_var_scope().message(),
            "no variable scope for declaration"
        );
        assert_eq!(
            eval_const_mutation("$x").message(),
            "cannot mutate CONST $x: CONST bindings are deeply immutable"
        );
        assert_eq!(
            eval_undeclared_declare("x", "TYPE").message(),
            "undeclared variable $x: declare it first with LET $x: TYPE = ..."
        );
        assert_eq!(eval_undeclared("x").message(), "undeclared variable $x");
        assert_eq!(
            eval_list_not_list("x", "STRING").message(),
            "LIST_APPEND $x: variable is STRING, not LIST"
        );
        assert_eq!(
            eval_list_value_not_list("x").message(),
            "LIST_APPEND $x: variable is not a LIST value"
        );
        assert_eq!(
            eval_list_element_failed(&anyhow::anyhow!("x")).message(),
            "x"
        );
        assert_eq!(
            eval_scratch_materialize_failed(&anyhow::anyhow!("x")).message(),
            "x"
        );
        assert_eq!(
            eval_task_cap_exceeded(64).message(),
            "concurrency limit exceeded: maximum of 64 running background tasks allowed"
        );
        assert_eq!(
            eval_const_arg_keypath().message(),
            "key paths on $ALIAS::NAME are not supported; copy to a local with LET or CONST first"
        );
        assert_eq!(
            eval_const_arg_bare_sigil("foo").message(),
            "bare `foo` never reads a binding here; use `$foo`"
        );
        assert_eq!(
            eval_template_missing_key("foo", false).message(),
            "undefined script variable: 'foo'; did you mean '$foo'?"
        );
        assert_eq!(
            eval_template_missing_key("foo", true).message(),
            "undefined script variable: 'foo'; did you mean 'env:foo' (environment variable)?"
        );
        assert_eq!(
            eval_template_empty_placeholder().message(),
            "empty template placeholder"
        );
        assert_eq!(
            eval_template_invalid_key("foo").message(),
            "invalid placeholder key: '$foo'"
        );
        assert_eq!(
            eval_template_empty_env().message(),
            "undefined environment variable: ''"
        );
        assert_eq!(
            eval_template_keypath().message(),
            "key paths on $ALIAS::NAME are not supported; copy to a local with LET or CONST first"
        );
        assert_eq!(
            eval_template_bare_sigil("foo").message(),
            "bare `foo` never reads a binding here; use `$foo`"
        );
        assert_eq!(
            eval_template_placeholder_miss("unknown placeholder function `M::F`").message(),
            "unknown placeholder function `M::F`"
        );
        assert_eq!(
            std_pipe_read_failed("TEXT_REPLACE", &anyhow::anyhow!("denied")).message(),
            "TEXT_REPLACE cannot read PIPE input: denied"
        );
        assert_eq!(
            std_pipe_create_failed("TEXT_REPLACE", &anyhow::anyhow!("denied")).message(),
            "TEXT_REPLACE cannot create PIPE output: denied"
        );
        assert_eq!(
            std_pipe_spawn_failed("TEXT_REPLACE", &std::io::Error::other("denied"),).message(),
            "TEXT_REPLACE cannot spawn pump thread: denied"
        );
        assert_eq!(
            std_pipe_reader_lock_poisoned("TEXT_REPLACE").message(),
            "TEXT_REPLACE pipe reader lock poisoned"
        );
        assert_eq!(
            std_pipe_writer_lock_poisoned("TEXT_REPLACE").message(),
            "TEXT_REPLACE pipe writer lock poisoned"
        );
        assert_eq!(
            std_pipe_handle_spent().message(),
            "OS pipe handle has already been consumed by another binding; declare a fresh LET $x: PIPE for a new session"
        );
        assert_eq!(
            std_pipe_needs_pipe("TEXT_REPLACE", "STRING").message(),
            "host TEXT_REPLACE needs a PIPE value, got STRING"
        );
        assert_eq!(
            std_pipe_close_needs_script().message(),
            "host close_pipe needs a script-materialized pipe (unbound and OS handles cannot be force-closed)"
        );
        assert_eq!(
            std_assert_lock_poisoned("TEXT_REPLACE").message(),
            "TEXT_REPLACE poisoned"
        );
        assert_eq!(
            std_pipe_chunk_read_failed("TEXT_REPLACE", &std::io::Error::other("denied"),).message(),
            "TEXT_REPLACE cannot read PIPE input: denied"
        );
        assert_eq!(
            std_pipe_input_not_utf8("TEXT_REPLACE").message(),
            "TEXT_REPLACE PIPE input is not valid UTF-8"
        );
        assert_eq!(
            std_pipe_spill_failed("TEXT_REPLACE", &std::io::Error::other("denied"),).message(),
            "TEXT_REPLACE PIPE input exceeds the spill backlog: denied"
        );
        assert_eq!(
            std_pipe_write_failed("TEXT_REPLACE", &std::io::Error::other("denied"),).message(),
            "TEXT_REPLACE cannot write PIPE output: denied"
        );
        assert_eq!(
            eval_compare_equality_shape().message(),
            "internal error: equality shape"
        );
        assert_eq!(
            eval_ordering_needs_numeric(
                &crate::Value::string("x".to_string()),
                &crate::Value::string("y".to_string()),
            )
            .message(),
            "Type Error: ordering comparison requires Int or Float, found STRING(\"x\") and STRING(\"y\")"
        );
        assert_eq!(
            eval_unstaged_int_boundary(9223372036854775808).message(),
            "internal error: unstaged integer boundary 9223372036854775808"
        );
        assert_eq!(
            eval_undefined_var("nope").message(),
            "undefined variable $nope"
        );
        assert_eq!(
            eval_key_not_found("k").message(),
            "Key 'k' not found in map"
        );
        assert_eq!(
            eval_invalid_array_index("k").message(),
            "Invalid array index 'k'"
        );
        assert_eq!(
            eval_index_out_of_bounds(7).message(),
            "Index 7 out of bounds"
        );
        assert_eq!(
            eval_traverse_scalar("k").message(),
            "Cannot traverse into scalar value at key 'k'"
        );
        assert_eq!(
            eval_path_type_needs_arg().message(),
            "PATH_TYPE requires a path argument"
        );
        assert_eq!(
            eval_path_type_needs_string().message(),
            "PATH_TYPE path argument must evaluate to a string"
        );
        assert_eq!(
            eval_is_exec_needs_arg().message(),
            "IS_EXEC requires a name argument"
        );
        assert_eq!(
            eval_is_exec_needs_string().message(),
            "IS_EXEC name argument must evaluate to a string"
        );
        assert_eq!(
            eval_path_writable_needs_arg().message(),
            "PATH_WRITABLE requires a path argument"
        );
        assert_eq!(
            eval_path_writable_needs_string().message(),
            "PATH_WRITABLE path argument must evaluate to a string"
        );
        assert_eq!(
            eval_path_resolve_failed("p", &anyhow::anyhow!("denied")).message(),
            "failed to resolve path 'p': denied"
        );
        assert_eq!(
            eval_workdir_resolve_failed(&anyhow::anyhow!("denied")).message(),
            "failed to resolve working directory: denied"
        );
        assert!(
            std_text_invalid_pattern("TEXT_MATCH", "[", &regex_err_sample())
                .message()
                .starts_with("TEXT_MATCH invalid pattern \"[\": "),
            "pattern failure must keep its prefix"
        );
        assert_eq!(
            std_text_needs_search("TEXT_REPLACE").message(),
            "TEXT_REPLACE needs a non-empty search string"
        );
        assert_eq!(
            std_text_needs_needle("TEXT_COUNT").message(),
            "TEXT_COUNT needs a non-empty needle"
        );
        assert_eq!(
            std_text_needs_one_based_start("TEXT_LINES_RANGE", 0).message(),
            "TEXT_LINES_RANGE needs a 1-based start line, got 0"
        );
        assert_eq!(
            std_text_needs_ordered_range("TEXT_LINES_RANGE", 3, 2).message(),
            "TEXT_LINES_RANGE needs start <= end, got 3..=2"
        );
        assert_eq!(
            std_pipe_needs_step_io("TEXT_REPLACE").message(),
            "TEXT_REPLACE on PIPE input needs step IO, unavailable to placeholder calls; pass a STRING there"
        );
        assert_eq!(
            std_bool_cannot_coerce("BOOL_OR", "x").message(),
            "BOOL_OR cannot coerce \"x\" to BOOL"
        );
        assert_eq!(
            std_contains_haystack_type("INT").message(),
            "CONTAINS needs a STRING or LIST haystack, found `INT`"
        );
        assert_eq!(
            std_to_json_non_finite_float(f64::INFINITY).message(),
            "TO_JSON cannot encode non-finite float inf"
        );
        assert_eq!(
            std_to_json_unencodable("VOID").message(),
            "TO_JSON cannot encode VOID values; use STRING, INT, FLOAT, BOOL, LIST, or MAP"
        );
        assert!(
            std_to_json_encode_failed(
                &serde_json::from_str::<serde_json::Value>("{bad").unwrap_err(),
            )
            .message()
            .starts_with("encode JSON: "),
            "json encode failure must keep its prefix"
        );
        assert_eq!(
            std_load_toml_needs_arg().message(),
            "LOAD_TOML requires a path argument"
        );
        assert_eq!(
            std_load_toml_needs_string().message(),
            "LOAD_TOML path argument must evaluate to a string"
        );
        assert_eq!(
            std_load_toml_resolve_failed("p", &anyhow::anyhow!("denied")).message(),
            "failed to resolve TOML path 'p': denied"
        );
        assert_eq!(
            std_load_toml_read_failed("p", &anyhow::anyhow!("denied")).message(),
            "failed to read TOML file 'p': denied"
        );
        assert_eq!(
            std_load_toml_not_utf8("p", &utf8_err_sample()).message(),
            "invalid UTF-8 in TOML file 'p': invalid utf-8 sequence of 1 bytes from index 0"
        );
        assert_eq!(
            std_parse_toml_needs_string("INT").message(),
            "PARSE_TOML expects a STRING, got INT"
        );
        assert_eq!(
            std_load_json_needs_arg().message(),
            "LOAD_JSON requires a path argument"
        );
        assert_eq!(
            std_load_json_needs_string().message(),
            "LOAD_JSON path argument must evaluate to a string"
        );
        assert_eq!(
            std_load_json_resolve_failed("p", &anyhow::anyhow!("denied")).message(),
            "failed to resolve JSON path 'p': denied"
        );
        assert_eq!(
            std_load_json_read_failed("p", &anyhow::anyhow!("denied")).message(),
            "failed to read JSON file 'p': denied"
        );
        assert_eq!(
            std_load_json_not_utf8("p", &utf8_err_sample()).message(),
            "invalid UTF-8 in JSON file 'p': invalid utf-8 sequence of 1 bytes from index 0"
        );
        assert_eq!(
            std_parse_json_needs_string("INT").message(),
            "PARSE_JSON expects a STRING, got INT"
        );
        assert_eq!(
            std_glob_needs_arg().message(),
            "GLOB requires a pattern argument"
        );
        assert_eq!(
            std_glob_needs_string().message(),
            "GLOB pattern argument must evaluate to a string"
        );
        assert_eq!(
            std_glob_resolve_failed(&anyhow::anyhow!("denied")).message(),
            "denied"
        );
        assert_eq!(
            std_glob_list_failed(&std::io::Error::other("denied")).message(),
            "denied"
        );
        assert_eq!(
            std_has_key_needs_map("INT").message(),
            "HAS_KEY expects a MAP as its first argument, got INT"
        );
        assert_eq!(
            std_map_set_needs_map("INT").message(),
            "MAP_SET expects a MAP as its first argument, got INT"
        );
        assert_eq!(
            std_map_set_duplicate("k").message(),
            "MAP_SET duplicate key 'k'"
        );
        assert_eq!(
            std_merge_maps_needs_list("INT").message(),
            "MERGE_MAPS expects a LIST of MAPs, got INT"
        );
        assert_eq!(
            std_merge_maps_needs_map(1, "INT").message(),
            "MERGE_MAPS element 1 is INT, not MAP"
        );
        assert_eq!(
            std_merge_maps_duplicate("k").message(),
            "duplicate values key 'k'; later files must not repeat it under fail_on_duplicate"
        );
        assert!(
            std_json_parse_failed(&serde_json::from_str::<serde_json::Value>("{bad").unwrap_err())
                .message()
                .starts_with("JSON parse error: "),
            "json failure must keep its prefix"
        );
        assert!(
            std_toml_parse_failed(&toml::from_str::<serde_json::Value>("a = ").unwrap_err())
                .message()
                .starts_with("TOML parse error: "),
            "toml failure must keep its prefix"
        );
        assert_eq!(
            std_base64_encode_needs_string("INT").message(),
            "BASE64_ENCODE expects a STRING, got INT"
        );
        assert_eq!(
            std_base64_decode_needs_string("INT").message(),
            "BASE64_DECODE expects a STRING, got INT"
        );
        assert_eq!(
            std_base64_decode_failed("!!!", &base64::DecodeError::InvalidByte(0, b'!')).message(),
            "BASE64_DECODE cannot decode \"!!!\": Invalid symbol 33, offset 0."
        );
        assert_eq!(
            json_handle_needs_object(&serde_json::json!(7)).message(),
            "HANDLE argument must be an object like {\"task_id\": 7}, got 7"
        );
        assert_eq!(
            std_base64_decode_not_utf8().message(),
            "BASE64_DECODE decoded bytes are not UTF-8"
        );
        assert_eq!(
            json_handle_exact_shape().message(),
            "HANDLE argument must be exactly {\"task_id\": N} with no other keys"
        );
        assert_eq!(
            json_handle_task_id_positive().message(),
            "HANDLE argument `task_id` must be a positive integer"
        );
        assert_eq!(
            json_handle_encode_needs_handle("STRING").message(),
            "cannot encode STRING as HANDLE; expected a task handle"
        );
    }

    #[test]
    fn check_messages_match_canonical_wording() {
        assert_eq!(
            import_not_in_closure("f").message(),
            "IMPORT `f` cannot be read: not in the import closure"
        );
        assert_eq!(
            import_export_type_mismatch("f", "T").message(),
            "IMPORT `f` EXPORT TYPE `T` does not match any top-level TYPE in that file"
        );
        assert_eq!(
            check_var_tag("v", &anyhow::anyhow!("boom")).message(),
            "variable `$v`: boom"
        );
        assert_eq!(
            check_func_tag("F", &anyhow::anyhow!("boom")).message(),
            "function `F`: boom"
        );
        assert_eq!(
            check_step_wrap(3, &anyhow::anyhow!("boom")).message(),
            "step 3: boom"
        );
        assert_eq!(
            check_ctx_wrap("ctx", &anyhow::anyhow!("boom")).message(),
            "ctx: boom"
        );
        assert_eq!(
            check_exact_mismatch("ctx", &TypeTag::Int, &TypeTag::String).message(),
            "ctx: TypeMismatch: expected `INT`, got `STRING`"
        );
        assert_eq!(
            check_cannot_unify("ctx", &TypeTag::Int, &TypeTag::String).message(),
            "ctx: cannot unify `INT` and `STRING`"
        );
        assert_eq!(
            check_undefined_var("step 1: ", "x").message(),
            "step 1: undefined variable `$x`"
        );
        assert_eq!(
            check_var_not_guaranteed("x").message(),
            "variable `$x` is not guaranteed to be defined on all reachable paths for this step"
        );
        assert_eq!(
            check_unknown_field("step 1: ", "a", &[]).message(),
            "step 1: unknown field `a`; expected fields: "
        );
        assert_eq!(
            check_invalid_array_index("", "0").message(),
            "invalid array index `0`"
        );
        assert_eq!(
            check_cannot_traverse("", "k").message(),
            "cannot traverse into scalar value at key `k`"
        );
        assert_eq!(
            check_duplicate_func("F", "f.oxdock").message(),
            "duplicate function `F` from `f.oxdock` collides across imports"
        );
        assert_eq!(
            check_redeclare_let(2, "x").message(),
            "step 2: redeclaration error: `$x` already declared in this scope; use `$x` = ... to mutate"
        );
        assert_eq!(
            check_redeclare_const(2, "x").message(),
            "step 2: redeclaration error: `x` is a CONST binding and cannot be redeclared; CONST and LET share one namespace"
        );
        assert_eq!(
            check_undeclared_env("step 1", "top", "env:", "K").message(),
            "step 1 in top: uses undeclared environment variable `env:K`: declare it first with ENV or INHERIT_ENV"
        );
        assert_eq!(
            check_placeholder_keypath().message(),
            "key paths on $ALIAS::NAME are not supported; copy to a local with LET or CONST first"
        );
        assert_eq!(
            check_empty_placeholder_var().message(),
            "invalid placeholder variable: empty variable reference"
        );
        assert!(check_bare_sigil("$x").message().contains("`$x`"));
        assert_eq!(
            check_undefined_script_var("k").message(),
            "undefined script variable: `k`"
        );
        assert_eq!(
            check_placeholder_unknown("A::F").message(),
            "unknown placeholder function `A::F`"
        );
        assert_eq!(
            check_template_arity_exact("step 1", "top", "F", 1, 2).message(),
            "step 1 in top: F() expects 1 argument(s), got 2"
        );
        assert_eq!(
            check_template_arity_range("step 1", "top", "F", 1, 2, 3).message(),
            "step 1 in top: F() expects 1 to 2 argument(s), got 3"
        );
        assert_eq!(
            check_template_arg_allowed("step 1", "top", "F", "p", &["a"], "b").message(),
            "step 1 in top: F() argument `$p` must be one of: a, got \"b\""
        );
        assert!(
            check_template_arg_depth()
                .message()
                .contains("nests too deep")
        );
        assert_eq!(
            check_empty_placeholder_arg().message(),
            "invalid placeholder call argument: empty variable reference"
        );
        assert_eq!(
            check_invalid_placeholder_arg("x").message(),
            "invalid placeholder call argument `x`"
        );
        assert_eq!(
            check_invalid_map_entry("e").message(),
            "invalid map literal entry `e`: use key: value"
        );
        assert_eq!(
            check_duplicate_map_key("k").message(),
            "duplicate key `k` in map literal"
        );
        assert_eq!(check_void_no_value().message(), "VOID has no value to pass");
        assert_eq!(
            check_redeclare_const(2, "x").message(),
            "step 2: redeclaration error: `x` is a CONST binding and cannot be redeclared; CONST and LET share one namespace"
        );
        assert_eq!(
            check_capture_mismatch(1, "LET", "x", &TypeTag::String, &TypeTag::Int).message(),
            "step 1: LET `$x` captures STRING but declares INT"
        );
        assert_eq!(
            check_flow_outside_loop(1, "CONTINUE").message(),
            "step 1: CONTINUE outside loop"
        );
        assert_eq!(
            check_flow_crosses_func(1, "BREAK", "F").message(),
            "step 1: BREAK cannot cross function boundary (in F())"
        );
        assert_eq!(
            check_unknown_function("F", " (did you mean G?)").message(),
            "unknown function `F` (did you mean G?)"
        );
        assert_eq!(
            check_call_arity_exact(1, "F", 1, 2).message(),
            "step 1: F() expects 1 argument(s), got 2"
        );
        assert_eq!(
            check_arithmetic_type("step 1: ").message(),
            "step 1: Type Error: arithmetic requires Int or Float"
        );
        assert_eq!(
            check_rpn_underflow().message(),
            "internal error: RPN stack underflow"
        );
        assert_eq!(
            check_block_fallthrough("step 1").message(),
            "step 1: block may fall through with no RETURN on some path"
        );
    }

    #[test]
    fn mcp_messages_match_canonical_wording() {
        assert_eq!(
            mcp_export_type_unknown("T").message(),
            "EXPORT TYPE `T` matches no local TYPE and no imported file exports it"
        );
        assert_eq!(
            mcp_break_outside_loop(3).message(),
            "step 3: BREAK outside loop"
        );
        assert_eq!(
            mcp_continue_outside_loop(3).message(),
            "step 3: CONTINUE outside loop"
        );
        assert_eq!(
            mcp_return_outside_function(3).message(),
            "step 3: RETURN outside function, ASYNC task, or LET block"
        );
        assert_eq!(mcp_unknown_tool("FOO").message(), "unknown MCP tool `FOO`");
        let err = anyhow::anyhow!("boom");
        assert_eq!(mcp_call_failed(&err, "", "").message(), "boom");
        assert_eq!(
            mcp_call_failed(&err, "out", "oops").message(),
            "boom\n--- captured stdout ---\nout\n--- captured stderr ---\noops"
        );
        // Long tails truncate to the last 4096 chars.
        let big = "x".repeat(5000);
        let d = mcp_call_failed(&err, &big, "");
        assert!(
            d.message().ends_with(&"x".repeat(4096)),
            "{}",
            d.message().len()
        );
        assert!(!d.message().contains(&"x".repeat(4097)));
    }

    #[test]
    fn import_messages_match_canonical_wording() {
        assert_eq!(
            import_cycle(&["a".to_string(), "b".to_string()]).message(),
            "IMPORT cycle detected: a -> b"
        );
        assert_eq!(
            import_depth_limit(8, "f").message(),
            "IMPORT depth limit exceeded (8): `f`"
        );
        assert!(import_not_found("f").message().contains("not found"));
        assert!(
            import_cannot_resolve("f", &anyhow::anyhow!("root"))
                .message()
                .contains("cannot be resolved")
        );
        assert!(
            import_cannot_read("f", &anyhow::anyhow!("root"))
                .message()
                .contains("cannot be read")
        );
        assert!(
            import_bad_utf8("f", &anyhow::anyhow!("root"))
                .message()
                .contains("not valid UTF-8")
        );
        assert!(
            import_parse_failed("f", &anyhow::anyhow!("root"))
                .message()
                .contains("failed to parse")
        );
        assert_eq!(
            import_missing_export("f", "TYPE", "T").message(),
            "IMPORT `f` does not export TYPE `T`"
        );
        assert_eq!(
            import_missing_export("f", "FUNC", "g").message(),
            "IMPORT `f` does not export `g`"
        );
        assert_eq!(
            import_unknown_member("f", "function", "g").message(),
            "IMPORT `f` has no such function `g`"
        );
        assert_eq!(
            import_unknown_type("f", "T", None).message(),
            "IMPORT `f` has no such TYPE `T`"
        );
        assert!(
            import_unknown_type("f", "T", Some(&anyhow::anyhow!("root")))
                .message()
                .contains(": root")
        );
        assert!(
            import_failed_validation("f", &anyhow::anyhow!("root"))
                .message()
                .contains("failed validation")
        );
        assert_eq!(
            import_stateful_harvest("f", "g").message(),
            "IMPORT `f` cannot harvest stateful function `g`: import the file"
        );
        assert_eq!(
            import_unknown_function("f", "g").message(),
            "IMPORT `f` has no such function `g`"
        );
        assert!(
            import_type_collision("f", "T", "a", "b")
                .message()
                .contains("already defined with target `a`, new target `b`")
        );
        assert_eq!(
            import_const_no_alias("q", "A").message(),
            "unknown CONST `q`: no import alias `A` exports that name"
        );
        assert_eq!(
            import_const_hinted("q", "h").message(),
            "unknown CONST `q`: h"
        );
        assert_eq!(import_const_unknown("q").message(), "unknown CONST `q`");
        assert!(
            import_const_uninitialized("q")
                .message()
                .contains("never initialized")
        );
        assert!(import_const_is_let("q").message().contains("LET binding"));
        assert_eq!(
            import_const_forward_failed(&anyhow::anyhow!("unknown CONST `U::N`")).message(),
            "unknown CONST `U::N`"
        );
        assert_eq!(
            import_flow_crosses_boundary("BREAK", 3, "f").message(),
            "step 3: BREAK cannot cross import boundary (in IMPORT `f`)"
        );
    }
}
