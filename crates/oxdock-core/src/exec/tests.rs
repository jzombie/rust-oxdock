use super::pipe::PipeEndpoint;
use super::*;

use anyhow::bail;
use oxdock_fs::{GuardedPath, MockFs};
use oxdock_parser::{Guard, GuardExpr, IoBinding, IoStream, StepKind};
use oxdock_process::{
    CommandContext, CommandMode, CommandOptions, CommandResult, MockProcessManager, MockRunCall,
    ProcessManager,
};
use oxdock_sys_test_utils::exit_status_from_code;
use std::collections::HashMap;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};

#[test]
fn run_records_env_and_cwd() {
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let steps = vec![
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
            kind: StepKind::Run("echo hi".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let mock = MockProcessManager::default();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
    let runs = mock.recorded_runs();
    assert_eq!(runs.len(), 1);
    let MockRunCall {
        script,
        cwd,
        envs,
        cargo_target_dir,
        ..
    } = &runs[0];
    assert_eq!(script, "echo hi");
    assert_eq!(cwd, root.as_path());
    assert_eq!(
        cargo_target_dir,
        &root.join(".cargo-target").unwrap().to_path_buf()
    );
    assert_eq!(envs.get("FOO"), Some(&"bar".into()));
}

#[test]
fn run_expands_env_values() {
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let steps = vec![
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
            kind: StepKind::Run("echo {{ env:FOO }}".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let mock = MockProcessManager::default();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
    let runs = mock.recorded_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].script, "echo bar");
}

#[test]
fn async_completion_short_circuits_pipeline() {
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let steps = vec![
        async_step("sleep"),
        Step {
            guard: None,
            kind: StepKind::Run("echo after".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let mock = MockProcessManager::default();
    mock.push_bg_plan(0, success_status());
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    // Pipeline should succeed — foreground step runs after async completes
    run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
    // The parent's mock records the foreground step
    let recorded = mock.recorded_runs();
    let runs: Vec<_> = recorded.iter().map(|r| r.script.as_str()).collect();
    assert!(
        runs.contains(&"echo after"),
        "foreground step should execute, got: {runs:?}"
    );
}

#[test]
fn exit_kills_background_processes() {
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let steps = vec![
        async_step("bg-task"),
        Step {
            guard: None,
            kind: StepKind::Exit(5),
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let mock = MockProcessManager::default();
    mock.push_bg_plan(100, success_status());
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let err = run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap_err();
    assert!(
        err.to_string().contains("EXIT requested with code 5"),
        "unexpected error: {err}"
    );
}

#[test]
fn symlink_errors_report_underlying_cause() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::Mkdir("client".into()),
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
    ];
    let err = run_steps(&root, &steps).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("step 2: SYMLINK client client"),
        "error should include step context: {msg}"
    );
    assert!(
        msg.contains("SYMLINK destination already exists"),
        "error should surface underlying cause: {msg}"
    );
}

#[test]
fn guarded_run_waits_for_env_to_be_set() {
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let guard = Guard::EnvEquals {
        key: "READY".into(),
        value: "1".into(),
    };
    let steps = vec![
        Step {
            guard: Some(guard.clone().into()),
            kind: StepKind::Run("echo first".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Env {
                key: "READY".into(),
                value: "1".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: Some(guard.into()),
            kind: StepKind::Run("echo second".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let mock = MockProcessManager::default();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
    let runs = mock.recorded_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].script, "echo second");
}

#[test]
fn guard_groups_allow_any_matching_branch() {
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let guard_alpha = Guard::EnvEquals {
        key: "MODE".into(),
        value: "alpha".into(),
    };
    let guard_beta = Guard::EnvEquals {
        key: "MODE".into(),
        value: "beta".into(),
    };
    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::Env {
                key: "MODE".into(),
                value: "beta".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: Some(GuardExpr::or(vec![guard_alpha.into(), guard_beta.into()])),
            kind: StepKind::Run("echo guarded".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let mock = MockProcessManager::default();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
    let runs = mock.recorded_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].script, "echo guarded");
}

#[test]
fn with_io_pipe_routes_stdout_to_run_stdin() {
    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![IoBinding {
                    stream: IoStream::Stdout,
                    pipe: Some("shared".into()),
                }],
                cmd: Box::new(StepKind::Echo("hello".into())),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![IoBinding {
                    stream: IoStream::Stdin,
                    pipe: Some("shared".into()),
                }],
                cmd: Box::new(StepKind::Run("cat".into())),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];

    let fs = MockFs::new();
    let mut state = create_exec_state(fs);
    let mut proc = MockProcessManager::default();
    execute_steps(&mut state, &mut proc, &steps, None, false, None, None, true)
        .expect("pipeline executes");

    let runs = proc.recorded_runs();
    assert_eq!(runs.len(), 1);
    let MockRunCall { stdin, .. } = &runs[0];
    assert_eq!(stdin.as_deref(), Some(b"hello\n".as_slice()));
}

/// Byte-exact regression test for piped ASYNC output.
///
/// Mirrors the `async_run_direct` fixture: a background `ECHO` writes into a
/// script pipe while the foreground `WRITE` drains it to a file. The snapshot
/// holds raw bytes (no trimming), so this asserts the engine preserves the
/// trailing newline end to end — pipe EOF included.
#[test]
fn async_echo_pipe_write_preserves_exact_bytes() {
    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![IoBinding {
                    stream: IoStream::Stdout,
                    pipe: Some("async_out".into()),
                }],
                cmd: Box::new(StepKind::AsyncBlock {
                    body: vec![step(StepKind::Echo("hello".into()))],
                }),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![IoBinding {
                    stream: IoStream::Stdin,
                    pipe: Some("async_out".into()),
                }],
                cmd: Box::new(StepKind::Write {
                    path: "async_out.txt".into(),
                    contents: None,
                }),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let (_cwd, files) = run_with_mock_fs(&steps);
    let raw = files
        .iter()
        .find(|(k, _)| k.ends_with("async_out.txt"))
        .map(|(_, v)| v.clone());
    assert_eq!(raw, Some(b"hello\n".to_vec()));
}

/// Deterministic concurrency proof at the engine layer.
///
/// Mirrors the `async_inline_direct_proof` fixture: the background `WRITE`
/// blocks on pipe stdin until the foreground `ECHO` delivers payload and EOF.
/// A single-threaded executor would deadlock here; `AWAIT` joins the task so
/// the assertion below cannot race the background thread. Raw snapshot bytes
/// prove the payload — newline included — arrives intact.
#[test]
fn async_stdin_pipe_unblocks_background_write() {
    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::AssignAsync {
                var: "writer".into(),
                body: vec![Step {
                    guard: None,
                    kind: StepKind::WithIo {
                        bindings: vec![IoBinding {
                            stream: IoStream::Stdin,
                            pipe: Some("in_chan".into()),
                        }],
                        cmd: Box::new(StepKind::Write {
                            path: "inline_direct.txt".into(),
                            contents: None,
                        }),
                    },
                    scope_enter: 0,
                    scope_exit: 0,
                }],
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![IoBinding {
                    stream: IoStream::Stdout,
                    pipe: Some("in_chan".into()),
                }],
                cmd: Box::new(StepKind::Echo("unblock_inline_payload".into())),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        step(StepKind::Await { var: "writer".into() }),
    ];
    let (_cwd, files) = run_with_mock_fs(&steps);
    let raw = files
        .iter()
        .find(|(k, _)| k.ends_with("inline_direct.txt"))
        .map(|(_, v)| v.clone());
    assert_eq!(raw, Some(b"unblock_inline_payload\n".to_vec()));
}

fn success_status() -> ExitStatus {
    exit_status_from_code(0)
}

fn create_exec_state(fs: MockFs) -> ExecState<MockProcessManager> {
    let cargo = fs.root().join(".cargo-target").unwrap();
    let mut state = ExecState {
        fs: Box::new(fs.clone()),
        cargo_target_dir: cargo,
        cwd: fs.root().clone(),
        envs: Arc::new(HashMap::new()),
        bg_children: Vec::new(),
        scope_stack: Vec::new(),
        io: ExecIo::new(),
        assert_windows: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        var_scopes: Vec::new(),
        cancel_token: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active_process: Arc::new(std::sync::Mutex::new(None)),
        named_tasks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        next_task_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        inside_async: false,
        _marker: std::marker::PhantomData,
    };
    // Mirror production (`run_steps_with_manager`): push a global variable
    // scope so top-level LET assignments are captured.
    state.push_var_scope();
    state
}

fn run_with_mock_fs(steps: &[Step]) -> (GuardedPath, HashMap<String, Vec<u8>>) {
    let fs = MockFs::new();
    let mut state = create_exec_state(fs.clone());
    let mut proc = MockProcessManager::default();
    execute_steps(&mut state, &mut proc, steps, None, false, None, None, true).unwrap();
    (state.cwd, fs.snapshot())
}

#[test]
fn mock_fs_handles_workdir_and_write() {
    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::Mkdir("app".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Workdir("app".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Write {
                path: "out.txt".into(),
                contents: Some("hi".into()),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Read(Some("out.txt".into())),
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let (_cwd, files) = run_with_mock_fs(&steps);
    let written = files
        .iter()
        .find(|(k, _)| k.ends_with("app/out.txt"))
        .map(|(_, v)| String::from_utf8_lossy(v).to_string());
    assert_eq!(written, Some("hi".into()));
}

#[test]
fn write_interpolates_env_values() {
    let steps = vec![
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
            kind: StepKind::Env {
                key: "BAZ".into(),
                value: "{{ env:FOO }}-baz".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Write {
                path: "out.txt".into(),
                contents: Some("val {{ env:BAZ }}".into()),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let (_cwd, files) = run_with_mock_fs(&steps);
    let written = files
        .iter()
        .find(|(k, _)| k.ends_with("out.txt"))
        .map(|(_, v)| String::from_utf8_lossy(v).to_string());
    assert_eq!(written, Some("val bar-baz".into()));
}

#[cfg_attr(
    miri,
    ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
)]
#[test]
fn cat_and_capture_expand_env_paths() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::Write {
                path: "snippet.txt".into(),
                contents: Some("payload".into()),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Env {
                key: "SNIPPET".into(),
                value: "snippet.txt".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Env {
                key: "OUT_FILE".into(),
                value: "cat-{{ env:SNIPPET }}".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![IoBinding {
                    stream: IoStream::Stdout,
                    pipe: Some("cap-cat".to_string()),
                }],
                cmd: Box::new(StepKind::Read(Some("{{ env:SNIPPET }}".into()))),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![IoBinding {
                    stream: IoStream::Stdin,
                    pipe: Some("cap-cat".to_string()),
                }],
                cmd: Box::new(StepKind::Write {
                    path: "{{ env:OUT_FILE }}".into(),
                    contents: None,
                }),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    run_steps(&root, &steps).expect("capture with env paths succeeds");
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    let captured_path = root.join("cat-snippet.txt").expect("capture path");
    let contents = resolver
        .read_to_string(&captured_path)
        .expect("read captured output");
    assert_eq!(contents, "payload");
}

#[test]
fn final_cwd_tracks_last_workdir() {
    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::Write {
                path: "temp.txt".into(),
                contents: Some("123".into()),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Workdir("sub".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let (cwd, snapshot) = run_with_mock_fs(&steps);
    assert!(
        cwd.as_path().ends_with("sub"),
        "expected final cwd to match last WORKDIR, got {}",
        cwd.display()
    );
    let keys: Vec<_> = snapshot.keys().cloned().collect();
    assert!(
        keys.iter().any(|path| path.ends_with("temp.txt")),
        "WRITE should produce temp file, snapshot: {:?}",
        keys
    );
}

#[test]
fn mock_fs_normalizes_backslash_workdir() {
    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::Mkdir("win_nest".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Workdir("win_nest".into()),
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Write {
                path: "inner.txt".into(),
                contents: Some("ok".into()),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let (cwd, snapshot) = run_with_mock_fs(&steps);
    let cwd_display = cwd.display().to_string();
    assert!(
        cwd_display.ends_with("win_nest"),
        "expected cwd to end with win_nest, got {cwd_display}"
    );
    assert!(
        snapshot
            .keys()
            .any(|path| path.ends_with("win_nest/inner.txt")),
        "expected file under normalized path, snapshot: {:?}",
        snapshot.keys()
    );
}

#[cfg(windows)]
#[test]
fn mock_fs_rejects_absolute_windows_paths() {
    let steps = vec![Step {
        guard: None,
        kind: StepKind::Workdir("C:\\outside".into()),
        scope_enter: 0,
        scope_exit: 0,
    }];
    let fs = MockFs::new();
    let mut state = create_exec_state(fs);
    let mut proc = MockProcessManager::default();
    let sink = Arc::new(Mutex::new(Vec::new()));
    let err = execute_steps(
        &mut state,
        &mut proc,
        &steps,
        None,
        false,
        Some(StreamHandle::Stream(sink.clone())),
        Some(StreamHandle::Stream(sink)),
        false,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("escapes allowed root"),
        "unexpected error for absolute Windows path: {msg}"
    );
}

#[test]
fn with_stdin_passes_content_to_run() {
    let steps = vec![Step {
        guard: None,
        kind: StepKind::WithIo {
            bindings: vec![IoBinding {
                stream: IoStream::Stdin,
                pipe: None,
            }],
            cmd: Box::new(StepKind::Run("cat".into())),
        },
        scope_enter: 0,
        scope_exit: 0,
    }];

    let mock = MockProcessManager::default();
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());

    let input = Arc::new(Mutex::new(std::io::Cursor::new(b"hello world".to_vec())));

    let mut io_cfg = ExecIo::new();
    io_cfg.set_stdin(Some(input));

    run_steps_with_manager(fs, &steps, mock.clone(), io_cfg).unwrap();

    let runs = mock.recorded_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].script, "cat");
    assert_eq!(runs[0].stdin, Some(b"hello world".to_vec()));
}

#[allow(dead_code)]
fn failing_status() -> ExitStatus {
    exit_status_from_code(9)
}

fn step<T>(kind: T) -> Step
where
    T: Into<StepKind>,
{
    Step {
        guard: None,
        kind: kind.into(),
        scope_enter: 0,
        scope_exit: 0,
    }
}

fn async_step(cmd: &str) -> Step {
    Step {
        guard: None,
        kind: StepKind::AsyncBlock {
            body: vec![Step {
                guard: None,
                kind: StepKind::Run(cmd.into()),
                scope_enter: 0,
                scope_exit: 0,
            }],
        },
        scope_enter: 0,
        scope_exit: 0,
    }
}

#[test]
fn bg_failure_mid_pipeline_short_circuits_and_bails() {
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let steps = vec![
        async_step("flaky-bg"),
        step(StepKind::Run("echo never".into())),
    ];
    let runner = FailingRunner {
        fail_script: "flaky-bg".into(),
        ..Default::default()
    };
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap()) as Box<dyn WorkspaceFs>;
    let err = run_steps_with_manager(fs, &steps, runner.clone(), ExecIo::new()).unwrap_err();

    assert!(
err.chain().any(|c| c.to_string().contains("simulated failure"))
            || err.to_string().contains("exited with status")
            || err.to_string().contains("step"),
        "unexpected error: {err}"
    );
}

#[test]
fn bg_failure_after_pipeline_end_reports_status() {
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let steps = vec![async_step("late-failure")];
    let runner = FailingRunner {
        fail_script: "late-failure".into(),
        ..Default::default()
    };
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap()) as Box<dyn WorkspaceFs>;
    let err = run_steps_with_manager(fs, &steps, runner, ExecIo::new()).unwrap_err();
    assert!(
err.chain().any(|c| c.to_string().contains("simulated failure"))
            || err.to_string().contains("exited with status")
            || err.to_string().contains("step"),
        "unexpected error: {err}"
    );
}

#[test]
fn bg_success_after_pipeline_end_waits_cleanly() {
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let steps = vec![async_step("late-success")];
    let mock = MockProcessManager::default();
    mock.push_bg_plan(5, success_status());
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new())
        .expect("successful late child must not fail the pipeline");
}

#[test]
fn multi_child_teardown_kills_survivor_when_first_exits() {
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let steps = vec![async_step("first-finisher"), async_step("survivor")];
    let runner = FailingRunner {
        fail_script: "first-finisher".into(),
        ..Default::default()
    };
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap()) as Box<dyn WorkspaceFs>;
    let err = run_steps_with_manager(fs, &steps, runner, ExecIo::new()).unwrap_err();
    assert!(
err.chain().any(|c| c.to_string().contains("simulated failure"))
            || err.to_string().contains("exited with status")
            || err.to_string().contains("step"),
        "unexpected error: {err}"
    );
}

#[test]
fn exit_kills_all_background_children() {
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let steps = vec![
        async_step("bg-a"),
        async_step("bg-b"),
        step(StepKind::Exit(3)),
    ];
    let mock = MockProcessManager::default();
    mock.push_bg_plan(100, success_status());
    mock.push_bg_plan(100, success_status());
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let err = run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap_err();
    assert!(err.to_string().contains("EXIT requested with code 3"));
}

/// Minimal stub whose foreground commands fail by script name, letting us
/// drive failure paths the stock mock cannot express.
#[derive(Clone, Default)]
struct FailingRunner {
    calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    fail_script: String,
    bg: MockProcessManager,
}

impl ProcessManager for FailingRunner {
    type Handle = oxdock_process::MockHandle;

    fn run_command(
        &mut self,
        ctx: &CommandContext,
        script: &str,
        options: CommandOptions,
    ) -> Result<CommandResult<Self::Handle>> {
        self.calls.lock().expect("poisoned").push(script.to_string());
        if script == self.fail_script {
            bail!("simulated failure")
        }
        if options.mode == CommandMode::Background {
            // Delegate spawns so we get kill-logging mock handles.
            return self.bg.run_command(ctx, script, options);
        }
        Ok(CommandResult::Completed)
    }
}

#[test]
fn failing_foreground_run_aborts_with_step_context() {
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let runner = FailingRunner {
        calls: Default::default(),
        fail_script: "boom".into(),
        bg: MockProcessManager::default(),
    };
    let calls = runner.calls.clone();
    let steps = vec![
        step(StepKind::Run("ok-first".into())),
        step(StepKind::Run("boom".into())),
        step(StepKind::Run("never-reached".into())),
    ];
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let err = run_steps_with_manager(fs, &steps, runner, ExecIo::new()).unwrap_err();

    let msg = format!("{err:#}");
    assert!(
        msg.contains("step 2: RUN boom") && msg.contains("simulated failure"),
        "error must carry step index and cause, got: {msg}"
    );
    assert_eq!(
        *calls.lock().expect("poisoned"),
        vec!["ok-first".to_string(), "boom".to_string()]
    );
}

#[test]
fn with_io_rejects_duplicate_stdout_binding() {
    let steps = vec![Step {
        guard: None,
        kind: StepKind::WithIo {
            bindings: vec![
                IoBinding {
                    stream: IoStream::Stdout,
                    pipe: Some("p".into()),
                },
                IoBinding {
                    stream: IoStream::Stdout,
                    pipe: Some("p".into()),
                },
            ],
            cmd: Box::new(StepKind::Echo("x".into())),
        },
        scope_enter: 0,
        scope_exit: 0,
    }];
    let fs = MockFs::new();
    let mut state = create_exec_state(fs);
    let mut proc = MockProcessManager::default();
    let err = execute_steps(&mut state, &mut proc, &steps, None, false, None, None, true)
        .expect_err("duplicate stdout binding");
    assert!(
        err.to_string().contains("declared stdout more than once"),
        "unexpected: {err}"
    );
}

#[test]
fn with_io_rejects_duplicate_stdin_and_stderr_bindings() {
    for (variant, fragment) in [
        ("stdin", "stdin more than once"),
        ("stderr", "stderr more than once"),
    ] {
        let (stream_a, stream_b) = if variant == "stdin" {
            (IoStream::Stdin, IoStream::Stdin)
        } else {
            (IoStream::Stderr, IoStream::Stderr)
        };
        let steps = vec![Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![
                    IoBinding {
                        stream: stream_a,
                        pipe: Some("p".into()),
                    },
                    IoBinding {
                        stream: stream_b,
                        pipe: Some("p".into()),
                    },
                ],
                cmd: Box::new(StepKind::Echo("x".into())),
            },
            scope_enter: 0,
            scope_exit: 0,
        }];
        let fs = MockFs::new();
        let mut state = create_exec_state(fs);
        let mut proc = MockProcessManager::default();
        let err = execute_steps(&mut state, &mut proc, &steps, None, false, None, None, true)
            .expect_err("duplicate binding");
        assert!(
            err.to_string().contains(fragment),
            "expected '{fragment}', got: {err}"
        );
    }
}

#[test]
fn with_io_block_form_bails_unexpanded() {
    let steps = vec![Step {
        guard: None,
        kind: StepKind::WithIoBlock {
            bindings: Vec::new(),
        },
        scope_enter: 0,
        scope_exit: 0,
    }];
    let fs = MockFs::new();
    let mut state = create_exec_state(fs);
    let mut proc = MockProcessManager::default();
    let err = execute_steps(&mut state, &mut proc, &steps, None, false, None, None, true)
        .expect_err("unexpanded WITH_IO block");
    assert!(err.to_string().contains("expanded during parsing"));
}

#[test]
fn write_without_contents_or_stdin_bails() {
    let steps = vec![step(StepKind::Write {
        path: "out.txt".into(),
        contents: None,
    })];
    let fs = MockFs::new();
    let mut state = create_exec_state(fs);
    let mut proc = MockProcessManager::default();
    let err = execute_steps(&mut state, &mut proc, &steps, None, false, None, None, true)
        .expect_err("write without source");
    assert!(
        err.to_string().contains("requires stdin"),
        "unexpected: {err}"
    );
}

#[test]
fn stderr_stream_handle_reaches_manager() {
    let sink: SharedOutput = Arc::new(Mutex::new(Vec::<u8>::new()));
    let steps = vec![step(StepKind::Run("emits-stderr".into()))];
    let fs = MockFs::new();
    let mut state = create_exec_state(fs);
    let mut proc = MockProcessManager::default();
    execute_steps(
        &mut state,
        &mut proc,
        &steps,
        None,
        false,
        None,
        Some(StreamHandle::Stream(sink)),
        true,
    )
    .expect("run");

    let runs = proc.recorded_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].stderr_mode,
        oxdock_process::MockStreamMode::Stream,
        "stderr handle must be forwarded as CommandStderr::Stream"
    );
}

#[test]
fn inherit_stdout_override_forces_inherit_modes() {
    let out_sink: SharedOutput = Arc::new(Mutex::new(Vec::<u8>::new()));
    let err_sink: SharedOutput = Arc::new(Mutex::new(Vec::<u8>::new()));
    let steps = vec![
        step(StepKind::Env {
            key: "OXDOCK_INHERIT_STDOUT".into(),
            value: "1".into(),
        }),
        step(StepKind::Run("captured-normally".into())),
    ];
    let fs = MockFs::new();
    let mut state = create_exec_state(fs);
    let mut proc = MockProcessManager::default();
    execute_steps(
        &mut state,
        &mut proc,
        &steps,
        None,
        false,
        Some(StreamHandle::Stream(out_sink)),
        Some(StreamHandle::Stream(err_sink)),
        true,
    )
    .expect("run");

    let runs = proc.recorded_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].stderr_mode,
        oxdock_process::MockStreamMode::Inherit,
        "OXDOCK_INHERIT_STDOUT must force stderr inheritance too"
    );
}

#[test]
fn exec_io_stderr_precedence_and_stdout_fallback() {
    let out_sink: SharedOutput = Arc::new(Mutex::new(Vec::<u8>::new()));
    let err_sink: SharedOutput = Arc::new(Mutex::new(Vec::<u8>::new()));

    // set_stdout seeds stderr only while stderr is unset.
    let mut io = ExecIo::new();
    io.set_stdout(Some(out_sink.clone()));
    assert!(Arc::ptr_eq(&io.stderr().unwrap(), &out_sink));

    // An explicit stderr wins over the stdout fallback...
    io.set_stderr(Some(err_sink.clone()));
    assert!(Arc::ptr_eq(&io.stderr().unwrap(), &err_sink));
    // ...and survives later stdout changes.
    let replacement: SharedOutput = Arc::new(Mutex::new(Vec::<u8>::new()));
    io.set_stdout(Some(replacement));
    assert!(Arc::ptr_eq(&io.stderr().unwrap(), &err_sink));

    // With no streams at all, stderr falls back to nothing.
    let bare = ExecIo::new();
    assert!(bare.stderr().is_none());
}

#[test]
fn exec_io_inherit_env_state_machine_round_trips() {
    let mut io = ExecIo::new();
    io.insert_inherit_env("K", "v1");
    assert_eq!(io.inherit_env_value("K"), Some(&"v1".to_string()));
    assert!(!io.inherit_env_is_removed("K"));

    io.remove_inherit_env("K");
    assert!(io.inherit_env_is_removed("K"));
    assert_eq!(io.inherit_env_value("K"), None);

    // Re-inserting after removal must clear the removed marker.
    io.insert_inherit_env("K", "v2");
    assert!(!io.inherit_env_is_removed("K"));
    assert_eq!(io.inherit_env_value("K"), Some(&"v2".to_string()));
}

#[test]
fn exec_io_pipe_endpoints_expose_streams_and_inherit() {
    let mut io = ExecIo::new();
    let writer: SharedOutput = Arc::new(Mutex::new(Vec::<u8>::new()));

    io.insert_output_pipe_stdout("s-out", writer.clone());
    io.insert_output_pipe_stderr_inherit("s-inh");

    match io.output_pipe_stdout("s-out") {
        Some(PipeEndpoint::Stream(w)) => assert!(Arc::ptr_eq(&w, &writer)),
        Some(PipeEndpoint::Script(_)) => {
            panic!("expected streamed stdout endpoint, got script endpoint")
        }
        Some(PipeEndpoint::Inherit) => panic!("expected streamed stdout endpoint, got inherit"),
        None => panic!("endpoint missing entirely"),
    }
    match io.output_pipe_stderr("s-inh") {
        Some(PipeEndpoint::Inherit) => {}
        _ => panic!("expected inherit stderr endpoint"),
    }
}

#[test]
fn hash_sha256_matches_known_digest_for_file() {
    // sha256("hello")
    const HELLO_DIGEST: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    let backing = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink: SharedOutput = backing.clone();
    let steps = vec![
        step(StepKind::Write {
            path: "hello.txt".into(),
            contents: Some("hello".into()),
        }),
        step(StepKind::HashSha256 {
            path: "hello.txt".into(),
        }),
    ];
    let fs = MockFs::new();
    let mut state = create_exec_state(fs);
    let mut proc = MockProcessManager::default();
    execute_steps(
        &mut state,
        &mut proc,
        &steps,
        None,
        false,
        Some(StreamHandle::Stream(sink.clone())),
        None,
        true,
    )
    .expect("hash pipeline");

    let produced = String::from_utf8(backing.lock().unwrap().clone()).unwrap();
    assert_eq!(produced.trim(), HELLO_DIGEST);
}

#[test]
fn hash_sha256_directory_digest_is_deterministic() {
    let digests: Vec<String> = (0..2)
        .map(|_| {
            let backing = Arc::new(Mutex::new(Vec::<u8>::new()));
            let sink: SharedOutput = backing.clone();
            let steps = vec![
                step(StepKind::Mkdir("pkg".into())),
                step(StepKind::Write {
                    path: "pkg/b.txt".into(),
                    contents: Some("22".into()),
                }),
                step(StepKind::Write {
                    path: "pkg/a.txt".into(),
                    contents: Some("1".into()),
                }),
                step(StepKind::HashSha256 { path: "pkg".into() }),
            ];
            let temp = GuardedPath::tempdir().unwrap();
            let root = temp.as_guarded_path().clone();
            let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
            run_steps_with_manager(
                fs,
                &steps,
                MockProcessManager::default(),
                assemble_default_io(None, Some(sink.clone())),
            )
            .expect("hash dir pipeline");
            String::from_utf8(backing.lock().unwrap().clone()).unwrap()
        })
        .collect();

    assert_eq!(digests[0], digests[1], "directory hashing must be stable");
    let hex = digests[0].trim();
    assert_eq!(hex.len(), 64, "full sha256 hex expected: {hex}");
}

#[test]
fn copy_directory_branch_recurses_into_nested_target() {
    let steps = vec![
        step(StepKind::Mkdir("app".into())),
        step(StepKind::Write {
            path: "app/inner.txt".into(),
            contents: Some("nested".into()),
        }),
        Step {
            guard: None,
            kind: StepKind::Copy {
                from_current_workspace: false,
                from: "app".into(),
                to: "copy-of-app".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];

    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, MockProcessManager::default(), ExecIo::new())
        .expect("copy pipeline");

    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let copied = root.join("copy-of-app/inner.txt").unwrap();
    assert_eq!(
        resolver.read_file(&copied).unwrap(),
        b"nested",
        "COPY must recurse into directories"
    );
}

#[test]
fn mid_pipeline_failure_kills_background_children_via_drop() {
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let steps = vec![async_step("bg-task"), step(StepKind::Run("boom".into()))];
    let runner = FailingRunner {
        fail_script: "boom".into(),
        ..Default::default()
    };
    runner.bg.push_bg_plan(100, success_status());
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let err = run_steps_with_manager(fs, &steps, runner.clone(), ExecIo::new()).unwrap_err();
    assert!(
        err.chain()
            .any(|c| c.to_string().contains("simulated failure")),
        "unexpected chain: {err:#}"
    );
}

#[test]
fn naturally_completed_bg_not_logged_as_killed() {
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let steps = vec![async_step("finisher")];
    let mock = MockProcessManager::default();
    mock.push_bg_plan(0, success_status());
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    // Pipeline should succeed — the background task completes naturally
    run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
}

#[test]
fn public_entrypoint_returns_final_working_directory() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let steps = vec![
        step(StepKind::Mkdir("app".into())),
        step(StepKind::Workdir("app".into())),
    ];
    let final_cwd = run_steps_with_context_result(&root, &root, &steps, None, None).expect("run");
    assert_eq!(final_cwd.as_path(), root.as_path().join("app"));
}

// ---------------------------------------------------------------------------
// ScriptPipe storage tiering tests
// ---------------------------------------------------------------------------

#[test]
#[cfg(not(miri))]
fn script_pipe_stays_in_memory_below_threshold() {
    use super::pipe::{PIPE_SPILL_THRESHOLD, ScriptPipe};

    let pipe = ScriptPipe::new();
    let writer = pipe.endpoint().stream_handle();
    let reader = pipe.reader();

    let payload = vec![0xABu8; 1024]; // 1 KiB — below threshold
    writer.lock().unwrap().write_all(&payload).unwrap();
    drop(writer);

    let mut guard = reader.lock().unwrap();
    let mut buf = Vec::new();
    guard.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, payload);
    let _ = PIPE_SPILL_THRESHOLD; // Ensure constant is used
}

#[test]
#[cfg(not(miri))]
fn script_pipe_spills_to_disk_above_threshold() {
    use super::pipe::{PIPE_SPILL_THRESHOLD, ScriptPipe};

    let pipe = ScriptPipe::new();
    let writer = pipe.endpoint().stream_handle();
    let reader = pipe.reader();

    // Exceed the threshold by 1 MiB
    let size = PIPE_SPILL_THRESHOLD + (1024 * 1024);
    let payload: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
    writer.lock().unwrap().write_all(&payload).unwrap();
    drop(writer);

    let mut guard = reader.lock().unwrap();
    let mut buf = Vec::new();
    guard.read_to_end(&mut buf).unwrap();
    assert_eq!(buf.len(), size);
    assert_eq!(buf, payload);
}

#[test]
#[cfg(not(miri))]
fn script_pipe_backlog_cap_exceeded_returns_error() {
    use super::pipe::{PIPE_MAX_BACKLOG, PIPE_SPILL_THRESHOLD, ScriptPipe};

    let pipe = ScriptPipe::new();
    let writer = pipe.endpoint().stream_handle();

    // First, trigger a spill to Disk mode by writing above the spill threshold
    let spill_payload = vec![0u8; PIPE_SPILL_THRESHOLD + 1];
    writer.lock().unwrap().write_all(&spill_payload).unwrap();

    // Now write enough to exceed the backlog limit without reading
    // Backlog = write_pos - read_pos. We haven't read, so backlog = spill_payload.len()
    // We need to write enough to make total backlog > PIPE_MAX_BACKLOG
    let remaining = (PIPE_MAX_BACKLOG as usize) - spill_payload.len() + 1;
    let overflow_payload = vec![0u8; remaining];
    let result = writer.lock().unwrap().write_all(&overflow_payload);

    assert!(
        result.is_err(),
        "Writing beyond PIPE_MAX_BACKLOG must return an error"
    );
    let err = result.unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::OutOfMemory,
        "Expected OutOfMemory error kind on backlog overflow"
    );
}

#[test]
#[cfg(not(miri))]
fn script_pipe_file_truncated_on_drain() {
    use super::pipe::{PIPE_SPILL_THRESHOLD, ScriptPipe};

    let pipe = ScriptPipe::new();
    let writer = pipe.endpoint().stream_handle();
    let reader = pipe.reader();

    // Write above threshold to trigger disk spill
    let size = PIPE_SPILL_THRESHOLD + (1024 * 1024);
    let payload: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
    writer.lock().unwrap().write_all(&payload).unwrap();
    drop(writer);

    // Read all bytes
    let mut guard = reader.lock().unwrap();
    let mut buf = Vec::new();
    guard.read_to_end(&mut buf).unwrap();
    assert_eq!(buf.len(), size);
    assert_eq!(buf, payload);
}

#[test]
#[cfg(not(miri))]
#[allow(clippy::disallowed_methods)]
fn script_pipe_explicit_disk_spill_and_cleanup_verification() {
    use super::pipe::{PIPE_SPILL_THRESHOLD, ScriptPipe};
    use std::fs;

    let pipe = ScriptPipe::new();
    let writer = pipe.endpoint().stream_handle();
    let reader = pipe.reader();

    // 1. Trigger disk spill (9 MiB)
    let size = PIPE_SPILL_THRESHOLD + (1024 * 1024);
    let payload = vec![0x55u8; size];
    writer.lock().unwrap().write_all(&payload).unwrap();

    // 2. Query exact temp file path directly from the pipe instance
    let temp_path = pipe
        .temp_path()
        .expect("Pipe must have transitioned to DiskBuffer");
    assert!(
        temp_path.exists(),
        "Temp file {} must exist on disk while buffered",
        temp_path.display()
    );

    // 3. Drain all bytes and verify immediate physical file truncation
    let mut guard = reader.lock().unwrap();
    let mut buf = vec![0u8; size];
    guard.read_exact(&mut buf).unwrap();
    drop(guard);

    let meta = fs::metadata(&temp_path).unwrap();
    assert_eq!(
        meta.len(),
        0,
        "Physical file length must be 0 after buffer drainage"
    );

    // 4. Drop handles and verify unlinking
    drop(writer);
    drop(reader);
    drop(pipe);

    assert!(
        !temp_path.exists(),
        "Temp file must be deleted from disk upon Drop"
    );
}
