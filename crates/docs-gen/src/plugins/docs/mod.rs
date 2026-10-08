//! Generic deferred placeholder expansion for document pipelines.
//!
//! `DOCS::DEFER` wraps a target domain call during pass 1 expansion,
//! minting a sentinel that `DOCS::EXPAND_DEFERRED` replaces in pass 2
//! by dispatching the full rendered document plus the stored arguments
//! to the named function. Domain functions stay immediate and
//! pass-agnostic: any `fn(document: STRING, ...args: ANY) -> STRING`
//! defers through this wrapper without sentinel logic of its own.
//! Only `crate::run` registers this module; the core language never
//! sees it.

use anyhow::{Context, Result, bail};
use oxdock_core::{HostModule, OxDockFn, StepCtx, TypeTag, Value};
use oxdock_func_macro::oxdock_func;
use oxdock_process::ProcessManager;

/// One compile-time sentinel framing, shared by mint and scan. Both
/// edges carry the same random nonce, so neither edge can match
/// arbitrary file content: only sentinels minted through `DEFER`
/// trigger replacement. The macro runs exactly once here, so the
/// minting and scanning sites agree within the built binary.
const SENTINEL: (&str, &str) = oxdock_func_macro::deferred_sentinel!("DEFER");
/// Opening delimiter for deferred sentinels (see [`SENTINEL`]).
const SENTINEL_PREFIX: &str = SENTINEL.0;
/// Closing delimiter for deferred sentinels (see [`SENTINEL`]).
const SENTINEL_SUFFIX: &str = SENTINEL.1;

/// Envelope keys in the base64url JSON payload of every sentinel.
const ENVELOPE_TARGET: &str = "target";
/// Stored positional arguments in the payload of every sentinel.
const ENVELOPE_ARGS: &str = "args";

/// Check a deferral target names a `MODULE::FUNC` entry: two nonempty
/// ASCII identifiers, so pass 2 can split and dispatch it without
/// parsing code.
fn check_target_shape(target: &str) -> Result<()> {
    let Some((module, func)) = target.split_once("::") else {
        bail!("DOCS::DEFER target must be MODULE::FUNC, got {target:?}");
    };
    for part in [module, func] {
        if part.is_empty()
            || !part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            bail!("DOCS::DEFER target must be MODULE::FUNC, got {target:?}");
        }
    }
    Ok(())
}

/// Mint one sentinel for a validated target and its remaining
/// arguments. Values cross the pass boundary through the shared
/// [`oxdock_core::value_to_json`] conversion, so only plain data
/// (STRING, INT, FLOAT, BOOL, LIST, MAP) qualifies: handles and
/// friends fail at the boundary naming their type instead of
/// rendering a silent empty. The payload is base64url JSON naming the
/// target plus its arguments: pass 2 decodes it natively without
/// parsing DSL.
fn mint_deferred(target: &str, args: &[Value]) -> Result<String> {
    use base64::Engine as _;
    check_target_shape(target)?;
    let mut arg_json = Vec::with_capacity(args.len());
    for arg in args {
        arg_json.push(oxdock_core::value_to_json(arg).with_context(|| {
            format!(
                "DOCS::DEFER args hold {} values, which cannot cross the pass boundary",
                arg.type_name()
            )
        })?);
    }
    let envelope = serde_json::json!({
        ENVELOPE_TARGET: target,
        ENVELOPE_ARGS: arg_json,
    });
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(envelope.to_string());
    Ok(format!("{SENTINEL_PREFIX}{encoded}{SENTINEL_SUFFIX}"))
}

