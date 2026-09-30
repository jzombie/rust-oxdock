//! Record data to Markdown table (issue 170).
//!
//! Single source for cell escaping and record to table conversion. The
//! `MARKDOWN` host module and the
//! `{{ MARKDOWN::MAP_TO_MD_TABLE($var) }}` placeholder call both
//! piggyback on [`map_to_table`]. Pure logic only: no filesystem,
//! no process, no engine calls.

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
}
