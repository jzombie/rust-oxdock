//! DSL spelling diagnostics, owned by `oxdock-parser`.
//!
//! One constructor per diagnostic at the owning function. Each carries
//! a stable `dsl.*` key, structured fields, and a whole-sentence
//! template. Call sites pass decoded values, never `format!` strings
//! and never raw mid-parse text containing `WS_SENTINEL`; the
//! transport sanitizes any residual marker so untrusted input cannot
//! crash release.

use oxdock_diagnostics::Diagnostic;

/// Decode a mid-parse fragment for user output: the transient
/// `WS_SENTINEL` boundary becomes a space. Single owner for the decode
/// rule in this crate.
pub fn decode_spelling_fragment(raw: &str) -> String {
    raw.replace(crate::tag::WS_SENTINEL, " ")
}

/// Second `ALIAS` on the same field.
pub fn alias_duplicate_on_field(field: &str, first: &str, second: &str) -> Diagnostic {
    let message = format!(
        "field '{field}' already has alias '{first}', found second alias '{second}': only one ALIAS per field"
    );
    Diagnostic::new(
        "dsl.alias.duplicate_on_field",
        message,
        Diagnostic::make_fields(&[("field", field), ("first", first), ("second", second)]),
    )
}

/// `ALIAS` without a named field.
pub fn alias_needs_field(got: &str) -> Diagnostic {
    let message = format!("ALIAS needs a named field, found bare {got:?}");
    Diagnostic::new(
        "dsl.alias.needs_field",
        message,
        Diagnostic::make_fields(&[("got", got)]),
    )
}

/// `ALIAS` without a lowercase alias name.
pub fn alias_needs_name() -> Diagnostic {
    Diagnostic::new(
        "dsl.alias.needs_name",
        "ALIAS needs a lowercase alias name".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `ALIAS` name must start lowercase. Takes the decoded offending
/// character and the decoded spelling; the template owns quoting.
pub fn alias_name_lowercase(found: &str, spelling: &str) -> Diagnostic {
    let message = format!("ALIAS name must start lowercase, found {found:?} in {spelling:?}");
    Diagnostic::new(
        "dsl.alias.name_lowercase",
        message,
        Diagnostic::make_fields(&[("found", found), ("spelling", spelling)]),
    )
}

/// Unexpected trailing text after a complete spelling.
pub fn spelling_trailing_text(got: &str) -> Diagnostic {
    let message = format!("unexpected trailing text in type spelling: {got:?}");
    Diagnostic::new(
        "dsl.spelling.trailing_text",
        message,
        Diagnostic::make_fields(&[("found", got)]),
    )
}

/// Generic nesting deeper than the cap. Takes the numeric cap so no
/// `format!` leaks to the call site.
pub fn spelling_max_depth(max: usize, got: &str) -> Diagnostic {
    let message = format!("type spelling exceeds maximum generic depth of {max}: {got:?}");
    Diagnostic::new(
        "dsl.spelling.max_depth",
        message,
        Diagnostic::make_fields(&[("max", &max.to_string()), ("found", got)]),
    )
}

/// Expected a type name but found end of input.
pub fn spelling_expected_type_name() -> Diagnostic {
    Diagnostic::new(
        "dsl.spelling.expected_type_name",
        "expected type name, found end of spelling".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Expected an uppercase type name. Takes the decoded character.
pub fn spelling_expected_uppercase(found: &str) -> Diagnostic {
    let message = format!("expected uppercase type name, found {found:?}");
    Diagnostic::new(
        "dsl.spelling.expected_uppercase",
        message,
        Diagnostic::make_fields(&[("found", found)]),
    )
}

/// Expected `,` or `>` to continue a generic spelling.
pub fn spelling_expected_delimiter(got: &str) -> Diagnostic {
    let message = format!("expected ',' or '>' in type spelling, found {got:?}");
    Diagnostic::new(
        "dsl.spelling.expected_delimiter",
        message,
        Diagnostic::make_fields(&[("found", got)]),
    )
}

/// Unterminated quoted string default.
pub fn spelling_unterminated_string(got: &str) -> Diagnostic {
    let message = format!("unterminated string default in type spelling: {got:?}");
    Diagnostic::new(
        "dsl.spelling.unterminated_string",
        message,
        Diagnostic::make_fields(&[("found", got)]),
    )
}

/// Expected a default literal.
pub fn spelling_expected_literal(got: &str) -> Diagnostic {
    let message = format!(
        "expected default literal (INT, FLOAT, BOOL, or quoted STRING) in type spelling, found {got:?}"
    );
    Diagnostic::new(
        "dsl.spelling.expected_literal",
        message,
        Diagnostic::make_fields(&[("found", got)]),
    )
}

/// Float default that does not parse as finite.
pub fn spelling_invalid_float(got: &str) -> Diagnostic {
    let message = format!("bad float default in type spelling: {got:?}");
    Diagnostic::new(
        "dsl.spelling.invalid_float",
        message,
        Diagnostic::make_fields(&[("found", got)]),
    )
}

/// Integer default that does not parse.
pub fn spelling_invalid_int(got: &str) -> Diagnostic {
    let message = format!("bad integer default in type spelling: {got:?}");
    Diagnostic::new(
        "dsl.spelling.invalid_int",
        message,
        Diagnostic::make_fields(&[("found", got)]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_names_both_spellings() {
        let d = alias_duplicate_on_field("h", "help", "h2");
        assert_eq!(d.key(), "dsl.alias.duplicate_on_field");
        assert!(d.message().contains("'h'"));
        assert!(d.message().contains("'help'"));
        assert!(d.message().contains("'h2'"));
    }

    #[test]
    fn trailing_text_is_unexpected_not_expected() {
        let d = spelling_trailing_text("foo");
        assert_eq!(d.key(), "dsl.spelling.trailing_text");
        assert!(d.message().starts_with("unexpected trailing text"));
    }

    #[test]
    fn each_failure_has_a_distinct_key() {
        let keys = [
            spelling_trailing_text("x").key(),
            spelling_max_depth(8, "x").key(),
            spelling_expected_type_name().key(),
            spelling_expected_uppercase("a").key(),
            spelling_expected_delimiter("x").key(),
            spelling_unterminated_string("x").key(),
            spelling_expected_literal("x").key(),
            spelling_invalid_float("x").key(),
            spelling_invalid_int("x").key(),
            alias_needs_field("x").key(),
            alias_needs_name().key(),
            alias_name_lowercase("B", "B").key(),
            alias_duplicate_on_field("h", "a", "b").key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }
}
