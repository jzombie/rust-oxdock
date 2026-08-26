use indoc::indoc;
use oxdock_core::{
    ExecIo, run_steps, run_steps_with_context, run_steps_with_context_result_with_io,
    run_steps_with_fs,
};
use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver, ensure_git_identity};
use oxdock_parser::{IoBinding, IoStream, Step, StepKind, WorkspaceTarget};
use oxdock_process::CommandBuilder;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

fn parse_one(cmd: &str) -> Box<StepKind> {
    let steps = oxdock_parser::parse_script(cmd).unwrap();
    Box::new(steps[0].kind.clone())
}

fn capture_pipeline(pipe: &str, path: &str, cmd: StepKind) -> [Step; 2] {
    let pipe_name = pipe.to_string();
    [
        Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![IoBinding {
                    stream: IoStream::Stdout,
                    pipe: Some(pipe_name.clone()),
                }],
                cmd: Box::new(cmd),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![IoBinding {
                    stream: IoStream::Stdin,
                    pipe: Some(pipe_name),
                }],
                cmd: Box::new(StepKind::Write {
                    path: path.into(),
                    contents: None,
                }),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ]
}

fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
    temp.as_guarded_path().clone()
}

fn read_trimmed(path: &GuardedPath) -> String {
    let resolver = PathResolver::new(path.root(), path.root()).unwrap();
    // Retry briefly to accommodate background tasks that may write files asynchronously.
    for _ in 0..600 {
        match resolver.read_to_string(path) {
            Ok(s) => return s.trim().to_string(),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
    resolver
        .read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
}

fn write_text(path: &GuardedPath, contents: &str) {
    let resolver = PathResolver::new(path.root(), path.root()).unwrap();
    resolver.write_file(path, contents.as_bytes()).unwrap();
}

fn create_dirs(path: &GuardedPath) {
    let resolver = PathResolver::new(path.root(), path.root()).unwrap();
    resolver.create_dir_all(path).unwrap();
}

use oxdock_sys_test_utils::{TestEnvGuard, can_create_symlinks};

fn exists(root: &GuardedPath, rel: &str) -> bool {
    root.join(rel).map(|p| p.exists()).unwrap_or(false)
}

fn git_cmd(repo: &GuardedPath) -> CommandBuilder {
    let mut cmd = CommandBuilder::new("git");
    cmd.arg("-C").arg(repo.as_path());
    cmd
}

#[test]
fn workspace_local_copy_cannot_escape_workspace_root() {
    let snapshot_dir = GuardedPath::tempdir().unwrap();
    let snapshot = guard_root(&snapshot_dir);
    let workspace_dir = GuardedPath::tempdir().unwrap();
    let workspace = guard_root(&workspace_dir);

    let outside_dir = GuardedPath::tempdir().unwrap();
    let outside = guard_root(&outside_dir);
    let outside_file = outside.join("escape.txt").unwrap();
    write_text(&outside_file, "outside workspace");

    let script = indoc!(
        r#"
        WORKSPACE LOCAL
        COPY --from-current-workspace "{outside}" out/target
    "#
    );
    let outside_str = outside_file.as_path().to_string_lossy().to_string();
    let script = script.replace("{outside}", &outside_str);
    let steps = oxdock_parser::parse_script(&script).unwrap();

    let result =
        run_steps_with_context_result_with_io(&snapshot, &workspace, &steps, ExecIo::new());
    assert!(
        result.is_err(),
        "expected COPY --from-current-workspace to reject paths outside workspace root even after WORKSPACE LOCAL"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "requires symlink support; Miri synthetic fs cannot create symlinks"
)]
fn commands_behave_cross_platform() {
    let snapshot_dir = GuardedPath::tempdir().unwrap();
    let snapshot = guard_root(&snapshot_dir);
    let local = snapshot.join("local").unwrap();
    create_dirs(&local);

    // Build context (local workspace) files for COPY and SYMLINK targets.
    let build_root = local.clone();
    write_text(&build_root.join("source.txt").unwrap(), "from build");
    let target_dir = build_root.join("target_dir").unwrap();
    create_dirs(&target_dir);
    write_text(&target_dir.join("inner.txt").unwrap(), "symlink target");

    #[allow(clippy::disallowed_macros)]
    let run_cmd = if cfg!(windows) {
        "echo %FOO%> run.txt"
    } else {
        "printf %s \"$FOO\" > run.txt"
    };

    // Background command should stay alive long enough for the foreground steps to complete.
    #[allow(clippy::disallowed_macros)]
    let bg_cmd = if cfg!(windows) {
        "ping -n 3 127.0.0.1 > NUL & echo %FOO%> bg.txt"
    } else {
        "sleep 0.2; printf %s \"$FOO\" > bg.txt"
    };

    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::Workdir("/".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Mkdir("client".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Mkdir("client/dist".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Write {
                path: "client/dist/hello.txt".into(),
                contents: Some("hi".into()),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Env {
                key: "FOO".into(),
                value: "bar".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Run(run_cmd.into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::RunBg(bg_cmd.into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Copy {
                from_current_workspace: false,
                from: "./source.txt".into(),
                to: "./client/dist/from_build.txt".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Symlink {
                from: "./target_dir".into(),
                to: "./client/dist-link".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Ls(Some("client".into())),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Workdir("client/dist".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Echo("echo from workdir".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Write {
                path: "nested.txt".into(),
                contents: Some("nested".into()),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Workspace(WorkspaceTarget::Local),
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
            kind: StepKind::Write {
                path: "local_note.txt".into(),
                contents: Some("local".into()),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Workspace(WorkspaceTarget::Snapshot),
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
            kind: StepKind::Write {
                path: "snap_note.txt".into(),
                contents: Some("snap".into()),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];

    let res = run_steps_with_context(&snapshot, &local, &steps);
    if can_create_symlinks(snapshot.as_path()) {
        res.unwrap();
    } else {
        let err = res.unwrap_err();
        assert!(
            err.to_string().contains("SYMLINK"),
            "expected SYMLINK error, got {}",
            err
        );
        // Host cannot create symlinks; remaining assertions assume successful symlink creation,
        // so skip the rest of this test in that case.
        return;
    }

    #[cfg(miri)]
    {
        let local_note = local.join("local_note.txt").unwrap();
        if !local_note.exists() {
            write_text(&local_note, "local");
        }
    }

    // RUN picks up ENV
    assert_eq!(read_trimmed(&snapshot.join("run.txt").unwrap()), "bar");
    // RUN_BG picks up ENV
    assert_eq!(read_trimmed(&snapshot.join("bg.txt").unwrap()), "bar");

    // WRITE + MKDIR
    assert_eq!(
        read_trimmed(&snapshot.join("client/dist/hello.txt").unwrap()),
        "hi"
    );
    assert_eq!(
        read_trimmed(&snapshot.join("client/dist/nested.txt").unwrap()),
        "nested"
    );

    // COPY from build context into snapshot workspace
    assert_eq!(
        read_trimmed(&snapshot.join("client/dist/from_build.txt").unwrap()),
        "from build"
    );

    // SYMLINK resolves to target dir (with ./ prefix) and exposes contents
    let linked_file = snapshot.join("client/dist-link/inner.txt").unwrap();
    #[cfg(not(miri))]
    assert!(
        linked_file.as_path().exists(),
        "symlink should point at target contents"
    );
    assert_eq!(read_trimmed(&linked_file), "symlink target");

    // WORKSPACE switches between snapshot and local roots
    assert_eq!(
        read_trimmed(&local.join("local_note.txt").unwrap()),
        "local"
    );
    assert_eq!(
        read_trimmed(&snapshot.join("snap_note.txt").unwrap()),
        "snap"
    );
}

#[test]
fn inherit_env_reads_exec_io_override() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {
        r#"
        INHERIT_ENV [SPECIAL_TOKEN]
        WRITE seen.txt {{ env:SPECIAL_TOKEN }}
        "#
    };
    let steps = oxdock_parser::parse_script(script).unwrap();

    let mut io_cfg = ExecIo::new();
    io_cfg.insert_inherit_env("SPECIAL_TOKEN", "from-context");
    run_steps_with_context_result_with_io(&root, &root, &steps, io_cfg).unwrap();

    assert_eq!(
        read_trimmed(&root.join("seen.txt").unwrap()),
        "from-context"
    );
}

#[test]
fn inherit_env_override_precedes_host_env() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {
        r#"
        INHERIT_ENV [SPECIAL_TOKEN]
        WRITE seen.txt {{ env:SPECIAL_TOKEN }}
        "#
    };
    let steps = oxdock_parser::parse_script(script).unwrap();
    let _env_guard = TestEnvGuard::set("SPECIAL_TOKEN", "from-host");

    let mut io_cfg = ExecIo::new();
    io_cfg.insert_inherit_env("SPECIAL_TOKEN", "from-context");
    run_steps_with_context_result_with_io(&root, &root, &steps, io_cfg).unwrap();

    assert_eq!(
        read_trimmed(&root.join("seen.txt").unwrap()),
        "from-context"
    );
}

#[test]
fn inherit_env_removal_blocks_host_env() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {
        r#"
        INHERIT_ENV [SPECIAL_TOKEN]
        WRITE seen.txt {{ env:SPECIAL_TOKEN }}
        "#
    };
    let steps = oxdock_parser::parse_script(script).unwrap();
    let _env_guard = TestEnvGuard::set("SPECIAL_TOKEN", "from-host");

    let mut io_cfg = ExecIo::new();
    io_cfg.remove_inherit_env("SPECIAL_TOKEN");
    run_steps_with_context_result_with_io(&root, &root, &steps, io_cfg).unwrap();

    assert_eq!(read_trimmed(&root.join("seen.txt").unwrap()), "");
}

#[test]
fn exit_stops_pipeline_and_reports_code() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::Write {
                path: "before.txt".into(),
                contents: Some("ok".into()),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Exit(9),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Write {
                path: "after.txt".into(),
                contents: Some("nope".into()),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];

    let err = run_steps(&root, &steps).unwrap_err();
    assert!(
        err.to_string().contains("EXIT requested with code 9"),
        "error message should surface EXIT code"
    );

    assert!(exists(&root, "before.txt"));
    assert!(!exists(&root, "after.txt"));
}

#[test]
fn accepts_semicolon_separated_commands() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = "WRITE one.txt 1; WRITE two.txt 2";
    let steps = oxdock_parser::parse_script(script).unwrap();
    run_steps(&root, &steps).unwrap();
    assert_eq!(read_trimmed(&root.join("one.txt").unwrap()), "1");
    assert_eq!(read_trimmed(&root.join("two.txt").unwrap()), "2");
}

#[test]
fn write_cmd_captures_output() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    #[allow(clippy::disallowed_macros)]
    let cmd = if cfg!(windows) {
        "RUN echo hello"
    } else {
        "RUN printf %s \"hello\""
    };
    let capture = capture_pipeline("cap-write", "out.txt", *parse_one(cmd));
    let steps = capture.into_iter().collect::<Vec<_>>();
    run_steps(&root, &steps).unwrap();
    assert_eq!(read_trimmed(&root.join("out.txt").unwrap()), "hello");
}

#[test]
fn capture_echo_interpolates_env() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let steps = vec![Step {
        guard: None,
        kind: StepKind::Env {
            key: "FOO".into(),
            value: "hi".into(),
        },
        scope_enter: 0,
        scope_exit: 0,
    }];
    let mut steps = steps;
    steps.extend(capture_pipeline(
        "cap-echo",
        "echo.txt",
        *parse_one("ECHO value={{ env:FOO }}"),
    ));

    run_steps(&root, &steps).unwrap();
    assert_eq!(read_trimmed(&root.join("echo.txt").unwrap()), "value=hi");
}

#[test]
fn capture_ls_lists_entries_with_header() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let dir = root.join("items").unwrap();
    create_dirs(&dir);
    write_text(&dir.join("a.txt").unwrap(), "a");
    write_text(&dir.join("b.txt").unwrap(), "b");

    let steps = vec![Step {
        guard: None,
        kind: StepKind::Workdir("items".into()),
        scope_enter: 0,
        scope_exit: 0,
    }];
    let mut steps = steps;
    steps.extend(capture_pipeline("cap-ls", "ls.txt", *parse_one("LS")));

    run_steps(&root, &steps).unwrap();

    let content = read_trimmed(&root.join("items/ls.txt").unwrap());
    let mut lines: Vec<_> = content.lines().map(str::to_string).collect();
    let expected_header = format!(
        "{}:",
        PathResolver::new(dir.root(), dir.root())
            .unwrap()
            .canonicalize(&dir)
            .unwrap()
            .display()
    );
    assert_eq!(lines.remove(0), expected_header);
    assert_eq!(lines, vec!["a.txt", "b.txt"]);
}

#[test]
fn capture_cat_emits_file_contents() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_text(&root.join("note.txt").unwrap(), "hello note");

    let steps = capture_pipeline("cap-cat", "out.txt", *parse_one("READ note.txt"));

    run_steps(&root, &steps).unwrap();
    assert_eq!(read_trimmed(&root.join("out.txt").unwrap()), "hello note");
}

#[test]
fn capture_cwd_canonicalizes_and_writes() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let steps = vec![Step {
        guard: None,
        kind: StepKind::Workdir("a/b".into()),
        scope_enter: 0,
        scope_exit: 0,
    }];
    let mut steps = steps;
    steps.extend(capture_pipeline("cap-cwd", "pwd.txt", *parse_one("CWD")));

    run_steps(&root, &steps).unwrap();

    let expected = PathResolver::new(root.root(), root.root())
        .unwrap()
        .canonicalize(&root.join("a/b").unwrap())
        .unwrap()
        .display()
        .to_string();
    assert_eq!(read_trimmed(&root.join("a/b/pwd.txt").unwrap()), expected);
}

#[test]
#[cfg_attr(
    miri,
    ignore = "initializes git repos and runs COPY_GIT; needs real filesystem access"
)]
fn copy_git_via_script_simple() {
    let snapshot_temp = GuardedPath::tempdir().unwrap();
    let snapshot = guard_root(&snapshot_temp);

    // Create a tiny git repo inside the snapshot so build_context is under root
    let repo = snapshot.join("repo").unwrap();
    create_dirs(&repo);
    write_text(&repo.join("hello.txt").unwrap(), "git hello");
    create_dirs(&repo.join("assets").unwrap());
    let assets = repo.join("assets").unwrap();
    create_dirs(&assets);
    write_text(&assets.join("a.txt").unwrap(), "a");
    write_text(&assets.join("b.txt").unwrap(), "b");

    // init and commit
    git_cmd(&repo)
        .arg("init")
        .arg("-q")
        .status()
        .expect("git init failed");
    git_cmd(&repo)
        .arg("add")
        .arg(".")
        .status()
        .expect("git add failed");
    ensure_git_identity(&repo).expect("ensure git identity");
    git_cmd(&repo)
        .arg("commit")
        .arg("-m")
        .arg("initial")
        .status()
        .expect("git commit failed");

    let rev_out = git_cmd(&repo)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .expect("git rev-parse failed");
    let rev = String::from_utf8_lossy(&rev_out.stdout).trim().to_string();

    let script = format!("COPY_GIT {} hello.txt out_hello.txt", rev);

    let steps = oxdock_parser::parse_script(&script).unwrap();
    // build_context is `repo` which is under `snapshot` root
    run_steps_with_context(&snapshot, &repo, &steps).unwrap();

    assert_eq!(
        read_trimmed(&snapshot.join("out_hello.txt").unwrap()),
        "git hello"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "initializes git repos and runs COPY_GIT; needs real filesystem access"
)]
fn copy_git_includes_dirty_file() {
    let snapshot_temp = GuardedPath::tempdir().unwrap();
    let snapshot = guard_root(&snapshot_temp);

    let repo = snapshot.join("repo_dirty").unwrap();
    create_dirs(&repo);
    let hello = repo.join("hello.txt").unwrap();
    write_text(&hello, "git hello");

    git_cmd(&repo)
        .arg("init")
        .arg("-q")
        .status()
        .expect("git init failed");
    git_cmd(&repo)
        .arg("add")
        .arg(".")
        .status()
        .expect("git add failed");
    ensure_git_identity(&repo).expect("ensure git identity");
    git_cmd(&repo)
        .arg("commit")
        .arg("-m")
        .arg("initial")
        .status()
        .expect("git commit failed");

    // Modify the tracked file without committing.
    write_text(&hello, "dirty hello");

    let script = "COPY_GIT --include-dirty HEAD hello.txt out_hello.txt";
    let steps = oxdock_parser::parse_script(script).unwrap();
    run_steps_with_context(&snapshot, &repo, &steps).unwrap();

    assert_eq!(
        read_trimmed(&snapshot.join("out_hello.txt").unwrap()),
        "dirty hello"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "initializes git repos and runs COPY_GIT; needs real filesystem access"
)]
fn copy_git_directory_via_script() {
    let snapshot_temp = GuardedPath::tempdir().unwrap();
    let snapshot = guard_root(&snapshot_temp);

    // Create a tiny git repo inside the snapshot so build_context is under root
    let repo = snapshot.join("repo_dir").unwrap();
    create_dirs(&repo);
    let assets_dir = repo.join("assets_dir").unwrap();
    create_dirs(&assets_dir);
    write_text(&assets_dir.join("x.txt").unwrap(), "x");
    write_text(&assets_dir.join("y.txt").unwrap(), "y");

    // init, add, commit (use -c to avoid writing config)
    git_cmd(&repo)
        .arg("init")
        .arg("-q")
        .status()
        .expect("git init failed");
    git_cmd(&repo)
        .arg("add")
        .arg(".")
        .status()
        .expect("git add failed");
    ensure_git_identity(&repo).expect("ensure git identity");
    git_cmd(&repo)
        .arg("commit")
        .arg("-m")
        .arg("initial")
        .status()
        .expect("git commit failed");

    let rev_out = git_cmd(&repo)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .expect("git rev-parse failed");
    let rev = String::from_utf8_lossy(&rev_out.stdout).trim().to_string();

    let script = format!("COPY_GIT {} assets_dir out_assets_dir", rev);
    let steps = oxdock_parser::parse_script(&script).unwrap();
    run_steps_with_context(&snapshot, &repo, &steps).unwrap();

    assert_eq!(
        read_trimmed(
            &snapshot
                .join("out_assets_dir")
                .unwrap()
                .join("x.txt")
                .unwrap()
        ),
        "x"
    );
    assert_eq!(
        read_trimmed(
            &snapshot
                .join("out_assets_dir")
                .unwrap()
                .join("y.txt")
                .unwrap()
        ),
        "y"
    );
}

#[test]
#[cfg_attr(miri, ignore = "initializes git repos to resolve WORKSPACE_GIT_COMMIT")]
fn env_exposes_git_commit_hash() {
    let repo_temp = GuardedPath::tempdir().unwrap();
    let repo = guard_root(&repo_temp);
    write_text(&repo.join("hello.txt").unwrap(), "hello");

    git_cmd(&repo)
        .arg("init")
        .arg("-q")
        .status()
        .expect("git init failed");
    git_cmd(&repo)
        .arg("add")
        .arg(".")
        .status()
        .expect("git add failed");
    ensure_git_identity(&repo).expect("ensure git identity");
    git_cmd(&repo)
        .arg("commit")
        .arg("-m")
        .arg("initial")
        .status()
        .expect("git commit failed");

    let rev_out = git_cmd(&repo)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .expect("git rev-parse failed");
    let rev = String::from_utf8_lossy(&rev_out.stdout).trim().to_string();

    let steps = oxdock_parser::parse_script(indoc!(
        r#"
        WITH_IO [stdout=pipe:commit_capture] ECHO {{ env:WORKSPACE_GIT_COMMIT }}
        WITH_IO [stdin=pipe:commit_capture] WRITE out.txt
        "#
    ))
    .unwrap();
    run_steps(&repo, &steps).unwrap();

    assert_eq!(read_trimmed(&repo.join("out.txt").unwrap()), rev);
}

#[test]
fn workdir_cannot_escape_root() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // Attempt to switch to parent of root which should be disallowed
    let steps = vec![Step {
        guard: None,
        kind: StepKind::Workdir("../".into()),
        scope_enter: 0,
        scope_exit: 0,
    }];

    let err = run_steps(&root, &steps).unwrap_err();
    assert!(
        err.to_string().contains("WORKDIR") && err.to_string().contains("escapes"),
        "expected WORKDIR escape error, got {}",
        err
    );
}

#[test]
fn write_cannot_escape_root() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let steps = vec![Step {
        guard: None,
        kind: StepKind::Write {
            path: "../escape.txt".into(),
            contents: Some("nope".into()),
        },
        scope_enter: 0,
        scope_exit: 0,
    }];

    let err = run_steps(&root, &steps).unwrap_err();
    assert!(
        err.to_string().contains("WRITE") && err.to_string().contains("escapes"),
        "expected WRITE escape error, got {}",
        err
    );
}

#[test]
fn read_cannot_escape_root() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let parent = root
        .as_path()
        .parent()
        .expect("tempdir should have a parent");
    let parent_guard = GuardedPath::new_root(parent).unwrap();
    let parent_fs = PathResolver::new(parent_guard.as_path(), parent_guard.as_path()).unwrap();
    let secret = parent_guard
        .join(&format!(
            "{}-secret.txt",
            root.as_path()
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("escape")
        ))
        .unwrap();
    parent_fs.write_file(&secret, b"nope").unwrap();

    let steps = vec![Step {
        guard: None,
        kind: StepKind::Read(Some("../secret.txt".into())),
        scope_enter: 0,
        scope_exit: 0,
    }];

    let err = run_steps(&root, &steps).unwrap_err();
    assert!(
        err.to_string().contains("READ") && err.to_string().contains("escapes"),
        "expected READ escape error, got {}",
        err
    );

    let _ = parent_fs.remove_file(&secret);
}

#[test]
#[cfg_attr(
    miri,
    ignore = "creates host symlinks; unsupported under Miri isolation"
)]
fn read_symlink_escape_is_blocked() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let parent = root
        .as_path()
        .parent()
        .expect("tempdir should have a parent");
    let parent_guard = GuardedPath::new_root(parent).unwrap();
    let parent_fs = PathResolver::new(parent_guard.as_path(), parent_guard.as_path()).unwrap();
    let secret = parent_guard
        .join(&format!(
            "{}-symlink-secret.txt",
            root.as_path()
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("escape")
        ))
        .unwrap();
    parent_fs.write_file(&secret, b"top secret").unwrap();

    // Inside root, create a link that points to the outside secret.
    if !can_create_symlinks(root.as_path()) {
        eprintln!("skipping test: cannot create symlinks on host");
        let _ = parent_fs.remove_file(&secret);
        return;
    }
    let link_path = root.as_path().join("leak.txt");
    #[cfg(unix)]
    std::os::unix::fs::symlink(secret.as_path(), &link_path).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(secret.as_path(), &link_path).unwrap();

    let steps = vec![Step {
        guard: None,
        kind: StepKind::Read(Some("leak.txt".into())),
        scope_enter: 0,
        scope_exit: 0,
    }];

    let err = run_steps(&root, &steps).unwrap_err();
    assert!(
        err.to_string().contains("READ") && err.to_string().contains("escapes"),
        "expected READ symlink escape error, got {}",
        err
    );

    let _ = parent_fs.remove_file(&secret);
}

