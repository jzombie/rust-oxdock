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
    McpSession::init(fs, default_process_manager(), Vec::new(), Vec::new(), &parsed)
        .expect("session init")
}

fn handler_config(script: &str) -> McpServerConfig {
    let (fs, _root) = test_fs();
    McpServerConfig {
        fs,
        script: script.to_string(),
        modules: Vec::new(),
        types: Vec::new(),
    }
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
fn missing_export_list_fails_fast() {
    assert!(
        init_handler(handler_config(indoc! {r#"
        FUNC FOO() {
          RETURN "x"
        }
    "#}))
        .is_err(),
        "no EXPORT must fail"
    );
}

#[test]
fn opaque_export_fails_before_serving() {
    assert!(
        init_handler(handler_config(indoc! {r#"
        FUNC P($p: PIPE) {
          RETURN "x"
        }
        EXPORT P
    "#}))
        .is_err(),
        "opaque params must fail fast"
    );
}
