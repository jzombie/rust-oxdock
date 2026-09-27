//! Render runtime values back to DSL source literals.
//!
//! The parser owns both directions of the syntax: `Display` prints AST
//! nodes while this module prints *values* (`Value::int(7)` renders as
//! `7`). The output must reparse to the identical type and value, which
//! Rust defaults do not guarantee (`format!("{}", 3.0)` prints `3`,
//! which parses as `INT`). The primary consumer is sealed remote
//! execution, which ships values as script text, but the renderer knows
//! nothing about transports.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, bail};

use super::command::format_duration;
use super::value::Value;

/// Canonical DSL string escaping: the exact inverse of the runtime string
/// expander (which decodes `\\`, `\{{`, `\n`, `\t`, `\r`, `\"`). Every
/// backslash the renderer emits precedes a recognized escape, so rendered
/// values reparse byte identical and can never break out of their literal
/// into surrounding instructions. Separate from the `quote_*` `Display`
/// helpers in `commands.rs`, which do not cover control characters.
pub fn escape_dsl_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // A `{{` pair triggers interpolation: emit the `\{{`
            // triple the expander recognizes and consume both braces,
            // so runs like `{{{` split into non-overlapping pairs
            // (`\{{` plus `{`) instead of overlapping `\{` fragments
            // whose second backslash breaks the triple.
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push_str("\\{{");
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Render one value to a DSL literal. Tight set: scalars plus `LIST` /
/// `MAP` composed of scalars. `DURATION` renders quoted in its `30s`
/// shape; a nested duration arrives as `STRING` on reparse (only a
/// top-level `LET $d: DURATION` declaration drives coercion), which the
/// renderer documents rather than hides. Everything else bails naming
/// the type.
pub fn render_dsl_literal(value: &Value) -> Result<String> {
    match value {
        _ if value.as_i64().is_some() => Ok(value.as_i64().unwrap_or(0).to_string()),
        _ if value.as_f64().is_some() => render_float(value.as_f64().unwrap_or(0.0)),
        _ if value.as_bool().is_some() => Ok(value.as_bool().unwrap_or(false).to_string()),
        _ if value.as_str().is_some() => Ok(render_quoted(value.as_str().unwrap_or(""))),
        _ if value.as_duration().is_some() => {
            let text = format_duration(&value.as_duration().unwrap_or(Duration::from_secs(0)));
            Ok(render_quoted(&text))
        }
        _ if value.as_list().is_some() => {
            let mut items = Vec::new();
            for item in value.as_list().unwrap_or(&Vec::new()) {
                items.push(render_composite_item(item)?);
            }
            Ok(format!("[{}]", items.join(", ")))
        }
        _ if value.as_map().is_some() => {
            let mut entries = Vec::new();
            for (key, item) in value.as_map().unwrap_or(&BTreeMap::new()) {
                entries.push(format!(
                    "{}: {}",
                    render_quoted(key),
                    render_composite_item(item)?
                ));
            }
            Ok(format!("{{{}}}", entries.join(", ")))
        }
        _ => bail!(
            "value of type {} has no DSL literal form",
            value.type_name()
        ),
    }
}

/// Render one float so it reparses as `FLOAT`: Rust prints integral
/// floats without a point (`3`), which the parser reads as `INT`.
/// Non-finite values have no literal form and bail loudly.
fn render_float(raw: f64) -> Result<String> {
    if !raw.is_finite() {
        bail!("non-finite FLOAT has no DSL literal form");
    }
    let mut text = format!("{raw}");
    if !text.contains('.') {
        text.push_str(".0");
    }
    Ok(text)
}

/// Render one raw string as a quoted DSL literal (`"..."` with
/// [`escape_dsl_string`] applied). The string half of
/// [`render_dsl_literal`], exposed for callers holding plain text
/// rather than a `Value`.
pub fn render_quoted(raw: &str) -> String {
    format!("\"{}\"", escape_dsl_string(raw))
}

fn render_composite_item(value: &Value) -> Result<String> {
    if value.as_duration().is_some() {
        let text = format_duration(&value.as_duration().unwrap_or(Duration::from_secs(0)));
        return Ok(render_quoted(&text));
    }
    render_dsl_literal(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_round_trips_hostile_strings() {
        for raw in [
            "plain",
            "quote \" inside",
            "back\\slash",
            "line\nbreak",
            "return\r carriage",
            "tab\there",
            "template {{ $x }} here",
            "semi; brace} hash#",
            "\\{{ already }}",
        ] {
            let rendered = render_quoted(raw);
            assert!(rendered.starts_with('"') && rendered.ends_with('"'));
            let inner = &rendered[1..rendered.len() - 1];
            // Every template pair must carry its escape backslash.
            let stripped = inner.replace("\\{{", "");
            assert!(
                !stripped.contains("{{"),
                "unescaped template pair in {rendered:?}"
            );
        }
    }

    #[test]
    fn escape_table_inverts_expand_string() {
        // Every escape the renderer emits must be one the runtime decodes.
        let rendered = escape_dsl_string("a\"b\\c\nd\re\tf{{g}}");
        assert_eq!(rendered, "a\\\"b\\\\c\\nd\\re\\tf\\{{g}}");
    }

    #[test]
    fn render_float_always_carries_a_point() {
        assert_eq!(render_float(3.0).unwrap(), "3.0");
        assert!(render_float(f64::NAN).is_err());
        assert!(render_float(f64::INFINITY).is_err());
    }

    #[test]
    fn render_rejects_handles_and_paths() {
        for value in [
            Value::pipe_fresh(),
            Value::handle(7),
            Value::path("/tmp/x".into()),
        ] {
            assert!(render_dsl_literal(&value).is_err());
        }
    }

    #[test]
    fn render_scalars_and_composites() {
        assert_eq!(render_dsl_literal(&Value::int(7)).unwrap(), "7");
        assert_eq!(
            render_dsl_literal(&Value::string("hi".to_string())).unwrap(),
            "\"hi\""
        );
        assert_eq!(render_dsl_literal(&Value::bool(true)).unwrap(), "true");
        let list = Value::list(vec![Value::int(1), Value::string("a\"b".to_string())]);
        assert_eq!(render_dsl_literal(&list).unwrap(), "[1, \"a\\\"b\"]");
        let mut entries = BTreeMap::new();
        entries.insert("k".to_string(), Value::int(2));
        assert_eq!(
            render_dsl_literal(&Value::map(entries)).unwrap(),
            "{\"k\": 2}"
        );
    }
}