/// One decoded sentinel: its target plus remaining arguments as
/// script values.
struct Deferred {
    target: String,
    args: Vec<Value>,
}
/// Decode one sentinel span (prefix through suffix) into its target
/// and options. Anything that fails here was never minted by
/// [`mint_deferred`]: corrupt framing, undecodable payloads, and
/// missing keys all bail instead of dispatching blind.
fn decode_deferred(sentinel: &str) -> Result<Deferred> {
    use base64::Engine as _;
    let payload = sentinel
        .strip_prefix(SENTINEL_PREFIX)
        .and_then(|rest| rest.strip_suffix(SENTINEL_SUFFIX))
        .ok_or_else(|| anyhow::anyhow!("deferred sentinel has invalid framing"))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| anyhow::anyhow!("deferred sentinel payload is not base64url"))?;
    let envelope: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("deferred sentinel payload is not JSON"))?;
    let target = envelope
        .get(ENVELOPE_TARGET)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("deferred sentinel is missing its target"))?;
    check_target_shape(target)?;
    let args_json = envelope
        .get(ENVELOPE_ARGS)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("deferred sentinel is missing its args"))?;
    let mut args = Vec::with_capacity(args_json.len());
    for item in args_json {
        args.push(oxdock_core::json_to_value(item.clone()));
    }
    Ok(Deferred {
        target: target.to_string(),
        args,
    })
}

/// Split text into literal and sentinel spans: each entry is either
/// plain output or one full sentinel span to replace.
fn split_sentinels(text: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(SENTINEL_PREFIX) {
        let after_prefix = &rest[start..];
        let search_from = SENTINEL_PREFIX.len();
        let Some(relative_end) = after_prefix[search_from..].find(SENTINEL_SUFFIX) else {
            break;
        };
        let end = start + search_from + relative_end + SENTINEL_SUFFIX.len();
        if start > 0 {
            spans.push(&rest[..start]);
        }
        spans.push(&rest[start..end]);
        rest = &rest[end..];
    }
    spans.push(rest);
    spans
}

/// True when a span is a sentinel rather than literal output.
fn is_sentinel(span: &str) -> bool {
    span.starts_with(SENTINEL_PREFIX) && span.ends_with(SENTINEL_SUFFIX)
}

/// Defer a domain call to pass 2.
///
/// Pass 1 only checks the envelope: the target names a `MODULE::FUNC`
/// entry and the args hold plain data. It never invokes the target
/// because the document is incomplete. `EXPAND_DEFERRED` calls the
/// target with the full rendered text first, then these args in order.
///
/// # Targets
///
/// Any pure `fn(document: STRING, ...args: ANY) -> STRING` qualifies:
/// pass 2 invokes `target($full_text, ...args)` with exactly that
/// shape, so option maps ride as ordinary trailing arguments (for
/// example `MARKDOWN::TOC` takes its options map second).
#[oxdock_func(pure, returns = TypeTag::String)]
fn defer(
    /// Target deferred call in `MODULE::FUNC` form.
    target: String,
    /// Remaining arguments forwarded to the target after the document text.
    args: Vec<Value>,
) -> Result<Value> {
    Ok(Value::string(mint_deferred(&target, &args)?))
}

/// Expand every deferred sentinel in fully rendered text.
///
/// Pure string to string transform with no file I/O: each sentinel
/// decodes to a target plus pre-validated args, then dispatches
/// through the live pure registry as `target($full_text, ...args)`.
/// Text without sentinels returns unchanged, so outputs without
/// deferrals behave exactly as before. This never re-runs generic
/// `EXPAND`: literal `{{ }}` doc examples stay untouched.
#[oxdock_func(returns = TypeTag::String)]
fn expand_deferred<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Fully rendered document text possibly holding sentinels.
    full_text: String,
) -> Result<Value> {
    let table = cx.pure_functions();
    let mut out = String::with_capacity(full_text.len());
    for span in split_sentinels(&full_text) {
        if !is_sentinel(span) {
            out.push_str(span);
            continue;
        }
        let deferred = decode_deferred(span)?;
        let (module, func) = deferred.target.split_once("::").ok_or_else(|| {
            anyhow::anyhow!("deferred target '{}' is not MODULE::FUNC", deferred.target)
        })?;
        let callee = table
            .get(module)
            .and_then(|entries| entries.get(func))
            .ok_or_else(|| {
                let mut known: Vec<String> = table
                    .iter()
                    .flat_map(|(m, entries)| entries.keys().map(move |f| format!("{m}::{f}")))
                    .collect();
                known.sort();
                anyhow::anyhow!(
                    "unknown deferred target '{}'; known functions: {}",
                    deferred.target,
                    known.join(", ")
                )
            })?;
        let rendered = {
            let mut call_args = Vec::with_capacity(deferred.args.len() + 1);
            call_args.push(Value::string(full_text.clone()));
            call_args.extend(deferred.args.iter().cloned());
            callee(call_args)
        }
        .with_context(|| format!("deferred call '{}' failed", deferred.target))?;
        let text = rendered.as_str().ok_or_else(|| {
            anyhow::anyhow!(
                "deferred call '{}' must return a STRING, got {}",
                deferred.target,
                rendered.type_name()
            )
        })?;
        out.push_str(text);
    }
    Ok(Value::string(out))
}

