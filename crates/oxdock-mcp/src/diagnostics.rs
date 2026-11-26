//! MCP user-facing errors behind single constructors with stable keys.
//!
//! This module owns every MCP template and key (`mcp.*`). It builds on
//! the generic [`oxdock_diagnostics::Diagnostic`] transport but owns its
//! wording: changing MCP text happens here, never at the call sites.
//! Call sites pass structured data only, never `format!` strings.

use oxdock_diagnostics::Diagnostic;
use oxdock_parser::KEYWORD_EXPORT;

/// Structured field keys. Single spelling for the whole module:
/// constructors reference these, never literals, so a key rename is
/// one edit.
const FIELD_TOOL: &str = "tool";
const FIELD_PARAM: &str = "param";
const FIELD_TYPE: &str = "type";
const FIELD_GOT: &str = "got";
const FIELD_MIN: &str = "min";
const FIELD_MAX: &str = "max";
const FIELD_NAME: &str = "name";
const FIELD_REASON: &str = "reason";

/// Two exported functions share a bare MCP name. Takes the bare name;
/// quoting lives here.
pub fn tool_duplicate_name(base: &str) -> Diagnostic {
    let message = format!("duplicate MCP tool name `{base}`");
    Diagnostic::new(
        "mcp.tool.duplicate_name",
        message,
        Diagnostic::make_fields(&[(FIELD_TOOL, base)]),
    )
}

/// Exported tool whose return type has no JSON projection. Takes the
/// bare tool name; quoting lives here.
pub fn tool_opaque_return(tool: &str) -> Diagnostic {
    let message = format!("cannot expose MCP tool `{tool}` with opaque return type");
    Diagnostic::new(
        "mcp.tool.opaque_return",
        message,
        Diagnostic::make_fields(&[(FIELD_TOOL, tool)]),
    )
}

/// Exported tool with a parameter that has no JSON projection. Takes
/// the bare tool name and the parameter name; quoting lives here.
pub fn tool_opaque_param(tool: &str, param: &str) -> Diagnostic {
    let message =
        format!("cannot expose MCP tool `{tool}` with opaque parameter type for `${param}`");
    Diagnostic::new(
        "mcp.tool.opaque_param",
        message,
        Diagnostic::make_fields(&[(FIELD_TOOL, tool), (FIELD_PARAM, param)]),
    )
}

/// Nested type with no JSON projection found during schema lowering.
/// Takes the rendered type name; quoting lives here.
pub fn schema_opaque_type(type_name: &str) -> Diagnostic {
    let message = format!("cannot expose MCP tool with opaque parameter type `{type_name}`");
    Diagnostic::new(
        "mcp.schema.opaque_type",
        message,
        Diagnostic::make_fields(&[(FIELD_TYPE, type_name)]),
    )
}

