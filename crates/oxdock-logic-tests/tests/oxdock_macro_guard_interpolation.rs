#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::GuardedPath;
use oxdock_macros::oxdock;
use std::path::Path;

#[test]
fn guard_env_exists_interpolation() {
    let flag = "OXDOCK_TEST_FLAG";
    let steps = oxdock! {
        INHERIT_ENV [OXDOCK_TEST_FLAG]
        [#flag] WRITE guard-test.txt guard-passed
    };

    let root = GuardedPath::new_root(Path::new(".")).unwrap();

    // Guard passes: flag is set, file should exist
    let mut io = ExecIo::new();
    io.insert_inherit_env("OXDOCK_TEST_FLAG", "1");
    run_steps_with_context_result_with_io(&root, &root, &steps, io).unwrap();
    let file = root.join("guard-test.txt").unwrap();
    assert!(file.exists(), "file should exist when guard passes");

    // Guard fails: flag not set, file should NOT exist
    std::fs::remove_file(file.as_path()).ok();
    let io_no_flag = ExecIo::new();
    run_steps_with_context_result_with_io(&root, &root, &steps, io_no_flag).unwrap();
    assert!(!file.exists(), "file should not exist when guard fails");
}

#[test]
fn guard_env_equals_interpolation() {
    let key = "OXDOCK_TEST_MODE";
    let val = "release";
    let steps = oxdock! {
        INHERIT_ENV [OXDOCK_TEST_MODE]
        [eq(env:#key, #val)] WRITE mode-test.txt matched
    };

    let root = GuardedPath::new_root(Path::new(".")).unwrap();

    // Guard passes: value matches
    let mut io = ExecIo::new();
    io.insert_inherit_env("OXDOCK_TEST_MODE", "release");
    run_steps_with_context_result_with_io(&root, &root, &steps, io).unwrap();
    let file = root.join("mode-test.txt").unwrap();
    assert!(file.exists(), "file should exist when env equals match");

    // Guard fails: value doesn't match
    std::fs::remove_file(file.as_path()).ok();
    let mut io_wrong = ExecIo::new();
    io_wrong.insert_inherit_env("OXDOCK_TEST_MODE", "debug");
    run_steps_with_context_result_with_io(&root, &root, &steps, io_wrong).unwrap();
    assert!(
        !file.exists(),
        "file should not exist when env equals mismatch"
    );
}

#[test]
fn guard_not_interpolation() {
    let flag = "OXDOCK_TEST_SKIP";
    let steps = oxdock! {
        INHERIT_ENV [OXDOCK_TEST_SKIP]
        [not(env:#flag)] WRITE not-skip-test.txt ran
    };

    let root = GuardedPath::new_root(Path::new(".")).unwrap();

    // Guard passes: flag NOT set, so negation passes
    let io = ExecIo::new();
    run_steps_with_context_result_with_io(&root, &root, &steps, io).unwrap();
    let file = root.join("not-skip-test.txt").unwrap();
    assert!(file.exists(), "file should exist when negated guard passes");

    // Guard fails: flag IS set, so negation fails
    std::fs::remove_file(file.as_path()).ok();
    let mut io_set = ExecIo::new();
    io_set.insert_inherit_env("OXDOCK_TEST_SKIP", "1");
    run_steps_with_context_result_with_io(&root, &root, &steps, io_set).unwrap();
    assert!(
        !file.exists(),
        "file should not exist when negated guard fails"
    );
}

#[test]
fn guard_static_bool_interpolation() {
    let flag = true;
    let steps = oxdock! {
        [bool:#flag] WRITE bool-test.txt passed
    };

    let root = GuardedPath::new_root(Path::new(".")).unwrap();

    // Guard passes: flag is true
    let io = ExecIo::new();
    run_steps_with_context_result_with_io(&root, &root, &steps, io).unwrap();
    let file = root.join("bool-test.txt").unwrap();
    assert!(file.exists(), "file should exist when bool guard is true");

    // Guard fails: flag is false
    std::fs::remove_file(file.as_path()).ok();
    let flag = false;
    let steps = oxdock! {
        [bool:#flag] WRITE bool-test.txt should-not-exist
    };
    let io = ExecIo::new();
    run_steps_with_context_result_with_io(&root, &root, &steps, io).unwrap();
    assert!(
        !file.exists(),
        "file should not exist when bool guard is false"
    );
}
