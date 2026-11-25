//! Markdown failures behind single constructors with stable keys.
//!
//! This module owns every markdown template and key (`md.*`). It builds
//! on the generic [`oxdock_diagnostics::Diagnostic`] transport but owns
//! its wording: changing markdown text happens here, never at the call
//! sites. Call sites pass structured data only, never `format!`
//! strings. Fixed-shape failures are parameterless; value failures
//! take the offending value; level failures take the option name (the
//! `OPT_*` constants) so the `min_level` / `max_level` modes share one
//! key.

use oxdock_diagnostics::Diagnostic;

/// Structured field keys. Single spelling for the whole module:
/// constructors reference these, never literals, so a key rename is
/// one edit.
const FIELD_TYPE: &str = "type";
const FIELD_FLOAT: &str = "float";
const FIELD_OTHER: &str = "other";
const FIELD_OPTION: &str = "option";
const FIELD_VALUE: &str = "value";
const FIELD_MIN: &str = "min";
const FIELD_MAX: &str = "max";
const FIELD_NEEDLE: &str = "needle";

/// Table cell of an unrenderable type. Takes the value type name;
/// rendering lives here. Shared by the cell and nested-JSON paths:
/// same failure mode, one key.
pub fn table_unrenderable_type(type_name: &str) -> Diagnostic {
    let message = format!(
        "MAP_TO_MD_TABLE cannot render {type_name} values; use STRING, INT, FLOAT, BOOL, LIST, or MAP"
    );
    Diagnostic::new(
        "md.table.unrenderable_type",
        message,
        Diagnostic::make_fields(&[(FIELD_TYPE, type_name)]),
    )
}

/// Non-finite float in nested JSON. Takes the float; rendering lives
/// here.
pub fn table_non_finite_float(value: f64) -> Diagnostic {
    let message = format!("MAP_TO_MD_TABLE cannot encode non-finite float {value}");
    Diagnostic::new(
        "md.table.non_finite_float",
        message,
        Diagnostic::make_fields(&[(FIELD_FLOAT, &value.to_string())]),
    )
}

