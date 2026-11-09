//! `RENDER_ALL` byte layout, rendered in-DSL.
//!
//! Each driver below is an `oxdock!` block: DSL tokens at compile
//! time, never Rust-assembled script text. The parent imports the
//! real `src/render.oxfile` and reads back the assembled child
//! text: registration order is execution order, every plugin exposes
//! every lifecycle method so every stage calls every plugin, and
//! `SETUP` unrolls before the target loop. Execution is covered by
//! `render_pipe` (pipe mechanics) and `render` (end to end).

use oxdock_core::Engine;
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_macros::oxdock;

fn seed(root: &GuardedPath, resolver: &PathResolver) {
    for (rel, contents) in [
        ("render.oxfile", include_str!("../src/render.oxfile")),
        ("types.oxfile", include_str!("../src/types.oxfile")),
    ] {
        let path = root.join(rel).expect("join seed");
        resolver
            .write_file(&path, contents.as_bytes())
            .expect("write seed");
    }
}

fn binding_mod(output: oxdock_core::EngineOutput) -> String {
    output
        .bindings
        .get("mod")
        .and_then(|v| v.as_str())
        .expect("mod binding")
        .to_string()
}

#[test]
fn render_all_derives_qualifiers_from_stems_in_order() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let steps = oxdock! {
        IMPORT ["render.oxfile" AS C]
        LET $plugins: LIST<STRING> = ["hooks/generic.oxfile", "hooks/oxdock.oxfile"]
        LET $stages: LIST<STRING> = ["INGEST"]
        LET $mod: STRING = C::RENDER_ALL($plugins, $stages, "types.oxfile")
    };
    let got = binding_mod(
        Engine::new()
            .run_steps_without_snapshot(&root, &steps)
            .expect("render runs"),
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
fn render_all_calls_every_plugin_for_every_stage() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let steps = oxdock! {
        IMPORT ["render.oxfile" AS C]
        LET $plugins: LIST<STRING> = ["a.oxfile", "b.oxfile"]
        LET $stages: LIST<STRING> = ["INGEST", "VERIFY"]
        LET $mod: STRING = C::RENDER_ALL($plugins, $stages, "types.oxfile")
    };
    let got = binding_mod(
        Engine::new()
            .run_steps_without_snapshot(&root, &steps)
            .expect("render runs"),
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
fn render_all_unrolls_setup_before_target_loop() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let steps = oxdock! {
        IMPORT ["render.oxfile" AS C]
        LET $plugins: LIST<STRING> = ["a.oxfile"]
        LET $stages: LIST<STRING> = ["INGEST"]
        LET $mod: STRING = C::RENDER_ALL($plugins, $stages, "types.oxfile")
    };
    let got = binding_mod(
        Engine::new()
            .run_steps_without_snapshot(&root, &steps)
            .expect("render runs"),
    );
    let setup = got.find("$run = A_HOOKS::SETUP($run)").expect("setup call");
    let seen = got.find("LET $seen").expect("target loop start");
    assert!(setup < seen, "SETUP unrolls before the target loop");
}

#[test]
fn render_all_rejects_backslash_paths() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let steps = oxdock! {
        IMPORT ["render.oxfile" AS C]
        LET $plugins: LIST<STRING> = ["a\\b.oxfile"]
        LET $stages: LIST<STRING> = ["INGEST"]
        LET $mod: STRING = C::RENDER_ALL($plugins, $stages, "types.oxfile")
    };
    let err = Engine::new()
        .run_steps_without_snapshot(&root, &steps)
        .expect_err("render must fail");
    let err = format!("{err:#}");
    assert!(
        err.contains("EXIT requested with code 1"),
        "backslash path exits 1: {err}"
    );
}

#[test]
fn render_all_rejects_qualifier_collisions_naming_both() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let steps = oxdock! {
        IMPORT ["render.oxfile" AS C]
        LET $plugins: LIST<STRING> = ["x/ingest.oxfile", "y/ingest.oxfile"]
        LET $stages: LIST<STRING> = ["INGEST"]
        LET $mod: STRING = C::RENDER_ALL($plugins, $stages, "types.oxfile")
    };
    let err = Engine::new()
        .run_steps_without_snapshot(&root, &steps)
        .expect_err("render must fail");
    let err = format!("{err:#}");
    assert!(
        err.contains("EXIT requested with code 1"),
        "shared qualifier fails closed: {err}"
    );
}

#[test]
fn render_all_fails_closed_on_zero_targets() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    let steps = oxdock! {
        IMPORT ["render.oxfile" AS C]
        LET $plugins: LIST<STRING> = ["a.oxfile"]
        LET $stages: LIST<STRING> = ["INGEST"]
        LET $mod: STRING = C::RENDER_ALL($plugins, $stages, "types.oxfile")
    };
    let got = binding_mod(
        Engine::new()
            .run_steps_without_snapshot(&root, &steps)
            .expect("render runs"),
    );
    assert!(
        got.contains("LET $count: INT = 0"),
        "driver counts discovered targets"
    );
    assert!(
        got.contains("no targets discovered"),
        "empty discovery fails naming the scopes"
    );
}
