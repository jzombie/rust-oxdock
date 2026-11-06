//! `COMPOSE_ALL` byte layout, composed in-DSL.
//!
//! The parent script imports the real `src/compose.oxfile` and reads
//! back the assembled child text: imports follow registration order,
//! per-target calls follow stage order with qualified names, `SETUP`
//! unrolls before the target loop. Execution is covered by
//! `compose_pipe` (pipe mechanics) and `render` (end to end).

use oxdock_core::Engine;
use oxdock_fs::{GuardedPath, PathResolver};

fn seed(root: &GuardedPath, resolver: &PathResolver) {
    for (rel, contents) in [
        ("compose.oxfile", include_str!("../src/compose.oxfile")),
        ("types.oxfile", include_str!("../src/types.oxfile")),
    ] {
        let path = root.join(rel).expect("join seed");
        resolver
            .write_file(&path, contents.as_bytes())
            .expect("write seed");
    }
}

fn compose_text(root: &GuardedPath, plugins_literal: &str, stages_literal: &str) -> String {
    let parent = format!(
        indoc::indoc! {r#"
            IMPORT [TYPE CTX FROM "types.oxfile"]
            IMPORT [TYPE PLUGIN FROM "types.oxfile"]
            IMPORT ["compose.oxfile" AS C]
            LET $plugins: LIST<PLUGIN> = {plugins}
            LET $stages: LIST<STRING> = {stages}
            LET $mod: STRING = C::COMPOSE_ALL($plugins, $stages, "types.oxfile")
        "#},
        plugins = plugins_literal,
        stages = stages_literal,
    );
    let output = Engine::new()
        .run_script(root, &parent)
        .expect("compose runs");
    output
        .bindings
        .get("mod")
        .and_then(|v| v.as_str())
        .expect("mod binding")
        .to_string()
}

fn compose_err(root: &GuardedPath, plugins_literal: &str, stages_literal: &str) -> String {
    let parent = format!(
        indoc::indoc! {r#"
            IMPORT [TYPE CTX FROM "types.oxfile"]
            IMPORT [TYPE PLUGIN FROM "types.oxfile"]
            IMPORT ["compose.oxfile" AS C]
            LET $plugins: LIST<PLUGIN> = {plugins}
            LET $stages: LIST<STRING> = {stages}
            LET $mod: STRING = C::COMPOSE_ALL($plugins, $stages, "types.oxfile")
        "#},
        plugins = plugins_literal,
        stages = stages_literal,
    );
    let err = Engine::new()
        .run_script(root, &parent)
        .expect_err("compose must fail");
    format!("{err:#}")
}

#[test]
fn compose_all_orders_imports_by_registration() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let got = compose_text(
        &root,
        r#"[{path: "a.oxfile", alias: "P0", hooks: ["INGEST"]}, {path: "b.oxfile", alias: "P1", hooks: ["INGEST"]}]"#,
        r#"["INGEST"]"#,
    );
    let ia = got.find("IMPORT [\"a.oxfile\" AS P0]").expect("a import");
    let ib = got.find("IMPORT [\"b.oxfile\" AS P1]").expect("b import");
    assert!(ia < ib, "imports follow registration order");
}

#[test]
fn compose_all_orders_per_target_calls_by_stage() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let got = compose_text(
        &root,
        r#"[{path: "a.oxfile", alias: "P0", hooks: ["WRITE_OUT", "INGEST"]}]"#,
        r#"["INGEST", "WRITE_OUT"]"#,
    );
    let ingest = got.find("$ctx = P0::INGEST($ctx)").expect("ingest call");
    let write = got.find("$ctx = P0::WRITE_OUT($ctx)").expect("write call");
    assert!(
        ingest < write,
        "per-target calls follow stage order, not registration order"
    );
}

