//! Session lifecycle: linear init once, per-call dispatch with capture
//! drain and truncation, init-semantics proof, and fail-fast validation.

use indoc::indoc;
use oxdock_core::{
    Engine, McpSession, StreamHook, TeeStream, Value, parse_script_with_modules_and_exports,
};
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

const STREAM_DEMO: &str = indoc! {r#"
    # Says two lines, then returns. Both lines stream live.
    FUNC CHATTY() {
      ECHO "one"
      ECHO "two"
      RETURN "done"
    }
    # Writes only into a pipe: nothing default-routed, so nothing
    # streams. Redirected bytes stay silent by construction.
    FUNC QUIET() {
      LET $p: PIPE
      WITH_IO [stdout=$p] ECHO "into pipe"
      RETURN "done"
    }
    EXPORT [CHATTY, QUIET]
"#};

#[test]
fn default_routed_output_streams_live_per_write() {
    let mut session = init_session(STREAM_DEMO);
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    session.set_stream_hook(Some(StreamHook {
        tx,
        dropped: dropped.clone(),
    }));
    let out = session.call("CHATTY", vec![]).expect("call");
    session.set_stream_hook(None);
    assert_eq!(out.value.as_str(), Some("done"));
    // Final content still carries the full text: streaming adds live
    // delivery, it does not remove the record.
    assert!(out.stdout.contains("one") && out.stdout.contains("two"));
    let chunks: Vec<_> = rx.try_iter().collect();
    assert!(chunks.len() >= 2, "writes stream as they happen");
    assert!(chunks.iter().all(|chunk| chunk.kind == TeeStream::Stdout));
    let streamed: Vec<u8> = chunks.into_iter().flat_map(|chunk| chunk.bytes).collect();
    assert_eq!(streamed, b"one\ntwo\n");
    assert_eq!(dropped.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[test]
fn redirected_output_never_streams() {
    let mut session = init_session(STREAM_DEMO);
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    session.set_stream_hook(Some(StreamHook {
        tx,
        dropped: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    }));
    let out = session.call("QUIET", vec![]).expect("call");
    session.set_stream_hook(None);
    assert_eq!(out.value.as_str(), Some("done"));
    assert_eq!(rx.try_iter().count(), 0, "redirected bytes must not stream");
}

#[test]
fn bare_run_output_streams_through_the_pump() {
    // Bare RUN (no LET capture) routes child output to the default
    // stdout: the pump path must stream exactly like ECHO does.
    // `echo` via sh keeps this platform-gated to non-Windows; the
    // ECHO test above covers the shared tee path everywhere.
    let script = indoc! {r#"
        FUNC LOUD() {
          RUN ["sh", "-c", "echo loud"]
          RETURN "done"
        }
        EXPORT [LOUD]
    "#};
    let mut session = init_session(script);
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    session.set_stream_hook(Some(StreamHook {
        tx,
        dropped: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    }));
    let out = session.call("LOUD", vec![]).expect("call");
    session.set_stream_hook(None);
    assert_eq!(out.value.as_str(), Some("done"));
    let streamed: Vec<u8> = rx
        .try_iter()
        .flat_map(|chunk| {
            assert_eq!(chunk.kind, TeeStream::Stdout);
            chunk.bytes
        })
        .collect();
    assert!(
        streamed.windows(b"loud".len()).any(|w| w == b"loud"),
        "pump output must stream, got: {}",
        String::from_utf8_lossy(&streamed)
    );
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
    let parsed = parse_script_with_modules_and_exports(script, engine.module_table())
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
    assert!(
        metas
            .iter()
            .any(|m| oxdock_parser::base_name(&m.name) == "TEXT_TRIM")
    );
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
        .expect_err("MCP must hard-fail, never silently skip");
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
    let err = init_handler(config).err().expect("MCP init must hard-fail");
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

const HANDLE_DEMO: &str = indoc! {r#"
    IMPORT [STD]
    # Spawns a sleeper and returns its handle for polling.
    FUNC SPAWN_SLOW() {
      LET $t: HANDLE = ASYNC { SLEEP 30s }
      RETURN $t
    }
    # Spawns a fast task and returns its handle for polling.
    FUNC SPAWN_FAST() {
      LET $t: HANDLE = ASYNC { ECHO "hi"; RETURN "done" }
      RETURN $t
    }
    # Polls any task handle by value.
    FUNC POLL_WRAP($t: HANDLE) {
      RETURN TASK_POLL($t)
    }
    EXPORT [SPAWN_SLOW, SPAWN_FAST, POLL_WRAP, STD::TASK_POLL]
"#};

fn handle_param() -> Vec<oxdock_core::FuncParam> {
    vec![oxdock_core::FuncParam {
        name: "task".to_string(),
        param_type: Some(oxdock_parser::TypeTag::Handle),
        allowed: None,
        docs: "",
        options: None,
        optional: false,
    }]
}

fn poll_once(
    session: &mut McpSession<oxdock_process::DefaultProcessManager>,
    id: i64,
) -> serde_json::Value {
    let args = oxdock_mcp::tools::map_json_args(
        &handle_param(),
        &serde_json::json!({"task": {"task_id": id}}),
    )
    .expect("task_id maps");
    let snap = session.call("POLL_WRAP", args).expect("poll live task");
    oxdock_mcp::tools::value_to_json_result(None, &snap.value).expect("snapshot json")
}

fn poll_until_finished(
    session: &mut McpSession<oxdock_process::DefaultProcessManager>,
    id: i64,
) -> serde_json::Value {
    for _ in 0..200 {
        let snap = poll_once(session, id);
        if snap["state"] == "finished" {
            return snap;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("background task {id} did not finish in time");
}

#[test]
fn handle_crosses_the_boundary_as_task_id() {
    use oxdock_mcp::build_tool_table;
    use std::collections::HashMap;
    let mut session = init_session(HANDLE_DEMO);
    let metas = session.tool_metas().expect("metas");
    let table = build_tool_table(metas, &HashMap::new()).expect("handle table serves");
    let names: Vec<String> = table.iter().map(|tool| tool.name.clone()).collect();
    assert!(names.contains(&"TASK_POLL".to_string()), "{names:?}");
    let spawn = table
        .iter()
        .find(|tool| tool.name == "SPAWN_SLOW")
        .expect("spawn tool");
    assert!(
        spawn.output_schema.is_object(),
        "handle return has a schema"
    );

    let out = session.call("SPAWN_SLOW", vec![]).expect("spawn");
    let id = out.value.as_handle().expect("spawn returns a handle");
    let wire =
        oxdock_mcp::tools::value_to_json_result(None, &out.value).expect("untyped projection");
    assert_eq!(wire, serde_json::json!({"task_id": id}));
}

#[test]
#[cfg_attr(miri, ignore = "spawns real background worker threads")]
fn task_poll_reports_running() {
    let mut session = init_session(HANDLE_DEMO);
    let out = session.call("SPAWN_SLOW", vec![]).expect("spawn");
    let id = out.value.as_handle().expect("spawn returns a handle") as i64;

    let first = poll_once(&mut session, id);
    assert_eq!(first["state"], "running", "{first}");
}

#[test]
#[cfg_attr(miri, ignore = "spawns real background worker threads")]
fn task_poll_returns_value_and_tail() {
    let mut session = init_session(HANDLE_DEMO);
    let out = session.call("SPAWN_FAST", vec![]).expect("spawn");
    let id = out.value.as_handle().expect("spawn returns a handle") as i64;

    let done = poll_until_finished(&mut session, id);
    assert_eq!(done["state"], "finished");
    assert_eq!(done["ok"], true);
    assert_eq!(done["result"], "done");
    assert!(
        done["stdout_tail"]
            .as_str()
            .is_some_and(|s| s.contains("hi")),
        "{done}"
    );

    let again = poll_once(&mut session, id);
    assert_eq!(again["state"], "finished", "poll never consumes");
}

#[test]
fn task_poll_unknown_id_fails() {
    let mut session = init_session(HANDLE_DEMO);
    let args = oxdock_mcp::tools::map_json_args(
        &handle_param(),
        &serde_json::json!({"task": {"task_id": 999}}),
    )
    .expect("shape ok");
    let err = session
        .call("POLL_WRAP", args)
        .expect_err("unknown id must fail");
    assert!(err.to_string().contains("not found"), "{err:#}");
}

#[test]
fn task_poll_rejects_non_handle() {
    let mut session = init_session(HANDLE_DEMO);
    let err = session
        .call("POLL_WRAP", vec![Value::int(1)])
        .expect_err("int must fail");
    assert!(err.to_string().contains("expected HANDLE"), "{err:#}");
}

const BG_WORKSPACE_DEMO: &str = indoc! {r#"
    IMPORT [STD]
    # Detaches a directory read under a workspace switch.
    FUNC BG_WHERE() {
      LET $t: HANDLE = ASYNC {
        WORKSPACE LOCAL
        LET $d: STRING = CWD
        RETURN TEXT_TRIM($d)
      }
      RETURN $t
    }
    EXPORT [BG_WHERE, STD::TASK_POLL]
"#};

#[test]
fn async_block_supports_workspace_switch_and_handle_return() {
    let mut session = init_session(BG_WORKSPACE_DEMO);
    let out = session.call("BG_WHERE", vec![]).expect("spawn");
    let id = out.value.as_handle().expect("spawn returns a handle") as i64;
    let args = oxdock_mcp::tools::map_json_args(
        &handle_param(),
        &serde_json::json!({"task": {"task_id": id}}),
    )
    .expect("task_id maps");
    let mut done = serde_json::json!(null);
    for _ in 0..200 {
        let snap = session.call("TASK_POLL", args.clone()).expect("poll");
        let payload =
            oxdock_mcp::tools::value_to_json_result(None, &snap.value).expect("snapshot json");
        if payload["state"] == "finished" {
            done = payload;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(done["state"], "finished", "{done}");
    assert!(done["result"].as_str().is_some_and(|s| !s.is_empty()));
}

#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns real background worker threads")]
fn async_block_runs_child_processes() {
    let mut session = init_session(indoc! {r#"
        IMPORT [STD]
        FUNC BG_RUN() {
          LET $t: HANDLE = ASYNC {
            LET $o: STRING = RUN ["echo", "bg-hi"]
            RETURN TEXT_TRIM($o)
          }
          RETURN $t
        }
        EXPORT [BG_RUN, STD::TASK_POLL]
    "#});
    let out = session.call("BG_RUN", vec![]).expect("spawn");
    let id = out.value.as_handle().expect("spawn returns a handle") as i64;
    let mut done = serde_json::json!(null);
    for _ in 0..200 {
        let args = oxdock_mcp::tools::map_json_args(
            &handle_param(),
            &serde_json::json!({"task": {"task_id": id}}),
        )
        .expect("task_id maps");
        let snap = session.call("TASK_POLL", args).expect("poll");
        let payload =
            oxdock_mcp::tools::value_to_json_result(None, &snap.value).expect("snapshot json");
        if payload["state"] == "finished" {
            done = payload;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(done["state"], "finished", "{done}");
    assert_eq!(done["result"], "bg-hi");
}
