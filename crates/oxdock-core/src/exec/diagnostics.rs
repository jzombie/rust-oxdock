//! DSL typing diagnostics, owned by `oxdock-core`.
//!
//! One constructor per diagnostic at the owning function (`typing.rs`
//! resolution and `collect_type_aliases`). Stable `dsl.*` keys,
//! structured fields, and whole-sentence templates. Call sites pass
//! decoded values, never `format!` strings; joining and quoting live
//! here.

use oxdock_diagnostics::Diagnostic;
use oxdock_parser::{ANY_TYPE_NAME, HANDLE_TYPE_NAME, LIST_TYPE_NAME, MAP_TYPE_NAME, Spelled};

/// Duplicate field name inside one MAP spelling.
pub fn map_duplicate_field(field: &str) -> Diagnostic {
    let message = format!("duplicate field `{field}` in `{MAP_TYPE_NAME}` spelling");
    Diagnostic::new(
        "dsl.map.duplicate_field",
        message,
        Diagnostic::make_fields(&[("field", field)]),
    )
}

/// Alias duplicates its own field name.
pub fn alias_self(field: &str, alias: &str) -> Diagnostic {
    let message = format!("alias `{alias}` duplicates its own field `{field}`");
    Diagnostic::new(
        "dsl.alias.self",
        message,
        Diagnostic::make_fields(&[("field", field), ("alias", alias)]),
    )
}

/// Alias clashes with another field or alias. Takes the alias, the
/// field that declares it, and the owner it clashes with.
pub fn alias_clash(alias: &str, field: &str, owner: &str) -> Diagnostic {
    let message = format!("alias `{alias}` for field `{field}` clashes with `{owner}`");
    Diagnostic::new(
        "dsl.alias.clash",
        message,
        Diagnostic::make_fields(&[("alias", alias), ("field", field), ("owner", owner)]),
    )
}

/// Generic nesting deeper than the cap, resolving a bare name.
pub fn typing_max_depth(max: usize, name: &str) -> Diagnostic {
    let message = format!("type spelling exceeds maximum generic depth of {max}: `{name}`");
    Diagnostic::new(
        "dsl.typing.max_depth",
        message,
        Diagnostic::make_fields(&[("max", &max.to_string()), ("spelling", name)]),
    )
}

/// Generic nesting deeper than the cap, building a parsed spelling.
/// Same key as [`typing_max_depth`]: one rule, two entry shapes. The
/// `Debug` rendering lives here so call sites pass the parsed value.
pub fn typing_max_depth_spelled(max: usize, spelled: &Spelled) -> Diagnostic {
    let rendered = format!("{spelled:?}");
    let message = format!("type spelling exceeds maximum generic depth of {max}: `{rendered}`");
    Diagnostic::new(
        "dsl.typing.max_depth",
        message,
        Diagnostic::make_fields(&[("max", &max.to_string()), ("spelling", &rendered)]),
    )
}

/// Bare `MAP` without arguments.
pub fn typing_bare_map() -> Diagnostic {
    let message = format!(
        "bare `{MAP_TYPE_NAME}` is not a declaration type: write a shape (`{MAP_TYPE_NAME}<name: TYPE, ...>`) or `{MAP_TYPE_NAME}<{ANY_TYPE_NAME}>` for maps with unknown keys"
    );
    Diagnostic::new(
        "dsl.typing.bare_map",
        message,
        Diagnostic::make_fields(&[("type", MAP_TYPE_NAME)]),
    )
}

/// Bare `LIST` without arguments.
pub fn typing_bare_list() -> Diagnostic {
    let message = format!(
        "bare `{LIST_TYPE_NAME}` is not a declaration type: write a shape (`{LIST_TYPE_NAME}<TYPE>`) or `{LIST_TYPE_NAME}<{ANY_TYPE_NAME}>` for lists with unknown elements"
    );
    Diagnostic::new(
        "dsl.typing.bare_list",
        message,
        Diagnostic::make_fields(&[("type", LIST_TYPE_NAME)]),
    )
}

