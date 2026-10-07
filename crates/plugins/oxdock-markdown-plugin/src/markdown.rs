//! Record data to Markdown table (issue 170).
//!
//! Single source for cell escaping and record to table conversion. The
//! `MARKDOWN` host module and the
//! `{{ MARKDOWN::MAP_TO_MD_TABLE($var) }}` placeholder call both
//! piggyback on [`map_to_table`]. Pure logic only: no filesystem,
//! no process, no engine calls.
//!
//! Table of contents support lives here too: [`TocOptions`] resolution,
//! [`Heading`] extraction via `pulldown-cmark`, and [`generate_toc`]
//! rendering. All immediate and pass-agnostic: deferred pipelines wrap
//! these through `DOCS::DEFER` instead of calling them mid-expansion.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

/// Escape pipe characters so `|` inside a value does not break the
/// enclosing Markdown table. Newlines become `<br>` so a multi line
/// value stays within one table row.
pub fn escape_table_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', "<br>")
}

/// Render one value as table cell text. Scalars stay verbatim, nested
/// maps and lists fall back to compact JSON, durations and paths use
/// their display forms, and opaque handles fail instead of rendering
/// noise.
fn cell_text(value: &oxdock_parser::Value) -> Result<String> {
    if let Some(s) = value.as_str() {
        return Ok(escape_table_cell(s));
    }
    if let Some(i) = value.as_i64() {
        return Ok(i.to_string());
    }
    if let Some(f) = value.as_f64() {
        return Ok(f.to_string());
    }
    if let Some(b) = value.as_bool() {
        return Ok(b.to_string());
    }
    if let Some(d) = value.as_duration() {
        return Ok(escape_table_cell(&oxdock_parser::command::format_duration(
            &d,
        )));
    }
    if let Some(p) = value.as_path() {
        return Ok(escape_table_cell(&p.to_string_lossy()));
    }
    if value.as_map().is_some() || value.as_list().is_some() {
        return Ok(escape_table_cell(&serde_json::to_string(&to_json(value)?)?));
    }
    bail!(
        "MAP_TO_MD_TABLE cannot render {} values; use STRING, INT, FLOAT, BOOL, LIST, or MAP",
        value.type_name()
    )
}

/// Convert a script value to JSON for nested cell fallback. Mirrors the
/// shapes table cells accept; anything else fails naming the type.
///
/// Deliberately not the shared [`oxdock_core::value_to_json`]: table
/// cells need display forms, not data fidelity. Durations and paths
/// render through their display strings here (the shared converter
/// rejects them), and the result is embedded inside already-escaped
/// cell text rather than emitted as standalone JSON.
fn to_json(value: &oxdock_parser::Value) -> Result<serde_json::Value> {
    if let Some(map) = value.as_map() {
        let mut out = serde_json::Map::with_capacity(map.len());
        for (key, item) in map {
            out.insert(key.clone(), to_json(item)?);
        }
        return Ok(serde_json::Value::Object(out));
    }
    if let Some(list) = value.as_list() {
        return list
            .iter()
            .map(to_json)
            .collect::<Result<Vec<_>>>()
            .map(serde_json::Value::Array);
    }
    if let Some(s) = value.as_str() {
        return Ok(serde_json::Value::String(s.to_string()));
    }
    if let Some(i) = value.as_i64() {
        return Ok(serde_json::Value::Number(serde_json::Number::from(i)));
    }
    if let Some(f) = value.as_f64() {
        let number = serde_json::Number::from_f64(f)
            .ok_or_else(|| anyhow::anyhow!("MAP_TO_MD_TABLE cannot encode non-finite float {f}"))?;
        return Ok(serde_json::Value::Number(number));
    }
    if let Some(b) = value.as_bool() {
        return Ok(serde_json::Value::Bool(b));
    }
    if let Some(d) = value.as_duration() {
        return Ok(serde_json::Value::String(
            oxdock_parser::command::format_duration(&d),
        ));
    }
    if let Some(p) = value.as_path() {
        return Ok(serde_json::Value::String(p.to_string_lossy().to_string()));
    }
    bail!(
        "MAP_TO_MD_TABLE cannot render {} values; use STRING, INT, FLOAT, BOOL, LIST, or MAP",
        value.type_name()
    )
}

