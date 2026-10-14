use oxdock_core::run_steps;
use oxdock_fs::{GuardedPath, PathResolver, env as oxdock_env};
use oxdock_parser::{Step, StepKind};

fn read_file(path: &GuardedPath) -> String {
    let resolver = PathResolver::new(path.root(), path.root()).unwrap();
    resolver
        .read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
        .trim()
        .to_string()
}

#[test]
/// Verifies that `run_steps` can be called multiple times against the same filesystem root,
/// allowing for incremental state accumulation (e.g., incremental compilation artifacts).
/// This supports the `oxbook-cli` feature where subsequent code blocks run in the same
/// persistent environment (filesystem).
fn incremental_execution_persists_filesystem_state() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    // First execution: Create a file
    let steps1 = vec![Step {
        guard: None,
        kind: StepKind::Write {
            path: "state.txt".into(),
            contents: Some("initial".into()),
        },
        scope_enter: 0,
        scope_exit: 0,
    }];

    run_steps(&root, &steps1).unwrap();
    assert_eq!(read_file(&root.join("state.txt").unwrap()), "initial");

    // Second execution: Modify the file
    // This simulates a second command running against the same environment (filesystem)
    // Closed child env: the shell resolves through the listed PATH.
    let steps2 = vec![
        Step {
            guard: None,
            kind: StepKind::InheritEnv {
                keys: vec![
                    oxdock_env::os::PATH.to_string(),
                    oxdock_env::os::windows::SYSTEM_ROOT.to_string(),
                ],
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            #[allow(clippy::disallowed_macros)]
            kind: StepKind::Run(if cfg!(windows) {
                "echo updated> state.txt".into()
            } else {
                "echo updated > state.txt".into()
            }),
            scope_enter: 0,
            scope_exit: 0,
        },
    ];

    run_steps(&root, &steps2).unwrap();
    assert_eq!(read_file(&root.join("state.txt").unwrap()), "updated");
}

#[test]
fn incremental_execution_persists_cwd() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    // First execution: Create a directory and file inside it
    let steps1 = vec![
        Step {
            guard: None,
            kind: StepKind::Mkdir("subdir".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Write {
                path: "subdir/marker.txt".into(),
                contents: Some("here".into()),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];

    run_steps(&root, &steps1).unwrap();

    // Second execution: Verify we can access the file created in the previous step
    // Note: run_steps resets CWD to root by default unless we pass a modified context.
    // But here we just want to verify the file exists.
    // Closed child env: the shell resolves through the listed PATH.
    let steps2 = vec![
        Step {
            guard: None,
            kind: StepKind::InheritEnv {
                keys: vec![
                    oxdock_env::os::PATH.to_string(),
                    oxdock_env::os::windows::SYSTEM_ROOT.to_string(),
                ],
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            #[allow(clippy::disallowed_macros)]
            kind: StepKind::Run(if cfg!(windows) {
                "type subdir\\marker.txt".into()
            } else {
                "cat subdir/marker.txt".into()
            }),
            scope_enter: 0,
            scope_exit: 0,
        },
    ];

    run_steps(&root, &steps2).unwrap();
}