/// Bare `HANDLE` without arguments.
pub fn typing_bare_handle() -> Diagnostic {
    let message = format!(
        "bare `{HANDLE_TYPE_NAME}` is not a declaration type: write `{HANDLE_TYPE_NAME}<T>` for a known payload or `{HANDLE_TYPE_NAME}<{ANY_TYPE_NAME}>` for an unknown one"
    );
    Diagnostic::new(
        "dsl.typing.bare_handle",
        message,
        Diagnostic::make_fields(&[("type", HANDLE_TYPE_NAME)]),
    )
}

/// Alias resolution cycle. Takes the resolution chain including the
/// repeated name; joining lives here.
pub fn alias_cycle(chain: &[String]) -> Diagnostic {
    let joined = chain.join(" -> ");
    let message = format!("type alias cycle: {joined}");
    Diagnostic::new(
        "dsl.alias.cycle",
        message,
        Diagnostic::make_fields(&[("chain", &joined)]),
    )
}

/// Unknown type name. Takes the name plus the directory listings;
/// joining lives here so call sites pass slices.
pub fn typing_unknown(
    name: &str,
    types: &[String],
    schemas: &[String],
    aliases: &[String],
) -> Diagnostic {
    let types = types.join(", ");
    let schemas = schemas.join(", ");
    let aliases = aliases.join(", ");
    let message = format!(
        "unknown type `{name}`; known types: {types} and schemas: {schemas} and aliases: {aliases}"
    );
    Diagnostic::new(
        "dsl.typing.unknown",
        message,
        Diagnostic::make_fields(&[
            ("name", name),
            ("types", &types),
            ("schemas", &schemas),
            ("aliases", &aliases),
        ]),
    )
}

/// Type arguments on a head that takes none.
pub fn typing_no_args(name: &str) -> Diagnostic {
    let message = format!("`{name}` takes no type arguments");
    Diagnostic::new(
        "dsl.typing.no_args",
        message,
        Diagnostic::make_fields(&[("type", name)]),
    )
}

/// `HANDLE` with anything but one bare argument.
pub fn typing_handle_arity() -> Diagnostic {
    let message = format!("`{HANDLE_TYPE_NAME}` takes exactly one bare type argument");
    Diagnostic::new(
        "dsl.typing.handle_arity",
        message,
        Diagnostic::make_fields(&[("type", HANDLE_TYPE_NAME)]),
    )
}

/// `LIST` with anything but one bare argument.
pub fn typing_list_arity() -> Diagnostic {
    let message = format!("`{LIST_TYPE_NAME}` takes exactly one bare type argument");
    Diagnostic::new(
        "dsl.typing.list_arity",
        message,
        Diagnostic::make_fields(&[("type", LIST_TYPE_NAME)]),
    )
}

/// Bare argument where `MAP` requires `name: TYPE`. Takes the head
/// name for the trailing clause.
pub fn typing_map_bare(head: &str) -> Diagnostic {
    let message =
        format!("`{MAP_TYPE_NAME}` fields require `name: TYPE`; `{head}` takes no bare arguments");
    Diagnostic::new(
        "dsl.typing.map_bare",
        message,
        Diagnostic::make_fields(&[("head", head)]),
    )
}

/// Default literal that does not check against its field type. Takes
/// the field name and the rendered expected type.
pub fn typing_default_mismatch(field: &str, expected: &str) -> Diagnostic {
    let message = format!("default for field `{field}` does not match {expected}");
    Diagnostic::new(
        "dsl.typing.default_mismatch",
        message,
        Diagnostic::make_fields(&[("field", field), ("expected", expected)]),
    )
}

