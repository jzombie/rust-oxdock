//! Lifecycle dispatcher end to end, composed in-DSL.
//!
//! The parent script imports the real `dispatcher.oxfile` and calls
//! `COMPOSE_STAGE`: the DSL itself assembles the lifecycle module
//! (`FUNC SOME_STAGE` wrapping the dynamically inserted hook calls),
//! exactly as the engine will at runtime. The engine stays agnostic:
//! it only knows the stage name and calls `LC::STAGE($ctx)` on the
//! assembled module. Transport over a real pipe plus staged binary is
//! the pre-proven README pattern, not re-proven here.

use oxdock_core::Engine;
use oxdock_fs::{GuardedPath, PathResolver};

/// Context shape threaded through every stage in these tests: `name`
/// arrives from the caller, `saw` is added by the hook. Both optional
/// so the unknown-hook test can pass an empty map and still reach the
/// missing-hook failure instead of a shape error.
const CTX_TYPE: &str = "MAP<name?: STRING, saw?: STRING>";
const PLUGIN_TYPE: &str = "LIST<MAP<path: STRING, alias: STRING, hooks: LIST<STRING>>>";

fn seed_files(root: &GuardedPath, resolver: &PathResolver) {
    let hook = root.join("hook.oxfile").expect("join hook");
    resolver
        .write_file(
            &hook,
            indoc::indoc! {r#"
                IMPORT [STD]
                FUNC PRE_RENDER($ctx: MAP<name?: STRING, saw?: STRING>) {
                    RETURN MAP_SET($ctx, "saw", "hook-ran")
                }
                EXPORT PRE_RENDER
            "#}
            .as_bytes(),
        )
        .expect("write hook");
    let source = include_str!("../dispatcher.oxfile").as_bytes();
    let dest = root.join("dispatcher.oxfile").expect("join dispatcher");
    resolver
        .write_file(&dest, source)
        .expect("write dispatcher");
}

fn parent_script(plugins_literal: &str, stage: &str, out: &str) -> String {
    format!(
        indoc::indoc! {r#"
            IMPORT ["dispatcher.oxfile" AS D]
            LET $plugins: {plugin} = {plugins}
            D::COMPOSE_STAGE($plugins, "{stage}", "{ctx}", "{out}")
        "#},
        plugin = PLUGIN_TYPE,
        plugins = plugins_literal,
        stage = stage,
        ctx = CTX_TYPE,
        out = out,
    )
}

#[test]
fn dispatcher_composes_stage_module_with_hook_calls() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed_files(&root, &resolver);

    let parent = parent_script(
        r#"[{path: "hook.oxfile", alias: "P0", hooks: ["PRE_RENDER"]}]"#,
        "PRE_RENDER",
        "lifecycle.oxfile",
    );
    Engine::new()
        .run_script(&root, &parent)
        .expect("parent composes lifecycle");

    let path = root.join("lifecycle.oxfile").expect("join lifecycle");
    let composed = resolver.read_to_string(&path).expect("read lifecycle");
    assert_eq!(
        composed,
        indoc::indoc! {r#"
            IMPORT [STD]
            IMPORT ["hook.oxfile" AS P0]
            FUNC PRE_RENDER($ctx: MAP<name?: STRING, saw?: STRING>) {
                $ctx = P0::PRE_RENDER($ctx)
                RETURN $ctx
            }
            EXPORT PRE_RENDER
        "#}
    );

    let caller = indoc::indoc! {r#"
        IMPORT ["lifecycle.oxfile" AS LC]
        LET $ctx: MAP<name?: STRING, saw?: STRING> = {name: "Ada"}
        $ctx = LC::PRE_RENDER($ctx)
        LET $saw: STRING = $ctx.saw
    "#};
    let output = Engine::new()
        .run_script(&root, caller)
        .expect("stage runs hooks");
    assert_eq!(
        output.bindings.get("saw").and_then(|value| value.as_str()),
        Some("hook-ran"),
        "hook mutation threads back through $ctx: {:?}",
        output.bindings
    );
}

#[test]
fn dispatcher_orders_plugins_and_filters_other_stages() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed_files(&root, &resolver);

    let parent = parent_script(
        r#"[{path: "a.oxfile", alias: "P0", hooks: ["PRE_RENDER", "POST_RENDER"]}, {path: "b.oxfile", alias: "P1", hooks: ["PRE_RENDER"]}]"#,
        "PRE_RENDER",
        "lifecycle.oxfile",
    );
    Engine::new()
        .run_script(&root, &parent)
        .expect("parent composes lifecycle");

    let path = root.join("lifecycle.oxfile").expect("join lifecycle");
    let composed = resolver.read_to_string(&path).expect("read lifecycle");
    assert_eq!(
        composed,
        indoc::indoc! {r#"
            IMPORT [STD]
            IMPORT ["a.oxfile" AS P0]
            IMPORT ["b.oxfile" AS P1]
            FUNC PRE_RENDER($ctx: MAP<name?: STRING, saw?: STRING>) {
                $ctx = P0::PRE_RENDER($ctx)
                $ctx = P1::PRE_RENDER($ctx)
                RETURN $ctx
            }
            EXPORT PRE_RENDER
        "#}
    );
}

#[test]
fn dispatcher_unknown_hook_fails_naming_it() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed_files(&root, &resolver);

    let parent = parent_script(
        r#"[{path: "hook.oxfile", alias: "P0", hooks: ["NOPE"]}]"#,
        "NOPE",
        "lifecycle.oxfile",
    );
    Engine::new()
        .run_script(&root, &parent)
        .expect("parent composes lifecycle");

    let caller = indoc::indoc! {r#"
        IMPORT ["lifecycle.oxfile" AS LC]
        LET $ctx: MAP<name?: STRING, saw?: STRING> = {}
        $ctx = LC::NOPE($ctx)
    "#};
    let err = format!(
        "{:#}",
        Engine::new()
            .run_script(&root, caller)
            .expect_err("unknown hook must fail")
    );
    assert!(err.contains("NOPE"), "names the violator: {err}");
}
