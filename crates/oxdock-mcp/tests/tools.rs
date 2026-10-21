//! Tool table, JSON Schema projection, and argument mapping.

use oxdock_core::{FuncMeta, FuncParam};
use oxdock_mcp::{
    build_tool_table,
    schema::{is_json_compatible, schema_for_tag},
    tools::map_json_args,
};
use oxdock_parser::TypeTag;
use serde_json::json;

use std::sync::Arc;

fn param(name: &str, tag: TypeTag) -> FuncParam {
    FuncParam {
        name: name.to_string(),
        param_type: Some(tag),
        allowed: None,
        docs: "",
        options: None,
        optional: false,
    }
}

fn meta(name: &str, params: Vec<FuncParam>) -> FuncMeta {
    FuncMeta {
        name: name.to_string(),
        module: "SCRIPT".to_string(),
        kind: oxdock_core::FuncKind::Script,
        params: Some(params),
        returns: None,
        rpn: false,
        summary: "summary",
        docs: "docs",
        examples: &[],
    }
}

#[test]
fn schema_leaves_map_to_json_types() {
    let cases = [
        (TypeTag::String, "string"),
        (TypeTag::Int, "integer"),
        (TypeTag::Float, "number"),
        (TypeTag::Bool, "boolean"),
        (TypeTag::List, "array"),
        (TypeTag::Map, "object"),
    ];
    for (tag, expected) in cases {
        let schema = schema_for_tag(&tag).expect("leaf schema");
        assert_eq!(schema["type"], expected, "{tag:?}");
    }
    assert!(schema_for_tag(&TypeTag::Any).expect("any").is_object());
}

#[test]
fn schema_nesting_projects() {
    let list_int = schema_for_tag(&TypeTag::ListOf(Arc::new(TypeTag::Int)))
        .expect("list schema");
    assert_eq!(list_int["type"], "array");
    assert_eq!(list_int["items"]["type"], "integer");

    let map_values = schema_for_tag(&TypeTag::MapOf(Arc::new(TypeTag::String)))
        .expect("map schema");
    assert_eq!(map_values["type"], "object");
    assert_eq!(map_values["additionalProperties"]["type"], "string");

    let record = schema_for_tag(&TypeTag::Record(
        vec![
            oxdock_parser::Field {
                name: "name".to_string(),
                ty: TypeTag::String,
                docs: "",
                optional: false,
            },
            oxdock_parser::Field {
                name: "nick".to_string(),
                ty: TypeTag::String,
                docs: "",
                optional: true,
            },
        ]
        .into(),
    ))
    .expect("record schema");
    assert_eq!(record["type"], "object");
    assert_eq!(record["required"], json!(["name"]));

    let union = schema_for_tag(&TypeTag::Union(
        "STRING|INT".into(),
        vec![TypeTag::String, TypeTag::Int].into(),
    ))
    .expect("union schema");
    assert_eq!(union["anyOf"][0]["type"], "string");
    assert_eq!(union["anyOf"][1]["type"], "integer");
}

#[test]
fn schema_rejects_opaque_types() {
    for tag in [
        TypeTag::Pipe,
        TypeTag::Handle,
        TypeTag::Duration,
        TypeTag::Path,
        TypeTag::Semaphore,
        TypeTag::Permit,
    ] {
        schema_for_tag(&tag).expect_err(format!("{tag:?} must fail").as_str());
        assert!(!is_json_compatible(&tag));
    }
    assert!(is_json_compatible(&TypeTag::ListOf(Arc::new(
        TypeTag::Int
    ))));
    assert!(!is_json_compatible(&TypeTag::ListOf(Arc::new(
        TypeTag::Pipe
    ))));
}

#[test]
fn tool_table_builds_schema_and_validates() {
    let metas = vec![meta(
        "SCRIPT::ADD",
        vec![param("a", TypeTag::Int), param("b", TypeTag::Int)],
    )];
    let table = build_tool_table(metas, &Default::default()).expect("table");
    assert_eq!(table.len(), 1);
    assert_eq!(table[0].name, "ADD");
    assert_eq!(table[0].qualified, "SCRIPT::ADD");
    assert_eq!(table[0].input_schema["required"], json!(["a", "b"]));
}

#[test]
fn tool_table_rejects_duplicates_and_opaque() {
    let dupes = vec![
        meta("SCRIPT::FOO", vec![]),
        meta("SCRIPT::FOO", vec![]),
    ];
    build_tool_table(dupes, &Default::default()).expect_err("duplicates must fail");

    let opaque = vec![meta("SCRIPT::P", vec![param("p", TypeTag::Pipe)])];
    build_tool_table(opaque, &Default::default()).expect_err("opaque params must fail");

    let mut returns_pipe = meta("SCRIPT::Q", vec![]);
    returns_pipe.returns = Some(TypeTag::Handle);
    build_tool_table(vec![returns_pipe], &Default::default())
        .expect_err("opaque returns must fail");
}

#[test]
fn tool_description_prefers_script_docs() {
    use std::collections::HashMap;
    let metas = vec![meta("SCRIPT::FOO", vec![])];
    let mut docs = HashMap::new();
    docs.insert(
        "SCRIPT::FOO".to_string(),
        ("Adds one.".to_string(), "Adds one.\nSecond line.".to_string()),
    );
    let table = build_tool_table(metas, &docs).expect("table");
    assert_eq!(table[0].description, "Adds one.\nSecond line.");
}

#[test]
fn map_json_args_orders_and_fills() {
    let params = vec![param("a", TypeTag::Int), param("b", TypeTag::Int)];
    let values = map_json_args(&params, &json!({"b": 3, "a": 2})).expect("ordered");
    assert_eq!(values.len(), 2);
    assert_eq!(values[0].as_i64(), Some(2));
    assert_eq!(values[1].as_i64(), Some(3));

    map_json_args(&params, &json!({"a": 1})).expect_err("missing required must fail");
    map_json_args(&params, &json!({"a": 1, "b": 2, "c": 3}))
        .expect_err("surplus must fail");
    map_json_args(&params, &json!([1, 2])).expect_err("non-object must fail");
}

#[test]
fn output_envelope_is_always_an_object() {
    use oxdock_mcp::{output_schema_for, wrap_structured};
    // Bare primitives wrap intact: the client-reported contract bug.
    for value in [json!(6), json!("hi"), json!(true), json!([1, 2])] {
        let wrapped = wrap_structured(value.clone());
        assert!(wrapped.is_object(), "{wrapped}");
        assert_eq!(wrapped["result"], value);
    }
    let int_meta = meta("SCRIPT::ADD", vec![param("a", TypeTag::Int)]);
    let schema = output_schema_for(&int_meta).expect("output schema");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["required"], json!(["result"]));

    let mut returns_int = meta("SCRIPT::ADD", vec![]);
    returns_int.returns = Some(TypeTag::Int);
    let typed = output_schema_for(&returns_int).expect("typed output schema");
    assert_eq!(typed["properties"]["result"]["type"], "integer");

    // Opaque returns fail here, before the reactor opens.
    returns_int.returns = Some(TypeTag::Pipe);
    output_schema_for(&returns_int).expect_err("opaque returns must fail");
}

#[test]
fn map_json_args_optional_trailing_fills() {
    let mut opt = param("opts", TypeTag::Map);
    opt.optional = true;
    let values = map_json_args(&[param("a", TypeTag::Int), opt], &json!({"a": 1}))
        .expect("optional fills");
    assert_eq!(values.len(), 2);
    assert!(values[1].as_map().is_some());
}
