//! Static type tags for function metadata and variable bindings.
//!
//! The DSL surface stays text (`LET $x: MAP`, `DESCRIBE`), but everything
//! the engine reasons about is a [`TypeTag`]: builtins are enum variants
//! a typo cannot spell, host handles carry their descriptor, and shaped
//! maps carry their field table. Strings survive only at display and
//! parse boundaries through [`TypeTag::name`] and [`TypeTag::builtin`].

use anyhow::{Result, bail};

use crate::{TypeDescriptor, Value};

/// One named field of a [`TypeTag::Record`] schema.
#[derive(Debug, Clone, Copy)]
pub struct Field {
    /// Field key as it appears in the value map.
    pub name: &'static str,
    /// Expected type of the field value. Nests freely.
    pub ty: TypeTag,
}

/// A declarative type for function params, returns, and bindings.
///
/// Builtins are variants, so exhaustiveness is compiler-checked wherever
/// tags are matched. [`TypeTag::Custom`] carries a host descriptor for
/// opaque handles. [`TypeTag::ListOf`] and [`TypeTag::Record`] shape
/// composite values: a bare `List`/`Map` accepts anything of that word
/// kind, while the shaped forms lock fields and elements in code.
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
    Custom(&'static TypeDescriptor),
    ListOf(&'static TypeTag),
    Record(&'static [Field]),
}
impl std::fmt::Debug for TypeTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeTag::Custom(descriptor) => {
                write!(f, "Custom({})", descriptor.name)
            }
            TypeTag::ListOf(element) => write!(f, "ListOf({element:?})"),
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
            TypeTag::Custom(descriptor) => descriptor.name,
            TypeTag::ListOf(_) => "LIST",
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

/// Check a value against a tag, failing naming the expected shape and
/// the offending path. Records reject missing AND extra fields: a
/// schema is a lock, not a hint.
pub fn check_value(tag: &TypeTag, value: &Value) -> Result<()> {
    check_value_at(tag, value, "$")
}

fn check_value_at(tag: &TypeTag, value: &Value, path: &str) -> Result<()> {
    match tag {
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
        TypeTag::Record(fields) => match value.as_map() {
            Some(map) => {
                for field in *fields {
                    match map.get(field.name) {
                        Some(item) => {
                            check_value_at(&field.ty, item, &format!("{path}.{}", field.name))?;
                        }
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
            "STRING", "INT", "FLOAT", "BOOL", "LIST", "MAP", "PIPE", "HANDLE", "DURATION",
            "PATH", "SEMAPHORE", "PERMIT",
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
            Field { name: "a", ty: TypeTag::String },
            Field { name: "b", ty: TypeTag::Int },
        ];
        let tag = TypeTag::Record(FIELDS);
        let good = map(&[
            ("a", Value::string("x".to_string())),
            ("b", Value::int(1)),
        ]);
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
    fn list_of_checks_elements_with_index() {
        let tag = TypeTag::ListOf(&TypeTag::Int);
        let good = Value::list(vec![Value::int(1), Value::int(2)]);
        assert!(check_value(&tag, &good).is_ok());
        let bad = Value::list(vec![Value::int(1), Value::string("x".to_string())]);
        let err = check_value(&tag, &bad).expect_err("bad element must fail");
        assert!(format!("{err:#}").contains("[1]"), "names the index: {err:#}");
    }
}