/// Tool arguments arrived as a non-object JSON value. Parameterless:
/// the wording is fixed.
pub fn args_not_object() -> Diagnostic {
    Diagnostic::new(
        "mcp.args.not_object",
        "MCP tool arguments must be a JSON object".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Required tool argument missing from the JSON object. Takes the
/// parameter name; quoting lives here.
pub fn args_missing_required(param: &str) -> Diagnostic {
    let message = format!("missing required MCP argument `{param}`");
    Diagnostic::new(
        "mcp.args.missing_required",
        message,
        Diagnostic::make_fields(&[(FIELD_PARAM, param)]),
    )
}

/// Mapped argument count outside the required-to-declared range.
/// Takes counts; the range rendering lives here.
pub fn args_count_out_of_range(got: usize, min: usize, max: usize) -> Diagnostic {
    let message = format!("MCP argument count {got} out of range {min}..={max}");
    Diagnostic::new(
        "mcp.args.count_out_of_range",
        message,
        Diagnostic::make_fields(&[
            (FIELD_GOT, &got.to_string()),
            (FIELD_MIN, &min.to_string()),
            (FIELD_MAX, &max.to_string()),
        ]),
    )
}

/// Surplus key in the JSON arguments object. Takes the key; quoting
/// lives here.
pub fn args_unknown(key: &str) -> Diagnostic {
    let message = format!("unknown MCP argument `{key}`");
    Diagnostic::new(
        "mcp.args.unknown",
        message,
        Diagnostic::make_fields(&[(FIELD_GOT, key)]),
    )
}

/// `EXPORT` of the synthesized host status tool name. Takes the
/// reserved name; quoting lives here.
pub fn export_status_name_reserved(name: &str) -> Diagnostic {
    let message = format!("`{name}` is reserved for the host status tool");
    Diagnostic::new(
        "mcp.export.status_name_reserved",
        message,
        Diagnostic::make_fields(&[(FIELD_NAME, name)]),
    )
}

/// Script with no `EXPORT` entries to serve. Parameterless: the
/// `EXPORT [NAME, ...]` hint lives here.
pub fn export_no_entries() -> Diagnostic {
    Diagnostic::new(
        "mcp.export.no_entries",
        format!(
            "no {KEYWORD_EXPORT} entries: add `{KEYWORD_EXPORT} [NAME, ...]` to expose MCP tools"
        ),
        Diagnostic::make_fields(&[]),
    )
}

/// MCP session lock poisoned during hot reload. Parameterless: the
/// phase lives here.
pub fn session_lock_poisoned_reload() -> Diagnostic {
    Diagnostic::new(
        "mcp.session.lock_poisoned_reload",
        "MCP session lock poisoned during reload".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// MCP session lock poisoned during a tool call. Takes the tool name;
/// quoting lives here.
pub fn session_lock_poisoned_call(name: &str) -> Diagnostic {
    let message = format!("MCP session lock poisoned during `{name}`");
    Diagnostic::new(
        "mcp.session.lock_poisoned_call",
        message,
        Diagnostic::make_fields(&[(FIELD_NAME, name)]),
    )
}

/// Stdio transport failed to serve. Takes the underlying failure and
/// renders its chain inline.
pub fn stdio_serve_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("MCP stdio serve failed: {reason_text}");
    Diagnostic::new(
        "mcp.serve.stdio_serve_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// Served stdio session ended in failure. Takes the underlying
/// failure and renders its chain inline.
pub fn stdio_service_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("MCP stdio service failed: {reason_text}");
    Diagnostic::new(
        "mcp.serve.stdio_service_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// Tokio runtime for stdio serving failed to build. Takes the
/// underlying failure and renders its chain inline.
pub fn serve_runtime_build_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("build MCP tokio runtime: {reason_text}");
    Diagnostic::new(
        "mcp.serve.runtime_build_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_has_a_distinct_key() {
        let err = anyhow::anyhow!("boom");
        let keys = [
            tool_duplicate_name("FOO").key(),
            tool_opaque_return("FOO").key(),
            tool_opaque_param("FOO", "p").key(),
            schema_opaque_type("PIPE").key(),
            args_not_object().key(),
            args_missing_required("p").key(),
            args_count_out_of_range(3, 1, 2).key(),
            args_unknown("zzz").key(),
            export_status_name_reserved("MCP_STATUS").key(),
            export_no_entries().key(),
            session_lock_poisoned_reload().key(),
            session_lock_poisoned_call("FOO").key(),
            stdio_serve_failed(&err).key(),
            stdio_service_failed(&err).key(),
            serve_runtime_build_failed(&err).key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn messages_match_canonical_wording() {
        assert_eq!(
            tool_duplicate_name("FOO").message(),
            "duplicate MCP tool name `FOO`"
        );
        assert_eq!(
            tool_opaque_return("FOO").message(),
            "cannot expose MCP tool `FOO` with opaque return type"
        );
        assert_eq!(
            tool_opaque_param("FOO", "p").message(),
            "cannot expose MCP tool `FOO` with opaque parameter type for `$p`"
        );
        assert_eq!(
            schema_opaque_type("PIPE").message(),
            "cannot expose MCP tool with opaque parameter type `PIPE`"
        );
        assert_eq!(
            args_not_object().message(),
            "MCP tool arguments must be a JSON object"
        );
        assert_eq!(
            args_missing_required("p").message(),
            "missing required MCP argument `p`"
        );
        assert_eq!(
            args_count_out_of_range(3, 1, 2).message(),
            "MCP argument count 3 out of range 1..=2"
        );
        assert_eq!(args_unknown("zzz").message(), "unknown MCP argument `zzz`");
        assert_eq!(
            export_status_name_reserved("MCP_STATUS").message(),
            "`MCP_STATUS` is reserved for the host status tool"
        );
        assert_eq!(
            export_no_entries().message(),
            "no EXPORT entries: add `EXPORT [NAME, ...]` to expose MCP tools"
        );
        assert_eq!(
            session_lock_poisoned_reload().message(),
            "MCP session lock poisoned during reload"
        );
        assert_eq!(
            session_lock_poisoned_call("FOO").message(),
            "MCP session lock poisoned during `FOO`"
        );
        let err = anyhow::anyhow!("gone");
        assert_eq!(
            stdio_serve_failed(&err).message(),
            "MCP stdio serve failed: gone"
        );
    }
}