/// Render record data as a Markdown table, dataframe style.
///
/// Each object is one row and each key is one column: a bare MAP
/// renders a single row, while a LIST of MAPs renders one row per
/// element with the sorted union of keys as columns. Cells missing
/// from a row render empty. Columns pad to their widest cell so pipes
/// align when read as plain text.
///
/// ```rust
/// let rows = oxdock_parser::Value::list(vec![
///     oxdock_parser::Value::map(
///         [
///             (
///                 "name".to_string(),
///                 oxdock_parser::Value::string("demo".to_string()),
///             ),
///             ("stars".to_string(), oxdock_parser::Value::int(3)),
///         ]
///         .into_iter()
///         .collect::<std::collections::BTreeMap<_, _>>(),
///     ),
///     oxdock_parser::Value::map(
///         [
///             (
///                 "name".to_string(),
///                 oxdock_parser::Value::string("other".to_string()),
///             ),
///             (
///                 "license".to_string(),
///                 oxdock_parser::Value::string("MIT".to_string()),
///             ),
///         ]
///         .into_iter()
///         .collect::<std::collections::BTreeMap<_, _>>(),
///     ),
/// ]);
/// let table =
///     oxdock_markdown_plugin::markdown::map_to_table(&rows).expect("table");
/// assert_eq!(
///     table,
///     indoc::indoc! {"
///         | license | name  | stars |
///         | ------- | ----- | ----- |
///         |         | demo  | 3     |
///         | MIT     | other |       |
///     "}
/// );
/// ```
///
/// Scalars, lists holding non MAPs, and empty input with no columns
/// fail strict instead of emitting a misleading table.
pub fn map_to_table(value: &oxdock_parser::Value) -> Result<String> {
    let records: Vec<&BTreeMap<String, oxdock_parser::Value>>;
    if let Some(map) = value.as_map() {
        records = vec![map];
    } else if let Some(items) = value.as_list() {
        let mut vec = Vec::with_capacity(items.len());
        for item in items {
            match item.as_map() {
                Some(map) => vec.push(map),
                None => bail!("MAP_TO_MD_TABLE expects a MAP or a LIST of MAPs"),
            }
        }
        records = vec;
    } else {
        bail!("MAP_TO_MD_TABLE expects a MAP or a LIST of MAPs");
    }
    // Raw keys drive lookups, escaped keys drive display: a key holding
    // `|` or a newline must not break the table it heads.
    let keys: Vec<&String> = records
        .iter()
        .flat_map(|record| record.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if keys.is_empty() {
        bail!("MAP_TO_MD_TABLE found no columns to render");
    }
    let headers: Vec<String> = keys.iter().map(|k| escape_table_cell(k)).collect();
    // Render every cell up front so unrenderable values fail strict here
    // instead of silently widening a column they never fill.
    let mut body: Vec<Vec<String>> = Vec::with_capacity(records.len());
    for record in &records {
        let mut row = Vec::with_capacity(keys.len());
        for key in &keys {
            match record.get(*key) {
                Some(cell) => row.push(cell_text(cell)?),
                None => row.push(String::new()),
            }
        }
        body.push(row);
    }
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, header)| {
            body.iter()
                .map(|row| row[i].chars().count())
                .max()
                .unwrap_or(0)
                .max(header.chars().count())
        })
        .collect();
    let mut out = String::new();
    out.push_str(&format_frame_row(
        &headers.iter().map(|h| h.as_str()).collect::<Vec<_>>(),
        &widths,
    ));
    let separators: Vec<String> = widths.iter().map(|w| "-".repeat((*w).max(3))).collect();
    out.push_str(&format_frame_row(
        &separators.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        &widths,
    ));
    for row in &body {
        out.push_str(&format_frame_row(
            &row.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
            &widths,
        ));
    }
    Ok(out)
}