/// The internal `DOCS` plugin for deferred pipelines. Registered only
/// by `crate::run`; the core language never sees it.
pub fn module<P: ProcessManager>() -> HostModule<P> {
    HostModule {
        name: "DOCS".to_string(),
        funcs: vec![Defer::registration(), ExpandDeferred::registration()],
        types: vec![],
        record_schemas: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn options(entries: &[(&str, Value)]) -> BTreeMap<String, Value> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    #[test]
    fn target_shape_rejects_non_calls() {
        for bad in [
            "TOC",
            "",
            "::TOC",
            "MARKDOWN::",
            "a::b::c",
            "has space::TOC",
        ] {
            assert!(
                check_target_shape(bad).is_err(),
                "must reject target: {bad}"
            );
        }
        assert!(check_target_shape("MARKDOWN::TOC").is_ok());
    }

    #[test]
    fn options_json_round_trips_plain_data() {
        // The envelope rides the shared core conversion: this pins the
        // round-trip through the same functions the pipeline uses.
        let original = options(&[
            ("level", Value::int(2)),
            ("format", Value::string("inline".to_string())),
            ("ratio", Value::float(0.5)),
            ("flag", Value::bool(true)),
            (
                "nested",
                Value::map(
                    [("k".to_string(), Value::string("v".to_string()))]
                        .into_iter()
                        .collect(),
                ),
            ),
            (
                "tags",
                Value::list(vec![Value::string("a".to_string()), Value::int(1)]),
            ),
        ]);
        let mut json_map = serde_json::Map::new();
        for (key, item) in &original {
            json_map.insert(
                key.clone(),
                oxdock_core::value_to_json(item).expect("encode"),
            );
        }
        let back: BTreeMap<String, Value> = json_map
            .iter()
            .map(|(key, item)| (key.clone(), oxdock_core::json_to_value(item.clone())))
            .collect();
        assert_eq!(back, original);
    }

    #[test]
    fn sentinel_round_trips_target_and_args() {
        let minted = mint_deferred(
            "MARKDOWN::TOC",
            &[
                Value::string("appendix-a".to_string()),
                Value::map([("depth".to_string(), Value::int(2))].into_iter().collect()),
            ],
        )
        .expect("mint");
        assert!(!minted.contains("{{") && !minted.contains("}}"));
        let document = format!("before {minted} after");
        let spans = split_sentinels(&document);
        assert_eq!(spans.len(), 3);
        assert!(is_sentinel(spans[1]));
        let decoded = decode_deferred(spans[1]).expect("decode");
        assert_eq!(decoded.target, "MARKDOWN::TOC");
        assert_eq!(decoded.args.len(), 2);
        assert_eq!(
            decoded.args[0].as_str(),
            Some("appendix-a"),
            "positional args round-trip in order"
        );
        assert_eq!(
            decoded.args[1]
                .as_map()
                .and_then(|map| map.get("depth"))
                .and_then(|value| value.as_i64()),
            Some(2),
            "option maps ride as ordinary args"
        );
    }

    #[test]
    fn split_leaves_clean_text_whole() {
        assert_eq!(split_sentinels("plain"), vec!["plain"]);
        assert!(!is_sentinel("plain"));
    }
}
