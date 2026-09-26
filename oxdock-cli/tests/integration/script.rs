use oxdock_cli::{Step, StepKind, run_script};
use oxdock_fs::{GuardedPath, PathResolver};

#[cfg_attr(
    miri,
    ignore = "spawns subprocesses; process spawning not supported under Miri"
)]
#[test]
fn script_runs_copy_and_symlink() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    // Prepare minimal workspace structure using the PathResolver to avoid
    // calling `std::fs` directly from non-fs crates.
    let resolver = PathResolver::new(root.as_path(), root.as_path()).unwrap();
    resolver
        .create_dir_all(&root.join("client/dist").unwrap())
        .unwrap();
    resolver
        .create_dir_all(&root.join("server").unwrap())
        .unwrap();
    // Seed dist with a file
    resolver
        .write_file(&root.join("client/dist/test.txt").unwrap(), b"hello\n")
        .unwrap();

    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::Workdir("client".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Run("echo ok".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Workdir("/".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Copy {
                from_workspace: None,
                from_host: false,
                to_host: false,
                from: "./client/dist".into(),
                to: "./client/dist-copy".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Symlink {
                from_workspace: None,
                from: "./client/dist".into(),
                to: "./server/dist".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Run("echo ok".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
    ];

    let res = run_script(&root, &steps);

    // Copy should exist and contain the file
    let copied = root.join("client/dist-copy/test.txt").unwrap();
    assert!(copied.as_path().exists());
    let contents = resolver.read_to_string(&copied).unwrap();
    assert!(contents.contains("hello"));

    // Symlink: on Unix it should succeed; on non-Unix we expect an explicit error
    #[cfg(unix)]
    {
        res.unwrap();
        let linked = root.join("server/dist/test.txt").unwrap();
        assert!(linked.as_path().exists());
    }
    #[cfg(not(unix))]
    {
        if oxdock_sys_test_utils::can_create_symlinks(root.as_path()) {
            // Host supports symlinks : the script should succeed and the link should exist.
            res.unwrap();
            let linked = root.join("server/dist/test.txt").unwrap();
            assert!(linked.as_path().exists());
        } else {
            // Host cannot create symlinks : we expect an explicit error and no copy fallback.
            assert!(
                res.is_err(),
                "SYMLINK should error on platforms without symlink privilege"
            );
            let linked_copy = root.join("server/dist/test.txt").unwrap();
            assert!(
                !linked_copy.as_path().exists(),
                "No copy fallback should occur"
            );
        }
    }
}
