//! DSL typing diagnostics, owned by `oxdock-core`.
//!
//! One constructor per diagnostic at the owning function (`typing.rs`
//! resolution and `collect_type_aliases`). Stable `dsl.*` keys,
//! structured fields, and whole-sentence templates. Call sites pass
//! decoded values, never `format!` strings; joining and quoting live
//! here.

use oxdock_diagnostics::Diagnostic;
use oxdock_parser::{ANY_TYPE_NAME, HANDLE_TYPE_NAME, LIST_TYPE_NAME, MAP_TYPE_NAME, Spelled};

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
        "bare `{MAP_TYPE_NAME}` is not a declaration type: write a shape (`{MAP_TYPE_NAME}<name: TYPE, ...>`) or `{MAP_TYPE_NAME}<{ANY_TYPE_NAME}>` for maps with unknown keys"
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
        "bare `{LIST_TYPE_NAME}` is not a declaration type: write a shape (`{LIST_TYPE_NAME}<TYPE>`) or `{LIST_TYPE_NAME}<{ANY_TYPE_NAME}>` for lists with unknown elements"
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
    let message =
        format!("`{MAP_TYPE_NAME}` fields require `name: TYPE`; `{head}` takes no bare arguments");
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
    let message =
        format!("EXPORT TYPE `{name}` matches no local TYPE and no imported file exports it");
    Diagnostic::new(
        "dsl.mcp.export_type_unknown",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// MCP linear init hit `BREAK` outside any loop. Takes the 1-based
/// step number, matching the core runtime `step N` convention.
pub fn mcp_break_outside_loop(step: usize) -> Diagnostic {
    let message = format!("step {step}: BREAK outside loop");
    Diagnostic::new(
        "dsl.mcp.break_outside_loop",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string())]),
    )
}

/// MCP linear init hit `CONTINUE` outside any loop.
pub fn mcp_continue_outside_loop(step: usize) -> Diagnostic {
    let message = format!("step {step}: CONTINUE outside loop");
    Diagnostic::new(
        "dsl.mcp.continue_outside_loop",
        message,
        Diagnostic::make_fields(&[("step", &step.to_string())]),
    )
}

/// MCP linear init hit `RETURN` outside any function, task, or block.
pub fn mcp_return_outside_function(step: usize) -> Diagnostic {
    let message = format!("step {step}: RETURN outside function, ASYNC task, or LET block");
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
    let message = format!("IMPORT cycle detected: {joined}");
    Diagnostic::new(
        "dsl.import.cycle",
        message,
        Diagnostic::make_fields(&[("chain", &joined)]),
    )
}

/// IMPORT nesting deeper than the cap. Takes the cap and the path
/// that exceeded it.
pub fn import_depth_limit(max: usize, path: &str) -> Diagnostic {
    let message = format!("IMPORT depth limit exceeded ({max}): `{path}`");
    Diagnostic::new(
        "dsl.import.depth_limit",
        message,
        Diagnostic::make_fields(&[("max", &max.to_string()), ("path", path)]),
    )
}

