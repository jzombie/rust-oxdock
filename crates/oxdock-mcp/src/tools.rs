//! MCP tool table: from exported `FuncMeta` to rmcp `Tool` plus the
//! JSON-arguments to `Value` boundary.
//!
//! Validation is fail fast: any exported signature containing an opaque
//! host type errors before the reactor opens, never mid session.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Result, bail};
use oxdock_core::{
    FuncMeta, FuncParam, TypeTag, Value, fill_optional_args, json_to_value_typed,
    value_to_json_typed,
};
use serde_json::{Map, Value as JsonValue};

use crate::schema::{is_json_compatible, schema_for_tag};

/// One servable tool: bare MCP name, qualified script name, and the
/// JSON Schemas for its arguments and wrapped result.
#[derive(Debug, Clone)]
pub struct McpTool {
    /// Bare name exposed over MCP (`ADD`).
    pub name: String,
    /// Qualified registry name (`SCRIPT::ADD`).
    pub qualified: String,
    /// Human description from `#` docs with `FuncMeta` fallback.
    pub description: String,
    /// JSON Schema object for the arguments.
    pub input_schema: JsonValue,
    /// JSON Schema object for the wrapped result envelope.
    pub output_schema: JsonValue,
    /// Parameter table in declaration order, for arg mapping.
    pub params: Vec<FuncParam>,
    /// Declared return tag, for result projection. `None` renders
    /// through the untyped boundary.
    pub returns: Option<TypeTag>,
}

/// Build the servable tool table from exported metas and script docs.
/// `docs` maps qualified names to (summary, docs). Errors on duplicate
/// bare names and on opaque parameter or return types.
pub fn build_tool_table(metas: Vec<FuncMeta>, docs: &HashMapDoc) -> Result<Vec<McpTool>> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(metas.len());
    for meta in metas {
        let base = oxdock_parser::base_name(&meta.name).to_string();
        if !seen.insert(base.clone()) {
            bail!("duplicate MCP tool name `{base}`");
        }
        let description = tool_description(&meta, docs);
        let (input_schema, params) = input_schema_for(&meta)?;
        let output_schema = output_schema_for(&meta)?;
        out.push(McpTool {
            name: base,
            qualified: meta.name.clone(),
            description,
            input_schema,
            output_schema,
            params,
            returns: meta.returns.clone(),
        });
    }
    Ok(out)
}

/// Script docs keyed by qualified name.
pub type HashMapDoc = std::collections::HashMap<String, (String, String)>;

/// Description prefers script `#` docs, then `FuncMeta` summary and docs.
/// Single-line comment blocks read as one line, not a doubled line.
fn tool_description(meta: &FuncMeta, docs: &HashMapDoc) -> String {
    if let Some((summary, body)) = docs.get(&meta.name) {
        if body.is_empty() {
            return summary.clone();
        }
        if summary.is_empty() {
            return body.clone();
        }
        if body.starts_with(summary) {
            return body.clone();
        }
        return format!("{summary}\n{body}");
    }
    if meta.docs.is_empty() {
        return meta.summary.to_string();
    }
    if meta.summary.is_empty() {
        return meta.docs.to_string();
    }
    format!("{}\n{}", meta.summary, meta.docs)
}

/// Build the result envelope schema: `{result: <returns>}`. The `result`
/// key keeps `structuredContent` a JSON object (as MCP requires) while
/// the inner schema stays the function's real return type.
pub fn output_schema_for(meta: &FuncMeta) -> Result<JsonValue> {
    let inner = match &meta.returns {
        Some(tag) => schema_for_tag(tag)?,
        None => JsonValue::Object(Map::new()),
    };
    let mut properties = Map::new();
    properties.insert("result".to_string(), inner);
    let mut schema = Map::new();
    schema.insert("type".to_string(), JsonValue::String("object".to_string()));
    schema.insert("properties".to_string(), JsonValue::Object(properties));
    schema.insert(
        "required".to_string(),
        JsonValue::Array(vec![JsonValue::String("result".to_string())]),
    );
    Ok(JsonValue::Object(schema))
}

