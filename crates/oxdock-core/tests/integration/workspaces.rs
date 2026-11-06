use indoc::indoc;
use oxdock_core::{Engine, parse_script, run_steps_without_snapshot};
use oxdock_fs::GuardedPath;

#[test]
fn direct_runner_collapses_snapshot_and_local_to_given_root() {
    // Without a snapshot there is no second root: the direct resolver
    // pre-publishes the given root as the snapshot concrete root, so
    // WORKSPACE SNAPSHOT and WORKSPACE LOCAL select the same directory
    // and writes under either land in the given root.
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let steps = parse_script(indoc! {r#"
        IMPORT [STD]
        WORKSPACE SNAPSHOT
        LET $s: STRING = CWD()
        WORKSPACE LOCAL
        LET $l: STRING = CWD()
        ASSERT_EQ $s $l
        WORKSPACE SNAPSHOT
        WRITE snap.txt snap
        WORKSPACE LOCAL
        WRITE local.txt local
    "#})
    .unwrap();
    run_steps_without_snapshot(&root, &steps).unwrap();
    assert!(
        root.join("snap.txt").unwrap().exists(),
        "snapshot-targeted write must land in the given root"
    );
    assert!(
        root.join("local.txt").unwrap().exists(),
        "local-targeted write must land in the given root"
    );
}

#[test]
fn lazy_runner_keeps_snapshot_and_local_distinct() {
    // The lazy default stages a snapshot apart from the build context, so
    // the two targets resolve differently. No writes here, so no snapshot
    // directory is ever materialized.
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let steps = parse_script(indoc! {r#"
        IMPORT [STD]
        WORKSPACE SNAPSHOT
        LET $s: STRING = CWD()
        WORKSPACE LOCAL
        LET $l: STRING = CWD()
        ASSERT_NE $s $l
    "#})
    .unwrap();
    Engine::new().run_steps(&root, &steps).unwrap();
}