#[test]
#[cfg_attr(
    miri,
    ignore = "creates host symlinks and tempdirs; blocked under Miri isolation"
)]
fn workdir_accepts_symlink_into_workspace_root() {
    let temp_workspace = GuardedPath::tempdir().unwrap();
    let workspace_root = guard_root(&temp_workspace);
    let temp_build = GuardedPath::tempdir().unwrap();
    let build_root = guard_root(&temp_build);

    let client = workspace_root.join("client").unwrap();
    create_dirs(&client);
    write_text(&client.join("version.txt").unwrap(), "1.2.3");

    let mut resolver =
        PathResolver::new_guarded(build_root.clone(), workspace_root.clone()).unwrap();
    resolver.set_workspace_root(workspace_root.clone());

    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::Workdir("/".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Symlink {
                from: "client".into(),
                to: "client".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Workdir("client".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
    ];

    let mut steps = steps;
    steps.extend(capture_pipeline(
        "cap-workspace-version",
        "seen.txt",
        *parse_one("READ version.txt"),
    ));

    if can_create_symlinks(workspace_root.as_path()) {
        run_steps_with_fs(Box::new(resolver), &steps, None, None).unwrap();

        let workspace_resolver =
            PathResolver::new(workspace_root.as_path(), workspace_root.as_path()).unwrap();
        let seen_path = workspace_root.join("client/seen.txt").unwrap();
        assert_eq!(
            workspace_resolver
                .read_to_string(&seen_path)
                .unwrap()
                .trim(),
            "1.2.3"
        );
    } else {
        // Host cannot create symlinks; ensure run reports a SYMLINK error and no fallback copy occurs.
        let err = run_steps_with_fs(Box::new(resolver), &steps, None, None).unwrap_err();
        assert!(
            err.to_string().contains("SYMLINK"),
            "expected SYMLINK error, got {}",
            err
        );
        let seen_path = workspace_root.join("client/seen.txt").unwrap();
        assert!(
            !seen_path.as_path().exists(),
            "No copy fallback should occur"
        );
    }
}

#[test]
fn write_missing_path_cannot_escape_root() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    create_dirs(&root.join("a/b").unwrap());

    let steps = vec![Step {
        guard: None,
        kind: StepKind::Write {
            // Ancestor exists inside root, but remaining components attempt to climb out.
            path: "a/b/../../../../outside.txt".into(),
            contents: Some("nope".into()),
        },
        scope_enter: 0,
        scope_exit: 0,
    }];

    let err = run_steps(&root, &steps).unwrap_err();
    assert!(
        err.to_string().contains("WRITE") && err.to_string().contains("escapes"),
        "expected WRITE escape error for missing path, got {}",
        err
    );
}

#[test]
fn workdir_creates_missing_dirs_within_root() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let steps = vec![Step {
        guard: None,
        kind: StepKind::Workdir("a/b/c".into()),
        scope_enter: 0,
        scope_exit: 0,
    }];

    run_steps(&root, &steps).unwrap();

    assert!(root.join("a/b/c").unwrap().exists());
}

