//! DSL typing diagnostics, owned by `oxdock-core`.
//!
//! One constructor per diagnostic at the owning function (`typing.rs`
//! uniqueness checks). Stable `dsl.*` keys, structured fields, and
//! whole-sentence templates. Call sites pass decoded values, never
//! `format!` strings.

use oxdock_diagnostics::Diagnostic;

/// Duplicate field name inside one MAP spelling.
pub fn map_duplicate_field(field: &str) -> Diagnostic {
    let message = format!("duplicate field '{field}' in MAP spelling");
    Diagnostic::new(
        "dsl.map.duplicate_field",
        message,
        Diagnostic::make_fields(&[("field", field)]),
    )
}

/// Alias duplicates its own field name.
pub fn alias_self(field: &str, alias: &str) -> Diagnostic {
    let message = format!("alias '{alias}' duplicates its own field '{field}'");
    Diagnostic::new(
        "dsl.alias.self",
        message,
        Diagnostic::make_fields(&[("field", field), ("alias", alias)]),
    )
}

/// Alias clashes with another field or alias. Takes the alias, the
/// field that declares it, and the owner it clashes with.
pub fn alias_clash(alias: &str, field: &str, owner: &str) -> Diagnostic {
    let message = format!("alias '{alias}' for field '{field}' clashes with '{owner}'");
    Diagnostic::new(
        "dsl.alias.clash",
        message,
        Diagnostic::make_fields(&[("alias", alias), ("field", field), ("owner", owner)]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clash_names_both_sides() {
        let d = alias_clash("x", "y", "z");
        assert_eq!(d.key(), "dsl.alias.clash");
        assert_eq!(d.message(), "alias 'x' for field 'y' clashes with 'z'");
    }

    #[test]
    fn self_and_duplicate_have_distinct_keys() {
        assert_eq!(alias_self("a", "a").key(), "dsl.alias.self");
        assert_eq!(map_duplicate_field("a").key(), "dsl.map.duplicate_field");
    }
}
