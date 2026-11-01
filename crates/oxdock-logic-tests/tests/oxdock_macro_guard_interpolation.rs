#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::GuardedPath;
use oxdock_macros::oxdock;

#[test]
fn guard_env_exists_interpolation() {
    let flag = "OXDOCK_TEST_FLAG";
    let steps = oxdock! {
        INHERIT_ENV [OXDOCK_TEST_FLAG]
        [#flag] WRITE guard-test.txt guard-passed
    };

    // Guard passes: flag is set, file should exist
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let mut io = ExecIo::new();
    io.insert_inherit_env("OXDOCK_TEST_FLAG", "1");
    run_steps_with_context_result_with_io(&root, &root, &steps, io).unwrap();
    let file = root.join("guard-test.txt").unwrap();
    assert!(file.exists(), "file should exist when guard passes");

    // Guard fails: flag not set, file should NOT exist
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let io_no_flag = ExecIo::new();
    run_steps_with_context_result_with_io(&root, &root, &steps, io_no_flag).unwrap();
    let file = root.join("guard-test.txt").unwrap();
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

    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    // Guard passes: value matches
    let mut io = ExecIo::new();
    io.insert_inherit_env("OXDOCK_TEST_MODE", "release");
    run_steps_with_context_result_with_io(&root, &root, &steps, io).unwrap();
    let file = root.join("mode-test.txt").unwrap();
    assert!(file.exists(), "file should exist when env equals match");

    // Guard fails: value doesn't match
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let mut io_wrong = ExecIo::new();
    io_wrong.insert_inherit_env("OXDOCK_TEST_MODE", "debug");
    run_steps_with_context_result_with_io(&root, &root, &steps, io_wrong).unwrap();
    let file = root.join("mode-test.txt").unwrap();
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

    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    // Guard passes: flag NOT set, so negation passes
    let io = ExecIo::new();
    run_steps_with_context_result_with_io(&root, &root, &steps, io).unwrap();
    let file = root.join("not-skip-test.txt").unwrap();
    assert!(file.exists(), "file should exist when negated guard passes");

    // Guard fails: flag IS set, so negation fails
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let mut io_set = ExecIo::new();
    io_set.insert_inherit_env("OXDOCK_TEST_SKIP", "1");
    run_steps_with_context_result_with_io(&root, &root, &steps, io_set).unwrap();
    let file = root.join("not-skip-test.txt").unwrap();
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

    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    // Guard passes: flag is true
    let io = ExecIo::new();
    run_steps_with_context_result_with_io(&root, &root, &steps, io).unwrap();
    let file = root.join("bool-test.txt").unwrap();
    assert!(file.exists(), "file should exist when bool guard is true");

    // Guard fails: flag is false
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let flag = false;
    let steps = oxdock! {
        [bool:#flag] WRITE bool-test.txt should-not-exist
    };
    let io = ExecIo::new();
    run_steps_with_context_result_with_io(&root, &root, &steps, io).unwrap();
    let file = root.join("bool-test.txt").unwrap();
    assert!(
        !file.exists(),
        "file should not exist when bool guard is false"
    );
}

#[test]
fn macro_block_string_dedents_and_runs() {
    let steps = oxdock! {
        LET $msg: STRING = """
            hello
            world
            """
        WRITE block.txt "{{ $msg }}"
    };

    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let io = ExecIo::new();
    run_steps_with_context_result_with_io(&root, &root, &steps, io).unwrap();
    let resolver = oxdock_fs::PathResolver::new(root.root(), root.root()).unwrap();
    let text = resolver
        .read_to_string(&root.join("block.txt").unwrap())
        .unwrap();
    assert_eq!(text, "hello\nworld");
}

#[test]
fn macro_block_string_interpolates() {
    let steps = oxdock! {
        LET $name: STRING = "world"
        LET $greeting: STRING = """
            hello {{ $name }}
            bye {{ $name }}
            """
        WRITE greet.txt "{{ $greeting }}"
    };

    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let io = ExecIo::new();
    run_steps_with_context_result_with_io(&root, &root, &steps, io).unwrap();
    let resolver = oxdock_fs::PathResolver::new(root.root(), root.root()).unwrap();
    let text = resolver
        .read_to_string(&root.join("greet.txt").unwrap())
        .unwrap();
    assert_eq!(text, "hello world\nbye world");
}
