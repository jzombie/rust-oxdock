//! GLOB sandbox containment: patterns are sandbox-root-relative, so any `..`
//! component matches nothing (no traversal outside the root), while normal
//! patterns keep listing sandbox contents. LOAD_TOML rides `resolve_read`,
//! which rejects escapes.

use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, PathResolver};

fn run_script(root: &GuardedPath, script: &str) -> Result<(), anyhow::Error> {
    let steps = oxdock_core::parse_script(script).expect("parse script");
    run_steps_with_context_result_with_io(root, root, &steps, ExecIo::new()).map(|_| ())
}

fn read_trimmed(root: &GuardedPath, rel: &str) -> String {
    let path = root.join(rel).unwrap();
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    resolver.read_to_string(&path).unwrap().trim().to_string()
}

fn assert_absent(root: &GuardedPath, rel: &str) {
    // Guarded existence check: raw `Path::exists` issues a host `statx`,
    // which Miri isolation rejects.
    let path = root.join(rel).unwrap();
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    assert!(
        resolver.entry_kind(&path).is_err(),
        "{rel} should not exist"
    );
}

#[cfg_attr(
    miri,
    ignore = "GLOB iteration needs host filesystem traversal; blocked under Miri isolation"
)]
#[test]
fn glob_parent_patterns_match_nothing() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
            WRITE probe.txt "x"
            FOR $f: STRING IN GLOB("../") { WRITE escaped.txt "leak" }
            FOR $f: STRING IN GLOB("../*") { WRITE escaped2.txt "leak" }
            ASSERT_ABSENT escaped.txt
            ASSERT_ABSENT escaped2.txt
        "#},
    )
    .unwrap();
    // Empty iteration writes nothing; double-check outside the DSL too.
    assert_absent(&root, "escaped.txt");
    assert_absent(&root, "escaped2.txt");
}

#[cfg_attr(
    miri,
    ignore = "GLOB iteration needs host filesystem traversal; blocked under Miri isolation"
)]
#[test]
fn glob_nested_parent_pattern_matches_nothing() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
            MKDIR a
            WRITE a/inner.txt "x"
            FOR $f: STRING IN GLOB("a/../../*") { WRITE escaped3.txt "leak" }
            ASSERT_ABSENT escaped3.txt
        "#},
    )
    .unwrap();
    assert_absent(&root, "escaped3.txt");
}

#[cfg_attr(
    miri,
    ignore = "GLOB iteration needs host filesystem traversal; blocked under Miri isolation"
)]
#[test]
fn glob_normal_patterns_still_list_contents() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
            WRITE probe.txt "x"
            FOR $f: STRING IN GLOB("*.txt") { WRITE seen.txt "saw" }
            ASSERT_FILE seen.txt "saw"
        "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "seen.txt"), "saw");
}

#[test]
fn load_toml_rejects_parent_dir_escape() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let err =
        run_script(&root, r#"LET $d: MAP = LOAD_TOML("../escape.toml")"#).expect_err("escape must fail");
    assert!(
        err.to_string().contains("escape"),
        "expected escape error, got {err}"
    );
}