/// IMPORT naming a file that does not exist at run start.
pub fn import_not_found(path: &str) -> Diagnostic {
    let message = format!(
        "IMPORT `{path}` not found: the file must exist when the run starts (generated imports are not supported)"
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
    let message = format!("IMPORT `{path}` cannot be resolved: {reason_text}");
    Diagnostic::new(
        "dsl.import.cannot_resolve",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// IMPORT file that cannot be read.
pub fn import_cannot_read(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("IMPORT `{path}` cannot be read: {reason_text}");
    Diagnostic::new(
        "dsl.import.cannot_read",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// IMPORT file that is not valid UTF-8.
pub fn import_bad_utf8(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("IMPORT `{path}` is not valid UTF-8: {reason_text}");
    Diagnostic::new(
        "dsl.import.bad_utf8",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// IMPORT file that fails to parse.
pub fn import_parse_failed(path: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("IMPORT `{path}` failed to parse: {reason_text}");
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
    let what = if kind == "FUNC" {
        String::new()
    } else {
        format!("{kind} ")
    };
    let message = format!("IMPORT `{path}` does not export {what}`{name}`");
    Diagnostic::new(
        "dsl.import.missing_export",
        message,
        Diagnostic::make_fields(&[("path", path), ("kind", kind), ("name", name)]),
    )
}

/// IMPORT file with no such member. Takes the noun as rendered
/// (`function`/`TYPE`/`CONST`).
pub fn import_unknown_member(path: &str, noun: &str, name: &str) -> Diagnostic {
    let message = format!("IMPORT `{path}` has no such {noun} `{name}`");
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
        Some(reason) => format!("IMPORT `{path}` has no such TYPE `{name}`: {reason:#}"),
        None => format!("IMPORT `{path}` has no such TYPE `{name}`"),
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
    let message = format!("IMPORT `{path}` failed validation: {reason_text}");
    Diagnostic::new(
        "dsl.import.failed_validation",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    )
}

/// IMPORT harvesting a stateful function (the file body never runs,
/// so there is no initialized state to observe).
pub fn import_stateful_harvest(path: &str, offender: &str) -> Diagnostic {
    let message =
        format!("IMPORT `{path}` cannot harvest stateful function `{offender}`: import the file");
    Diagnostic::new(
        "dsl.import.stateful_harvest",
        message,
        Diagnostic::make_fields(&[("path", path), ("offender", offender)]),
    )
}

/// IMPORT file with no such function.
pub fn import_unknown_function(path: &str, name: &str) -> Diagnostic {
    let message = format!("IMPORT `{path}` has no such function `{name}`");
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
        "IMPORT `{path}` TYPE `{name}` collides: already defined with target `{existing}`, new target `{target}`"
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
    let message =
        format!("unknown CONST `{qualified}`: no import alias `{alias}` exports that name");
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
    let message =
        super::native::ambiguous_alias_message(alias, name, files).replacen("function", "CONST", 1);
    Diagnostic::new(
        "dsl.import.const_ambiguous",
        message,
        Diagnostic::make_fields(&[("alias", alias), ("name", name)]),
    )
}

/// `unknown CONST` with a resolution hint (unexported `CONST` or
/// private `LET` under the alias).
pub fn import_const_hinted(qualified: &str, hint: &str) -> Diagnostic {
    let message = format!("unknown CONST `{qualified}`: {hint}");
    Diagnostic::new(
        "dsl.import.const_hinted",
        message,
        Diagnostic::make_fields(&[("name", qualified), ("hint", hint)]),
    )
}

/// Bare `unknown CONST` (no holder, no terminal, no record).
pub fn import_const_unknown(qualified: &str) -> Diagnostic {
    let message = format!("unknown CONST `{qualified}`");
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
        "`{qualified}` is a LET binding and cannot be exported: only CONST bindings cross the module boundary"
    );
    Diagnostic::new(
        "dsl.import.const_is_let",
        message,
        Diagnostic::make_fields(&[("name", qualified)]),
    )
}

/// Control flow escaping an import boundary during file init. Takes
/// the flow kind (`BREAK`/`CONTINUE`), the 1-based step number, and
/// the importing path.
pub fn import_flow_crosses_boundary(flow: &str, step: usize, path: &str) -> Diagnostic {
    let message = format!("step {step}: {flow} cannot cross import boundary (in IMPORT `{path}`)");
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
    let message =
        format!("EXPORT `{name}` matches no defined script function or known host function");
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
    let message = format!("CONTAINS needs a STRING needle for a STRING haystack, found `{actual}`");
    Diagnostic::new(
        "dsl.std.contains_needle_type",
        message,
        Diagnostic::make_fields(&[("actual", actual)]),
    )
}

/// `CONTAINS` with a non-string, non-list haystack.
pub fn std_contains_haystack_type(actual: &str) -> Diagnostic {
    let message = format!("CONTAINS needs a STRING or LIST haystack, found `{actual}`");
    Diagnostic::new(
        "dsl.std.contains_haystack_type",
        message,
        Diagnostic::make_fields(&[("actual", actual)]),
    )
}

/// `LEN` with a non-string, non-list, non-map value.
pub fn std_len_unexpected_type(actual: &str) -> Diagnostic {
    let message = format!("LEN needs a STRING, LIST, or MAP value, got `{actual}`");
    Diagnostic::new(
        "dsl.std.len_unexpected_type",
        message,
        Diagnostic::make_fields(&[("actual", actual)]),
    )
}

/// `IS_TERMINAL` with an unexpected stream name.
pub fn std_is_terminal_expected(stream: &str) -> Diagnostic {
    let message =
        format!("IS_TERMINAL expects \"stdin\", \"stdout\", or \"stderr\", got {stream:?}");
    Diagnostic::new(
        "dsl.std.is_terminal_expected",
        message,
        Diagnostic::make_fields(&[("got", stream)]),
    )
}

/// `SEMAPHORE_NEW` with a non-positive max.
pub fn std_semaphore_needs_positive(max: i64) -> Diagnostic {
    let message = format!("SEMAPHORE_NEW() requires a positive max, got {max}");
    Diagnostic::new(
        "dsl.std.semaphore_needs_positive",
        message,
        Diagnostic::make_fields(&[("max", &max.to_string())]),
    )
}

/// `BOOL` with a non-bool, non-string value. Takes the value;
/// `Debug` rendering lives here.
pub fn std_bool_needs_bool_or_string(got: &super::Value) -> Diagnostic {
    let message = format!("BOOL needs a Bool or String, found {got:?}");
    Diagnostic::new(
        "dsl.std.bool_needs_bool_or_string",
        message,
        Diagnostic::make_fields(&[("got", &format!("{got:?}"))]),
    )
}

/// `BOOL`/`BOOL_OR` with an uncoercible string. Takes the invoking
/// command (same failure mode, one key).
pub fn std_bool_cannot_coerce(command: &str, got: &str) -> Diagnostic {
    let message = format!("{command} cannot coerce {got:?} to BOOL");
    Diagnostic::new(
        "dsl.std.bool_cannot_coerce",
        message,
        Diagnostic::make_fields(&[("command", command), ("got", got)]),
    )
}

/// `PATH_BASE` failure. Takes the underlying failure.
pub fn std_path_base_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("PATH_BASE: {reason_text}");
    Diagnostic::new(
        "dsl.std.path_base_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// `PATH_REL` failure. Takes the underlying failure.
pub fn std_path_rel_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("PATH_REL: {reason_text}");
    Diagnostic::new(
        "dsl.std.path_rel_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// `SHELL_SPLIT_ARGS` failure. Takes the underlying failure.
pub fn std_shell_split_failed(reason: &shell_words::ParseError) -> Diagnostic {
    let reason_text = format!("{reason}");
    let message = format!("SHELL_SPLIT_ARGS: {reason_text}");
    Diagnostic::new(
        "dsl.std.shell_split_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    )
}

/// `PARSE_ARGS` with other than one argument.
pub fn std_parse_args_arity(got: usize) -> Diagnostic {
    let message = format!("PARSE_ARGS() expects 1 argument, got {got}");
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
        "PARSE_ARGS flag --{flag} expects BOOL (true/false, 1/0, yes/no, on/off), got {got:?}"
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
    let message = format!("PARSE_ARGS flag --{flag} expects BOOL, got `{actual}`");
    Diagnostic::new(
        "dsl.std.parse_args_bool_type",
        message,
        Diagnostic::make_fields(&[("flag", flag), ("actual", actual)]),
    )
}

/// `PARSE_ARGS` flag without a value.
pub fn std_parse_args_needs_value(flag: &str) -> Diagnostic {
    let message = format!("PARSE_ARGS flag --{flag} needs a value: use --{flag}=<value>");
    Diagnostic::new(
        "dsl.std.parse_args_needs_value",
        message,
        Diagnostic::make_fields(&[("flag", flag)]),
    )
}

/// `PARSE_ARGS` INT flag with an unparseable value.
pub fn std_parse_args_expects_int(flag: &str) -> Diagnostic {
    let message = format!("PARSE_ARGS flag --{flag} expects INT");
    Diagnostic::new(
        "dsl.std.parse_args_expects_int",
        message,
        Diagnostic::make_fields(&[("flag", flag)]),
    )
}

/// `PARSE_ARGS` FLOAT flag with an unparseable value.
pub fn std_parse_args_expects_float(flag: &str) -> Diagnostic {
    let message = format!("PARSE_ARGS flag --{flag} expects FLOAT");
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
        "PARSE_ARGS: argument must be UTF-8".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `PARSE_ARGS` lexing failure. Takes the underlying failure.
pub fn std_parse_args_lex_error(reason: &lexopt::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("PARSE_ARGS: {reason_text}");
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
        "PARSE_ARGS schema flag names must be non-empty".to_string(),
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
    let message = format!("PARSE_ARGS duplicate flag {display}");
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
    let message = format!("PARSE_ARGS missing required flag {display}");
    Diagnostic::new(
        "dsl.std.parse_args_missing_flag",
        message,
        Diagnostic::make_fields(&[("flag", flag), ("display", &display)]),
    )
}

/// `PARSE_ARGS` with an undeclared flag.
pub fn std_parse_args_unknown_flag(flag: &str) -> Diagnostic {
    let message = format!("PARSE_ARGS unknown flag --{flag}");
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
    let message = format!("PARSE_ARGS schema type {name:?}: {reason_text}");
    Diagnostic::new(
        "dsl.std.parse_args_schema_type",
        message,
        Diagnostic::make_fields(&[("name", name), ("reason", &reason_text)]),
    )
}

/// `PARSE_ARGS` schema type that is not a MAP of scalars.
pub fn std_parse_args_schema_shape(name: &str) -> Diagnostic {
    let message = format!("PARSE_ARGS schema type {name:?} must be a MAP shape with scalar fields");
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
        "PARSE_ARGS schema for --{field}: type must be one of STRING, BOOL, INT, or FLOAT, got {actual}"
    );
    Diagnostic::new(
        "dsl.std.parse_args_schema_field_type",
        message,
        Diagnostic::make_fields(&[("field", field), ("actual", actual)]),
    )
}

/// `PARSE_FLAGS` with other than one argument.
pub fn std_parse_flags_arity(got: usize) -> Diagnostic {
    let message = format!("PARSE_FLAGS() expects 1 argument, got {got}");
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
    let message = format!("PARSE_FLAGS() does not accept positional arguments; found {joined}");
    Diagnostic::new(
        "dsl.std.parse_flags_positional",
        message,
        Diagnostic::make_fields(&[("found", &joined)]),
    )
}

/// `PARSE_POSITIONALS` with other than one argument.
pub fn std_parse_positionals_arity(got: usize) -> Diagnostic {
    let message = format!("PARSE_POSITIONALS() expects 1 argument, got {got}");
    Diagnostic::new(
        "dsl.std.parse_positionals_arity",
        message,
        Diagnostic::make_fields(&[("got", &got.to_string())]),
    )
}

/// `PARSE_BYTE_COUNT` with a negative count.
pub fn std_parse_byte_count_negative(n: i64) -> Diagnostic {
    let message = format!("PARSE_BYTE_COUNT needs a non-negative byte count, got {n}");
    Diagnostic::new(
        "dsl.std.parse_byte_count_negative",
        message,
        Diagnostic::make_fields(&[("got", &n.to_string())]),
    )
}

/// `PARSE_BYTE_COUNT` with a non-integral float.
pub fn std_parse_byte_count_integral(got: f64) -> Diagnostic {
    let message = format!("PARSE_BYTE_COUNT needs an integral byte count, got {got:?}");
    Diagnostic::new(
        "dsl.std.parse_byte_count_integral",
        message,
        Diagnostic::make_fields(&[("got", &format!("{got:?}"))]),
    )
}

/// `PARSE_BYTE_COUNT` with neither a token nor a number. Takes
/// the value; `Debug` rendering lives here.
pub fn std_parse_byte_count_shape(got: &super::Value) -> Diagnostic {
    let message = format!("PARSE_BYTE_COUNT needs a token or number, found {got:?}");
    Diagnostic::new(
        "dsl.std.parse_byte_count_shape",
        message,
        Diagnostic::make_fields(&[("got", &format!("{got:?}"))]),
    )
}

/// `PARSE_BYTE_COUNT` out of `INT` range.
pub fn std_parse_byte_count_range(s: &str) -> Diagnostic {
    let message = format!("PARSE_BYTE_COUNT out of INT range: {s:?}");
    Diagnostic::new(
        "dsl.std.parse_byte_count_range",
        message,
        Diagnostic::make_fields(&[("got", s)]),
    )
}

/// `FORMAT_BYTE_COUNT` with a negative count.
pub fn std_format_byte_count_negative(bytes: i64) -> Diagnostic {
    let message = format!("FORMAT_BYTE_COUNT needs a non-negative byte count, got {bytes}");
    Diagnostic::new(
        "dsl.std.format_byte_count_negative",
        message,
        Diagnostic::make_fields(&[("got", &bytes.to_string())]),
    )
}

/// `FORMAT_DURATION` with negative milliseconds.
pub fn std_format_duration_negative(millis: i64) -> Diagnostic {
    let message = format!("FORMAT_DURATION needs non-negative milliseconds, got {millis}");
    Diagnostic::new(
        "dsl.std.format_duration_negative",
        message,
        Diagnostic::make_fields(&[("got", &millis.to_string())]),
    )
}

/// `PARSE_DURATION` out of `INT` range.
pub fn std_parse_duration_range(text: &str) -> Diagnostic {
    let message = format!("PARSE_DURATION out of INT range: {text:?}");
    Diagnostic::new(
        "dsl.std.parse_duration_range",
        message,
        Diagnostic::make_fields(&[("got", text)]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
            import_flow_crosses_boundary("BREAK", 3, "f").key(),
            func_zero_value_missing("MAP").key(),
            func_param_type("F", &anyhow::anyhow!("x")).key(),
            func_optional_param("F", "p", &anyhow::anyhow!("x")).key(),
            func_shadow_reserved("F").key(),
            func_duplicate_in_scope("F").key(),
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
            std_parse_flags_arity(2).key(),
            std_parse_flags_positional(&["a".to_string()]).key(),
            std_parse_positionals_arity(2).key(),
            std_parse_byte_count_negative(-1).key(),
            std_parse_byte_count_integral(1.5).key(),
            std_parse_byte_count_shape(&crate::Value::string("x".to_string())).key(),
            std_parse_byte_count_range("x").key(),
            std_format_byte_count_negative(-1).key(),
            std_format_duration_negative(-1).key(),
            std_parse_duration_range("x").key(),
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
            std_bool_cannot_coerce("BOOL_OR", "x").message(),
            "BOOL_OR cannot coerce \"x\" to BOOL"
        );
        assert_eq!(
            std_contains_haystack_type("INT").message(),
            "CONTAINS needs a STRING or LIST haystack, found `INT`"
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
            import_flow_crosses_boundary("BREAK", 3, "f").message(),
            "step 3: BREAK cannot cross import boundary (in IMPORT `f`)"
        );
    }
}
