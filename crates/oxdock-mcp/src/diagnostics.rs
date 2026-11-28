//! MCP user-facing errors behind single constructors with stable keys.
//!
//! This module owns every MCP template and key (`mcp.*`). It builds on
//! the generic [`oxdock_diagnostics::Diagnostic`] transport but owns its
//! wording: changing MCP text happens here, never at the call sites.
//! Call sites pass structured data only, never `format!` strings.

use oxdock_diagnostics::{Diagnostic, flatten_chain};
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

/// Session filesystem guard root rejected. Takes the underlying
/// failure and renders its chain inline.
pub fn session_guard_workspace_root(reason: &Diagnostic) -> Diagnostic {
    let reason_text = flatten_chain(reason);
    let message = format!("guard workspace root: {reason_text}");
    Diagnostic::new(
        "mcp.session.guard_workspace_root",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// Script-read guard root rejected. Takes the underlying failure and
/// renders its chain inline.
pub fn session_guard_script_root(reason: &Diagnostic) -> Diagnostic {
    let reason_text = flatten_chain(reason);
    let message = format!("guard script root: {reason_text}");
    Diagnostic::new(
        "mcp.session.guard_script_root",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// Script text failed to parse during MCP load. Takes the underlying
/// failure and renders its chain inline.
pub fn session_script_parse_failed(reason: &Diagnostic) -> Diagnostic {
    let reason_text = flatten_chain(reason);
    let message = format!("MCP script parse failed: {reason_text}");
    Diagnostic::new(
        "mcp.session.script_parse_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// Script file failed to read during MCP load. Takes the underlying
/// failure and renders its chain inline.
pub fn session_script_read_failed(reason: &Diagnostic) -> Diagnostic {
    let reason_text = flatten_chain(reason);
    let message = format!("MCP script read failed: {reason_text}");
    Diagnostic::new(
        "mcp.session.script_read_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
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
pub fn stdio_serve_failed(reason: &Diagnostic) -> Diagnostic {
    let reason_text = flatten_chain(reason);
    let message = format!("MCP stdio serve failed: {reason_text}");
    Diagnostic::new(
        "mcp.serve.stdio_serve_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// Served stdio session ended in failure. Takes the underlying
/// failure and renders its chain inline.
pub fn stdio_service_failed(reason: &Diagnostic) -> Diagnostic {
    let reason_text = flatten_chain(reason);
    let message = format!("MCP stdio service failed: {reason_text}");
    Diagnostic::new(
        "mcp.serve.stdio_service_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// Tokio runtime for stdio serving failed to build. Takes the
/// underlying failure and renders its chain inline.
pub fn serve_runtime_build_failed(reason: &Diagnostic) -> Diagnostic {
    let reason_text = flatten_chain(reason);
    let message = format!("build MCP tokio runtime: {reason_text}");
    Diagnostic::new(
        "mcp.serve.runtime_build_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// Generic MCP-operation failure. Takes the underlying failure; the
/// flattened chain lives here so call sites stay `format!`-free.
/// Already-keyed payloads are reclaimed by [`from_anyhow`], so this
/// only names genuinely unkeyed fallout. Message is the reason
/// verbatim so reclaimed chains render byte-identical.
pub fn operation_failed(reason: &Diagnostic) -> Diagnostic {
    operation_failed_fallback(&flatten_chain(reason))
}

/// Unkeyed MCP-operation fallback. Takes the pre-rendered anyhow
/// chain text verbatim; rendering lives here. Single owner for the
/// `from_anyhow` fallback so the anyhow-to-Diagnostic direction
/// keeps one keyed home.
pub fn operation_failed_fallback(reason_text: &str) -> Diagnostic {
    Diagnostic::new(
        "mcp.operation.failed",
        reason_text.to_string(),
        Diagnostic::make_fields(&[(FIELD_REASON, reason_text)]),
    )
}

/// Reclaim an already-keyed diagnostic from an `anyhow` failure,
/// wrapping anything else as a generic MCP-operation failure.
/// Single owner for the MCP-surface mapping so parser diagnostics
/// (whose `anyhow` carriers already hold keyed `dsl.*` payloads)
/// and third-party transport errors preserve keys instead of
/// restating the downcast. Outer `.context(...)` frames above the
/// keyed payload replay natively via `with_contexts`, so no chain
/// text is lost at the flip boundary. Call sites lift-then-wrap
/// (`from_anyhow` plus the specific `*_failed` ctor) so the
/// prefixed outer keeps its distinct key while staying
/// `format!`-free.
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

    #[test]
    fn each_failure_has_a_distinct_key() {
        let reason = reason_sample("boom");
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
            session_guard_workspace_root(&reason).key(),
            session_guard_script_root(&reason).key(),
            session_script_parse_failed(&reason).key(),
            session_script_read_failed(&reason).key(),
            stdio_serve_failed(&reason).key(),
            stdio_service_failed(&reason).key(),
            serve_runtime_build_failed(&reason).key(),
            operation_failed(&reason).key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn messages_match_canonical_wording() {
        let reason = reason_sample("boom");
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
        assert_eq!(
            session_guard_workspace_root(&reason).message(),
            "guard workspace root: boom"
        );
        assert_eq!(
            session_guard_script_root(&reason).message(),
            "guard script root: boom"
        );
        assert_eq!(
            session_script_parse_failed(&reason).message(),
            "MCP script parse failed: boom"
        );
        assert_eq!(
            session_script_read_failed(&reason).message(),
            "MCP script read failed: boom"
        );
        assert_eq!(
            stdio_serve_failed(&reason).message(),
            "MCP stdio serve failed: boom"
        );
        assert_eq!(
            stdio_service_failed(&reason).message(),
            "MCP stdio service failed: boom"
        );
        assert_eq!(
            serve_runtime_build_failed(&reason).message(),
            "build MCP tokio runtime: boom"
        );
        assert_eq!(operation_failed(&reason).message(), "boom");
        assert_eq!(operation_failed_fallback("boom").message(), "boom");
        assert_eq!(from_anyhow(anyhow::anyhow!("boom")).message(), "boom");
        assert_eq!(
            from_anyhow(anyhow::anyhow!("boom")).key(),
            "mcp.operation.failed"
        );
        let keyed = export_no_entries();
        let keyed_key = keyed.key();
        let reclaimed = from_anyhow(anyhow::Error::new(keyed));
        assert_eq!(reclaimed.key(), keyed_key);
        let framed = export_no_entries();
        let framed_key = framed.key();
        let wrapped = anyhow::Error::new(framed).context("outer frame");
        let reclaimed_framed = from_anyhow(wrapped);
        assert_eq!(reclaimed_framed.key(), framed_key);
        assert_eq!(reclaimed_framed.contexts(), &["outer frame".to_string()]);
    }
}