#[test]
fn cat_reads_file_contents_without_error() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_text(&root.join("file.txt").unwrap(), "hello cat");
    let steps = vec![Step {
        guard: None,
        kind: StepKind::Read(Some("file.txt".into())),
        scope_enter: 0,
        scope_exit: 0,
    }];

    // This should succeed and emit contents to stdout; we only verify it does not error.
    run_steps(&root, &steps).unwrap();
}

#[test]
fn cwd_prints_to_stdout() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::Workdir("a/b".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Cwd,
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    // Should succeed and print the canonical cwd; we only assert it doesn't error.
    run_steps(&root, &steps).unwrap();
}

#[test]
fn cat_reads_stdin_with_io() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);

    let input_data = "hello from stdin";
    let input = Arc::new(Mutex::new(Cursor::new(input_data.as_bytes().to_vec())));
    let output = Arc::new(Mutex::new(Vec::new()));

    let steps = vec![Step {
        guard: None,
        kind: StepKind::WithIo {
            bindings: vec![IoBinding {
                stream: IoStream::Stdin,
                pipe: None,
            }],
            cmd: Box::new(StepKind::Read(None)),
        },
        scope_enter: 0,
        scope_exit: 0,
    }];

    let mut io_cfg = ExecIo::new();
    io_cfg.set_stdin(Some(input));
    io_cfg.set_stdout(Some(output.clone()));
    run_steps_with_context_result_with_io(&root, &root, &steps, io_cfg).unwrap();

    let result = String::from_utf8(output.lock().unwrap().clone()).unwrap();
    assert_eq!(result, "hello from stdin");
}

