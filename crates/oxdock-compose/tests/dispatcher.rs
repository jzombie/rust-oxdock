//! `COMPOSE_ALL` byte layout, composed in-DSL.
//!
//! The parent script imports the real `src/compose.oxfile` and reads
//! back the assembled child text: registration order is execution
//! order, every plugin exposes every lifecycle method so every
//! stage calls every plugin, and `SETUP` unrolls before the target
//! loop. Execution is covered by `compose_pipe` (pipe mechanics)
//! and `render` (end to end).

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
            IMPORT [TYPE PLUGIN FROM "types.oxfile"]
            IMPORT ["compose.oxfile" AS C]
            LET $plugins: LIST<ANY> = {plugins}
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
            IMPORT [TYPE PLUGIN FROM "types.oxfile"]
            IMPORT ["compose.oxfile" AS C]
            LET $plugins: LIST<ANY> = {plugins}
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
fn compose_all_derives_qualifiers_from_stems_in_order() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let got = compose_text(
        &root,
        r#"["hooks/generic.oxfile", "hooks/oxdock.oxfile"]"#,
        r#"["INGEST"]"#,
    );
    let generic = got
        .find("IMPORT [\"hooks/generic.oxfile\" AS GENERIC_HOOKS]")
        .expect("generic import");
    let oxdock = got
        .find("IMPORT [\"hooks/oxdock.oxfile\" AS OXDOCK_HOOKS]")
        .expect("oxdock import");
    assert!(
        generic < oxdock,
        "imports follow registration order with derived qualifiers"
    );
    assert!(
        got.contains("$ctx = GENERIC_HOOKS::INGEST($ctx)"),
        "stage call uses the derived qualifier"
    );
}

#[test]
fn compose_all_calls_every_plugin_for_every_stage() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let got = compose_text(
        &root,
        r#"["a.oxfile", "b.oxfile"]"#,
        r#"["INGEST", "VERIFY"]"#,
    );
    let ai = got.find("$ctx = A_HOOKS::INGEST($ctx)").expect("a ingest");
    let bi = got.find("$ctx = B_HOOKS::INGEST($ctx)").expect("b ingest");
    let av = got.find("$ctx = A_HOOKS::VERIFY($ctx)").expect("a verify");
    let bv = got.find("$ctx = B_HOOKS::VERIFY($ctx)").expect("b verify");
    assert!(
        ai < bi && bi < av && av < bv,
        "stage-major order in registration order"
    );
}

#[test]
fn compose_all_unrolls_setup_before_target_loop() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let got = compose_text(&root, r#"["a.oxfile"]"#, r#"["INGEST"]"#);
    let setup = got.find("$run = A_HOOKS::SETUP($run)").expect("setup call");
    let seen = got.find("LET $seen").expect("target loop start");
    assert!(setup < seen, "SETUP unrolls before the target loop");
}

#[test]
fn compose_all_rejects_backslash_paths() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let err = compose_err(&root, r#"["a\\b.oxfile"]"#, r#"["INGEST"]"#);
    assert!(
        err.contains("EXIT requested with code 1"),
        "backslash path exits 1: {err}"
    );
}

#[test]
fn compose_all_rejects_qualifier_collisions_naming_both() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let err = compose_err(
        &root,
        r#"["x/ingest.oxfile", "y/ingest.oxfile"]"#,
        r#"["INGEST"]"#,
    );
    assert!(
        err.contains("EXIT requested with code 1"),
        "shared qualifier fails closed: {err}"
    );
}