/// Render one padded table row with any column count: cells joined as
/// `| a | b |`, each filled to its column width. Widths count chars;
/// CJK wide chars drift by a column in monospace views, which padded
/// ASCII content avoids.
fn format_frame_row(cells: &[&str], widths: &[usize]) -> String {
    let mut out = String::from("|");
    for (cell, width) in cells.iter().zip(widths.iter()) {
        out.push_str(&format!(" {:width$} |", cell, width = width));
    }
    out.push('\n');
    out
}

/// TOC output layout: nested bullets or a flat delimited bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TocFormat {
    /// Nested `- [text](#anchor)` bullets indented relative to `min_level`.
    Tree,
    /// Flat `[text](#anchor)` links joined with `delimiter`. Single level only.
    Inline,
}

/// Validated TOC options for [`generate_toc`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocOptions {
    /// Minimum heading level to collect (1 is `#`, 2 is `##`).
    pub min_level: u8,
    /// Maximum heading level to collect.
    pub max_level: u8,
    /// Output layout.
    pub format: TocFormat,
    /// Separator for inline layout.
    pub delimiter: String,
}

/// One markdown heading with its GitHub-compatible anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    /// Heading level (1 is `#`, 2 is `##`).
    pub level: u8,
    /// Plain heading text with inline formatting stripped.
    pub text: String,
    /// GitHub-style slug anchor with dedup suffixes.
    pub anchor: String,
}

fn optional_toc_int(
    map: &BTreeMap<String, oxdock_parser::Value>,
    key: &str,
) -> Result<Option<i64>> {
    let Some(value) = map.get(key) else {
        return Ok(None);
    };
    let Some(n) = value.as_i64() else {
        bail!(
            "TOC option '{key}' must be an INT, got {}",
            value.type_name()
        );
    };
    Ok(Some(n))
}

fn optional_toc_string(
    map: &BTreeMap<String, oxdock_parser::Value>,
    key: &str,
) -> Result<Option<String>> {
    let Some(value) = map.get(key) else {
        return Ok(None);
    };
    let Some(text) = value.as_str() else {
        bail!(
            "TOC option '{key}' must be a STRING, got {}",
            value.type_name()
        );
    };
    Ok(Some(text.to_string()))
}

/// Resolve and validate TOC options at the call boundary.
///
/// All constraints fail fast here: level bounds `1 <= min_level <=
/// max_level <= 6`, `format` in `tree|inline`, and inline requiring a
/// single layer. When `format` is inline and `max_level` was not
/// explicitly provided, it collapses to `min_level` so a bare
/// `{format: "inline"}` means the `min_level` layer instead of an
/// unforced multi-level failure.
pub fn resolve_toc_options(map: &BTreeMap<String, oxdock_parser::Value>) -> Result<TocOptions> {
    let min_level = optional_toc_int(map, "min_level")?.unwrap_or(2);
    let max_level_explicit = optional_toc_int(map, "max_level")?;
    let format_raw = optional_toc_string(map, "format")?.unwrap_or_else(|| "tree".to_string());
    let delimiter = optional_toc_string(map, "delimiter")?.unwrap_or_else(|| " | ".to_string());
    let format = match format_raw.as_str() {
        "tree" => TocFormat::Tree,
        "inline" => TocFormat::Inline,
        other => bail!("TOC option 'format' must be one of: tree, inline, got {other:?}"),
    };
    let max_level = match (format, max_level_explicit) {
        (TocFormat::Inline, None) => min_level,
        (_, explicit) => explicit.unwrap_or(3),
    };
    if !(1..=6).contains(&min_level) {
        bail!("TOC option 'min_level' must be between 1 and 6, got {min_level}");
    }
    if !(1..=6).contains(&max_level) {
        bail!("TOC option 'max_level' must be between 1 and 6, got {max_level}");
    }
    if min_level > max_level {
        bail!("TOC option error: min_level ({min_level}) must not exceed max_level ({max_level})");
    }
    if format == TocFormat::Inline && max_level != min_level {
        bail!(
            "TOC option error: format 'inline' requires max_level == min_level (single depth layer), got min_level={min_level}, max_level={max_level}"
        );
    }
    let min_level = min_level as u8;
    let max_level = max_level as u8;
    Ok(TocOptions {
        min_level,
        max_level,
        format,
        delimiter,
    })
}

