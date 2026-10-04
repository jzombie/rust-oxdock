//! Static type tags for function metadata and variable bindings.
//!
//! The DSL surface stays text (`LET $x: MAP`, `DESCRIBE`), but everything
//! the engine reasons about is a [`TypeTag`]: builtins are enum variants
//! a typo cannot spell, host handles carry their descriptor, and shaped
//! maps carry their field table. Strings survive only at display and
//! parse boundaries through [`TypeTag::name`] and [`TypeTag::builtin`].

use anyhow::{Result, bail};

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::{TypeDescriptor, Value};

/// Maximum generic nesting for one spelling: syntactic `<` levels
/// and alias hops share this budget at resolution, so neither deep
/// nesting nor long alias chains can wedge the pass. Parser-bounded
/// scripts never approach it; exceeding it bails naming the shape.
pub const MAX_GENERIC_DEPTH: usize = 8;

/// One named field of a [`TypeTag::Record`] schema.
#[derive(Debug, Clone, Copy)]
pub struct Field {
    /// Field key as it appears in the value map.
    pub name: &'static str,
    /// Expected type of the field value. Nests freely.
    pub ty: TypeTag,
    /// What the field carries. Hand-registered schemas document every
    /// field; fields synthesized from inline spellings carry none.
    pub docs: &'static str,
    /// Optional fields (`MAP<permit?: PERMIT>`) may be absent: missing
    /// optional keys pass, missing required keys fail, extra keys
    /// always fail.
    pub optional: bool,
}

/// A declarative type for function params, returns, and bindings.
///
/// Builtins are variants, so exhaustiveness is compiler-checked wherever
/// tags are matched. [`TypeTag::Custom`] carries a host descriptor for
/// opaque handles. [`TypeTag::ListOf`], [`TypeTag::Record`], and
/// [`TypeTag::MapOf`] shape composite values: bare `List` and `Map`
/// are runtime words only, never declaration spellings. Unknown
/// lists declare `LIST<ANY>`; unknown maps declare [`TypeTag::MapOf`]
/// with [`TypeTag::Any`] values (`MAP<ANY>`).
#[derive(Clone, Copy)]
pub enum TypeTag {
    String,
    Int,
    Float,
    Bool,
    List,
    Map,
    Pipe,
    Handle,
    Duration,
    Path,
    Semaphore,
    Permit,
    /// Every word: for parameters that accept anything by design
    /// (coercion inputs, generic carriers). Boundary and coercion
    /// both accept unconditionally, so the tag never lies.
    Any,
    Custom(&'static TypeDescriptor),
    ListOf(&'static TypeTag),
    Record(&'static [Field]),
    MapOf(&'static TypeTag),
}

/// The `ANY` word for composed tags: one shared instance so generic
/// spellings like `MAP<ANY>` never mint competing statics.
pub const ANY_TAG: TypeTag = TypeTag::Any;

/// The MAP word for `TypeTag::ListOf` metadata: one shared instance
/// so generated registrations never mint competing statics.
pub const MAP_TAG: TypeTag = TypeTag::Map;

/// The LIST word for `TypeTag` metadata, matching `MAP_TAG`.
pub const LIST_TAG: TypeTag = TypeTag::List;
impl std::fmt::Debug for TypeTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeTag::Custom(descriptor) => {
                write!(f, "Custom({})", descriptor.name)
            }
            TypeTag::ListOf(element) => write!(f, "ListOf({element:?})"),
            TypeTag::MapOf(values) => write!(f, "MapOf({values:?})"),
            TypeTag::Record(fields) => {
                let names: Vec<&str> = fields.iter().map(|field| field.name).collect();
                write!(f, "Record({})", names.join(", "))
            }
            tag => write!(f, "{}", tag.name()),
        }
    }
}

impl std::fmt::Display for TypeTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
impl TypeTag {
    /// Display name: the builtin label, the custom descriptor name, or
    /// the coarse word kind for shaped composites. Field detail renders
    /// structurally, never through this string.
    pub fn name(&self) -> &'static str {
        match self {
            TypeTag::String => "STRING",
            TypeTag::Int => "INT",
            TypeTag::Float => "FLOAT",
            TypeTag::Bool => "BOOL",
            TypeTag::List => "LIST",
            TypeTag::Map => "MAP",
            TypeTag::Pipe => "PIPE",
            TypeTag::Handle => "HANDLE",
            TypeTag::Duration => "DURATION",
            TypeTag::Path => "PATH",
            TypeTag::Semaphore => "SEMAPHORE",
            TypeTag::Permit => "PERMIT",
            TypeTag::Any => "ANY",
            TypeTag::Custom(descriptor) => descriptor.name,
            TypeTag::ListOf(_) => "LIST",
            TypeTag::MapOf(_) => "MAP",
            TypeTag::Record(_) => "MAP",
        }
    }