/// Wrap a return value in the result envelope. MCP `structuredContent`
/// must be a JSON object; the envelope carries the raw value under
/// `result` so primitives survive the boundary intact.
pub fn wrap_structured(value: JsonValue) -> JsonValue {
    let mut map = Map::new();
    map.insert("result".to_string(), value);
    JsonValue::Object(map)
}
/// Build the arguments JSON Schema plus the ordered param table.
/// `params: None` (unconstrained `Value`) renders `{}` with no params.
fn input_schema_for(meta: &FuncMeta) -> Result<(JsonValue, Vec<FuncParam>)> {
    if let Some(returns) = &meta.returns
        && !is_json_compatible(returns)
    {
        bail!(
            "cannot expose MCP tool `{}` with opaque return type",
            oxdock_parser::base_name(&meta.name)
        );
    }
    let Some(params) = &meta.params else {
        return Ok((JsonValue::Object(Map::new()), Vec::new()));
    };
    let mut properties = Map::new();
    let mut required = Vec::new();
    for param in params {
        let Some(tag) = &param.param_type else {
            properties.insert(param.name.clone(), JsonValue::Object(Map::new()));
            required.push(JsonValue::String(param.name.clone()));
            continue;
        };
        if !is_json_compatible(tag) {
            bail!(
                "cannot expose MCP tool `{}` with opaque parameter type for `${}`",
                oxdock_parser::base_name(&meta.name),
                param.name
            );
        }
        let mut slot = schema_for_tag(tag)?;
        if let Some(allowed) = param.allowed
            && let JsonValue::Object(ref mut map) = slot
        {
            let values: Vec<JsonValue> = allowed
                .iter()
                .map(|s| JsonValue::String(s.to_string()))
                .collect();
            map.insert("enum".to_string(), JsonValue::Array(values));
        }
        if !param.docs.is_empty()
            && let JsonValue::Object(ref mut map) = slot
        {
            map.insert(
                "description".to_string(),
                JsonValue::String(param.docs.to_string()),
            );
        }
        properties.insert(param.name.clone(), slot);
        if !param.optional {
            required.push(JsonValue::String(param.name.clone()));
        }
    }
    let mut schema = Map::new();
    schema.insert("type".to_string(), JsonValue::String("object".to_string()));
    schema.insert("properties".to_string(), JsonValue::Object(properties));
    if !required.is_empty() {
        schema.insert("required".to_string(), JsonValue::Array(required));
    }
    Ok((JsonValue::Object(schema), params.clone()))
}

/// Map a JSON arguments object to ordered `Value` words per the param
/// table. Missing optional trailing MAP params fill `{}`; missing
/// required params and surplus keys both fail.
pub fn map_json_args(params: &[FuncParam], args: &JsonValue) -> Result<Vec<Value>> {
    let empty = Map::new();
    let object = match args {
        JsonValue::Object(map) => map,
        JsonValue::Null => &empty,
        _ => bail!("MCP tool arguments must be a JSON object"),
    };
    let required = oxdock_core::required_arity(params);
    let mut values = Vec::with_capacity(params.len());
    for param in params {
        match object.get(&param.name) {
            Some(json) => values.push(json_to_value_typed(
                param.param_type.as_ref(),
                json.clone(),
            )?),
            None => {
                if param.optional {
                    values.push(Value::map(BTreeMap::new()));
                } else {
                    bail!("missing required MCP argument `{}`", param.name);
                }
            }
        }
    }
    if values.len() < required || values.len() > params.len() {
        bail!(
            "MCP argument count {} out of range {}..={}",
            values.len(),
            required,
            params.len()
        );
    }
    for key in object.keys() {
        if !params.iter().any(|param| &param.name == key) {
            bail!("unknown MCP argument `{key}`");
        }
    }
    Ok(fill_optional_args(params, values))
}

/// Serialize a return `Value` to JSON through the declared return
/// tag. `HANDLE` projects as `{"task_id": N}`; every other tag rides
/// the untyped boundary. Opaque words fail naming the type.
pub fn value_to_json_result(tag: Option<&TypeTag>, value: &Value) -> Result<JsonValue> {
    value_to_json_typed(tag, value)
}