/// GitHub-style slug: lowercase, alphanumeric kept, spaces and hyphens
/// to `-`, underscores kept, other punctuation dropped. Matches the
/// `docs_conformance` anchor check byte for byte: runs of `-` are
/// never collapsed and edges are never trimmed, exactly like GitHub.
/// Duplicates gain `-1`, `-2` suffixes in document order.
pub fn github_slug(text: &str, counts: &mut std::collections::HashMap<String, usize>) -> String {
    let mut base = String::with_capacity(text.len());
    for c in text.to_lowercase().chars() {
        if c.is_alphanumeric() {
            base.push(c);
        } else if c == ' ' || c == '-' {
            base.push('-');
        } else if c == '_' {
            base.push('_');
        }
    }
    let seen = counts.entry(base.clone()).or_insert(0);
    let slug = if *seen == 0 {
        base.clone()
    } else {
        format!("{base}-{}", *seen - 1 + 1)
    };
    *seen += 1;
    slug
}

/// Extract headings in `[min_level, max_level]` via `pulldown-cmark`.
///
/// Fenced code blocks never surface as headings: the parser emits them
/// as code events, so only real heading events are collected. Inline
/// formatting (emphasis, code spans, links) reduces to plain text.
pub fn extract_headings(document: &str, min_level: u8, max_level: u8) -> Vec<Heading> {
    use pulldown_cmark::{Event, Parser, Tag};
    let mut out = Vec::new();
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut current_level: Option<u8> = None;
    let mut current_text = String::new();
    for event in Parser::new(document) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                current_level = Some(level as u8);
                current_text.clear();
            }
            Event::Text(text) | Event::Code(text) => {
                if current_level.is_some() {
                    current_text.push_str(&text);
                }
            }
            Event::End(pulldown_cmark::TagEnd::Heading(_)) => {
                if let Some(level) = current_level.take()
                    && level >= min_level
                    && level <= max_level
                {
                    let text = current_text.trim().to_string();
                    if !text.is_empty() {
                        let anchor = github_slug(&text, &mut counts);
                        out.push(Heading {
                            level,
                            text,
                            anchor,
                        });
                    }
                }
                current_text.clear();
            }
            _ => {}
        }
    }
    out
}

/// Render headings as nested bullets relative to `min_level`.
fn render_toc_tree(headings: &[Heading], min_level: u8) -> String {
    let mut out = String::new();
    for heading in headings {
        let indent = "  ".repeat(heading.level.saturating_sub(min_level) as usize);
        out.push_str(&format!(
            "{indent}- [{}](#{})\n",
            heading.text, heading.anchor
        ));
    }
    out
}

/// Render one heading layer as a flat delimited bar.
fn render_toc_inline(headings: &[Heading], delimiter: &str) -> String {
    headings
        .iter()
        .map(|heading| format!("[{}](#{})", heading.text, heading.anchor))
        .collect::<Vec<_>>()
        .join(delimiter)
}