    /// Resolve a builtin label to its tag. Custom, shaped, and unknown
    /// names return `None`: customs resolve through the run's type
    /// directory, shapes through the schema directory.
    pub fn builtin(name: &str) -> Option<TypeTag> {
        match name {
            "STRING" => Some(TypeTag::String),
            "INT" => Some(TypeTag::Int),
            "FLOAT" => Some(TypeTag::Float),
            "BOOL" => Some(TypeTag::Bool),
            "LIST" => Some(TypeTag::List),
            "MAP" => Some(TypeTag::Map),
            "PIPE" => Some(TypeTag::Pipe),
            "HANDLE" => Some(TypeTag::Handle),
            "DURATION" => Some(TypeTag::Duration),
            "PATH" => Some(TypeTag::Path),
            "SEMAPHORE" => Some(TypeTag::Semaphore),
            "PERMIT" => Some(TypeTag::Permit),
            "ANY" => Some(TypeTag::Any),
            _ => None,
        }
    }

    /// Field table for [`TypeTag::Record`], `None` otherwise.
    pub fn fields(&self) -> Option<&'static [Field]> {
        match self {
            TypeTag::Record(fields) => Some(fields),
            _ => None,
        }
    }
}

/// Render one tag structurally: shaped tags name their contents
/// (`LIST<MAP>`, `MAP<name: TYPE, ...>`), so signatures show the
/// generics the extractor enforces instead of the coarse word
/// kind. Single renderer for `DESCRIBE` and docs-gen alike.
pub fn render_structural(tag: &TypeTag) -> String {
    match tag {
        TypeTag::ListOf(inner) => format!("LIST<{}>", render_structural(inner)),
        TypeTag::MapOf(values) => format!("MAP<{}>", render_structural(values)),
        TypeTag::Record(fields) => {
            let field_list = fields
                .iter()
                .map(|field| {
                    format!(
                        "{}{}: {}",
                        field.name,
                        if field.optional { "?" } else { "" },
                        render_structural(&field.ty)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("MAP<{field_list}>")
        }
        tag => tag.name().to_string(),
    }
}

/// One parsed type spelling: a bare name or a generic application
/// with positional (`LIST<MAP>`) or named (`MAP<name: STRING>`)
/// arguments. A named field with a `?` suffix is optional
/// (`MAP<permit?: PERMIT>`). Arity and shape enforce at resolution,
/// never here: `LIST<A, B>` and `STRING<INT>` parse and fail later
/// naming the rule each breaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spelled {
    Named(String),
    Generic {
        name: String,
        args: Vec<SpelledField>,
    },
}

/// One generic argument: a bare type or a named (possibly optional)
/// record field carrying its nested spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpelledField {
    pub name: Option<String>,
    pub optional: bool,
    pub ty: Spelled,
}

/// Canonical spelling: exactly the structural representation
/// (`MAP<name: STRING, age: INT>`), so stored declarations, intern
/// keys, alias targets, and rendered signatures are one form, never
/// two. Any input spacing parses to the same shape and renders to
/// this form, which keeps every parse pathway in agreement.
/// Malformed input passes through trimmed for the resolver to
/// reject by name.
pub fn canonicalize_spelling(s: &str) -> String {
    let stripped: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    match parse_spelling(&stripped) {
        Ok(spelled) => render_spelled(&spelled),
        Err(_) => s.trim().to_string(),
    }
}

/// Render one parsed spelling in canonical form: `LIST<MAP>`,
/// `MAP<name: STRING, age: INT>`. Deterministic by construction,
/// so equal shapes share intern entries.
pub fn render_spelled(spelled: &Spelled) -> String {
    match spelled {
        Spelled::Named(name) => name.clone(),
        Spelled::Generic { name, args } => {
            let rendered: Vec<String> = args
                .iter()
                .map(|field| match &field.name {
                    Some(field_name) => format!(
                        "{}{}: {}",
                        field_name,
                        if field.optional { "?" } else { "" },
                        render_spelled(&field.ty)
                    ),
                    None => render_spelled(&field.ty),
                })
                .collect();
            format!("{name}<{}>", rendered.join(", "))
        }
    }
}

/// Parse a spelling into its shape. Hand-rolled descent: the
/// grammar guarantees shape for parser strings, this keeps the
/// public resolution API safe for direct callers. All whitespace
/// is stripped before parsing, so any spaced form parses.
pub fn parse_spelling(s: &str) -> Result<Spelled, String> {
    let text: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let (spelled, rest) = parse_spelled_one(&text, 0)?;
    if !rest.is_empty() {
        return Err(format!(
            "unexpected trailing text in type spelling: {rest:?}"
        ));
    }
    Ok(spelled)
}

fn parse_spelled_one(text: &str, depth: usize) -> Result<(Spelled, &str), String> {
    if depth > MAX_GENERIC_DEPTH {
        return Err(format!(
            "type spelling exceeds maximum generic depth of {MAX_GENERIC_DEPTH}: {text:?}"
        ));
    }
    let mut chars = text.char_indices();
    let Some((_, first)) = chars.next() else {
        return Err("expected type name, found end of spelling".to_string());
    };
    if !first.is_ascii_uppercase() {
        return Err(format!("expected uppercase type name, found {first:?}"));
    }
    let mut end = first.len_utf8();
    for (idx, ch) in chars {
        if ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_' {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    let (name, mut rest) = text.split_at(end);
    let name = name.to_string();
    if !rest.starts_with('<') {
        return Ok((Spelled::Named(name), rest));
    }
    rest = &rest[1..];
    let mut args = Vec::new();
    loop {
        let (field_name, optional, after_name) = parse_spelled_field_name(rest);
        let (arg, after_arg) = parse_spelled_one(after_name, depth + 1)?;
        args.push(SpelledField {
            name: field_name,
            optional,
            ty: arg,
        });
        rest = after_arg;
        if let Some(tail) = rest.strip_prefix(',') {
            rest = tail;
            continue;
        }
        if let Some(tail) = rest.strip_prefix('>') {
            rest = tail;
            break;
        }
        return Err(format!(
            "expected ',' or '>' in type spelling, found {rest:?}"
        ));
    }
    Ok((Spelled::Generic { name, args }, rest))
}

/// Optional `field:` or `field?:` prefix of one generic argument. A
/// lowercase name followed by an optional `?` and `:` claims the
/// prefix (recording optionality); anything else leaves the text
/// untouched for the bare-argument path.
fn parse_spelled_field_name(text: &str) -> (Option<String>, bool, &str) {
    let mut chars = text.char_indices();
    let Some((_, first)) = chars.next() else {
        return (None, false, text);
    };
    if !first.is_ascii_lowercase() {
        return (None, false, text);
    };
    let mut end = first.len_utf8();
    for (idx, ch) in chars {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    let (candidate, rest) = text.split_at(end);
    let (optional, rest) = match rest.strip_prefix('?') {
        Some(after) => (true, after),
        None => (false, rest),
    };
    match rest.strip_prefix(':') {
        Some(after) => (Some(candidate.to_string()), optional, after),
        None => (None, false, text),
    }
}

/// Process-global intern table for composed tags: one shared
/// `&'static TypeTag` per distinct canonical spelling. The lock is
/// held only for lookup/insert; leaf resolution happens outside it
/// in the caller.
static COMPOSED_TAGS: OnceLock<Mutex<HashMap<String, &'static TypeTag>>> = OnceLock::new();

fn composed_table() -> &'static Mutex<HashMap<String, &'static TypeTag>> {
    COMPOSED_TAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Look up an already-interned spelling. Resolution checks this
/// before building, so identical spellings share one pointer.
pub fn lookup_composed(canonical: &str) -> Option<&'static TypeTag> {
    composed_table()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(canonical)
        .copied()
}

/// Intern one composed tag under its canonical spelling: a raced
/// build stores once (the loser drops its duplicate leaves, still
/// bounded by distinct spellings per process). Exactly one leak
/// per distinct spelling per process: the leak is the point (a
/// process-global `'static` home for composed tags), localized
/// here so the rest of the crate stays deny-clean.
#[allow(clippy::disallowed_methods)]
pub fn intern_composed(canonical: String, tag: TypeTag) -> &'static TypeTag {
    let mut table = composed_table()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(hit) = table.get(&canonical).copied() {
        return hit;
    }
    let leaked: &'static TypeTag = Box::leak(Box::new(tag));
    table.insert(canonical, leaked);
    leaked
}

/// Check a value against a tag, failing naming the expected shape and
/// the offending path. Records reject missing AND extra fields: a
/// schema is a lock, not a hint.
pub fn check_value(tag: &TypeTag, value: &Value) -> Result<()> {
    check_value_at(tag, value, "$")
}

/// Shape conformance with a caller-chosen path prefix: the single
/// implementation behind generated parameter extractors, so boundary
/// errors name the function, the parameter, and the nested position
/// (`F() argument `$maps`[1]: expected MAP, got INT`) with no second
/// copy of the shape logic anywhere.
pub fn check_value_at(tag: &TypeTag, value: &Value, path: &str) -> Result<()> {
    match tag {
        TypeTag::Any => Ok(()),
        TypeTag::String if value.as_str().is_some() => Ok(()),
        TypeTag::Int if value.as_i64().is_some() => Ok(()),
        TypeTag::Float if value.as_f64().is_some() => Ok(()),
        TypeTag::Bool if value.as_bool().is_some() => Ok(()),
        TypeTag::Pipe if value.as_pipe_handle().is_some() => Ok(()),
        TypeTag::Handle if value.as_handle().is_some() => Ok(()),
        TypeTag::Duration if value.as_duration().is_some() => Ok(()),
        TypeTag::Path if value.as_path().is_some() => Ok(()),
        TypeTag::Semaphore if value.as_semaphore().is_some() => Ok(()),
        TypeTag::Permit if value.as_permit().is_some() => Ok(()),
        TypeTag::List => match value.as_list() {
            Some(_) => Ok(()),
            None => bail!("{path}: expected LIST, got {}", value.type_name()),
        },
        TypeTag::Map => match value.as_map() {
            Some(_) => Ok(()),
            None => bail!("{path}: expected MAP, got {}", value.type_name()),
        },
        TypeTag::ListOf(element) => match value.as_list() {
            Some(items) => {
                for (idx, item) in items.iter().enumerate() {
                    check_value_at(element, item, &format!("{path}[{idx}]"))?;
                }
                Ok(())
            }
            None => bail!("{path}: expected LIST, got {}", value.type_name()),
        },
        TypeTag::MapOf(values) => match value.as_map() {
            Some(map) => {
                for (key, item) in map.iter() {
                    check_value_at(values, item, &format!("{path}.{key}"))?;
                }
                Ok(())
            }
            None => bail!(
                "{path}: expected {}, got {}",
                render_structural(tag),
                value.type_name()
            ),
        },
        TypeTag::Record(fields) => match value.as_map() {
            Some(map) => {
                for field in *fields {
                    match map.get(field.name) {
                        Some(item) => {
                            check_value_at(&field.ty, item, &format!("{path}.{}", field.name))?;
                        }
                        None if field.optional => {}
                        None => bail!(
                            "{path}: missing field '{}'; expected fields: {}",
                            field.name,
                            field_list(fields),
                        ),
                    }
                }
                for key in map.keys() {
                    if !fields.iter().any(|field| field.name == key) {
                        bail!(
                            "{path}: unknown field '{key}'; expected fields: {}",
                            field_list(fields),
                        );
                    }
                }
                Ok(())
            }
            None => bail!("{path}: expected MAP, got {}", value.type_name()),
        },
        TypeTag::Custom(descriptor) => {
            if value.type_name() == descriptor.name {
                Ok(())
            } else {
                bail!(
                    "{path}: expected {}, got {}",
                    descriptor.name,
                    value.type_name(),
                )
            }
        }
        _ => bail!("{path}: expected {}, got {}", tag.name(), value.type_name()),
    }
}

fn field_list(fields: &[Field]) -> String {
    fields
        .iter()
        .map(|field| field.name)
        .collect::<Vec<_>>()
        .join(", ")
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
    fn builtin_round_trips_through_names() {
        for name in [
            "STRING",
            "INT",
            "FLOAT",
            "BOOL",
            "LIST",
            "MAP",
            "PIPE",
            "HANDLE",
            "DURATION",
            "PATH",
            "SEMAPHORE",
            "PERMIT",
        ] {
            let tag = TypeTag::builtin(name).expect("builtin");
            assert_eq!(tag.name(), name);
        }
        assert!(TypeTag::builtin("NOPE").is_none());
        assert!(TypeTag::builtin("string").is_none());
    }

    #[test]
    fn scalars_conform_by_discriminant() {
        assert!(check_value(&TypeTag::String, &Value::string("x".to_string())).is_ok());
        assert!(check_value(&TypeTag::Int, &Value::int(1)).is_ok());
        assert!(check_value(&TypeTag::String, &Value::int(1)).is_err());
        // No cross coercion here: 3.0 is FLOAT, not INT.
        assert!(check_value(&TypeTag::Int, &Value::float(3.0)).is_err());
    }

    #[test]
    fn record_rejects_missing_and_extra_fields() {
        static FIELDS: &[Field] = &[
            Field {
                name: "a",
                ty: TypeTag::String,
                docs: "",
                optional: false,
            },
            Field {
                name: "b",
                ty: TypeTag::Int,
                docs: "",
                optional: false,
            },
        ];
        let tag = TypeTag::Record(FIELDS);
        let good = map(&[("a", Value::string("x".to_string())), ("b", Value::int(1))]);
        assert!(check_value(&tag, &good).is_ok());
        let missing = map(&[("a", Value::string("x".to_string()))]);
        let err = check_value(&tag, &missing).expect_err("missing must fail");
        assert!(format!("{err:#}").contains('b'), "names the field: {err:#}");
        let extra = map(&[
            ("a", Value::string("x".to_string())),
            ("b", Value::int(1)),
            ("c", Value::int(2)),
        ]);
        let err = check_value(&tag, &extra).expect_err("extra must fail");
        assert!(format!("{err:#}").contains('c'), "names the field: {err:#}");
        let mistyped = map(&[
            ("a", Value::string("x".to_string())),
            ("b", Value::string("nope".to_string())),
        ]);
        assert!(check_value(&tag, &mistyped).is_err());
    }

    #[test]
    fn record_optional_fields_may_be_absent() {
        // Optional fields validate when present and pass when absent;
        // required fields and extra keys still fail exactly.
        static FIELDS: &[Field] = &[
            Field {
                name: "held",
                ty: TypeTag::Int,
                docs: "",
                optional: false,
            },
            Field {
                name: "permit",
                ty: TypeTag::String,
                docs: "",
                optional: true,
            },
        ];
        let tag = TypeTag::Record(FIELDS);
        let full = map(&[
            ("held", Value::int(1)),
            ("permit", Value::string("p".to_string())),
        ]);
        assert!(check_value(&tag, &full).is_ok());
        let missing_optional = map(&[("held", Value::int(0))]);
        assert!(check_value(&tag, &missing_optional).is_ok());
        let missing_required = map(&[("permit", Value::string("p".to_string()))]);
        let err = check_value(&tag, &missing_required).expect_err("missing required must fail");
        assert!(
            format!("{err:#}").contains("held"),
            "names the field: {err:#}"
        );
        let mistyped_optional = map(&[("held", Value::int(1)), ("permit", Value::int(2))]);
        assert!(check_value(&tag, &mistyped_optional).is_err());
    }

    #[test]
    fn list_of_checks_elements_with_index() {
        let tag = TypeTag::ListOf(&TypeTag::Int);
        let good = Value::list(vec![Value::int(1), Value::int(2)]);
        assert!(check_value(&tag, &good).is_ok());
        let bad = Value::list(vec![Value::int(1), Value::string("x".to_string())]);
        let err = check_value(&tag, &bad).expect_err("bad element must fail");
        assert!(
            format!("{err:#}").contains("[1]"),
            "names the index: {err:#}"
        );
    }

    #[test]
    fn canonicalization_unifies_input_spacing() {
        // Every spaced form lands on the structural representation:
        // the stored, keyed, and rendered form is one form.
        assert_eq!(canonicalize_spelling("LIST<MAP>"), "LIST<MAP>");
        assert_eq!(canonicalize_spelling("LIST< MAP >"), "LIST<MAP>");
        assert_eq!(
            canonicalize_spelling("MAP<name:STRING,age:INT>"),
            "MAP<name: STRING, age: INT>"
        );
        assert_eq!(
            canonicalize_spelling("MAP<name : STRING>"),
            "MAP<name: STRING>"
        );
    }

    #[test]
    fn spellings_parse_to_shapes() {
        assert_eq!(
            parse_spelling("LIST<MAP>").expect("parses"),
            Spelled::Generic {
                name: "LIST".to_string(),
                args: vec![SpelledField {
                    name: None,
                    optional: false,
                    ty: Spelled::Named("MAP".to_string()),
                }],
            }
        );
        assert_eq!(
            parse_spelling("MAP<name: STRING>").expect("parses"),
            Spelled::Generic {
                name: "MAP".to_string(),
                args: vec![SpelledField {
                    name: Some("name".to_string()),
                    optional: false,
                    ty: Spelled::Named("STRING".to_string()),
                }],
            }
        );
        // Arity violations parse; resolution rejects them naming
        // the rule (`LIST` arity, `MAP` bare fields).
        assert!(parse_spelling("LIST<A, B>").is_ok());
        assert!(parse_spelling("MAP<STRING>").is_ok());
    }

    #[test]
    fn optional_field_spelling_parses_and_renders() {
        // A `?` suffix marks one field optional: it parses into the
        // flag, canonicalizes with the mark, and renders back.
        assert_eq!(
            parse_spelling("MAP<permit?: PERMIT>").expect("parses"),
            Spelled::Generic {
                name: "MAP".to_string(),
                args: vec![SpelledField {
                    name: Some("permit".to_string()),
                    optional: true,
                    ty: Spelled::Named("PERMIT".to_string()),
                }],
            }
        );
        assert_eq!(
            canonicalize_spelling("MAP<permit ? : PERMIT>"),
            "MAP<permit?: PERMIT>"
        );
    }

    #[test]
    fn unknown_map_spelling_parses_and_renders() {
        // `MAP<ANY>` is the explicit unknown map: it parses like any
        // other generic spelling and renders back to itself.
        assert_eq!(
            parse_spelling("MAP<ANY>").expect("parses"),
            Spelled::Generic {
                name: "MAP".to_string(),
                args: vec![SpelledField {
                    name: None,
                    optional: false,
                    ty: Spelled::Named("ANY".to_string()),
                }],
            }
        );
        assert_eq!(canonicalize_spelling("MAP< ANY >"), "MAP<ANY>");
        assert_eq!(
            render_structural(&TypeTag::MapOf(&TypeTag::Any)),
            "MAP<ANY>"
        );
    }

    #[test]
    fn map_of_checks_values_with_paths() {
        // Value shapes walk every entry with key paths; non-maps fail
        // naming the structural shape, never the coarse word.
        let tag = TypeTag::MapOf(&TypeTag::Int);
        let good = map(&[("a", Value::int(1))]);
        assert!(check_value(&tag, &good).is_ok());
        let bad = map(&[("a", Value::string("x".to_string()))]);
        let err = check_value(&tag, &bad).expect_err("bad value must fail");
        assert!(format!("{err:#}").contains(".a"), "names the key: {err:#}");
        let not_map = Value::list(vec![Value::int(1)]);
        let err = check_value(&tag, &not_map).expect_err("non-map must fail");
        assert!(format!("{err:#}").contains("MAP<INT>"), "{err:#}");
    }

    #[test]
    fn spellings_reject_malformed_input() {
        for bad in [
            "",
            "LIST<>",
            "LIST<A,>",
            "MAP<name:>",
            "LIST<A",
            "list<MAP>",
            "LIST<A>>",
        ] {
            assert!(parse_spelling(bad).is_err(), "{bad:?} must fail");
        }
        // Depth cap counts syntactic nesting; deeper bails naming it.
        let mut deep = "INT".to_string();
        for _ in 0..MAX_GENERIC_DEPTH + 1 {
            deep = format!("LIST<{deep}>");
        }
        let err = parse_spelling(&deep).expect_err("over-deep spelling must fail");
        assert!(err.contains("maximum generic depth"), "{err}");
    }

    #[test]
    fn identical_spellings_share_one_interned_pointer() {
        let first = intern_composed("LIST<MAP>".to_string(), TypeTag::ListOf(&TypeTag::Map));
        let second = intern_composed("LIST<MAP>".to_string(), TypeTag::ListOf(&TypeTag::Map));
        assert!(
            std::ptr::eq(first, second),
            "same canonical spelling interns once"
        );
    }

    #[test]
    fn structural_rendering_names_contents() {
        static FIELDS: &[Field] = &[Field {
            name: "name",
            ty: TypeTag::String,
            docs: "",
            optional: false,
        }];
        static RECORD: TypeTag = TypeTag::Record(FIELDS);
        static LIST: TypeTag = TypeTag::ListOf(&RECORD);
        assert_eq!(render_structural(&LIST), "LIST<MAP<name: STRING>>");
        assert_eq!(render_structural(&TypeTag::Int), "INT");
    }
}
