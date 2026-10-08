//! The `MARKDOWN` host module: record data to Markdown table (issue 170).
//!
//! Thin OxDock wrapper over [`crate::markdown`], the single source for
//! table rendering. Values pass straight through on the shared value
//! model with no conversion.

use std::collections::BTreeMap;

use anyhow::Result;
use oxdock_core::{HostModule, OxDockFn, TypeTag, Value};
use oxdock_func_macro::oxdock_func;
use oxdock_process::ProcessManager;

/// Render record data as an aligned Markdown table: one row per MAP,
/// one column per key. Fails strict on shapes with no table in them.
///
#[oxdock_func(pure, returns = TypeTag::String)]
fn map_to_md_table(
    /// Record data: a MAP renders one row, a LIST of MAPs renders one row per entry with union columns.
    map: Value,
) -> Result<Value> {
    let table = crate::markdown::map_to_table(&map)?;
    Ok(Value::string(table))
}

/// Render a table of contents for markdown text, immediately.
///
/// Pure and pass-agnostic: takes document text plus options and returns
/// the TOC in one call, with zero knowledge of sentinels, passes, or
/// docs-gen. Deferred pipelines wrap this with `DOCS::DEFER` instead
/// of calling it during expansion. Links render GitHub-style
/// (`[text](#github-slug)` with `-1` dedup), so output pastes into any
/// GitHub-rendered document with working anchors.
#[oxdock_func(pure, returns = TypeTag::String)]
fn toc(
    /// Document markdown text to scan for headings.
    md: String,
    /// TOC options: heading bounds (`min_level` default 2 skips `#`
    /// titles, `max_level` default 3), output layout (`format` `tree`
    /// or `inline`, default `tree`), and the inline `delimiter`
    /// (default ` | `). A bare `{format: "inline"}` collects the
    /// `min_level` layer only.
    #[options(
        "min_level?: INT = 2",
        "max_level?: INT = 3",
        "format?: STRING = tree",
        "delimiter?: STRING =  | "
    )]
    options: BTreeMap<String, Value>,
) -> Result<Value> {
    let resolved = crate::markdown::resolve_toc_options(&options)?;
    let rendered = crate::markdown::generate_toc(&md, &resolved)?;
    Ok(Value::string(rendered))
}

/// Parse markdown headings into a structured list.
///
/// Immediate helper for composition and tests: returns one MAP per
/// heading with `level`, `text`, and `anchor` keys.
#[oxdock_func(pure, returns = TypeTag::ListOf(&TypeTag::Any))]
fn parse(
    /// Markdown document to scan for headings.
    md: String,
) -> Result<Value> {
    let headings = crate::markdown::extract_headings(&md, 1, 6);
    let items = headings
        .into_iter()
        .map(|heading| {
            Value::map(
                [
                    ("level".to_string(), Value::int(i64::from(heading.level))),
                    ("text".to_string(), Value::string(heading.text)),
                    ("anchor".to_string(), Value::string(heading.anchor)),
                ]
                .into_iter()
                .collect::<BTreeMap<_, _>>(),
            )
        })
        .collect::<Vec<_>>();
    Ok(Value::list(items))
}

/// Extract one document section by heading prefix.
///
/// Finds the first heading whose plain text starts with `needle` and
/// returns the heading plus its body, stopping before the next heading
/// of equal or higher level. Fenced code headings never match. Missing
/// needles fail naming the needle, so release notes extraction fails
/// loudly instead of publishing an empty section.
#[oxdock_func(pure, returns = TypeTag::String)]
fn section(
    /// Markdown document to slice.
    md: String,
    /// Heading text prefix selecting the section (e.g. `[0.24.0-alpha]`).
    needle: String,
) -> Result<Value> {
    let body = crate::markdown::extract_section(&md, &needle)?;
    Ok(Value::string(body))
}

/// The `MARKDOWN` host module for OxDock scripts and placeholders.
pub fn module_with<P: ProcessManager>() -> HostModule<P> {
    HostModule {
        name: "MARKDOWN".to_string(),
        funcs: vec![
            MapToMdTable::registration(),
            Toc::registration(),
            Parse::registration(),
            Section::registration(),
        ],
        types: vec![],
        record_schemas: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    #[test]
    fn renders_map_as_table() {
        let value = map(&[
            ("name", Value::string("demo".to_string())),
            ("stars", Value::int(3)),
        ]);
        let out = map_to_md_table(value)
            .expect("table")
            .as_str()
            .expect("string")
            .to_string();
        assert_eq!(
            out,
            "| name | stars |\n| ---- | ----- |\n| demo | 3     |\n"
        );
    }

    #[test]
    fn rejects_non_maps() {
        assert!(map_to_md_table(Value::string("nope".to_string())).is_err());
    }

    #[test]
    fn toc_renders_immediately_from_text_and_options() {
        let options = map(&[("max_level", Value::int(2))]);
        let out = toc(
            "# Title\n\n## Alpha\n\n### Sub\n".to_string(),
            options.as_map().expect("map").clone(),
        )
        .expect("toc")
        .as_str()
        .expect("string")
        .to_string();
        assert_eq!(out, "- [Alpha](#alpha)\n");
    }

    #[test]
    fn toc_rejects_bad_options_at_the_boundary() {
        let options = map(&[("format", Value::string("grid".to_string()))]);
        assert!(toc("## A\n".to_string(), options.as_map().expect("map").clone()).is_err());
    }
}