/// Union with no member shapes.
pub fn typing_empty_union() -> Diagnostic {
    Diagnostic::new(
        "dsl.typing.empty_union",
        "empty union: no member shapes to admit".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Alias name already defined as an alias.
pub fn alias_already_defined(name: &str) -> Diagnostic {
    let message = format!("type alias `{name}` is already defined");
    Diagnostic::new(
        "dsl.alias.already_defined",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// Alias name collides with a builtin type.
pub fn alias_builtin_collision(name: &str) -> Diagnostic {
    let message = format!("type alias `{name}` collides with a builtin type");
    Diagnostic::new(
        "dsl.alias.builtin_collision",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// Alias name collides with a registered host type.
pub fn alias_host_collision(name: &str) -> Diagnostic {
    let message = format!("type alias `{name}` collides with registered host type `{name}`");
    Diagnostic::new(
        "dsl.alias.host_collision",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// Alias name collides with a registered record schema.
pub fn alias_schema_collision(name: &str) -> Diagnostic {
    let message = format!("type alias `{name}` collides with registered record schema `{name}`");
    Diagnostic::new(
        "dsl.alias.schema_collision",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

/// Alias name collides with a top-level function.
pub fn alias_func_collision(name: &str) -> Diagnostic {
    let message = format!("type alias `{name}` collides with function `{name}`");
    Diagnostic::new(
        "dsl.alias.func_collision",
        message,
        Diagnostic::make_fields(&[("name", name)]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clash_names_both_sides() {
        let d = alias_clash("x", "y", "z");
        assert_eq!(d.key(), "dsl.alias.clash");
        assert_eq!(d.message(), "alias `x` for field `y` clashes with `z`");
    }

    #[test]
    fn self_and_duplicate_have_distinct_keys() {
        assert_eq!(alias_self("a", "a").key(), "dsl.alias.self");
        assert_eq!(map_duplicate_field("a").key(), "dsl.map.duplicate_field");
    }

    #[test]
    fn every_resolution_error_has_a_distinct_key() {
        let types = vec!["STRING".to_string()];
        let empty: Vec<String> = vec![];
        let keys = [
            typing_max_depth(8, "INT").key(),
            typing_bare_map().key(),
            typing_bare_list().key(),
            typing_bare_handle().key(),
            alias_cycle(&["A".to_string(), "A".to_string()]).key(),
            typing_unknown("NOPE", &types, &empty, &empty).key(),
            typing_no_args("STRING").key(),
            typing_handle_arity().key(),
            typing_list_arity().key(),
            typing_map_bare("MAP").key(),
            typing_default_mismatch("f", "INT").key(),
            typing_empty_union().key(),
            alias_already_defined("A").key(),
            alias_builtin_collision("A").key(),
            alias_host_collision("A").key(),
            alias_schema_collision("A").key(),
            alias_func_collision("A").key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn messages_match_canonical_wording() {
        assert_eq!(
            typing_bare_map().message(),
            "bare `MAP` is not a declaration type: write a shape (`MAP<name: TYPE, ...>`) or `MAP<ANY>` for maps with unknown keys"
        );
        assert_eq!(
            typing_bare_list().message(),
            "bare `LIST` is not a declaration type: write a shape (`LIST<TYPE>`) or `LIST<ANY>` for lists with unknown elements"
        );
        assert_eq!(
            typing_bare_handle().message(),
            "bare `HANDLE` is not a declaration type: write `HANDLE<T>` for a known payload or `HANDLE<ANY>` for an unknown one"
        );
        assert_eq!(
            typing_no_args("STRING").message(),
            "`STRING` takes no type arguments"
        );
        assert_eq!(
            typing_handle_arity().message(),
            "`HANDLE` takes exactly one bare type argument"
        );
        assert_eq!(
            typing_list_arity().message(),
            "`LIST` takes exactly one bare type argument"
        );
        assert_eq!(
            typing_empty_union().message(),
            "empty union: no member shapes to admit"
        );
        assert_eq!(
            alias_cycle(&["A".to_string(), "B".to_string(), "A".to_string()]).message(),
            "type alias cycle: A -> B -> A"
        );
    }
}