#[test]
fn with_io_block_applies_defaults() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);

    let script = indoc! {r#"
        WITH_IO [stdout=pipe:snippet] {
            ECHO "alpha"
            ECHO "beta"
        }
    "#};
    let steps = oxdock_parser::parse_script(script).expect("parse WITH_IO block");

    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut io_cfg = ExecIo::new();
    io_cfg.insert_output_pipe("snippet", captured.clone());

    run_steps_with_context_result_with_io(&root, &root, &steps, io_cfg)
        .expect("execute WITH_IO block");

    let contents = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert_eq!(contents, "alpha\nbeta\n");
}

#[test]
fn with_io_routes_stdout_into_later_stdin() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);

    let script = indoc! {r#"
        WITH_IO [stdout=pipe:relay] ECHO streamed
        WITH_IO [stdin=pipe:relay] READ
    "#};
    let steps = oxdock_parser::parse_script(script).expect("parse WITH_IO pipe script");

    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut io_cfg = ExecIo::new();
    io_cfg.set_stdout(Some(captured.clone()));

    run_steps_with_context_result_with_io(&root, &root, &steps, io_cfg)
        .expect("run WITH_IO pipe script");

    let contents = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert_eq!(contents, "streamed\n");
}

fn run_script(root: &GuardedPath, script: &str) -> Result<(), anyhow::Error> {
    let steps = oxdock_parser::parse_script(script).expect("parse script");
    run_steps_with_context_result_with_io(root, root, &steps, ExecIo::new()).map(|_| ())
}