/// Generate TOC markdown for a complete rendered document.
///
/// Headings outside `[min_level, max_level]` are ignored. No headings
/// means an empty string so empty sections still render.
pub fn generate_toc(document: &str, options: &TocOptions) -> Result<String> {
    if options.format == TocFormat::Inline && options.max_level != options.min_level {
        bail!(
            "TOC option error: format 'inline' requires max_level == min_level (single depth layer), got min_level={}, max_level={}",
            options.min_level,
            options.max_level
        );
    }
    let headings = extract_headings(document, options.min_level, options.max_level);
    match options.format {
        TocFormat::Tree => Ok(render_toc_tree(&headings, options.min_level)),
        TocFormat::Inline => Ok(render_toc_inline(&headings, &options.delimiter)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn str_value(s: &str) -> oxdock_parser::Value {
        oxdock_parser::Value::string(s.to_string())
    }

    fn map(entries: &[(&str, oxdock_parser::Value)]) -> oxdock_parser::Value {
        oxdock_parser::Value::map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    #[test]
    fn escapes_pipes_and_newlines() {
        assert_eq!(escape_table_cell("a|b"), "a\\|b");
        assert_eq!(escape_table_cell("a\nb"), "a<br>b");
    }

    #[test]
    fn renders_object_as_single_row_frame() {
        let value = map(&[("b", str_value("x")), ("a", oxdock_parser::Value::int(1))]);
        let table = map_to_table(&value).expect("table");
        assert_eq!(table, "| a | b |\n| --- | --- |\n| 1 | x |\n");
    }

    #[test]
    fn renders_list_of_maps_with_union_columns() {
        let value = oxdock_parser::Value::list(vec![
            map(&[
                ("name", str_value("demo")),
                ("stars", oxdock_parser::Value::int(3)),
            ]),
            map(&[("name", str_value("other")), ("license", str_value("MIT"))]),
        ]);
        let table = map_to_table(&value).expect("table");
        assert_eq!(
            table,
            indoc::indoc! {"
                | license | name  | stars |
                | ------- | ----- | ----- |
                |         | demo  | 3     |
                | MIT     | other |       |
            "}
        );
    }

    #[test]
    fn renders_nested_values_as_json() {
        let value = map(&[
            (
                "list",
                oxdock_parser::Value::list(vec![
                    oxdock_parser::Value::int(1),
                    oxdock_parser::Value::bool(true),
                ]),
            ),
            ("map", map(&[("k", str_value("v"))])),
            ("nil", str_value("")),
        ]);
        let table = map_to_table(&value).expect("table");
        assert_eq!(
            table,
            indoc::indoc! {"
                | list     | map       | nil |
                | -------- | --------- | --- |
                | [1,true] | {\"k\":\"v\"} |     |
            "}
        );
    }

    #[test]
    fn empty_object_has_no_columns() {
        assert!(map_to_table(&map(&[])).is_err());
    }

    #[test]
    fn empty_list_has_no_columns() {
        assert!(map_to_table(&oxdock_parser::Value::list(vec![])).is_err());
    }

    #[test]
    fn rejects_non_objects() {
        assert!(
            map_to_table(&oxdock_parser::Value::list(vec![
                oxdock_parser::Value::int(1),
                oxdock_parser::Value::int(2),
            ]))
            .is_err()
        );
        assert!(map_to_table(&str_value("s")).is_err());
    }

    fn toc_map(entries: &[(&str, oxdock_parser::Value)]) -> BTreeMap<String, oxdock_parser::Value> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn toc_defaults_skip_h1() {
        let options = resolve_toc_options(&toc_map(&[])).expect("defaults");
        assert_eq!(options.min_level, 2);
        assert_eq!(options.max_level, 3);
        assert_eq!(options.format, TocFormat::Tree);
        assert_eq!(options.delimiter, " | ");
    }

    #[test]
    fn toc_inline_collapses_default_max_level() {
        let options = resolve_toc_options(&toc_map(&[("format", str_value("inline"))]))
            .expect("inline defaults");
        assert_eq!(options.min_level, 2);
        assert_eq!(options.max_level, 2);
    }

    #[test]
    fn toc_rejects_multi_level_inline_in_pass_1() {
        let err = resolve_toc_options(&toc_map(&[
            ("format", str_value("inline")),
            ("min_level", oxdock_parser::Value::int(2)),
            ("max_level", oxdock_parser::Value::int(3)),
        ]))
        .expect_err("inline must be single level");
        assert!(err.to_string().contains("requires max_level == min_level"));
    }

    #[test]
    fn toc_rejects_bad_bounds_and_format() {
        assert!(
            resolve_toc_options(&toc_map(&[("min_level", oxdock_parser::Value::int(0))])).is_err()
        );
        assert!(
            resolve_toc_options(&toc_map(&[("max_level", oxdock_parser::Value::int(7))])).is_err()
        );
        assert!(
            resolve_toc_options(&toc_map(&[
                ("min_level", oxdock_parser::Value::int(4)),
                ("max_level", oxdock_parser::Value::int(2)),
            ]))
            .is_err()
        );
        assert!(resolve_toc_options(&toc_map(&[("format", str_value("grid"))])).is_err());
    }

    #[test]
    fn toc_slugs_match_conformance_anchors() {
        // Punctuation runs become hyphen runs with no collapsing and
        // no edge trimming, mirroring docs_conformance slugify (and
        // GitHub), so every generated `#anchor` resolves there.
        let mut counts = std::collections::HashMap::new();
        assert_eq!(
            github_slug("Workspaces & Filesystem", &mut counts),
            "workspaces--filesystem"
        );
        assert_eq!(
            github_slug("Testing & Coverage", &mut counts),
            "testing--coverage"
        );
    }

    #[test]
    fn toc_slugs_keep_non_ascii_alphanumeric() {
        // `is_alphanumeric` is Unicode-aware, matching the conformance
        // gate: accented and CJK characters survive instead of
        // vanishing into empty or hyphen-only anchors.
        let mut counts = std::collections::HashMap::new();
        assert_eq!(github_slug("Café au lait", &mut counts), "café-au-lait");
        assert_eq!(github_slug("日本語の見出し", &mut counts), "日本語の見出し");
        let document = "# Title\n\n## Café au lait\n\n## 日本語の見出し\n";
        let options = resolve_toc_options(&toc_map(&[
            ("min_level", oxdock_parser::Value::int(2)),
            ("max_level", oxdock_parser::Value::int(2)),
            ("format", str_value("inline")),
        ]))
        .expect("options");
        let toc = generate_toc(document, &options).expect("toc");
        assert_eq!(
            toc,
            "[Café au lait](#café-au-lait) | [日本語の見出し](#日本語の見出し)"
        );
    }

    #[test]
    fn toc_tree_ignores_h1_and_fenced_code() {
        let document = indoc::indoc! {r#"
            # Title

            ## Section One

            ```markdown
            ## Not a heading
            ```

            ### Sub One

            ## Section One

            ## Section Two
        "#};
        let options = resolve_toc_options(&toc_map(&[])).expect("options");
        let toc = generate_toc(document, &options).expect("toc");
        assert_eq!(
            toc,
            indoc::indoc! {r#"
                - [Section One](#section-one)
                  - [Sub One](#sub-one)
                - [Section One](#section-one-1)
                - [Section Two](#section-two)
            "#}
        );
    }

    #[test]
    fn toc_inline_joins_single_level_with_delimiter() {
        let document = "# Title\n\n## Alpha\n\n## Beta\n\n### Gamma\n";
        let options = resolve_toc_options(&toc_map(&[
            ("min_level", oxdock_parser::Value::int(2)),
            ("max_level", oxdock_parser::Value::int(2)),
            ("format", str_value("inline")),
        ]))
        .expect("options");
        let toc = generate_toc(document, &options).expect("toc");
        assert_eq!(toc, "[Alpha](#alpha) | [Beta](#beta)");
    }

    #[test]
    fn toc_empty_document_renders_empty() {
        let options = resolve_toc_options(&toc_map(&[])).expect("options");
        assert_eq!(generate_toc("# Only title\n", &options).expect("toc"), "");
    }
}