#[test]
fn compose_all_leaves_unregistered_stages_out() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let got = compose_text(
        &root,
        r#"[{path: "a.oxfile", alias: "P0", hooks: ["INGEST"]}]"#,
        r#"["INGEST", "VERIFY"]"#,
    );
    assert!(
        got.contains("$ctx = P0::INGEST($ctx)"),
        "registered hook emits a call"
    );
    assert!(
        !got.contains("VERIFY($ctx)"),
        "stage with no plugin emits no call"
    );
}

#[test]
fn compose_all_unrolls_setup_before_target_loop() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let got = compose_text(
        &root,
        r#"[{path: "a.oxfile", alias: "P0", hooks: ["SETUP", "INGEST"]}]"#,
        r#"["INGEST"]"#,
    );
    let setup = got.find("$run = P0::SETUP($run)").expect("setup call");
    let seen = got.find("LET $seen").expect("target loop start");
    assert!(setup < seen, "SETUP unrolls before the target loop");
}

#[test]
fn compose_all_rejects_unknown_hook() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    // The violator is ECHOed naming hook and alias on stdout before
    // EXIT 1; the error value only proves fail-closed (no stdout
    // capture on EngineOutput).
    let err = compose_err(
        &root,
        r#"[{path: "a.oxfile", alias: "P0", hooks: ["NOPE"]}]"#,
        r#"["INGEST"]"#,
    );
    assert!(
        err.contains("EXIT requested with code 1"),
        "unknown hook exits 1: {err}"
    );
}

#[test]
fn compose_all_rejects_backslash_paths() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let err = compose_err(
        &root,
        r#"[{path: "a\\b.oxfile", alias: "P0", hooks: ["INGEST"]}]"#,
        r#"["INGEST"]"#,
    );
    assert!(
        err.contains("EXIT requested with code 1"),
        "backslash hook path exits 1: {err}"
    );
}

#[test]
fn compose_all_emits_host_module_imports_once_with_qualified_calls() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let got = compose_text(
        &root,
        r#"[{module: "OXDOCK", hooks: ["SETUP", "INGEST"]}, {module: "OXDOCK", hooks: ["VERIFY"]}]"#,
        r#"["INGEST", "VERIFY"]"#,
    );
    assert_eq!(
        got.matches("IMPORT [OXDOCK]").count(),
        1,
        "host module imports once no matter how many entries name it"
    );
    assert!(
        got.contains("$run = OXDOCK::SETUP($run)"),
        "setup unrolls the host call"
    );
    let ingest = got
        .find("$ctx = OXDOCK::INGEST($ctx)")
        .expect("ingest call");
    let verify = got
        .find("$ctx = OXDOCK::VERIFY($ctx)")
        .expect("verify call");
    assert!(ingest < verify, "host calls follow stage order");
}

#[test]
fn compose_all_orders_mixed_file_and_module_entries_by_registration() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let got = compose_text(
        &root,
        r#"[{path: "a.oxfile", alias: "P0", hooks: ["INGEST"]}, {module: "OXDOCK", hooks: ["INGEST"]}]"#,
        r#"["INGEST"]"#,
    );
    let file_call = got.find("$ctx = P0::INGEST($ctx)").expect("file call");
    let host_call = got.find("$ctx = OXDOCK::INGEST($ctx)").expect("host call");
    assert!(
        file_call < host_call,
        "mixed entries keep registration order"
    );
}

#[test]
fn compose_all_rejects_entry_with_both_path_and_module() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let err = compose_err(
        &root,
        r#"[{path: "a.oxfile", module: "OXDOCK", alias: "P0", hooks: ["INGEST"]}]"#,
        r#"["INGEST"]"#,
    );
    assert!(
        err.contains("EXIT requested with code 1"),
        "exactly one of path or module: {err}"
    );
}

#[test]
fn compose_all_rejects_file_entry_without_alias() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let err = compose_err(
        &root,
        r#"[{path: "a.oxfile", hooks: ["INGEST"]}]"#,
        r#"["INGEST"]"#,
    );
    assert!(
        err.contains("EXIT requested with code 1"),
        "file entry needs an alias: {err}"
    );
}