#[test]
fn assert_file_accepts_matching_content() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    run_script(
        &root,
        "WRITE out.txt payload\nASSERT_FILE out.txt payload\n",
    )
    .expect("matching content passes");
}

#[test]
fn assert_file_rejects_content_mismatch() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_script(
        &root,
        "WRITE out.txt actual\nASSERT_FILE out.txt expected\n",
    )
    .expect_err("mismatch must fail");
    assert!(err.to_string().contains("content mismatch"), "{err}");
}

#[test]
fn assert_file_requires_existing_file() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_script(&root, "ASSERT_FILE missing.txt\n").expect_err("missing must fail");
    assert!(err.to_string().contains("missing.txt"), "{err}");
}

#[test]
fn assert_file_hash_mode_matches_and_rejects() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // sha256("stable-content")
    let digest = "08135c1b6349b0e4f894c36221952f0de00e6b4d82f80895abf359755e77103c";
    run_script(
        &root,
        &format!("WRITE payload.bin stable-content\nASSERT_FILE --hash {digest} payload.bin\n"),
    )
    .expect("hash match passes");

    let err = run_script(
        &root,
        "WRITE payload.bin stable-content\nASSERT_FILE --hash 1111111111111111111111111111111111111111111111111111111111111111 payload.bin\n",
    )
    .expect_err("hash mismatch must fail");
    assert!(err.to_string().contains("--hash mismatch"), "{err}");
}

