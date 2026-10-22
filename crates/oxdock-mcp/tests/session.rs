//! Session lifecycle: linear init once, per-call dispatch with capture
//! drain and truncation, init-semantics proof, and fail-fast validation.

use indoc::indoc;
use oxdock_core::{Engine, McpSession, Value, parse_script_with_modules_and_exports};
use oxdock_fs::{GuardedPath, PathResolver, WorkspaceFs};
use oxdock_mcp::{McpServerConfig, init_handler};
use oxdock_process::default_process_manager;

fn test_fs() -> (Box<dyn WorkspaceFs>, GuardedPath) {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let mut resolver = PathResolver::new_lazy(root.clone()).expect("lazy resolver");
    resolver.set_workspace_root(root.clone());
    let snapshot = resolver.snapshot_handle();
    std::mem::forget(snapshot);
    let fs: Box<dyn WorkspaceFs> = Box::new(resolver);
    (fs, root)
}

fn init_session(script: &str) -> McpSession<oxdock_process::DefaultProcessManager> {
    let engine = Engine::new();
    let parsed =
        parse_script_with_modules_and_exports(script, engine.module_table()).expect("parse");
    let (fs, _root) = test_fs();
    McpSession::init(
        fs,
        default_process_manager(),
        Vec::new(),
        Vec::new(),
        &parsed,
    )
    .expect("session init")
}

fn handler_config(script: &str) -> (McpServerConfig, GuardedPath) {
    let (_fs, root) = test_fs();
    let script_path = root.join("tools.oxdock").expect("script path");
    (
        McpServerConfig {
            workspace_root: root.clone(),
            script_path,
            script: script.to_string(),
            modules: Vec::new(),
            types: Vec::new(),
            watch: false,
        },
        root,
    )
}