/// Table input that is neither a MAP nor a LIST of MAPs.
/// Parameterless: the wording is fixed. Shared by the loop and
/// top-level sites: one key, one mode.
pub fn table_expects_map_or_list() -> Diagnostic {
    Diagnostic::new(
        "md.table.expects_map_or_list",
        "MAP_TO_MD_TABLE expects a MAP or a LIST of MAPs".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Table input with no columns to render. Parameterless: the wording
/// is fixed.
pub fn table_no_columns() -> Diagnostic {
    Diagnostic::new(
        "md.table.no_columns",
        "MAP_TO_MD_TABLE found no columns to render".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Unknown TOC `format` value. Takes the offending value; `Debug`
/// quoting lives here.
pub fn toc_invalid_format(other: &str) -> Diagnostic {
    let message = format!("TOC option 'format' must be one of: tree, inline, got {other:?}");
    Diagnostic::new(
        "md.toc.invalid_format",
        message,
        Diagnostic::make_fields(&[(FIELD_OTHER, other)]),
    )
}

/// TOC level outside 1-6. Takes the option name (the `OPT_*`
/// constants) plus the value; rendering lives here. Shared by the
/// `min_level` / `max_level` modes: one key, one mode.
pub fn toc_level_out_of_range(option: &str, value: i64) -> Diagnostic {
    let message = format!("TOC option '{option}' must be between 1 and 6, got {value}");
    Diagnostic::new(
        "md.toc.level_out_of_range",
        message,
        Diagnostic::make_fields(&[(FIELD_OPTION, option), (FIELD_VALUE, &value.to_string())]),
    )
}

/// `min_level` above `max_level`. Takes both levels; rendering lives
/// here.
pub fn toc_min_exceeds_max(min_level: i64, max_level: i64) -> Diagnostic {
    let message = format!(
        "TOC option error: min_level ({min_level}) must not exceed max_level ({max_level})"
    );
    Diagnostic::new(
        "md.toc.min_exceeds_max",
        message,
        Diagnostic::make_fields(&[
            (FIELD_MIN, &min_level.to_string()),
            (FIELD_MAX, &max_level.to_string()),
        ]),
    )
}

/// Inline TOC spanning several levels. Takes both levels; rendering
/// lives here. Shared by option resolution and generation: one key,
/// one mode (generation casts its `u8` layers to `i64`).
pub fn toc_inline_requires_single_level(min_level: i64, max_level: i64) -> Diagnostic {
    let message = format!(
        "TOC option error: format 'inline' requires max_level == min_level (single depth layer), got min_level={min_level}, max_level={max_level}"
    );
    Diagnostic::new(
        "md.toc.inline_requires_single_level",
        message,
        Diagnostic::make_fields(&[
            (FIELD_MIN, &min_level.to_string()),
            (FIELD_MAX, &max_level.to_string()),
        ]),
    )
}

/// Section needle matching no heading. Takes the needle; `Debug`
/// quoting lives here.
pub fn section_no_heading(needle: &str) -> Diagnostic {
    let message = format!("no heading starts with {needle:?}");
    Diagnostic::new(
        "md.section.no_heading",
        message,
        Diagnostic::make_fields(&[(FIELD_NEEDLE, needle)]),
    )
}

/// Section slice out of document bounds. Parameterless: the wording
/// is fixed.
pub fn section_offsets_out_of_bounds() -> Diagnostic {
    Diagnostic::new(
        "md.section.offsets_out_of_bounds",
        "section offsets out of bounds".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_has_a_distinct_key() {
        let keys = [
            table_unrenderable_type("HANDLE").key(),
            table_non_finite_float(f64::NAN).key(),
            table_expects_map_or_list().key(),
            table_no_columns().key(),
            toc_invalid_format("grid").key(),
            toc_level_out_of_range("min_level", 0).key(),
            toc_min_exceeds_max(4, 2).key(),
            toc_inline_requires_single_level(2, 3).key(),
            section_no_heading("[9.9.9]").key(),
            section_offsets_out_of_bounds().key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn shared_modes_keep_one_key() {
        assert_eq!(
            toc_level_out_of_range("min_level", 0).key(),
            toc_level_out_of_range("max_level", 7).key()
        );
    }

    #[test]
    fn messages_match_canonical_wording() {
        assert_eq!(
            table_unrenderable_type("HANDLE").message(),
            "MAP_TO_MD_TABLE cannot render HANDLE values; use STRING, INT, FLOAT, BOOL, LIST, or MAP"
        );
        assert_eq!(
            table_non_finite_float(f64::INFINITY).message(),
            "MAP_TO_MD_TABLE cannot encode non-finite float inf"
        );
        assert_eq!(
            table_expects_map_or_list().message(),
            "MAP_TO_MD_TABLE expects a MAP or a LIST of MAPs"
        );
        assert_eq!(
            table_no_columns().message(),
            "MAP_TO_MD_TABLE found no columns to render"
        );
        assert_eq!(
            toc_invalid_format("grid").message(),
            "TOC option 'format' must be one of: tree, inline, got \"grid\""
        );
        assert_eq!(
            toc_level_out_of_range("min_level", 0).message(),
            "TOC option 'min_level' must be between 1 and 6, got 0"
        );
        assert_eq!(
            toc_level_out_of_range("max_level", 7).message(),
            "TOC option 'max_level' must be between 1 and 6, got 7"
        );
        assert_eq!(
            toc_min_exceeds_max(4, 2).message(),
            "TOC option error: min_level (4) must not exceed max_level (2)"
        );
        assert_eq!(
            toc_inline_requires_single_level(2, 3).message(),
            "TOC option error: format 'inline' requires max_level == min_level (single depth layer), got min_level=2, max_level=3"
        );
        assert_eq!(
            section_no_heading("[9.9.9]").message(),
            "no heading starts with \"[9.9.9]\""
        );
        assert_eq!(
            section_offsets_out_of_bounds().message(),
            "section offsets out of bounds"
        );
    }
}