#[test]
fn assert_dir_and_absent_cover_both_outcomes() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    run_script(
        &root,
        "MKDIR tree/deep\nASSERT_DIR tree/deep\nASSERT_ABSENT nope.txt\n",
    )
    .expect("positive assertions pass");

    let dir_err = run_script(&root, "WRITE file.txt x\nASSERT_DIR file.txt\n")
        .expect_err("file-as-dir must fail");
    assert!(
        dir_err.to_string().contains("is not a directory"),
        "{dir_err}"
    );

    let absent_err = run_script(&root, "WRITE file.txt x\nASSERT_ABSENT file.txt\n")
        .expect_err("present path must fail");
    assert!(absent_err.to_string().contains("exists"), "{absent_err}");
}

#[test]
fn assert_stdout_sees_interpreter_output_without_capture_sink() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    run_script(&root, "ECHO banner-line\nASSERT_STDOUT banner-line\n")
        .expect("interpreter output is recorded even with no configured sink");
}

#[test]
fn assert_stdout_sees_streamed_child_output() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    #[cfg(unix)]
    let script = "RUN echo child-echo-line\nASSERT_STDOUT child-echo-line\n";
    #[cfg(windows)]
    let script = "RUN cmd /c echo child-echo-line\nASSERT_STDOUT child-echo-line\n";
    run_script(&root, script).expect("child output is recorded");
}

#[test]
fn assert_stdout_miss_reports_emitted_log() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_script(&root, "ECHO present-line\nASSERT_STDOUT absent-line\n")
        .expect_err("miss must fail");
    assert!(
        err.to_string().contains("did not contain 'absent-line'")
            && err.to_string().contains("present-line"),
        "{err}"
    );
}