const DEMO: &str = indoc! {r#"
    ENV APP_NAME=oxdock-demo
    # Adds two integers. Returns the sum.
    FUNC ADD($a: INT, $b: INT) {
      RETURN $a + $b
    }
    # Echoes, then returns the name unchanged.
    FUNC GREET($name: STRING) {
      ECHO "hello"
      RETURN $name
    }
    # Returns the ambient app name. Proves linear init ran.
    FUNC APP_NAME() {
      RETURN env:APP_NAME
    }
    EXPORT [ADD, GREET, APP_NAME]
"#};

#[test]
fn init_executes_linear_steps_and_serves_tools() {
    let mut session = init_session(DEMO);
    // APP_NAME reads top-level ENV: init ran the linear steps (discarding
    // them would leave env:APP_NAME undefined and fail here). Script
    // functions close over params only, so LET globals stay out of bodies.
    let out = session.call("APP_NAME", vec![]).expect("call");
    assert_eq!(out.value.as_str(), Some("oxdock-demo"));
    let metas = session.tool_metas().expect("metas");
    assert_eq!(metas.len(), 3);
}

#[test]
fn call_dispatches_math_and_truncates_captures() {
    let mut session = init_session(DEMO);
    let sum = session
        .call("ADD", vec![Value::int(2), Value::int(3)])
        .expect("call");
    assert_eq!(sum.value.as_i64(), Some(5));
    assert!(sum.stdout.is_empty());

    // ECHO output drains into the result, then truncates: a second
    // identical call sees exactly one greeting, never two.
    let one = session
        .call("GREET", vec![Value::string("x".to_string())])
        .expect("call");
    let two = session
        .call("GREET", vec![Value::string("x".to_string())])
        .expect("call");
    assert_eq!(one.stdout, two.stdout, "sinks must truncate between calls");
    assert_eq!(one.stdout.matches("hello").count(), 1);
    assert_eq!(one.value.as_str(), Some("x"));
}

#[test]
fn unknown_tool_fails() {
    let mut session = init_session(DEMO);
    session
        .call("MISSING", vec![])
        .expect_err("unknown tool must fail");
}

#[test]
fn qualified_host_export_serves_with_registry_docs() {
    use oxdock_mcp::build_tool_table;
    use std::collections::HashMap;
    let mut session = init_session(indoc! {r#"
        EXPORT [STD::INT]
    "#});
    let metas = session.tool_metas().expect("metas");
    assert_eq!(metas.len(), 1);
    assert_eq!(metas[0].name, "STD::INT");
    // Dispatch answers under the base name through the normal call path.
    let out = session
        .call("INT", vec![Value::string("42".to_string())])
        .expect("call");
    assert_eq!(out.value.as_i64(), Some(42));
    // No script comments exist, so the description falls back to the
    // registry summary: host docs carry over with no wrapper.
    let table = build_tool_table(metas, &HashMap::new()).expect("table");
    assert_eq!(table[0].name, "INT");
    assert!(!table[0].description.is_empty(), "registry docs carry over");
}

#[test]
fn missing_export_list_fails_fast() {
    let (config, _root) = handler_config(indoc! {r#"
        FUNC FOO() {
          RETURN "x"
        }
    "#});
    assert!(init_handler(config).is_err(), "no EXPORT must fail");
}

#[test]
fn opaque_export_fails_before_serving() {
    let (config, _root) = handler_config(indoc! {r#"
        FUNC P($p: PIPE) {
          RETURN "x"
        }
        EXPORT P
    "#});
    assert!(
        init_handler(config).is_err(),
        "opaque params must fail fast"
    );
}

#[test]
fn qualified_opaque_export_still_registers() {
    // Proves the EXPORT declaration is layer-agnostic: every step below
    // lives in oxdock-parser / oxdock-core, and no oxdock-mcp code runs.
    // TEXT_TRIM returns a STRING|PIPE union with no JSON projection, yet
    // all three stages accept it. If any of these fails, the export
    // itself is broken, not just its MCP serving.
    let script = indoc! {r#"
        IMPORT [STD]
        EXPORT [STD::TEXT_TRIM]
    "#};
    // Stage 1 (oxdock-parser): the declaration parses and the export
    // list records the qualified name verbatim.
    let engine = Engine::new();
    let parsed =
        parse_script_with_modules_and_exports(script, engine.module_table())
            .expect("export must parse");
    assert!(parsed.exports.contains(&"STD::TEXT_TRIM".to_string()));
    // Stage 2 (oxdock-core): linear session init accepts the export.
    // McpSession is core session machinery (state + registry); despite
    // the name it performs no JSON projection and opens no transport.
    let (fs, _root) = test_fs();
    let session = McpSession::init(
        fs,
        default_process_manager(),
        Vec::new(),
        Vec::new(),
        &parsed,
    )
    .expect("session init must accept the export");
    assert!(session.exports().contains(&"STD::TEXT_TRIM".to_string()));
    // Stage 3 (oxdock-core registry): the qualified name resolves to
    // real function metadata. Registry lookup succeeds; only the
    // MCP-layer JSON projection (tested below) refuses it.
    let metas = session.tool_metas().expect("registry must resolve it");
    assert!(metas.iter().any(|m| oxdock_parser::base_name(&m.name) == "TEXT_TRIM"));
}

#[test]
fn mcp_table_build_hard_fails_on_opaque_host_signature() {
    use oxdock_mcp::build_tool_table;
    use std::collections::HashMap;

    // Proves the refusal lives in the MCP layer: build_tool_table is
    // oxdock-mcp code that projects registry metadata into JSON tool
    // schemas, and its own error names both the layer ("MCP tool") and
    // the function. It bails instead of skipping, so no table with a
    // silent hole can ever be built.
    let engine = Engine::new();
    let parsed = parse_script_with_modules_and_exports(
        indoc! {r#"
            IMPORT [STD]
            EXPORT [STD::TEXT_TRIM]
        "#},
        engine.module_table(),
    )
    .expect("parse");
    let (fs, _root) = test_fs();
    let session = McpSession::init(
        fs,
        default_process_manager(),
        Vec::new(),
        Vec::new(),
        &parsed,
    )
    .expect("session init");
    let metas = session.tool_metas().expect("metas");
    let err = build_tool_table(metas, &HashMap::new())
        .err()
        .expect("MCP must hard-fail, never silently skip");
    let msg = err.to_string();
    assert!(msg.contains("cannot expose MCP tool"), "{msg}");
    assert!(msg.contains("TEXT_TRIM"), "{msg}");
}

#[test]
fn mcp_init_hard_fails_before_serving_opaque_tool() {
    // Proves startup loudness end to end: init_handler is oxdock-mcp's
    // own constructor, and the two tests above already proved every
    // upstream stage (parse, session, registry) succeeds on this exact
    // script. So this failure can only come from the MCP layer, and the
    // server never opens with an unservable entry. The error names its
    // layer, not the export.
    let (config, _root) = handler_config(indoc! {r#"
        IMPORT [STD]
        EXPORT [STD::TEXT_TRIM]
    "#});
    let err = init_handler(config)
        .err()
        .expect("MCP init must hard-fail");
    let msg = err.to_string();
    assert!(msg.contains("cannot expose MCP tool"), "{msg}");
    assert!(msg.contains("TEXT_TRIM"), "{msg}");
}

#[test]
fn reload_swaps_tools_and_keeps_old_on_broken_edit() {
    let (config, _root) = handler_config(DEMO);
    let handler = init_handler(config).expect("init");
    assert_eq!(handler.tool_names().len(), 4);

    let root = test_fs().1;
    let modules = Vec::new();
    let types = Vec::new();
    handler
        .reload(
            indoc! {r#"
            FUNC SOLO() {
              RETURN "one"
            }
            EXPORT SOLO
            "#},
            modules.clone(),
            types.clone(),
            &root,
        )
        .expect("reload");
    assert_eq!(
        handler.tool_names(),
        vec!["SOLO".to_string(), "MCP_STATUS".to_string()]
    );

    handler
        .reload("FUNC BROKEN( {", modules, types, &root)
        .expect_err("broken edit must fail");
    assert_eq!(
        handler.tool_names(),
        vec!["SOLO".to_string(), "MCP_STATUS".to_string()],
        "broken edit keeps the live tools"
    );
}

#[test]
fn status_reports_generation_and_failure() {
    use oxdock_mcp::server::STATUS_TOOL_NAME;
    let (config, root) = handler_config(DEMO);
    let handler = init_handler(config).expect("init");
    assert_eq!(handler.generation(), 1);
    assert!(handler.tool_names().contains(&STATUS_TOOL_NAME.to_string()));

    let payload = handler.status_payload();
    assert_eq!(payload["generation"], 1);
    assert_eq!(payload["last_reload"]["ok"], true);
    let first_hash = payload["script_sha256"].as_str().expect("hash").to_string();

    handler
        .reload(DEMO, Vec::new(), Vec::new(), &root)
        .expect("reload");
    assert_eq!(handler.generation(), 2);
    let payload = handler.status_payload();
    assert_eq!(payload["generation"], 2);
    assert_eq!(payload["script_sha256"].as_str(), Some(first_hash.as_str()));

    handler
        .reload("FUNC BROKEN( {", Vec::new(), Vec::new(), &root)
        .expect_err("broken edit must fail");
    assert_eq!(handler.generation(), 2, "failed reload keeps generation");
    let payload = handler.status_payload();
    assert_eq!(payload["last_reload"]["ok"], false);
    assert!(
        payload["last_reload"]["message"]
            .as_str()
            .expect("message")
            .contains("BROKEN")
    );
}

#[test]
fn status_tool_name_is_reserved() {
    let (config, _root) = handler_config(indoc! {r#"
        FUNC MCP_STATUS() {
          RETURN "spoof"
        }
        EXPORT MCP_STATUS
    "#});
    assert!(init_handler(config).is_err(), "EXPORT MCP_STATUS must fail");
}
