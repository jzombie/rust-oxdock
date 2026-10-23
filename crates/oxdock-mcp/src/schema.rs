//! JSON Schema projection for exported function signatures.
//!
//! The MCP `tools/list` surface needs machine types, not the DSL's
//! display spellings. This module maps `TypeTag` to JSON Schema objects
//! and rejects opaque host types before the reactor opens (fail fast).

use anyhow::{Result, bail};
use oxdock_parser::TypeTag;
use serde_json::{Map, Value};

/// JSON-compatible leaves. Anything else fails validation naming the type.
fn leaf_type(tag: &TypeTag) -> Option<&'static str> {
    match tag {
        TypeTag::String => Some("string"),
        TypeTag::Int => Some("integer"),
        TypeTag::Float => Some("number"),
        TypeTag::Bool => Some("boolean"),
        TypeTag::List => Some("array"),
        TypeTag::Map => Some("object"),
        TypeTag::Any => None,
        _ => None,
    }
}

/// Emit the JSON Schema for one `TypeTag`. `Any`/ unconstrained renders
/// `{}` (accepts anything). Opaque words bail naming the type so the
/// caller fails fast before serving.
pub fn schema_for_tag(tag: &TypeTag) -> Result<Value> {
    match tag {
        TypeTag::String | TypeTag::Int | TypeTag::Float | TypeTag::Bool => {
            let mut map = Map::new();
            map.insert(
                "type".to_string(),
                Value::String(leaf_type(tag).unwrap_or("string").to_string()),
            );
            Ok(Value::Object(map))
        }
        TypeTag::List => {
            let mut map = Map::new();
            map.insert("type".to_string(), Value::String("array".to_string()));
            Ok(Value::Object(map))
        }
        TypeTag::Map => {
            let mut map = Map::new();
            map.insert("type".to_string(), Value::String("object".to_string()));
            Ok(Value::Object(map))
        }
        TypeTag::Any => Ok(Value::Object(Map::new())),
        TypeTag::Handle => {
            let mut id = Map::new();
            id.insert("type".to_string(), Value::String("integer".to_string()));
            id.insert("minimum".to_string(), Value::Number(1.into()));
            let mut properties = Map::new();
            properties.insert("task_id".to_string(), Value::Object(id));
            let mut map = Map::new();
            map.insert("type".to_string(), Value::String("object".to_string()));
            map.insert("properties".to_string(), Value::Object(properties));
            map.insert(
                "required".to_string(),
                Value::Array(vec![Value::String("task_id".to_string())]),
            );
            map.insert("additionalProperties".to_string(), Value::Bool(false));
            Ok(Value::Object(map))
        }
        TypeTag::ListOf(element) => {
            let mut map = Map::new();
            map.insert("type".to_string(), Value::String("array".to_string()));
            map.insert("items".to_string(), schema_for_tag(element)?);
            Ok(Value::Object(map))
        }
        TypeTag::MapOf(values) => {
            let mut map = Map::new();
            map.insert("type".to_string(), Value::String("object".to_string()));
            map.insert("additionalProperties".to_string(), schema_for_tag(values)?);
            Ok(Value::Object(map))
        }
        TypeTag::Record(fields) => {
            let mut properties = Map::new();
            let mut required = Vec::new();
            for field in fields.iter() {
                properties.insert(field.name.clone(), schema_for_tag(&field.ty)?);
                if !field.optional {
                    required.push(Value::String(field.name.clone()));
                }
            }
            let mut map = Map::new();
            map.insert("type".to_string(), Value::String("object".to_string()));
            map.insert("properties".to_string(), Value::Object(properties));
            if !required.is_empty() {
                map.insert("required".to_string(), Value::Array(required));
            }
            Ok(Value::Object(map))
        }
        TypeTag::Union(_, members) => {
            let mut variants = Vec::with_capacity(members.len());
            for member in members.iter() {
                variants.push(schema_for_tag(member)?);
            }
            let mut map = Map::new();
            map.insert("anyOf".to_string(), Value::Array(variants));
            Ok(Value::Object(map))
        }
        TypeTag::Pipe
        | TypeTag::Duration
        | TypeTag::Path
        | TypeTag::Semaphore
        | TypeTag::Permit
        | TypeTag::Custom(_) => {
            bail!(
                "cannot expose MCP tool with opaque parameter type {}",
                tag_name(tag)
            )
        }
    }
}

/// Short display name for error messages.
fn tag_name(tag: &TypeTag) -> String {
    match tag {
        TypeTag::String => "STRING".to_string(),
        TypeTag::Int => "INT".to_string(),
        TypeTag::Float => "FLOAT".to_string(),
        TypeTag::Bool => "BOOL".to_string(),
        TypeTag::List => "LIST".to_string(),
        TypeTag::Map => "MAP".to_string(),
        TypeTag::Pipe => "PIPE".to_string(),
        TypeTag::Handle => "HANDLE".to_string(),
        TypeTag::Duration => "DURATION".to_string(),
        TypeTag::Path => "PATH".to_string(),
        TypeTag::Semaphore => "SEMAPHORE".to_string(),
        TypeTag::Permit => "PERMIT".to_string(),
        TypeTag::Any => "ANY".to_string(),
        TypeTag::Custom(descriptor) => descriptor.name.to_string(),
        TypeTag::ListOf(element) => format!("LIST<{}>", tag_name(element)),
        TypeTag::MapOf(values) => format!("MAP<{}>", tag_name(values)),
        TypeTag::Record(fields) => {
            let names: Vec<String> = fields
                .iter()
                .map(|field| {
                    if field.optional {
                        format!("{}?", field.name)
                    } else {
                        field.name.clone()
                    }
                })
                .collect();
            format!("MAP<{}>", names.join(", "))
        }
        TypeTag::Union(spelling, _) => spelling.to_string(),
    }
}

/// Whether a tag survives the JSON value boundary. Mirrors the
/// `value_to_json` accept set plus the shaped containers built from it.
/// `HANDLE` projects as `{"task_id": N}`. All other opaque words return
/// false so callers can fail fast with context.
pub fn is_json_compatible(tag: &TypeTag) -> bool {
    match tag {
        TypeTag::String
        | TypeTag::Int
        | TypeTag::Float
        | TypeTag::Bool
        | TypeTag::List
        | TypeTag::Map
        | TypeTag::Any
        | TypeTag::Handle => true,
        TypeTag::ListOf(element) => is_json_compatible(element),
        TypeTag::MapOf(values) => is_json_compatible(values),
        TypeTag::Record(fields) => fields.iter().all(|field| is_json_compatible(&field.ty)),
        TypeTag::Union(_, members) => members.iter().all(is_json_compatible),
        TypeTag::Pipe
        | TypeTag::Duration
        | TypeTag::Path
        | TypeTag::Semaphore
        | TypeTag::Permit
        | TypeTag::Custom(_) => false,
    }
}
