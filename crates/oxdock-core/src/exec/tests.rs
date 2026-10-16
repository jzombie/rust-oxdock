use super::*;

use anyhow::bail;
use indoc::indoc;
use oxdock_fs::{EntryKind, GuardedPath, MockFs, WorkspaceFs, to_forward_slashes};
use oxdock_parser::{Expr, Guard, GuardExpr, IoBinding, IoStream, StepKind};
use oxdock_process::{
    BackgroundHandle, CommandContext, CommandMode, CommandOptions, CommandResult, CommandStdin,
    MockProcessManager, MockRunCall, ProcessManager,
};
use oxdock_sys_test_utils::{TestEnvGuard, can_create_symlinks, exit_status_from_code};
use std::collections::HashMap;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Run steps expecting failure: discards the success payload (which now
/// carries the filesystem handle back) so error assertions stay ergonomic.
fn run_expect_err<P: ProcessManager>(
    fs: Box<dyn WorkspaceFs>,
    steps: &[Step],
    process: P,
) -> anyhow::Error {
    run_steps_with_manager(fs, steps, process, ExecIo::new())
        .map(|_| ())
        .unwrap_err()
}

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
    assert_ne!(
        cargo_target_dir,
        &root.join(".cargo-target").unwrap().to_path_buf(),
        "cargo outputs must stay out of the workspace tree (isolated scratch)"
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
fn run_shell_routes_dollar_forms_to_dsl_or_shell() {
    use oxdock_parser::{Expr, Value};

    // `$var` maps to the DSL variable; `\$var` is routed to the shell
    // untouched (backslash consumed, no DSL expansion); `{{ $var }}` and
    // `{{ env:K }}` interpolate; `\{{ $var }}` stays literal braces.
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let scripts = [
        "RUN echo $who",
        "RUN echo \\$who",
        "RUN echo \"{{ $who }}\"",
        "RUN echo \"\\{{ $who }}\"",
        "RUN echo \"{{ env:FOO }}\"",
        // Embedded in larger text, an undefined `$var` passes through for
        // the shell (a lone `$undefined` instead bails as a likely typo).
        "RUN echo hi-$undefined_var_xyz",
    ];
    let mut steps = vec![
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
            kind: StepKind::Assign {
                var: "who".into(),
                decl_type: "STRING".to_string(),
                expr: Expr::Literal(Value::string("world".to_string())),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    for script in scripts {
        let parsed = crate::parse_script(script).unwrap();
        steps.extend(parsed);
    }
    let mock = MockProcessManager::default();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
    let runs = mock.recorded_runs();
    let scripts: Vec<_> = runs.iter().map(|r| r.script.as_str()).collect();
    assert_eq!(
        scripts,
        vec![
            "echo world",
            "echo $who",
            "echo world",
            "echo {{ $who }}",
            "echo bar",
            "echo hi-$undefined_var_xyz",
        ]
    );
}

#[test]
fn run_exec_resolves_and_flattens_argv() {
    use oxdock_parser::{Arg, Expr, Value};

    let root = GuardedPath::new_root_from_str(".").unwrap();
    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::Env {
                key: "GREETING".into(),
                value: "hi".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Assign {
                var: "args".into(),
                decl_type: "LIST<STRING>".to_string(),
                expr: Expr::List(vec![
                    Expr::Literal(Value::string("-v".to_string())),
                    Expr::Literal(Value::string("--all".to_string())),
                ]),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::RunExec {
                argv: vec![
                    Arg::Expr(Expr::Literal(Value::string("cargo".to_string()))),
                    Arg::Expr(Expr::Var("args".to_string())),
                    Arg::Expr(Expr::Literal(Value::int(3))),
                    Arg::Expr(Expr::Literal(Value::bool(true))),
                    Arg::String("{{ env:GREETING }}".to_string(), false),
                    // Escapes stay literal and pass through directly.
                    Arg::String("\\$literal".to_string(), false),
                    Arg::String("\\{{ env:GREETING }}".to_string(), false),
                ],
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let mock = MockProcessManager::default();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
    let runs = mock.recorded_argv_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].argv,
        vec![
            "cargo",
            "-v",
            "--all",
            "3",
            "true",
            "hi",
            "\\$literal",
            "{{ env:GREETING }}"
        ]
    );
    // Shell dispatch must not have been used.
    assert!(mock.recorded_runs().is_empty());
}

#[test]
fn run_exec_rejects_map_elements_with_type_error() {
    use oxdock_parser::{Arg, Expr, Value};

    let root = GuardedPath::new_root_from_str(".").unwrap();
    let mut map = std::collections::BTreeMap::new();
    map.insert("k".to_string(), Value::string("v".to_string()));
    let steps = vec![Step {
        guard: None,
        kind: StepKind::RunExec {
            argv: vec![Arg::Expr(Expr::Literal(Value::map(map)))],
        },
        scope_enter: 0,
        scope_exit: 0,
    }];
    let mock = MockProcessManager::default();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let err = run_expect_err(fs, &steps, mock);
    assert!(
        format!("{err:#}").contains("must be a string"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn run_exec_resolves_every_variable_type() {
    use oxdock_parser::{Arg, Expr, Value};

    // Every localized variable type extrapolates through exec-form argv:
    // `$var`, `$map.key`, `{{ env:K }}`, `{{ $var }}`, `{{ $map.key }}`.
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let mut map = std::collections::BTreeMap::new();
    map.insert("k".to_string(), Value::string("keyval".to_string()));
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
            kind: StepKind::Assign {
                var: "who".into(),
                decl_type: "STRING".to_string(),
                expr: Expr::Literal(Value::string("world".to_string())),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Assign {
                var: "m".into(),
                decl_type: "MAP<k: STRING>".to_string(),
                expr: Expr::Map(vec![(
                    "k".to_string(),
                    Expr::Literal(Value::string("keyval".to_string())),
                )]),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::RunExec {
                argv: vec![
                    Arg::Expr(Expr::Literal(Value::string("echo".to_string()))),
                    // Whole-element `$var` and `$map.key` references.
                    Arg::Expr(Expr::Var("who".to_string())),
                    Arg::Expr(Expr::KeyPath {
                        base: "m".to_string(),
                        keys: vec!["k".to_string()],
                    }),
                    // Template placeholders in string elements.
                    Arg::String("{{ env:FOO }}".to_string(), false),
                    Arg::String("{{ $who }}".to_string(), false),
                    Arg::String("{{ $m.k }}".to_string(), false),
                    Arg::Expr(Expr::Literal(Value::string("{{ $who }}".to_string()))),
                ],
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let mock = MockProcessManager::default();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
    let runs = mock.recorded_argv_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].argv,
        vec!["echo", "world", "keyval", "bar", "world", "keyval", "world"]
    );
}

#[test]
fn run_exec_expands_templates_in_literal_elements_once() {
    use oxdock_parser::{Arg, Expr, Value};

    let root = GuardedPath::new_root_from_str(".").unwrap();
    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::Env {
                key: "GREETING".into(),
                value: "hi".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::RunExec {
                argv: vec![
                    Arg::Expr(Expr::Literal(Value::string("echo".to_string()))),
                    // Quoted `{{ ... }}` templates interpolate...
                    Arg::Expr(Expr::Literal(Value::string(
                        "{{ env:GREETING }}".to_string(),
                    ))),
                    // ...while `\{{ ... }}` escapes stay literal (single pass).
                    Arg::Expr(Expr::Literal(Value::string(
                        "\\{{ env:GREETING }}".to_string(),
                    ))),
                ],
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let mock = MockProcessManager::default();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
    let runs = mock.recorded_argv_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].argv, vec!["echo", "hi", "{{ env:GREETING }}"]);
}

#[test]
fn run_exec_processes_escapes_and_keeps_metachars_literal() {
    use oxdock_parser::{Arg, Expr, Value};

    // Backslash escapes resolve exactly once (`\"` -> `"`, `\\` -> `\`,
    // `\n` -> newline); shell metacharacters (`; $() `` > |`) are never
    // interpreted and reach argv verbatim : there is no shell to escape for.
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let steps = vec![Step {
        guard: None,
        kind: StepKind::RunExec {
            argv: vec![
                Arg::Expr(Expr::Literal(Value::string("echo".to_string()))),
                Arg::Expr(Expr::Literal(Value::string("a\\\"b\\\\c\\nd".to_string()))),
                Arg::Expr(Expr::Literal(Value::string(
                    "a; b $(c) `d` > e | f".to_string(),
                ))),
            ],
        },
        scope_enter: 0,
        scope_exit: 0,
    }];
    let mock = MockProcessManager::default();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
    let runs = mock.recorded_argv_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].argv,
        vec!["echo", "a\"b\\c\nd", "a; b $(c) `d` > e | f"]
    );
}

#[test]
fn run_exec_treats_variable_values_as_opaque() {
    use oxdock_parser::{Arg, Expr, Value};

    // A variable holding literal `{{ ... }}` text must pass through
    // verbatim: expansion applies to script-literal source text only,
    // never to evaluated runtime values (no second-order expansion).
    let root = GuardedPath::new_root_from_str(".").unwrap();
    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::Env {
                key: "SECRET".into(),
                value: "leaked".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Assign {
                var: "data".into(),
                decl_type: "STRING".to_string(),
                expr: Expr::Literal(Value::string("\\{{ env:SECRET }}".to_string())),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::RunExec {
                argv: vec![
                    Arg::Expr(Expr::Literal(Value::string("echo".to_string()))),
                    Arg::Expr(Expr::Var("data".to_string())),
                ],
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let mock = MockProcessManager::default();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
    let runs = mock.recorded_argv_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].argv, vec!["echo", "{{ env:SECRET }}"]);
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
    // Pipeline should succeed : foreground step runs after async completes
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
            kind: StepKind::Exit(oxdock_parser::Arg::String("5".to_string(), false)),
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let mock = MockProcessManager::default();
    mock.push_bg_plan(100, success_status());
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let err = run_expect_err(fs, &steps, mock.clone());
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
            kind: StepKind::Write {
                path: "taken.txt".into(),
                contents: Some("taken".into()),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        Step {
            guard: None,
            kind: StepKind::Symlink {
                from_workspace: None,
                from: "taken.txt".into(),
                to: "taken.txt".into(),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let err = run_steps(&root, &steps).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("step 2: SYMLINK taken.txt taken.txt"),
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
    let guard = Guard::Attr {
        ns: oxdock_parser::Ns::Env,
        key: Some("READY".into()),
        val: Some("1".into()),
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
    let guard_alpha = Guard::Attr {
        ns: oxdock_parser::Ns::Env,
        key: Some("MODE".into()),
        val: Some("alpha".into()),
    };
    let guard_beta = Guard::Attr {
        ns: oxdock_parser::Ns::Env,
        key: Some("MODE".into()),
        val: Some("beta".into()),
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
        step(StepKind::Assign {
            var: "shared".into(),
            decl_type: "PIPE".to_string(),
            expr: Expr::FreshPipe,
        }),
        Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![IoBinding {
                    stream: IoStream::Stdout,
                    pipe: Some(oxdock_parser::PipeTarget::Var("shared".into())),
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
                    pipe: Some(oxdock_parser::PipeTarget::Var("shared".into())),
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
    execute_steps(
        &mut state,
        &mut proc,
        &steps,
        CommandStdin::Null,
        false,
        None,
        None,
        None,
        None,
        true,
    )
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
/// trailing newline end to end : pipe EOF included.
#[test]
fn async_echo_pipe_write_preserves_exact_bytes() {
    let steps = vec![
        step(StepKind::Assign {
            var: "async_out".into(),
            decl_type: "PIPE".to_string(),
            expr: Expr::FreshPipe,
        }),
        Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![IoBinding {
                    stream: IoStream::Stdout,
                    pipe: Some(oxdock_parser::PipeTarget::Var("async_out".into())),
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
                    pipe: Some(oxdock_parser::PipeTarget::Var("async_out".into())),
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
/// prove the payload : newline included : arrives intact.
#[test]
fn async_stdin_pipe_unblocks_background_write() {
    let steps = vec![
        step(StepKind::Assign {
            var: "in_chan".into(),
            decl_type: "PIPE".to_string(),
            expr: Expr::FreshPipe,
        }),
        Step {
            guard: None,
            kind: StepKind::AssignAsync {
                var: "writer".into(),
                decl_type: "HANDLE".to_string(),
                body: vec![Step {
                    guard: None,
                    kind: StepKind::WithIo {
                        bindings: vec![IoBinding {
                            stream: IoStream::Stdin,
                            pipe: Some(oxdock_parser::PipeTarget::Var("in_chan".into())),
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
                    pipe: Some(oxdock_parser::PipeTarget::Var("in_chan".into())),
                }],
                cmd: Box::new(StepKind::Echo("unblock_inline_payload".into())),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
        step(StepKind::Await {
            var: "writer".into(),
        }),
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

/// Choke-point boundary (issue #131): AST handler modules must never
/// materialize, select, or fabricate execution roots directly. All snapshot
/// demand flows structurally through `PathResolver::resolve_read` /
/// `resolve_write` / `resolve_workdir` (plus `command_ctx()`, which resolves
/// its workdir through them). Any new handler that needs a concrete path
/// inherits laziness by resolving. There is nothing to register.
///
/// Enforced by scanning the handler sources at compile time via
/// `include_str!` (no filesystem I/O, so no abstraction-lint concerns):
/// - `.ensure()` / `.materialize()`: direct lazy-holder creation. (Unrelated
///   `ensure_parent_dir` / `ensure_pipe_for` do not match `.ensure()`.)
/// - `snapshot_handle` / `LazyGuardedTempDir`: direct handle plumbing.
/// - `GuardedPath::tempdir` / `tempdir_with`: eager tempdir creation.
///   (`capture.rs` is exempt: its >8MiB pipe-spill tempdir is pre-existing
///   capture behavior, not an execution root.)
/// - `set_root(`: legacy direct root flipping in handler/arg code. Root
///   selection goes through `switch_to_snapshot` / `switch_to_local`
///   (`state.rs` scope restore keeps using `set_root` and is not scanned).
/// - `anchor_path`: the never-created virtual anchor must never be named
///   outside `workspace_fs`.
#[test]
fn ast_handlers_route_snapshot_demand_through_resolve_choke_points() {
    const HANDLER_SOURCES: &[(&str, &str)] = &[
        ("handlers.rs", include_str!("handlers.rs")),
        ("args.rs", include_str!("args.rs")),
        ("steps.rs", include_str!("steps.rs")),
        ("state.rs", include_str!("state.rs")),
        ("fs_ops.rs", include_str!("fs_ops.rs")),
        ("io.rs", include_str!("io.rs")),
    ];
    const FORBIDDEN: &[&str] = &[
        ".ensure()",
        ".materialize()",
        "snapshot_handle",
        "LazyGuardedTempDir",
        "GuardedPath::tempdir",
        "tempdir_with",
        "anchor_path",
    ];
    let mut violations = Vec::new();
    for (file, source) in HANDLER_SOURCES {
        for needle in FORBIDDEN {
            if source.contains(needle) {
                violations.push(format!("{file} contains forbidden {needle}"));
            }
        }
        if (*file == "handlers.rs" || *file == "args.rs") && source.contains("set_root(") {
            violations.push(format!("{file} contains forbidden set_root("));
        }
    }
    assert!(
        violations.is_empty(),
        "choke-point boundary violated:\n{}",
        violations.join("\n")
    );
}

fn create_exec_state(fs: MockFs) -> ExecState<MockProcessManager> {
    let mut state = ExecState {
        fs: Box::new(fs.clone()),
        cargo_scratch: oxdock_fs::reserve_cargo_scratch().unwrap(),
        seed: Arc::new(HashMap::new()),
        cwd: fs.root().clone(),
        envs: Arc::new(HashMap::new()),
        bg_children: Vec::new(),
        scope_stack: Vec::new(),
        isolated_stack: Vec::new(),
        io: ExecIo::new(),
        assert_windows: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        assert_windows_stderr: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        exact_stdout: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        var_scopes: Vec::new(),
        cancel_token: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel_hub: Arc::new(oxdock_process::CancelHub::new()),
        active_process: Arc::new(std::sync::Mutex::new(None)),
        named_tasks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        next_task_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        inside_async: false,
        keeper_expiry: None,
        cancellable: false,
        functions: super::native::FunctionRegistry::with_builtins(),
        types: super::typing::startup_type_map(),
        record_schemas: std::collections::HashMap::new(),
        module_states: std::collections::HashMap::new(),
        type_aliases: std::collections::HashMap::new(),
        call_depth: 0,
        task_id: 0,
        push_manifest: Vec::new(),
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
    execute_steps(
        &mut state,
        &mut proc,
        steps,
        CommandStdin::Null,
        false,
        None,
        None,
        None,
        None,
        true,
    )
    .unwrap();
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

#[test]
fn for_int_key_binds_list_indices() {
    let steps = crate::parse_script(indoc! {r#"
        LET $items: LIST<STRING> = ["a", "b"]
        FOR $i: INT, $v: STRING IN $items {
            WRITE "{{ $v }}.txt" "{{ $i }}"
        }
    "#})
    .expect("parse typed loop");
    let (_cwd, files) = run_with_mock_fs(&steps);
    let content = |name: &str| {
        files
            .iter()
            .find(|(k, _)| k.ends_with(name))
            .map(|(_, v)| String::from_utf8_lossy(v).to_string())
    };
    assert_eq!(content("a.txt"), Some("0".to_string()));
    assert_eq!(content("b.txt"), Some("1".to_string()));

    // Map iteration with an INT key is rejected: map keys are strings.
    let steps = crate::parse_script(indoc! {r#"
        LET $m: MAP<k: STRING> = {"k": "v"}
        FOR $k: INT, $v: STRING IN $m {
            WRITE x.txt "hi"
        }
    "#})
    .expect("parse");
    let fs = MockFs::new();
    let mut state = create_exec_state(fs.clone());
    let mut proc = MockProcessManager::default();
    let err = execute_steps(
        &mut state,
        &mut proc,
        &steps,
        CommandStdin::Null,
        false,
        None,
        None,
        None,
        None,
        true,
    )
    .expect_err("INT key over MAP must fail");
    assert!(
        format!("{err:#}").contains("requires a STRING key"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn declared_bool_vs_string_treatment_differs() {
    // Same source text `!true` means different things per declared type:
    // BOOL evaluates the expression to false; quoted STRING stays literal.
    let steps = crate::parse_script(indoc! {r#"
        LET $b: BOOL = !true
        LET $s: STRING = "!true"
        WRITE b.txt "{{ $b }}"
        WRITE s.txt "{{ $s }}"
        IF $b {
            WRITE wrong.txt "bool was truthy"
        }
    "#})
    .expect("parse typed declarations");
    let (_cwd, files) = run_with_mock_fs(&steps);
    let content = |name: &str| {
        files
            .iter()
            .find(|(k, _)| k.ends_with(name))
            .map(|(_, v)| String::from_utf8_lossy(v).to_string())
    };
    assert_eq!(content("b.txt"), Some("false".to_string()));
    assert_eq!(content("s.txt"), Some("!true".to_string()));
    assert!(
        content("wrong.txt").is_none(),
        "BOOL false must skip the IF branch"
    );

    // A STRING variable is not a valid condition.
    let steps = crate::parse_script(indoc! {r#"
        LET $s: STRING = "!true"
        IF $s {
            WRITE x.txt "hi"
        }
    "#})
    .expect("parse");
    let fs = MockFs::new();
    let mut state = create_exec_state(fs.clone());
    let mut proc = MockProcessManager::default();
    let err = execute_steps(
        &mut state,
        &mut proc,
        &steps,
        CommandStdin::Null,
        false,
        None,
        None,
        None,
        None,
        true,
    )
    .expect_err("STRING condition must fail");
    assert!(
        format!("{err:#}").contains("must be a Bool"),
        "unexpected error: {err:#}"
    );
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
        step(StepKind::Assign {
            var: "cap_cat".into(),
            decl_type: "PIPE".to_string(),
            expr: Expr::FreshPipe,
        }),
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
                    pipe: Some(oxdock_parser::PipeTarget::Var("cap_cat".to_string())),
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
                    pipe: Some(oxdock_parser::PipeTarget::Var("cap_cat".to_string())),
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
        CommandStdin::Null,
        false,
        Some(StreamHandle::Stream(sink.clone())),
        Some(StreamHandle::Stream(sink)),
        None,
        None,
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
fn mock_fs_workspace_cache_and_system_round_trip() {
    // CACHE behaves like the other roots for relative paths; SYSTEM
    // additionally resolves absolute paths without confinement while
    // CACHE keeps rejecting escapes (issue #163).
    let steps = crate::parse_script(indoc! {r#"
        WORKSPACE CACHE
        WRITE cached.txt cached
        LET $v: STRING = READ cached.txt
        ASSERT_EQ $v cached
        WORKSPACE SYSTEM
        WRITE sys.txt sys
        LET $w: STRING = READ sys.txt
        ASSERT_EQ $w sys
    "#})
    .expect("parse");
    let (_cwd, files) = run_with_mock_fs(&steps);
    let content = |name: &str| {
        files
            .iter()
            .find(|(k, _)| k.ends_with(name))
            .map(|(_, v)| String::from_utf8_lossy(v).to_string())
    };
    assert_eq!(content("cached.txt"), Some("cached".to_string()));
    assert_eq!(content("sys.txt"), Some("sys".to_string()));
}

#[test]
fn mock_fs_workspace_scope_restores_cache_and_system() {
    // Guard-block scoping reverts WORKSPACE selection for all four roots:
    // after the block the previous root is back in force.
    let steps = crate::parse_script(indoc! {r#"
        WORKSPACE CACHE
        [bool:true] {
            WORKSPACE LOCAL
            WRITE inner.txt inner
        }
        WRITE outer.txt outer
        WORKSPACE SYSTEM
        [bool:true] {
            WORKSPACE SNAPSHOT
            WRITE snap.txt snap
        }
        WRITE after.txt after
    "#})
    .expect("parse");
    let (_cwd, files) = run_with_mock_fs(&steps);
    for name in ["inner.txt", "outer.txt", "snap.txt", "after.txt"] {
        assert!(
            files.keys().any(|k| k.ends_with(name)),
            "expected {name} in mock state: {:?}",
            files.keys()
        );
    }
}

#[test]
fn mock_fs_workspace_cache_local_round_trip() {
    // `WORKSPACE CACHE --local` dispatches through the same selection
    // machinery; the mock shares one namespace, so this pins the
    // parse-to-write flow rather than the on-disk layout.
    let steps = crate::parse_script(indoc! {r#"
        WORKSPACE CACHE --local
        WRITE local-cached.txt persistent
        LET $v: STRING = READ local-cached.txt
        ASSERT_EQ $v persistent
        WORKSPACE CACHE
        WRITE os-cached.txt os
        LET $w: STRING = READ os-cached.txt
        ASSERT_EQ $w os
    "#})
    .expect("parse");
    let (_cwd, files) = run_with_mock_fs(&steps);
    let content = |name: &str| {
        files
            .iter()
            .find(|(k, _)| k.ends_with(name))
            .map(|(_, v)| String::from_utf8_lossy(v).to_string())
    };
    assert_eq!(content("local-cached.txt"), Some("persistent".to_string()));
    assert_eq!(content("os-cached.txt"), Some("os".to_string()));
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
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap())
        as Box<dyn WorkspaceFs>;
    let err = run_expect_err(fs, &steps, runner.clone());

    assert!(
        err.chain()
            .any(|c| c.to_string().contains("simulated failure"))
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
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap())
        as Box<dyn WorkspaceFs>;
    let err = run_expect_err(fs, &steps, runner);
    assert!(
        err.chain()
            .any(|c| c.to_string().contains("simulated failure"))
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
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap())
        as Box<dyn WorkspaceFs>;
    let err = run_expect_err(fs, &steps, runner);
    assert!(
        err.chain()
            .any(|c| c.to_string().contains("simulated failure"))
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
        step(StepKind::Exit(oxdock_parser::Arg::String(
            "3".to_string(),
            false,
        ))),
    ];
    let mock = MockProcessManager::default();
    mock.push_bg_plan(100, success_status());
    mock.push_bg_plan(100, success_status());
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let err = run_expect_err(fs, &steps, mock.clone());
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
        self.calls
            .lock()
            .expect("poisoned")
            .push(script.to_string());
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
    let err = run_expect_err(fs, &steps, runner);

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
    let steps = vec![
        step(StepKind::Assign {
            var: "p".into(),
            decl_type: "PIPE".to_string(),
            expr: Expr::FreshPipe,
        }),
        Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![
                    IoBinding {
                        stream: IoStream::Stdout,
                        pipe: Some(oxdock_parser::PipeTarget::Var("p".into())),
                    },
                    IoBinding {
                        stream: IoStream::Stdout,
                        pipe: Some(oxdock_parser::PipeTarget::Var("p".into())),
                    },
                ],
                cmd: Box::new(StepKind::Echo("x".into())),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let fs = MockFs::new();
    let mut state = create_exec_state(fs);
    let mut proc = MockProcessManager::default();
    let err = execute_steps(
        &mut state,
        &mut proc,
        &steps,
        CommandStdin::Null,
        false,
        None,
        None,
        None,
        None,
        true,
    )
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
        let steps = vec![
            step(StepKind::Assign {
                var: "p".into(),
                decl_type: "PIPE".to_string(),
                expr: Expr::FreshPipe,
            }),
            Step {
                guard: None,
                kind: StepKind::WithIo {
                    bindings: vec![
                        IoBinding {
                            stream: stream_a,
                            pipe: Some(oxdock_parser::PipeTarget::Var("p".into())),
                        },
                        IoBinding {
                            stream: stream_b,
                            pipe: Some(oxdock_parser::PipeTarget::Var("p".into())),
                        },
                    ],
                    cmd: Box::new(StepKind::Echo("x".into())),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
        ];
        let fs = MockFs::new();
        let mut state = create_exec_state(fs);
        let mut proc = MockProcessManager::default();
        let err = execute_steps(
            &mut state,
            &mut proc,
            &steps,
            CommandStdin::Null,
            false,
            None,
            None,
            None,
            None,
            true,
        )
        .expect_err("duplicate binding");
        assert!(
            err.to_string().contains(fragment),
            "expected '{fragment}', got: {err}"
        );
    }
}

#[cfg(not(miri))]
#[test]
fn with_io_async_outer_pipe_stays_script() {
    // Scope-tracked promotion: the main thread never promotes (it is the
    // universal sharer), and the worker only promotes pipes it declared
    // itself. `$live` is main-declared and shared into the task, so it
    // stays a script pipe even though the task body is a single RUN.
    let steps = vec![
        step(StepKind::Assign {
            var: "live".into(),
            decl_type: "PIPE".to_string(),
            expr: Expr::FreshPipe,
        }),
        Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![IoBinding {
                    stream: IoStream::Stdout,
                    pipe: Some(oxdock_parser::PipeTarget::Var("live".into())),
                }],
                cmd: Box::new(StepKind::AsyncBlock {
                    body: vec![Step {
                        guard: None,
                        kind: StepKind::Run("echo hi".into()),
                        scope_enter: 0,
                        scope_exit: 0,
                    }],
                }),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let fs = MockFs::new();
    let mut state = create_exec_state(fs);
    let mut proc = MockProcessManager::default();
    execute_steps(
        &mut state,
        &mut proc,
        &steps,
        CommandStdin::Null,
        false,
        None,
        None,
        None,
        None,
        true,
    )
    .expect("run");
    let handle = state
        .get_var("live")
        .expect("var live")
        .as_pipe_handle()
        .expect("pipe live");
    // No rebind rule: a direct promote ask after the run cannot flip a
    // decided script backend into an OS pair.
    assert!(
        matches!(
            state.io.resolve_stdout(0, &handle, true, true),
            Ok((StreamHandle::Stream(_), _))
        ),
        "outer pipe shared into an async RUN task must stay script"
    );
    assert!(
        !state.io.inspect_pipe(&handle).kind.is_os(),
        "shared handle must inspect as a script pipe"
    );
}

#[cfg(not(miri))]
#[test]
fn with_io_async_guarded_and_exec_form_outer_pipe_stays_script() {
    // Same scope rule as above through guard and exec-form wrappers: the
    // pipe is main-declared and shared, so no producer shape promotes it.
    for (script, var) in [
        (
            indoc! {r#"
                LET $g: PIPE
                WITH_IO [stdout=$g] ASYNC { [bool:true] RUN "echo hi" }"#},
            "g",
        ),
        (
            indoc! {r#"
                LET $e: PIPE
                WITH_IO [stdout=$e] ASYNC RUN ["echo", "hi"]"#},
            "e",
        ),
    ] {
        let steps = crate::parse_script(script).expect("parse fixture script");
        let fs = MockFs::new();
        let mut state = create_exec_state(fs);
        let mut proc = MockProcessManager::default();
        execute_steps(
            &mut state,
            &mut proc,
            &steps,
            CommandStdin::Null,
            false,
            None,
            None,
            None,
            None,
            true,
        )
        .expect("run");
        let handle = state
            .get_var(var)
            .unwrap_or_else(|| panic!("var {var}"))
            .as_pipe_handle()
            .unwrap_or_else(|| panic!("pipe {var}"));
        assert!(
            matches!(
                state.io.resolve_stdout(0, &handle, true, true),
                Ok((StreamHandle::Stream(_), _))
            ),
            "guarded and exec form outer pipes must stay script: {script}"
        );
    }
}

#[test]
fn with_io_async_dsl_body_stays_script_pipe() {
    let steps = vec![
        step(StepKind::Assign {
            var: "plain".into(),
            decl_type: "PIPE".to_string(),
            expr: Expr::FreshPipe,
        }),
        Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![IoBinding {
                    stream: IoStream::Stdout,
                    pipe: Some(oxdock_parser::PipeTarget::Var("plain".into())),
                }],
                cmd: Box::new(StepKind::AsyncBlock {
                    body: vec![Step {
                        guard: None,
                        kind: StepKind::Echo("hi".into()),
                        scope_enter: 0,
                        scope_exit: 0,
                    }],
                }),
            },
            scope_enter: 0,
            scope_exit: 0,
        },
    ];
    let fs = MockFs::new();
    let mut state = create_exec_state(fs);
    let mut proc = MockProcessManager::default();
    execute_steps(
        &mut state,
        &mut proc,
        &steps,
        CommandStdin::Null,
        false,
        None,
        None,
        None,
        None,
        true,
    )
    .expect("run");
    let handle = state
        .get_var("plain")
        .expect("var plain")
        .as_pipe_handle()
        .expect("pipe plain");
    assert!(
        !state.io.inspect_pipe(&handle).kind.is_os(),
        "DSL producers must keep store and forward script pipes"
    );
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
    let err = execute_steps(
        &mut state,
        &mut proc,
        &steps,
        CommandStdin::Null,
        false,
        None,
        None,
        None,
        None,
        true,
    )
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
    let err = execute_steps(
        &mut state,
        &mut proc,
        &steps,
        CommandStdin::Null,
        false,
        None,
        None,
        None,
        None,
        true,
    )
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
        CommandStdin::Null,
        false,
        None,
        Some(StreamHandle::Stream(sink)),
        None,
        None,
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
            key: oxdock_process::INHERIT_STDOUT_ENV_VAR.into(),
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
        CommandStdin::Null,
        false,
        Some(StreamHandle::Stream(out_sink)),
        Some(StreamHandle::Stream(err_sink)),
        None,
        None,
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
fn exec_io_pipe_endpoints_resolve_through_handles() {
    use oxdock_parser::Value;
    let io = ExecIo::new();
    let handle = Value::pipe_fresh().as_pipe_handle().expect("fresh handle");

    // Script resolution hands out live stream halves plus the backend.
    let (stdin, stdin_backend) = io
        .resolve_stdin(0, &handle, false, false)
        .expect("resolve stdin");
    assert!(matches!(stdin, CommandStdin::Stream(_)));
    assert!(stdin_backend.is_some());
    let (stdout, stdout_backend) = io
        .resolve_stdout(0, &handle, false, false)
        .expect("resolve stdout");
    assert!(matches!(stdout, StreamHandle::Stream(_)));
    assert!(stdout_backend.is_some());
    assert!(matches!(
        io.resolve_stderr(0, &handle, false, false),
        Ok(StreamHandle::Stream(_))
    ));
    // Repeat reads share the backend: same channel, first-binding-wins.
    let info = io.inspect_pipe(&handle);
    assert!(!info.kind.is_os());
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
        CommandStdin::Null,
        false,
        Some(StreamHandle::Stream(sink.clone())),
        None,
        None,
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
                from_workspace: None,
                from_host: false,
                to_host: false,
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
fn archive_unarchive_round_trips_tree_gzip_and_raw() {
    for raw in [false, true] {
        let (archive, dst) = if raw {
            ("bundle.tar", "restored-raw")
        } else {
            ("bundle.tar.gz", "restored")
        };
        let steps = vec![
            step(StepKind::Write {
                path: "bundle/app.txt".into(),
                contents: Some("payload".into()),
            }),
            step(StepKind::Write {
                path: "bundle/nested/deep.txt".into(),
                contents: Some("deep".into()),
            }),
            step(StepKind::Mkdir("bundle/empty".into())),
            step(StepKind::Archive {
                src: "bundle".into(),
                dst: archive.into(),
                raw,
            }),
            step(StepKind::Unarchive {
                src: archive.into(),
                dst: dst.into(),
                max_bytes: None,
            }),
        ];
        let temp = GuardedPath::tempdir().unwrap();
        let root = temp.as_guarded_path().clone();
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        run_steps_with_manager(fs, &steps, MockProcessManager::default(), ExecIo::new())
            .expect("archive pipeline");
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
        let app = root.join(&format!("{dst}/app.txt")).unwrap();
        let deep = root.join(&format!("{dst}/nested/deep.txt")).unwrap();
        let empty = root.join(&format!("{dst}/empty")).unwrap();
        assert_eq!(
            resolver.read_file(&app).unwrap(),
            b"payload",
            "raw={raw}: top-level file must survive"
        );
        assert_eq!(
            resolver.read_file(&deep).unwrap(),
            b"deep",
            "raw={raw}: nested file must survive"
        );
        assert!(
            matches!(resolver.entry_kind(&empty), Ok(EntryKind::Dir)),
            "raw={raw}: empty dirs must survive"
        );
    }
}

#[test]
fn archive_single_file_packs_under_basename() {
    let steps = vec![
        step(StepKind::Write {
            path: "note.txt".into(),
            contents: Some("lone".into()),
        }),
        step(StepKind::Archive {
            src: "note.txt".into(),
            dst: "note.tar.gz".into(),
            raw: false,
        }),
        step(StepKind::Unarchive {
            src: "note.tar.gz".into(),
            dst: "out".into(),
            max_bytes: None,
        }),
    ];
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, MockProcessManager::default(), ExecIo::new())
        .expect("single-file pipeline");
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let restored = root.join("out/note.txt").unwrap();
    assert_eq!(
        resolver.read_file(&restored).unwrap(),
        b"lone",
        "single files land under their basename"
    );
}

#[test]
fn archive_root_absolute_src_ignores_cwd_shadow() {
    // `/f` must resolve against the workspace root even when the
    // working directory holds its own `f`: splitting on the leading
    // slash used to yield an empty parent and probe the cwd instead.
    let steps = vec![
        step(StepKind::Mkdir("sub".into())),
        step(StepKind::Write {
            path: "sub/f".into(),
            contents: Some("wrong".into()),
        }),
        step(StepKind::Write {
            path: "f".into(),
            contents: Some("right".into()),
        }),
        step(StepKind::Workdir("sub".into())),
        step(StepKind::Archive {
            src: "/f".into(),
            dst: "packed.tar.gz".into(),
            raw: false,
        }),
        step(StepKind::Unarchive {
            src: "packed.tar.gz".into(),
            dst: "out".into(),
            max_bytes: None,
        }),
    ];
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, MockProcessManager::default(), ExecIo::new())
        .expect("absolute archive pipeline");
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let restored = root.join("sub/out/f").unwrap();
    assert_eq!(
        resolver.read_file(&restored).unwrap(),
        b"right",
        "root-absolute sources must not resolve through the cwd shadow"
    );
}

#[test]
fn archive_destination_cannot_be_source_or_inside_packed_tree() {
    // Destination equal to the source would truncate it before reading.
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let steps = vec![
        step(StepKind::Write {
            path: "note.txt".into(),
            contents: Some("do not eat me".into()),
        }),
        step(StepKind::Archive {
            src: "note.txt".into(),
            dst: "note.txt".into(),
            raw: false,
        }),
    ];
    let err = run_expect_err(fs, &steps, MockProcessManager::default());
    assert!(
        err.chain()
            .any(|c| c.to_string().contains("cannot be the source itself")),
        "unexpected chain: {err:#}"
    );
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let survivor = root.join("note.txt").unwrap();
    assert_eq!(
        resolver.read_file(&survivor).unwrap(),
        b"do not eat me",
        "rejected self-archives must leave the source intact"
    );
    // A pre-existing destination inside the packed tree would be
    // truncated before its own append reads it back.
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let steps = vec![
        step(StepKind::Write {
            path: "bundle/a.txt".into(),
            contents: Some("a".into()),
        }),
        step(StepKind::Write {
            path: "bundle/out.tar.gz".into(),
            contents: Some("stale".into()),
        }),
        step(StepKind::Archive {
            src: "bundle".into(),
            dst: "bundle/out.tar.gz".into(),
            raw: false,
        }),
    ];
    let err = run_expect_err(fs, &steps, MockProcessManager::default());
    assert!(
        err.chain()
            .any(|c| c.to_string().contains("cannot overwrite inside")),
        "unexpected chain: {err:#}"
    );

    // A fresh destination inside the tree is safe (collection already
    // ran, so it cannot be a member) and keeps whole-tree archives
    // like `ARCHIVE project project.tar.gz` working.
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let steps = vec![
        step(StepKind::Write {
            path: "bundle/a.txt".into(),
            contents: Some("a".into()),
        }),
        step(StepKind::Archive {
            src: "bundle".into(),
            dst: "bundle/fresh.tar.gz".into(),
            raw: false,
        }),
        step(StepKind::Unarchive {
            src: "bundle/fresh.tar.gz".into(),
            dst: "elsewhere".into(),
            max_bytes: None,
        }),
    ];
    run_steps_with_manager(fs, &steps, MockProcessManager::default(), ExecIo::new())
        .expect("fresh inside-tree destination must pack");
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let restored = root.join("elsewhere/a.txt").unwrap();
    assert_eq!(resolver.read_file(&restored).unwrap(), b"a");
}

#[test]
fn unarchive_restores_read_only_dirs_after_children_land() {
    // A 0555 member dir applied eagerly would lock out its own file
    // creations with EACCES; modes must settle after extraction.
    let mut raw = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut raw);
        let mut dir_header = tar::Header::new_gnu();
        dir_header.set_entry_type(tar::EntryType::Directory);
        dir_header.set_mode(0o555);
        dir_header.set_size(0);
        dir_header.set_cksum();
        builder
            .append_data(&mut dir_header, "rodir", std::io::empty())
            .unwrap();
        let mut file_header = tar::Header::new_gnu();
        file_header.set_size(5);
        file_header.set_mode(0o644);
        file_header.set_cksum();
        builder
            .append_data(&mut file_header, "rodir/inner.txt", &b"inner"[..])
            .unwrap();
        builder.into_inner().unwrap();
    }
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let archive = root.join("locked.tar").unwrap();
    resolver.write_file(&archive, &raw).unwrap();
    let steps = vec![step(StepKind::Unarchive {
        src: "locked.tar".into(),
        dst: "out".into(),
        max_bytes: None,
    })];
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, MockProcessManager::default(), ExecIo::new())
        .expect("read-only member dirs must not block their children");
    let restored = root.join("out/rodir/inner.txt").unwrap();
    assert_eq!(resolver.read_file(&restored).unwrap(), b"inner");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = root.join("out/rodir").unwrap();
        let mode = resolver.metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o555, "deferred modes still settle, got {mode:o}");
    }
}

#[test]
fn unarchive_rejects_truncated_input() {
    for (name, bytes) in [("short", vec![0u8; 100]), ("almost-header", vec![0u8; 511])] {
        let temp = GuardedPath::tempdir().unwrap();
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
        let archive = root.join("cut.tar").unwrap();
        resolver.write_file(&archive, &bytes).unwrap();
        let steps = vec![step(StepKind::Unarchive {
            src: "cut.tar".into(),
            dst: "out".into(),
            max_bytes: None,
        })];
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        let err = run_expect_err(fs, &steps, MockProcessManager::default());
        assert!(
            err.chain().any(|c| c.to_string().contains("truncated")),
            "{name}: unexpected chain: {err:#}"
        );
    }
}

#[test]
fn unarchive_exact_cap_extraction_succeeds() {
    // A payload ending exactly on the cap must read clean: the meter
    // probes for EOF instead of tripping on the trailing read. One
    // byte over still fails.
    for (name, cap, ok) in [("exact", "5", true), ("over", "4", false)] {
        let temp = GuardedPath::tempdir().unwrap();
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
        let tool = root.join("tool.bin").unwrap();
        resolver.write_file(&tool, b"12345").unwrap();
        let steps = vec![
            step(StepKind::Archive {
                src: "tool.bin".into(),
                dst: "tool.tar.gz".into(),
                raw: false,
            }),
            step(StepKind::Unarchive {
                src: "tool.tar.gz".into(),
                dst: "out".into(),
                max_bytes: Some(cap.into()),
            }),
        ];
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        if ok {
            run_steps_with_manager(fs, &steps, MockProcessManager::default(), ExecIo::new())
                .expect("exact-cap payloads must extract");
            let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
            let restored = root.join("out/tool.bin").unwrap();
            assert_eq!(resolver.read_file(&restored).unwrap(), b"12345");
        } else {
            let err = run_expect_err(fs, &steps, MockProcessManager::default());
            assert!(
                err.chain().any(|c| c.to_string().contains("expansion cap")),
                "{name}: unexpected chain: {err:#}"
            );
        }
    }
}

#[test]
fn unarchive_expansion_cap_trips_on_bombs() {
    // 5 MiB of zeroes compress to kilobytes: metering archive bytes
    // would miss it, metering member bytes trips the cap. Seeded
    // through the resolver so no 5 MB literal rides the test source.
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let zeros = root.join("zeros.bin").unwrap();
    resolver
        .write_file(&zeros, &vec![0u8; 5 * 1_024 * 1_024])
        .unwrap();
    let steps = vec![
        step(StepKind::Archive {
            src: "zeros.bin".into(),
            dst: "bomb.tar.gz".into(),
            raw: false,
        }),
        step(StepKind::Unarchive {
            src: "bomb.tar.gz".into(),
            dst: "out".into(),
            max_bytes: Some("1MB".into()),
        }),
    ];
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let err = run_expect_err(fs, &steps, MockProcessManager::default());
    assert!(
        err.chain().any(|c| c
            .to_string()
            .contains("exceeds the 1000000-byte expansion cap")),
        "unexpected chain: {err:#}"
    );

    // The default 1 GiB ceiling clears the same 5 MiB honestly.
    let steps = vec![step(StepKind::Unarchive {
        src: "bomb.tar.gz".into(),
        dst: "wide".into(),
        max_bytes: None,
    })];
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, MockProcessManager::default(), ExecIo::new())
        .expect("default cap must clear megabyte payloads");
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let restored = root.join("wide/zeros.bin").unwrap();
    assert_eq!(
        resolver.read_file(&restored).unwrap().len(),
        5 * 1_024 * 1_024
    );
}

#[test]
fn unarchive_rejects_garbage_cap() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let tool = root.join("tool.bin").unwrap();
    resolver.write_file(&tool, b"tool").unwrap();
    let steps = vec![
        step(StepKind::Archive {
            src: "tool.bin".into(),
            dst: "tool.tar.gz".into(),
            raw: false,
        }),
        step(StepKind::Unarchive {
            src: "tool.tar.gz".into(),
            dst: "out".into(),
            max_bytes: Some("banana".into()),
        }),
    ];
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let err = run_expect_err(fs, &steps, MockProcessManager::default());
    assert!(
        err.chain()
            .any(|c| c.to_string().contains("invalid byte count")),
        "unexpected chain: {err:#}"
    );
}

#[test]
fn archive_refuses_nested_symlink_in_tree() {
    // The top-level source is a plain dir; the link hides one level
    // down. The walk must probe every descendant listing entry, not
    // just the source.
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    if !can_create_symlinks(root.as_path()) {
        return;
    }
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let target = root.join("bundle/real.txt").unwrap();
    resolver.write_file(&target, b"real").unwrap();
    let sub = root.join("bundle/sub").unwrap();
    resolver.create_dir_all(&sub).unwrap();
    let link = root.join("bundle/sub/link.txt").unwrap();
    resolver.symlink(&target, &link).unwrap();
    let steps = vec![step(StepKind::Archive {
        src: "bundle".into(),
        dst: "bundle.tar.gz".into(),
        raw: false,
    })];
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let err = run_expect_err(fs, &steps, MockProcessManager::default());
    assert!(
        err.chain()
            .any(|c| c.to_string().contains("refuses to pack symlink")),
        "unexpected chain: {err:#}"
    );
}

#[test]
fn archive_round_trips_unix_modes() {
    // Pack-side mode recording plus unpack-side restore: an executable
    // bit set before ARCHIVE must survive the round trip.
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let tool = root.join("tool.sh").unwrap();
    resolver.write_file(&tool, b"tool").unwrap();
    resolver.set_permissions_mode_unix(&tool, 0o755).unwrap();
    let steps = vec![
        step(StepKind::Archive {
            src: "tool.sh".into(),
            dst: "tool.tar.gz".into(),
            raw: false,
        }),
        step(StepKind::Unarchive {
            src: "tool.tar.gz".into(),
            dst: "out".into(),
            max_bytes: None,
        }),
    ];
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    run_steps_with_manager(fs, &steps, MockProcessManager::default(), ExecIo::new())
        .expect("mode round trip");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let restored = root.join("out/tool.sh").unwrap();
        let mode = resolver.metadata(&restored).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "executable bit must survive, got {mode:o}");
    }
}

#[test]
fn archive_hashes_through_hash_sha256() {
    // No built-in hashing flag: the finished archive hashes like any
    // other file.
    let backing = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink: SharedOutput = backing.clone();
    let steps = vec![
        step(StepKind::Write {
            path: "tool.bin".into(),
            contents: Some("tool".into()),
        }),
        step(StepKind::Archive {
            src: "tool.bin".into(),
            dst: "tool.tar.gz".into(),
            raw: false,
        }),
        step(StepKind::HashSha256 {
            path: "tool.tar.gz".into(),
        }),
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
    .expect("hash pipeline");
    let hex = String::from_utf8(backing.lock().unwrap().clone()).unwrap();
    let hex = hex.trim();
    assert_eq!(hex.len(), 64, "full sha256 hex expected: {hex}");
    assert!(
        hex.chars().all(|c| c.is_ascii_hexdigit()),
        "hex digest expected: {hex}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "probes real executable bits and host PATH layout; unsupported under Miri"
)]
fn is_exec_finds_host_tools_and_direct_files() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let tools = root.join("tools").unwrap();
    resolver.create_dir_all(&tools).unwrap();
    // Both spellings exist so each platform finds its own: Unix runs
    // the extensionless file, Windows the `.exe` one. A `.bat` file
    // proves the wider extension search: runnable on Windows, a plain
    // file on Unix.
    for name in ["faketool", "faketool.exe", "fakebot.bat"] {
        let exe = tools.join(name).unwrap();
        resolver.write_file(&exe, b"fake").unwrap();
        resolver.set_permissions_mode_unix(&exe, 0o755).unwrap();
    }
    let tools_str = to_forward_slashes(&tools.as_path().to_string_lossy());
    let script = format!(
        indoc! {r#"
            IMPORT [STD]
            WORKSPACE SYSTEM
            ENV PATH="{tools}"
            LET $found: BOOL = IS_EXEC("faketool")
            ASSERT_EQ $found true
            LET $miss: BOOL = IS_EXEC("no-such-tool-xyz")
            ASSERT_EQ $miss false
            LET $empty: BOOL = IS_EXEC("")
            ASSERT_EQ $empty false
            [family:unix] LET $batch: BOOL = IS_EXEC("fakebot")
            [family:unix] ASSERT_EQ $batch false
            [family:windows] LET $batch: BOOL = IS_EXEC("fakebot")
            [family:windows] ASSERT_EQ $batch true
            # Explicit script names resolve directly on both platforms:
            # no spurious `.exe` gets appended.
            LET $batdirect: BOOL = IS_EXEC("{tools}/fakebot.bat")
            ASSERT_EQ $batdirect true
            [family:unix] LET $direct: BOOL = IS_EXEC("{tools}/faketool")
            [family:windows] LET $direct: BOOL = IS_EXEC("{tools}/faketool.exe")
            ASSERT_EQ $direct true
        "#},
        tools = tools_str
    );
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let steps = crate::parse_script(&script).unwrap();
    run_steps_with_manager(fs, &steps, MockProcessManager::default(), ExecIo::new())
        .expect("tool probe pipeline");
}

#[test]
#[cfg(windows)]
fn parse_pathext_shapes_and_defaults() {
    use super::args::parse_pathext;
    assert!(
        parse_pathext(None).is_empty(),
        "unset PATHEXT probes nothing"
    );
    assert!(parse_pathext(Some("")).is_empty());
    assert_eq!(parse_pathext(Some("exe;bat")), vec![".exe", ".bat"]);
    assert_eq!(
        parse_pathext(Some(" .EXE;;.exe; ")),
        vec![".EXE"],
        "trims, skips empties, dedupes case-insensitively",
    );
    assert_eq!(
        parse_pathext(Some(".COM;.EXE;.BAT;.CMD")),
        vec![".COM", ".EXE", ".BAT", ".CMD"],
    );
}

#[test]
#[cfg(windows)]
#[cfg_attr(
    miri,
    ignore = "probes real files and script PATHEXT layout; unsupported under Miri"
)]
fn is_exec_honors_pathext() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let tools = root.join("tools").unwrap();
    resolver.create_dir_all(&tools).unwrap();
    // A custom extension the default set never probes, plus a stock
    // `.bat` the custom set must ignore.
    for name in ["custom.foo", "plain.bat"] {
        let file = tools.join(name).unwrap();
        resolver.write_file(&file, b"fake").unwrap();
    }
    let tools_str = to_forward_slashes(&tools.as_path().to_string_lossy());
    let script = format!(
        indoc! {r#"
            IMPORT [STD]
            WORKSPACE SYSTEM
            ENV PATH="{tools}"
            ENV PATHEXT=".FOO"
            LET $custom: BOOL = IS_EXEC("custom")
            ASSERT_EQ $custom true
            LET $plain: BOOL = IS_EXEC("plain")
            ASSERT_EQ $plain false
            # An explicitly empty PATHEXT probes nothing at all.
            ENV PATHEXT=""
            LET $gone: BOOL = IS_EXEC("custom")
            ASSERT_EQ $gone false
        "#},
        tools = tools_str
    );
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let steps = crate::parse_script(&script).unwrap();
    run_steps_with_manager(fs, &steps, MockProcessManager::default(), ExecIo::new())
        .expect("pathext pipeline");
}

#[test]
fn archive_refuses_symlink_source() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    if !can_create_symlinks(root.as_path()) {
        return;
    }
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let target = root.join("target.txt").unwrap();
    resolver.write_file(&target, b"target").unwrap();
    let link = root.join("link.txt").unwrap();
    resolver.symlink(&target, &link).unwrap();
    let steps = vec![step(StepKind::Archive {
        src: "link.txt".into(),
        dst: "link.tar.gz".into(),
        raw: false,
    })];
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let err = run_expect_err(fs, &steps, MockProcessManager::default());
    assert!(
        err.chain()
            .any(|c| c.to_string().contains("refuses to pack symlink")),
        "unexpected chain: {err:#}"
    );
}

#[test]
fn unarchive_refuses_hostile_members() {
    let mut symlink_tar = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut symlink_tar);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_link(&mut header, "link.txt", "target.txt")
            .unwrap();
        builder.into_inner().unwrap();
    }
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "traversal",
            hostile_tar_bytes("../evil.txt", b"evil"),
            "escapes",
        ),
        (
            "absolute",
            hostile_tar_bytes("/abs.txt", b"evil"),
            "absolute",
        ),
        ("symlink", symlink_tar, "refuses symlink member"),
        (
            "fifo",
            hostile_tar_typed("fifo", b"", Some(b'6')),
            "unsupported type",
        ),
    ];
    for (name, bytes, needle) in cases {
        let temp = GuardedPath::tempdir().unwrap();
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
        let archive = root.join("hostile.tar").unwrap();
        resolver.write_file(&archive, &bytes).unwrap();
        let steps = vec![step(StepKind::Unarchive {
            src: "hostile.tar".into(),
            dst: "out".into(),
            max_bytes: None,
        })];
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        let err = run_expect_err(fs, &steps, MockProcessManager::default());
        assert!(
            err.chain().any(|c| c.to_string().contains(needle)),
            "{name}: unexpected chain: {err:#}"
        );
        assert!(
            !root.join("evil.txt").unwrap().exists(),
            "{name}: no byte may land outside the destination"
        );
        let out = root.join("out").unwrap();
        let leftovers = resolver.read_dir_entries(&out).unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "{name}: no byte may land inside the destination either"
        );
    }
}

#[test]
fn unarchive_refuses_planted_link_at_member_path() {
    // A link waiting where a member will land: the write must fail
    // instead of truncating the link's target. Follows would destroy
    // `sensitive.txt`; the nofollow open refuses with ELOOP.
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    if !can_create_symlinks(root.as_path()) {
        return;
    }
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let sensitive = root.join("sensitive.txt").unwrap();
    resolver.write_file(&sensitive, b"secret").unwrap();
    let out = root.join("out").unwrap();
    resolver.create_dir_all(&out).unwrap();
    let planted = out.join("evil.txt").unwrap();
    resolver.symlink(&sensitive, &planted).unwrap();
    let archive = root.join("planted.tar").unwrap();
    resolver
        .write_file(&archive, &hostile_tar_bytes("evil.txt", b"new"))
        .unwrap();
    let steps = vec![step(StepKind::Unarchive {
        src: "planted.tar".into(),
        dst: "out".into(),
        max_bytes: None,
    })];
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let err = run_expect_err(fs, &steps, MockProcessManager::default());
    assert!(
        err.chain().any(|c| c.to_string().contains("member")),
        "planted link must fail at the member write: {err:#}"
    );
    let kept = resolver.read_file(&sensitive).unwrap();
    assert_eq!(kept, b"secret", "link target must survive byte-identical");
}

#[test]
fn unarchive_refuses_planted_ancestor_link() {
    // A link waiting one level up: `out/sub` points at a shadow dir,
    // so member `sub/evil.txt` would land outside the destination.
    // The lexical wall passes (`out/sub/evil.txt` starts with `out`),
    // which is exactly why ancestors get their own `lstat` refusal.
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    if !can_create_symlinks(root.as_path()) {
        return;
    }
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let shadow = root.join("shadow").unwrap();
    resolver.create_dir_all(&shadow).unwrap();
    let out = root.join("out").unwrap();
    resolver.create_dir_all(&out).unwrap();
    let planted = out.join("sub").unwrap();
    resolver.symlink(&shadow, &planted).unwrap();
    let archive = root.join("ancestor.tar").unwrap();
    resolver
        .write_file(&archive, &hostile_tar_bytes("sub/evil.txt", b"new"))
        .unwrap();
    let steps = vec![step(StepKind::Unarchive {
        src: "ancestor.tar".into(),
        dst: "out".into(),
        max_bytes: None,
    })];
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
    let err = run_expect_err(fs, &steps, MockProcessManager::default());
    assert!(
        err.chain().any(|c| c.to_string().contains("ancestor")),
        "planted ancestor link must fail the unpack: {err:#}"
    );
    assert!(
        !shadow.join("evil.txt").unwrap().exists(),
        "no byte may land through the planted ancestor"
    );
}

/// Splice a hostile member name into an otherwise valid raw tar and
/// re-checksum its header: the `tar` builder refuses hostile names up
/// front, but hostile bytes arrive over the wire.
fn hostile_tar_bytes(name: &str, payload: &[u8]) -> Vec<u8> {
    hostile_tar_typed(name, payload, None)
}

/// [`hostile_tar_bytes`] plus a typeflag override for special members
/// (`b'6'` fifo, ...): the checksum covers the patched byte.
fn hostile_tar_typed(name: &str, payload: &[u8], entry_type: Option<u8>) -> Vec<u8> {
    let mut raw = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut raw);
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "victim.txt", payload)
            .unwrap();
        builder.into_inner().unwrap();
    }
    let name_bytes = name.as_bytes();
    assert!(name_bytes.len() < 100, "name must fit the tar name field");
    raw[..name_bytes.len()].copy_from_slice(name_bytes);
    raw[name_bytes.len()] = 0;
    if let Some(entry_type) = entry_type {
        raw[156] = entry_type;
    }
    for b in &mut raw[148..156] {
        *b = b' ';
    }
    let sum: u32 = raw[..512].iter().map(|b| *b as u32).sum();
    let octal = format!("{sum:06o}\0 ");
    raw[148..156].copy_from_slice(octal.as_bytes());
    raw
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
    let err = run_expect_err(fs, &steps, runner.clone());
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
    // Pipeline should succeed : the background task completes naturally
    run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
}

// ---------------------------------------------------------------------------
// TIMEOUT tests
// ---------------------------------------------------------------------------

fn timeout_step(duration: &str, body: Vec<Step>) -> Step {
    Step {
        guard: None,
        kind: StepKind::Timeout {
            duration: oxdock_parser::Arg::String(duration.to_string(), false),
            body,
        },
        scope_enter: 0,
        scope_exit: 0,
    }
}

#[test]
fn timeout_body_completes_within_deadline() {
    let steps = vec![timeout_step(
        "30s",
        vec![Step {
            guard: None,
            kind: StepKind::Write {
                path: "out.txt".into(),
                contents: Some("hi".into()),
            },
            scope_enter: 0,
            scope_exit: 0,
        }],
    )];
    let (_cwd, files) = run_with_mock_fs(&steps);
    let written = files
        .iter()
        .find(|(k, _)| k.ends_with("out.txt"))
        .map(|(_, v)| String::from_utf8_lossy(v).to_string());
    assert_eq!(written, Some("hi".into()));
}

#[test]
fn timeout_body_error_passes_through_without_firing() {
    // A body that fails fast must surface its own error unwrapped : no
    // TIMEOUT prefix when the deadline never elapsed.
    let steps = vec![timeout_step(
        "30s",
        vec![step(StepKind::Exit(oxdock_parser::Arg::String(
            "3".to_string(),
            false,
        )))],
    )];
    let fs = MockFs::new();
    let fs = Box::new(fs) as Box<dyn WorkspaceFs>;
    let err = run_expect_err(fs, &steps, MockProcessManager::default());
    assert!(
        err.to_string().contains("EXIT requested with code 3"),
        "unexpected error: {err:#}"
    );
    assert!(
        !err.to_string().contains("TIMEOUT"),
        "fast failure must not be wrapped as a timeout: {err:#}"
    );
}

/// ProcessManager double whose background commands block until killed,
/// modelling a hung OS process deterministically (no sleeps in the test:
///
/// the deadline watcher fires, kills via `active_process`, and the blocked
/// `wait()` unblocks immediately.
#[derive(Clone, Default)]
struct BlockingRunner {
    kills: Arc<Mutex<Vec<String>>>,
}

struct BlockingHandle {
    script: String,
    kills: Arc<Mutex<Vec<String>>>,
    state: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl Clone for BlockingHandle {
    fn clone(&self) -> Self {
        Self {
            script: self.script.clone(),
            kills: Arc::clone(&self.kills),
            state: Arc::clone(&self.state),
        }
    }
}

impl ProcessManager for BlockingRunner {
    type Handle = BlockingHandle;

    fn run_command(
        &mut self,
        _ctx: &CommandContext,
        script: &str,
        options: CommandOptions,
    ) -> Result<CommandResult<Self::Handle>, anyhow::Error> {
        match options.mode {
            CommandMode::Foreground => Ok(CommandResult::Completed),
            CommandMode::Background => Ok(CommandResult::Background(BlockingHandle {
                script: script.to_string(),
                kills: Arc::clone(&self.kills),
                state: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
            })),
        }
    }
}

impl BackgroundHandle for BlockingHandle {
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, anyhow::Error> {
        let (lock, _) = &*self.state;
        if *lock.lock().unwrap() {
            Ok(Some(exit_status_from_code(137)))
        } else {
            Ok(None)
        }
    }

    fn kill(&mut self) -> Result<(), anyhow::Error> {
        self.kills.lock().unwrap().push(self.script.clone());
        let (lock, cvar) = &*self.state;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
        Ok(())
    }

    fn wait(&mut self) -> Result<ExitStatus, anyhow::Error> {
        let (lock, cvar) = &*self.state;
        let mut done = lock.lock().unwrap();
        while !*done {
            done = cvar.wait(done).unwrap();
        }
        Ok(exit_status_from_code(137))
    }
}

#[test]
#[cfg_attr(
    miri,
    ignore = "TIMEOUT deadline is wall-clock and the test blocks on a condvar; untestable under Miri isolation"
)]
fn timeout_fires_and_kills_blocking_command() {
    let steps = vec![timeout_step(
        "200ms",
        vec![step(StepKind::Run("hang".into()))],
    )];
    let fs = MockFs::new();
    let fs = Box::new(fs) as Box<dyn WorkspaceFs>;
    let runner = BlockingRunner::default();
    let err = run_expect_err(fs, &steps, runner.clone());
    assert!(
        err.to_string().contains("TIMEOUT"),
        "expected deadline error, got: {err:#}"
    );
    assert_eq!(
        runner.kills.lock().unwrap().as_slice(),
        ["hang"],
        "deadline watcher must kill the blocking command"
    );
}

#[test]
fn cancelled_end_poll_reaps_and_bails() {
    // A preset cancellation token must break the end-of-pipeline poll loop
    // even when a background handle never becomes ready (e.g. after a
    // TIMEOUT watcher fires while joining background work).
    let fs = MockFs::new();
    let mut state = create_exec_state(fs.clone());
    let mut proc = MockProcessManager::default();
    state
        .cancel_token
        .store(true, std::sync::atomic::Ordering::SeqCst);
    proc.push_bg_plan(usize::MAX, success_status());
    let steps = vec![async_step("stuck")];
    let err = execute_steps(
        &mut state,
        &mut proc,
        &steps,
        CommandStdin::Null,
        false,
        None,
        None,
        None,
        None,
        true,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("cancelled"),
        "expected cancellation error, got: {err:#}"
    );
}

#[test]
fn timeout_preserves_preexisting_cancellation() {
    // A cancellation set before the TIMEOUT step belongs to the enclosing
    // scope (e.g. an outer deadline already fired while an inner region was
    // entered): a body that still completes must restore the signal, not
    // erase it, so nested deadline propagation keeps working. Calls
    // handlers::timeout directly : the step loop's own pre-check would bail
    // before dispatch, which is a separate (already covered) path.
    let fs = MockFs::new();
    let mut state = create_exec_state(fs.clone());
    let mut proc = MockProcessManager::default();
    state
        .cancel_token
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut cx = StepCtx {
        state: &mut state,
        process: &mut proc,
        stdin: CommandStdin::Null,
        expose_stdin: false,
        out: None,
        err: None,
        out_pipe: None,
        stdin_pipe: None,
    };
    super::handlers::timeout(&mut cx, 0, &std::time::Duration::from_secs(30), &[])
        .expect("empty body must succeed");
    assert!(
        cx.state
            .cancel_token
            .load(std::sync::atomic::Ordering::SeqCst),
        "pre-existing cancellation must survive a successful TIMEOUT body"
    );
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

/// Small spill/backlog thresholds so backend tests exercise the disk path
/// without multi-megabyte payloads (mirrors the production ratio).
/// Disk spilling is compiled out under Miri, with the tests using these.
#[cfg(not(miri))]
const TEST_SPILL_THRESHOLD: usize = 1_024 * 1_024;
#[cfg(not(miri))]
const TEST_MAX_BACKLOG: u64 = 2 * 1_024 * 1_024;

/// Test backend with small thresholds (see above).
#[cfg(not(miri))]
fn test_script_pipe() -> oxdock_pipe::ScriptPipe {
    oxdock_pipe::ScriptPipe::with_thresholds(TEST_SPILL_THRESHOLD, TEST_MAX_BACKLOG)
}

#[test]
#[cfg(not(miri))]
fn script_pipe_stays_in_memory_below_threshold() {
    let pipe = test_script_pipe();
    let writer = pipe.endpoint().stream_handle();
    let reader = pipe.reader();

    let payload = vec![0xABu8; 1_024]; // 1 KiB: below threshold
    writer.lock().unwrap().write_all(&payload).unwrap();
    drop(writer);

    let mut guard = reader.lock().unwrap();
    let mut buf = Vec::new();
    guard.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, payload);
}

#[test]
#[cfg(not(miri))]
fn script_pipe_spills_to_disk_above_threshold() {
    let pipe = test_script_pipe();
    let writer = pipe.endpoint().stream_handle();
    let reader = pipe.reader();

    // Exceed the threshold by 1 MiB
    let size = TEST_SPILL_THRESHOLD + (1_024 * 1_024);
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
    let pipe = test_script_pipe();
    let writer = pipe.endpoint().stream_handle();

    // First, trigger a spill to Disk mode by writing above the spill threshold
    let spill_payload = vec![0u8; TEST_SPILL_THRESHOLD + 1];
    writer.lock().unwrap().write_all(&spill_payload).unwrap();

    // Now write enough to exceed the backlog limit without reading
    // Backlog = write_pos - read_pos. We haven't read, so backlog = spill_payload.len()
    // We need to write enough to make total backlog > TEST_MAX_BACKLOG
    let remaining = (TEST_MAX_BACKLOG as usize) - spill_payload.len() + 1;
    let overflow_payload = vec![0u8; remaining];
    let result = writer.lock().unwrap().write_all(&overflow_payload);

    assert!(
        result.is_err(),
        "Writing beyond TEST_MAX_BACKLOG must return an error"
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
    let pipe = test_script_pipe();
    let writer = pipe.endpoint().stream_handle();
    let reader = pipe.reader();

    // Write above threshold to trigger disk spill
    let size = TEST_SPILL_THRESHOLD + (1_024 * 1_024);
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
    use std::fs;

    let pipe = test_script_pipe();
    let writer = pipe.endpoint().stream_handle();
    let reader = pipe.reader();

    // 1. Trigger disk spill (threshold + 1 MiB)
    let size = TEST_SPILL_THRESHOLD + (1_024 * 1_024);
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

/// Property tests for shell escape/expansion invariants (`expand_string` +
/// `expand_dsl_vars`, the exact two-pass order shell `RUN` resolution uses).
/// String-munging is where surprises hide, so the escape hatches get
/// randomized inputs, not just hand-picked examples: `\$` must never expand,
/// `\{{` must never interpolate, and real placeholders must always resolve.
mod escape_props {
    use super::super::args::{expand_dsl_vars, expand_string};
    use super::super::state::ExecState;
    use super::create_exec_state;
    use oxdock_fs::MockFs;
    use oxdock_parser::Value;
    use oxdock_process::MockProcessManager;
    use proptest::prelude::*;
    use std::sync::Arc;

    fn prop_state(
        envs: &[(String, String)],
        vars: &[(String, Value)],
    ) -> ExecState<MockProcessManager> {
        let mut state = create_exec_state(MockFs::new());
        for (k, v) in envs {
            Arc::make_mut(&mut state.envs).insert(k.clone(), v.clone());
        }
        for (k, v) in vars {
            // Declared type always matches the value's descriptor: the
            // property under test is expansion, not coercion.
            let kind = state
                .resolve_tag(v.type_name())
                .expect("startup types resolve");
            let _ = state.declare_var(k.clone(), kind, v.clone());
        }
        state
    }

    /// Shell `RUN` resolution order for a free-text argument: templates and
    /// backslash escapes first, bare `$var` second.
    fn shell_resolve(input: &str, state: &ExecState<MockProcessManager>) -> String {
        let expanded = expand_string(input, &state.envs, state).expect("expansion is infallible");
        expand_dsl_vars(&expanded, state)
    }

    proptest! {
        #[test]
        #[cfg_attr(miri, ignore = "proptest case loops are impractical under Miri isolation")]
        fn escaped_dollar_never_expands(
            name in "[a-z][a-zA-Z0-9_]{0,10}",
            value in "[a-zA-Z0-9 $\\{}/._-]{0,20}",
        ) {
            // Whatever the variable holds : even template-looking payloads :
            // `\$name` routes `$name` to the shell untouched.
            let state = prop_state(
                &[],
                &[(name.clone(), Value::string(value))],
            );
            prop_assert_eq!(shell_resolve(&format!("\\${name}"), &state), format!("${name}"));
        }

        #[test]
        #[cfg_attr(miri, ignore = "proptest case loops are impractical under Miri isolation")]
        fn escaped_template_never_interpolates(
            inner in "[a-zA-Z0-9 $\\_.,/:-]{0,24}",
            key in "[A-Z_]{1,8}",
            val in "[a-z0-9]{0,12}",
        ) {
            // `\{{ ... }}` (even wrapping real placeholder syntax, with tempting
            // environment values present) passes through byte-identical.
            let state = prop_state(
                &[(key, val)],
                &[],
            );
            prop_assert_eq!(
                shell_resolve(&format!("\\{{{{ {inner} }}}}"), &state),
                format!("{{{{ {inner} }}}}")
            );
        }

        #[test]
        #[cfg_attr(miri, ignore = "proptest case loops are impractical under Miri isolation")]
        fn env_template_always_interpolates(
            key in "[A-Z_]{1,8}",
            val in "[a-z0-9 ]{0,12}",
        ) {
            let state = prop_state(&[(key.clone(), val.clone())], &[]);
            prop_assert_eq!(shell_resolve(&format!("{{{{ env:{key} }}}}"), &state), val);
        }

        #[test]
        #[cfg_attr(miri, ignore = "proptest case loops are impractical under Miri isolation")]
        fn dollar_template_always_interpolates(
            name in "[a-z][a-zA-Z0-9_]{0,10}",
            val in "[a-z0-9 ]{0,12}",
        ) {
            let state = prop_state(
                &[],
                &[(name.clone(), Value::string(val.clone()))],
            );
            prop_assert_eq!(shell_resolve(&format!("{{{{ ${name} }}}}"), &state), val);
        }

        #[test]
        #[cfg_attr(miri, ignore = "proptest case loops are impractical under Miri isolation")]
        fn plain_text_passes_through_untouched(s in "[a-zA-Z0-9 .,!?/_:@=-]{0,30}") {
            // No `$`, `\`, or braces: both passes are the identity function.
            let state = prop_state(&[], &[]);
            prop_assert_eq!(shell_resolve(&s, &state), s);
        }

        #[test]
        #[cfg_attr(miri, ignore = "proptest case loops are impractical under Miri isolation")]
        fn remote_injection_literals_round_trip(
            raw in "[a-zA-Z0-9 .,!?/_:@=\\\"\n\r\t{} $-]{0,30}",
        ) {
            // The REMOTE header renderer must invert `expand_string`: a
            // hostile host string renders to a DSL literal that parses and
            // expands back byte identical on the guest.
            use oxdock_parser::render_dsl_literal;
            use oxdock_parser::{Expr, StepKind};
            let state = prop_state(&[], &[]);
            let literal = render_dsl_literal(&Value::string(raw.clone()))
                .expect("strings always render");
            let reparsed = crate::parse_script(&format!("LET $v: STRING = {literal}\n"))
                .expect("rendered literal parses");
            let payload = match &reparsed[0].kind {
                StepKind::Assign { expr, .. } => match expr {
                    Expr::Literal(value) => value.as_str().expect("string payload").to_string(),
                    other => panic!("expected literal, got {other:?}"),
                },
                other => panic!("expected Assign, got {other:?}"),
            };
            prop_assert_eq!(expand_string(&payload, &state.envs, &state).expect("expand"), raw);
        }
    }
}

// ---------------------------------------------------------------------------
// SpillBuffer (LET-capture sink) storage tiering tests
// ---------------------------------------------------------------------------

#[test]
#[cfg(not(miri))]
fn spill_buffer_stays_in_memory_below_threshold() {
    use super::capture::{SPILL_THRESHOLD, new_spill_buffer};
    use std::sync::Arc;

    let buf = Arc::new(new_spill_buffer());
    let writer = buf.writer();

    let payload = vec![0xABu8; 1_024]; // 1 KiB: below threshold
    writer.lock().unwrap().write_all(&payload).unwrap();
    assert!(!buf.is_spilled());
    assert_eq!(buf.drain_bytes().unwrap(), payload);
    let _ = SPILL_THRESHOLD; // Ensure constant is used
}

#[test]
#[cfg(not(miri))]
fn spill_buffer_spills_to_disk_above_threshold() {
    use super::capture::{SPILL_THRESHOLD, new_spill_buffer};
    use std::sync::Arc;

    let buf = Arc::new(new_spill_buffer());
    let writer = buf.writer();

    // Exceed the threshold by 1 MiB
    let size = SPILL_THRESHOLD + (1_024 * 1_024);
    let payload: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
    writer.lock().unwrap().write_all(&payload).unwrap();
    drop(writer);

    assert!(buf.is_spilled(), "buffer must have spilled to disk");
    assert_eq!(buf.drain_bytes().unwrap(), payload);
}

#[test]
#[cfg(not(miri))]
fn spill_buffer_backlog_cap_exceeded_returns_error() {
    use super::capture::{MAX_BACKLOG, SPILL_THRESHOLD, new_spill_buffer};
    use std::sync::Arc;

    let buf = Arc::new(new_spill_buffer());
    let writer = buf.writer();

    // First, trigger a spill to Disk mode by writing above the spill threshold
    let spill_payload = vec![0u8; SPILL_THRESHOLD + 1];
    writer.lock().unwrap().write_all(&spill_payload).unwrap();

    // Now write enough to exceed the backlog limit without reading
    let remaining = (MAX_BACKLOG as usize) - spill_payload.len() + 1;
    let overflow_payload = vec![0u8; remaining];
    let result = writer.lock().unwrap().write_all(&overflow_payload);

    assert!(
        result.is_err(),
        "Writing beyond MAX_BACKLOG must return an error"
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
#[allow(clippy::disallowed_methods)]
fn spill_buffer_file_truncated_on_drain_and_cleaned_on_drop() {
    use super::capture::{SPILL_THRESHOLD, new_spill_buffer};
    use std::fs;
    use std::sync::Arc;

    let buf = Arc::new(new_spill_buffer());
    let writer = buf.writer();

    let size = SPILL_THRESHOLD + (1_024 * 1_024);
    let payload = vec![0x55u8; size];
    writer.lock().unwrap().write_all(&payload).unwrap();
    drop(writer);

    let temp_path = buf
        .temp_path()
        .expect("buffer must have transitioned to disk");
    assert!(
        temp_path.exists(),
        "Temp file {} must exist on disk while buffered",
        temp_path.display()
    );

    assert_eq!(buf.drain_bytes().unwrap(), payload);
    let meta = fs::metadata(&temp_path).unwrap();
    assert_eq!(
        meta.len(),
        0,
        "Physical file length must be 0 after buffer drainage"
    );

    drop(buf);
    assert!(
        !temp_path.exists(),
        "Temp file must be deleted from disk upon Drop"
    );
}

#[test]
fn spill_buffer_drain_string_strict_round_trips_and_rejects_non_utf8() {
    use super::capture::new_spill_buffer;
    use std::sync::Arc;

    // Valid UTF-8 round-trips exactly (no stripping).
    let buf = Arc::new(new_spill_buffer());
    buf.writer().lock().unwrap().write_all(b"hi\n").unwrap();
    assert_eq!(buf.drain_string_strict().unwrap(), "hi\n");

    // Invalid UTF-8 is a strict error, never lossy.
    let buf = Arc::new(new_spill_buffer());
    buf.writer()
        .lock()
        .unwrap()
        .write_all(&[0x66, 0xff, 0xfe])
        .unwrap();
    let err = buf.drain_string_strict().expect_err("non-UTF8 must fail");
    assert!(err.to_string().contains("not valid UTF-8"), "{err}");
}

// ── Pipe spawn manifest & backend resolution ────────────────────────────
//
// Scripts under test stay `indoc` literals: no `format!`-built scripts,
// no `\n`-joined continuations, no ad-hoc substitution. Runners supply
// env bindings through the helpers below.

/// Run parsed steps on a mock filesystem with a watchdog: blocking tests
/// must fail on a hang, never freeze the suite. `env` seeds `{{ env:... }}`
/// lookups (fresh `ExecIo` is bypassed here, so entries go straight into
/// the run state, which is uniquely owned at this point).
fn run_mock_steps(
    steps: &[Step],
    env: Vec<(String, String)>,
) -> anyhow::Result<HashMap<String, Vec<u8>>> {
    let fs = MockFs::new();
    let mut state = create_exec_state(fs.clone());
    let state_envs = Arc::get_mut(&mut state.envs).expect("fresh state envs are owned");
    for (key, value) in env {
        state_envs.insert(key, value);
    }
    let mut proc = MockProcessManager::default();
    execute_steps(
        &mut state,
        &mut proc,
        steps,
        CommandStdin::Null,
        false,
        None,
        None,
        None,
        None,
        true,
    )?;
    Ok(fs.snapshot())
}

fn run_script_inner(
    steps: Vec<Step>,
    env: Vec<(String, String)>,
    limit: Duration,
) -> Result<HashMap<String, Vec<u8>>, String> {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(run_mock_steps(&steps, env));
    });
    match done_rx.recv_timeout(limit) {
        Ok(Ok(files)) => Ok(files),
        Ok(Err(err)) => Err(format!("{err:#}")),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            Err(format!("script did not complete within {limit:?}"))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err("script thread panicked".to_string())
        }
    }
}

fn run_script_with_timeout(
    steps: Vec<Step>,
    env: Vec<(String, String)>,
    limit: Duration,
) -> HashMap<String, Vec<u8>> {
    run_script_inner(steps, env, limit).unwrap_or_else(|err| panic!("{err}"))
}

/// Compiled everywhere; only called from Miri-ignored tests.
fn file_content(files: &HashMap<String, Vec<u8>>, name: &str) -> Vec<u8> {
    files
        .iter()
        .find(|(path, _)| path.ends_with(name))
        .map(|(_, bytes)| bytes.clone())
        .unwrap_or_else(|| panic!("expected file {name}, got {:?}", files.keys()))
}
#[test]
#[cfg_attr(
    miri,
    ignore = "spawns worker threads; thread scheduling under Miri isolation is nondeterministic"
)]
fn spawn_manifest_outer_pipe_stays_script() {
    // A task that READS an outer-declared pipe finds a script backend the
    // moment spawning returns: the spawn-time pin walk judges from the
    // worker's perspective, and the worker did not declare `$p`. Pinning
    // still runs synchronously in LET, so no worker scheduling is involved.
    // Script stdin has no kernel EOF: the main flow must produce a writer
    // (and close it by completing) so the mock's stdin snapshot finishes.
    // The command never executes (mock manager records it), so no platform
    // binary is required and the test is cross-platform.
    let steps = crate::parse_script(indoc! {r#"
        LET $p: PIPE
        LET $t: HANDLE = ASYNC {
          WITH_IO [stdin=$p] RUN ["stub-never-executed"]
        }
        WITH_IO [stdout=$p] ECHO "hello"
        AWAIT $t
        LET $info: MAP<ANY> = INSPECT($p)
        WRITE kind.txt "{{ $info.pipe_kind }}"
    "#})
    .expect("parse ok");
    let files = run_script_with_timeout(steps, vec![], Duration::from_secs(15));
    assert_eq!(file_content(&files, "kind.txt"), b"script");
}

#[test]
fn nested_spawn_marks_pipes_nested_in_collections_escaped() {
    // Pipes travel inside lists and maps: spawning a nested task must
    // mark them escaped through the collection. Here the pipe is
    // reachable ONLY through `$bag` (the producer's scope popped on
    // function return), so a shallow top-level scan sees no pipe at
    // spawn time; both spawner and child extract it afterwards. A
    // missed mark lets the spawner promote the shared backend to an OS
    // pair, and whichever side binds second then fails its take loudly.
    // Either order fails pre-fix; the shared script backend binds both
    // sides cleanly post-fix.
    let steps = crate::parse_script(indoc! {r#"
        FUNC MAKE() {
            LET $p: PIPE
            RETURN [$p]
        }
        LET $t1: HANDLE = ASYNC {
            LET $bag: LIST<PIPE> = MAKE()
            LET $t2: HANDLE = ASYNC {
                LET $q: PIPE = $bag.0
                WITH_IO [stdout=$q] ECHO "child"
            }
            LET $p2: PIPE = $bag.0
            WITH_IO [stdout=$p2] ECHO "spawner"
            AWAIT $t2
            LET $info: MAP<ANY> = INSPECT($p2)
            RETURN "{{ $info.pipe_kind }}"
        }
        LET $kind: STRING = AWAIT $t1
        WRITE kind.txt "{{ $kind }}"
    "#})
    .expect("parse ok");
    let files = run_script_with_timeout(steps, vec![], Duration::from_secs(15));
    assert_eq!(file_content(&files, "kind.txt"), b"script");
}

#[test]
fn bare_let_pipe_declarations_are_isolated_channels() {
    // Two bare declarations must never share a channel: the cross-read
    // below returns each pipe's own bytes. If both names aliased one
    // backend, FIFO order would surface "from-a" on the $b read instead.
    let steps = crate::parse_script(indoc! {r#"
        LET $a: PIPE
        LET $b: PIPE
        WITH_IO [stdout=$a] ECHO "from-a"
        WITH_IO [stdout=$b] ECHO "from-b"
        WITH_IO [stdin=$b] READ_LINE $second
        WITH_IO [stdin=$a] READ_LINE $first
        WRITE out.txt "{{ $first }}-{{ $second }}"
    "#})
    .expect("parse ok");
    let files = run_mock_steps(&steps, vec![]).expect("bare pipes round-trip");
    assert_eq!(file_content(&files, "out.txt"), b"from-a-from-b");
}

#[test]
fn bare_let_pipe_copies_share_one_backend() {
    // `LET $q: PIPE = $p` clones the handle: producer on `$p`, consumer
    // on `$q`, same channel (explicit-sharing fan-out per the plan).
    let steps = crate::parse_script(indoc! {r#"
        LET $p: PIPE
        LET $q: PIPE = $p
        WITH_IO [stdout=$p] ECHO "shared"
        WITH_IO [stdin=$q] READ_LINE $got
        WRITE out.txt "{{ $got }}"
    "#})
    .expect("parse ok");
    let files = run_mock_steps(&steps, vec![]).expect("aliased pipes round-trip");
    assert_eq!(file_content(&files, "out.txt"), b"shared");
}

#[test]
fn eof_false_while_buffered_true_once_drained() {
    // EOF means closed plus empty: buffered bytes answer false even
    // though no writer is alive, and the post-drain read answers true.
    let steps = crate::parse_script(indoc! {r#"
        IMPORT [STD]
        LET $p: PIPE
        WITH_IO [stdout=$p] ECHO "hi"
        LET $buffered: BOOL = EOF($p)
        IF $buffered {
            WRITE buffered.txt "eof-true"
        } ELSE {
            WRITE buffered.txt "eof-false"
        }
        WITH_IO [stdin=$p] READ_LINE $got
        LET $drained: BOOL = EOF($p)
        IF $drained {
            WRITE drained.txt "eof-true"
        } ELSE {
            WRITE drained.txt "eof-false"
        }
    "#})
    .expect("parse ok");
    let files = run_mock_steps(&steps, vec![]).expect("eof states read");
    assert_eq!(file_content(&files, "buffered.txt"), b"eof-false");
    assert_eq!(file_content(&files, "drained.txt"), b"eof-true");
}

#[test]
fn eof_false_with_live_writer_true_after_join() {
    // A task holding the write end answers false; once it exits and the
    // pipe closes, the same handle answers true.
    let steps = crate::parse_script(indoc! {r#"
        IMPORT [STD]
        LET $q: PIPE
        LET $t: HANDLE = ASYNC {
            WITH_IO [stdout=$q] SLEEP 5s
        }
        LET $live: BOOL = EOF($q)
        IF $live {
            WRITE live.txt "eof-true"
        } ELSE {
            WRITE live.txt "eof-false"
        }
        AWAIT $t
        LET $joined: BOOL = EOF($q)
        IF $joined {
            WRITE joined.txt "eof-true"
        } ELSE {
            WRITE joined.txt "eof-false"
        }
    "#})
    .expect("parse ok");
    let files = run_script_with_timeout(steps, vec![], Duration::from_secs(15));
    assert_eq!(file_content(&files, "live.txt"), b"eof-false");
    assert_eq!(file_content(&files, "joined.txt"), b"eof-true");
}

#[test]
fn eof_drain_loop_reads_to_end_without_sentinel() {
    // The documented shape: `WHILE !EOF($p)` drains buffered lines and
    // exits on close, with no sentinel line and no count baked in.
    let steps = crate::parse_script(indoc! {r#"
        IMPORT [STD]
        LET $d: PIPE
        WITH_IO [stdout=$d] {
            ECHO "one"
            ECHO "two"
        }
        LET $n: INT = 0
        WHILE !EOF($d) {
            WITH_IO [stdin=$d] READ_LINE $ln
            $n = $n + 1
        }
        WRITE count.txt "{{ $n }}"
    "#})
    .expect("parse ok");
    let files = run_mock_steps(&steps, vec![]).expect("eof drain loop runs");
    assert_eq!(file_content(&files, "count.txt"), b"2");
}

#[test]
fn eof_rejects_non_pipe() {
    // A non-pipe argument bails at the boundary extractor naming the
    // expected type, mirroring the other typed builtins. (The static
    // pass rejects it even earlier; see `static_pipe_param_*`.)
    let steps = crate::parse_script(indoc! {r#"
        IMPORT [STD]
        LET $bad: BOOL = EOF("nope")
    "#})
    .expect("parse ok");
    let err = run_mock_steps(&steps, vec![]).expect_err("non-pipe EOF must fail");
    assert!(
        format!("{err:#}").contains("must be a PIPE"),
        "boundary must name PIPE: {err:#}"
    );
}

#[test]
fn loop_body_declarations_mint_per_iteration() {
    // Per-iteration freshness comes free from declaration placement: a
    // `LET` inside the loop body mints an independent channel per turn
    // (fresh per-iteration scope), so no generation observes another's
    // bytes.
    let steps = crate::parse_script(indoc! {r#"
        LET $items: LIST<STRING> = ["a", "b"]
        FOR $x: STRING IN $items {
            LET $p: PIPE
            WITH_IO [stdout=$p] ECHO "{{ $x }}"
            WITH_IO [stdin=$p] READ_LINE $got
            WRITE "{{ $x }}.txt" "{{ $got }}"
        }
    "#})
    .expect("parse ok");
    let files = run_mock_steps(&steps, vec![]).expect("loop generations run");
    assert_eq!(file_content(&files, "a.txt"), b"a");
    assert_eq!(file_content(&files, "b.txt"), b"b");
}

#[test]
fn mixed_task_body_pins_script_and_run_adapts() {
    // Strict promotion: a RUN producer sharing a handle with a DSL
    // consumer pins script (not every binding is RUN-terminated). The RUN
    // side adapts through the shared path with existing feeder machinery;
    // repeat DSL access and host-facing reads all keep working there.
    let steps = crate::parse_script(indoc! {r#"
        LET $p: PIPE
        LET $t: HANDLE = ASYNC {
            WITH_IO [stdout=$p] RUN ["stub-never-executed"]
            WITH_IO [stdout=$p] ECHO "from-task"
        }
        WITH_IO [stdin=$p] READ_LINE $a
        AWAIT $t
        LET $info: MAP<ANY> = INSPECT($p)
        WRITE out.txt "{{ $a }}"
        WRITE kind.txt "{{ $info.pipe_kind }}"
    "#})
    .expect("parse ok");
    let files = run_script_with_timeout(steps, vec![], Duration::from_secs(15));
    assert_eq!(file_content(&files, "out.txt"), b"from-task");
    assert_eq!(file_content(&files, "kind.txt"), b"script");
}

#[test]
fn host_round_trip_through_adapters() {
    // Host functions move bytes with slice I/O on caller-owned buffers:
    // read DSL-produced bytes, write bytes the DSL reads, and return a
    // freshly minted handle the DSL can bind. Canonical reference shape
    // is `io::copy` between adapters (stack buffer, zero per-chunk
    // allocation); the minted handle is seeded through a second adapter.
    use std::io::Write;
    fn test_meta(name: &str) -> FuncMeta {
        FuncMeta {
            name: name.to_string(),
            module: "T".to_string(),
            kind: FuncKind::HostCtx,
            params: None,
            returns: Some(TypeTag::Pipe),
            rpn: false,
            summary: "test-only host pipe relay",
            docs: "test-only",
            examples: &[],
        }
    }
    let relay: NativeFn<MockProcessManager> = Arc::new(|cx, vals| {
        let mut src = PipeStream::reader(cx.pipe_reader(&vals[0])?);
        let mut dst = PipeStream::writer(cx.pipe_writer(&vals[1])?);
        std::io::copy(&mut src, &mut dst)?;
        dst.flush()?;
        let minted = cx.new_pipe();
        let mut seed = PipeStream::writer(cx.pipe_writer(&minted)?);
        seed.write_all(b"seed\n")?;
        seed.flush()?;
        Ok(minted)
    });
    let module = HostModule {
        name: "T".to_string(),
        funcs: vec![HostRegistration::Stateful {
            name: "RELAY".to_string(),
            meta: test_meta("RELAY"),
            func: relay,
        }],
        types: vec![],
        record_schemas: vec![],
        states: Vec::new(),
    };
    let table = oxdock_parser::ModuleTable {
        modules: std::collections::HashMap::from([(
            "T".to_string(),
            Some(oxdock_parser::ModuleFuncs {
                functions: std::collections::HashSet::from(["RELAY".to_string()]),
            }),
        )]),
    };
    let steps = crate::parse_script_with_modules(
        indoc! {r#"
        LET $in: PIPE
        LET $out: PIPE
        WITH_IO [stdout=$in] ECHO "hello host"
        LET $made: PIPE = T::RELAY($in, $out)
        WITH_IO [stdin=$out] READ_LINE $got
        WITH_IO [stdin=$made] READ_LINE $seed
        WRITE out.txt "{{ $got }}-{{ $seed }}"
    "#},
        table,
    )
    .expect("parse ok");
    let fs = MockFs::new();
    let mut state = create_exec_state(fs.clone());
    state.register_module(module);
    let mut proc = MockProcessManager::default();
    execute_steps(
        &mut state,
        &mut proc,
        &steps,
        CommandStdin::Null,
        false,
        None,
        None,
        None,
        None,
        true,
    )
    .expect("host round-trip runs");
    let files = fs.snapshot();
    assert_eq!(file_content(&files, "out.txt"), b"hello host-seed");
}

#[test]
fn expand_placeholder_calls_pure_host_function() {
    // `{{ T::SHOUT($who) }}` inside EXPAND input dispatches through the
    // registered pure function, sharing the registry DSL `CALL` uses.
    // The `\{{` escape keeps the placeholder literal through WRITE so
    // EXPAND is the stage that evaluates it.
    fn test_meta(name: &str) -> FuncMeta {
        FuncMeta {
            name: name.to_string(),
            module: "T".to_string(),
            kind: FuncKind::HostPure,
            params: None,
            returns: Some(TypeTag::String),
            rpn: false,
            summary: "test-only pure shout",
            docs: "test-only",
            examples: &[],
        }
    }
    let shout: PureFn = Arc::new(|vals| {
        let s = vals
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("SHOUT expects one string"))?;
        Ok(Value::string(s.to_uppercase()))
    });
    let module = HostModule {
        name: "T".to_string(),
        funcs: vec![HostRegistration::Pure {
            name: "SHOUT".to_string(),
            meta: test_meta("SHOUT"),
            func: shout,
        }],
        types: vec![],
        record_schemas: vec![],
        states: Vec::new(),
    };
    let table = oxdock_parser::ModuleTable {
        modules: std::collections::HashMap::from([(
            "T".to_string(),
            Some(oxdock_parser::ModuleFuncs {
                functions: std::collections::HashSet::from(["SHOUT".to_string()]),
            }),
        )]),
    };
    let steps = crate::parse_script_with_modules(
        indoc! {r#"
        LET $who: STRING = "bob"
        WRITE tmpl.txt "hi \{{ T::SHOUT($who) }}!"
        LET $buf: PIPE
        WITH_IO [stdout=$buf] EXPAND tmpl.txt
        WITH_IO [stdin=$buf] APPEND out.txt
    "#},
        table,
    )
    .expect("parse ok");
    let fs = MockFs::new();
    let mut state = create_exec_state(fs.clone());
    state.register_module(module);
    let mut proc = MockProcessManager::default();
    execute_steps(
        &mut state,
        &mut proc,
        &steps,
        CommandStdin::Null,
        false,
        None,
        None,
        None,
        None,
        true,
    )
    .expect("placeholder call runs");
    let files = fs.snapshot();
    assert_eq!(file_content(&files, "out.txt"), b"hi BOB!");
}

#[test]
fn parse_text_matches_file_loader_and_placeholder() {
    // In-memory parsing must produce the same values as file loading, and
    // the pure parser must also resolve through the placeholder registry.
    let steps = crate::parse_script(indoc! {r#"
        IMPORT [STD]
        WRITE seed.json "{\"a\": 1}"
        WRITE seed.toml "a = 1"
        LET $json_raw: STRING = READ seed.json
        LET $toml_raw: STRING = READ seed.toml
        LET $loaded_json: MAP<ANY> = LOAD_JSON("seed.json")
        LET $parsed_json: MAP<ANY> = PARSE_JSON($json_raw)
        LET $loaded_toml: MAP<ANY> = LOAD_TOML("seed.toml")
        LET $parsed_toml: MAP<ANY> = PARSE_TOML($toml_raw)
        ASSERT_EQ $parsed_json $loaded_json
        ASSERT_EQ $parsed_toml $loaded_toml
        LET $parity: STRING = TO_JSON($loaded_json)
        WRITE parity.txt "{{ $parity }}"
        WRITE tmpl.txt "parsed \{{ STD::PARSE_JSON($json_raw) }} and \{{ STD::PARSE_TOML($toml_raw) }}"
        LET $buf: PIPE
        WITH_IO [stdout=$buf] EXPAND tmpl.txt
        WITH_IO [stdin=$buf] APPEND expanded.txt
    "#})
    .expect("parse ok");
    let (_, files) = run_with_mock_fs(&steps);
    assert_eq!(file_content(&files, "parity.txt"), b"{\"a\":1}\n");
    assert_eq!(
        file_content(&files, "expanded.txt"),
        b"parsed \"a\": 1 and \"a\": 1"
    );
}

#[test]
fn resolve_tag_accepts_unknown_map_spelling() {
    // `MAP<ANY>` is the declared unknown map: it resolves to a value
    // shape, and spaced or nested forms canonicalize onto shared
    // entries like every other spelling.
    let fs = MockFs::new();
    let state = create_exec_state(fs);
    let tag = state.resolve_tag("MAP<ANY>").expect("MAP<ANY> resolves");
    assert!(matches!(tag, TypeTag::MapOf(_)));
    let nested = state
        .resolve_tag("LIST<MAP<ANY>>")
        .expect("nested resolves");
    assert!(matches!(nested, TypeTag::ListOf(_)));
}

#[test]
fn resolve_tag_rejects_bare_collections() {
    // Bare collections are runtime words, never declaration types:
    // every spelling position fails naming the explicit form.
    let fs = MockFs::new();
    let state = create_exec_state(fs);
    for bad in ["MAP", "LIST", "LIST<MAP>", "LIST<LIST>", "MAP<name: MAP>"] {
        let err = state.resolve_tag(bad).expect_err("bare form must fail");
        assert!(
            format!("{err:#}").contains("not a declaration type"),
            "{bad}: {err:#}"
        );
    }
}

#[test]
fn resolve_tag_accepts_optional_fields() {
    // A `?` suffix marks one field optional: it resolves into the
    // flag, and identical spellings share one interned entry.
    let fs = MockFs::new();
    let state = create_exec_state(fs);
    let tag = state
        .resolve_tag("MAP<held: BOOL, permit?: PERMIT>")
        .expect("optional field resolves");
    match &tag {
        TypeTag::Record(fields) => {
            assert_eq!(fields.len(), 2);
            assert!(!fields[0].optional);
            assert!(fields[1].optional);
            assert_eq!(fields[1].name.as_str(), "permit");
        }
        other => panic!("expected record, got {other:?}"),
    }
    let again = state
        .resolve_tag("MAP<held: BOOL,permit?: PERMIT>")
        .expect("resolves");
    match (&tag, &again) {
        (TypeTag::Record(a), TypeTag::Record(b)) => {
            assert!(std::sync::Arc::ptr_eq(a, b), "shared entry");
        }
        _ => panic!("expected records"),
    }
}

#[test]
fn resolve_tag_interns_identical_spellings_once() {
    // Identical canonical spellings share one interned pointer;
    // spaced forms canonicalize onto it, nesting included.
    let fs = MockFs::new();
    let state = create_exec_state(fs);
    let first = state.resolve_tag("LIST<MAP<ANY>>").expect("resolves");
    let second = state.resolve_tag("LIST< MAP< ANY > >").expect("resolves");
    match (first, second) {
        (TypeTag::ListOf(a), TypeTag::ListOf(b)) => {
            assert!(std::sync::Arc::ptr_eq(&a, &b), "shared interned entry");
        }
        other => panic!("expected shaped lists, got {other:?}"),
    }
}

#[test]
fn resolve_tag_rejects_generic_arity_violations() {
    // Arity violations parse (see tag.rs `spellings_parse_to_shapes`)
    // but fail here at resolution, naming the rule.
    let fs = MockFs::new();
    let mut state = create_exec_state(fs);
    let err = state
        .resolve_tag("LIST<MAP, STRING>")
        .expect_err("LIST with two args must fail");
    assert!(
        format!("{err:#}").contains("LIST takes exactly one bare type argument"),
        "{err:#}"
    );
    let err = state
        .resolve_tag("STRING<INT>")
        .expect_err("arguments on a scalar must fail");
    assert!(
        format!("{err:#}").contains("'STRING' takes no type arguments"),
        "{err:#}"
    );
    let err = state
        .resolve_tag("MAP<STRING>")
        .expect_err("bare MAP field must fail");
    assert!(
        format!("{err:#}").contains("MAP fields require 'name: TYPE'"),
        "{err:#}"
    );
    // Alias targets enforce the same rules: the violation rides
    // through the alias hop instead of escaping it.
    state
        .type_aliases
        .insert("BAD".to_string(), "LIST<MAP, STRING>".to_string());
    let err = state.resolve_tag("BAD").expect_err("alias arity must fail");
    assert!(
        format!("{err:#}").contains("LIST takes exactly one bare type argument"),
        "{err:#}"
    );
}

#[test]
fn resolve_tag_unions_admit_member_sets() {
    let fs = MockFs::new();
    let mut state = create_exec_state(fs);
    // Canonical spelling interns one shared instance.
    let first = state.resolve_tag("STRING|PIPE").expect("union resolves");
    let second = state
        .resolve_tag("STRING|PIPE")
        .expect("union resolves again");
    let TypeTag::Union(_, members) = &first else {
        panic!("union resolves to a union, got {first:?}");
    };
    // The spelling derives from the members: no hand-written string.
    assert_eq!(first.name(), "STRING|PIPE");
    assert_eq!(members.len(), 2);
    assert!(std::ptr::eq(&first, &second) || super::static_check::tags_equal(&first, &second));
    // Written order is preserved; behavior is order-free either way.
    let swapped = state.resolve_tag("PIPE|STRING").expect("order resolves");
    assert!(super::static_check::tags_equal(&first, &swapped));
    assert_eq!(swapped.name(), "PIPE|STRING");
    // Spacing canonicalizes away.
    let spaced = state
        .resolve_tag("  STRING | PIPE ")
        .expect("spacing resolves");
    assert!(super::static_check::tags_equal(&first, &spaced));
    // Lone survivors collapse: no union is minted.
    assert!(matches!(
        state.resolve_tag("STRING|STRING").expect("dedup"),
        TypeTag::String
    ));
    assert!(matches!(
        state.resolve_tag("STRING|ANY").expect("absorb"),
        TypeTag::Any
    ));
    // Shapes compose as members.
    let shaped = state
        .resolve_tag("LIST<STRING>|PIPE")
        .expect("shaped member");
    let TypeTag::Union(_, shaped_members) = shaped else {
        panic!("shaped union resolves to a union");
    };
    assert_eq!(shaped_members.len(), 2);
    // Four-way sets render in written order, `|`-joined.
    let wide = state
        .resolve_tag("STRING|PIPE|LIST<ANY>|MAP<ANY>")
        .expect("wide union resolves");
    assert_eq!(wide.name(), "STRING|PIPE|LIST<ANY>|MAP<ANY>");
    assert_eq!(
        oxdock_parser::render_structural(&wide),
        "STRING|PIPE|LIST<ANY>|MAP<ANY>"
    );
    // Bare LIST and MAP reject as members exactly like top level:
    // unions compose spellings, never relax them.
    let err = state
        .resolve_tag("STRING|LIST")
        .expect_err("bare LIST member fails");
    assert!(
        format!("{err:#}").contains("bare LIST is not a declaration type"),
        "{err:#}"
    );
    // Empty members and unknown names fail naming the rule.
    let err = state
        .resolve_tag("STRING|")
        .expect_err("empty member fails");
    assert!(
        format!("{err:#}").contains("invalid type spelling"),
        "{err:#}"
    );
    let err = state
        .resolve_tag("NOPE|STRING")
        .expect_err("unknown member fails");
    assert!(
        format!("{err:#}").contains("unknown type 'NOPE'"),
        "{err:#}"
    );
    // Unions compose inside generics: members resolve as siblings.
    let nested = state
        .resolve_tag("LIST<STRING|PIPE>")
        .expect("nested union resolves");
    assert_eq!(
        oxdock_parser::render_structural(&nested),
        "LIST<STRING|PIPE>"
    );
    let mapped = state
        .resolve_tag("MAP<name: STRING|PIPE>")
        .expect("union field resolves");
    assert_eq!(
        oxdock_parser::render_structural(&mapped),
        "MAP<name: STRING|PIPE>"
    );
    // Unions nest inside aliases through the shared path.
    state
        .type_aliases
        .insert("TEXTISH".to_string(), "STRING|PIPE".to_string());
    let aliased = state
        .resolve_tag("TEXTISH")
        .expect("alias to union resolves");
    assert!(super::static_check::tags_equal(&first, &aliased));
}

#[test]
fn union_declarations_coerce_like_members() {
    // Strict boundary: a string never parses into a numeric union
    // slot. The explicit `INT()` converter owns that parsing, so
    // `LET` into a union admits exactly what its members admit.
    let steps = crate::parse_script(indoc! {r#"
        LET $u: INT|FLOAT = 123
        ASSERT_EQ $u 123
        LET $v: FLOAT|STRING = 10
        WRITE out.txt "{{ $v }}"
    "#})
    .expect("parse ok");
    let (_, files) = run_with_mock_fs(&steps);
    assert_eq!(file_content(&files, "out.txt"), b"10");
}

#[test]
fn union_declaration_rejects_string_for_numeric_slot() {
    // `LET $u: INT|FLOAT = "123"` fails: strings parse only through
    // the explicit converter, never through coercion.
    let steps = crate::parse_script(indoc! {r#"
        LET $u: INT|FLOAT = "123"
    "#})
    .expect("parse ok");
    let fs: Box<dyn WorkspaceFs> = Box::new(MockFs::new());
    let err = run_expect_err(fs, &steps, MockProcessManager::default());
    assert!(
        format!("{err:#}").contains("expected INT|FLOAT"),
        "names the set: {err:#}"
    );
}

#[test]
fn int_params_reject_words_end_to_end() {
    // Integer slots are strict: whole floats and integer strings no
    // longer coerce into `INT` params. Callers convert explicitly
    // through `INT()`; the contract stays `INT`.
    let steps = crate::parse_script(indoc! {r#"
        IMPORT [STD]
        LET $i: INT = 2048
        LET $from_int: STRING = FORMAT_BYTE_COUNT($i)
        ASSERT_EQ $from_int "2KiB"
        LET $s: STRING = "2048"
        LET $from_string: STRING = FORMAT_BYTE_COUNT(INT($s))
        ASSERT_EQ $from_string "2KiB"
        LET $f: FLOAT = 2048.0
        LET $from_float: STRING = FORMAT_BYTE_COUNT(INT($f))
        ASSERT_EQ $from_float "2KiB"
        WRITE out.txt "done"
    "#})
    .expect("parse ok");
    let (_, files) = run_with_mock_fs(&steps);
    assert_eq!(file_content(&files, "out.txt"), b"done");
}

#[test]
fn int_params_reject_unconverted_words() {
    // A string or float at an `INT` param fails naming the slot:
    // no silent re-parse at the consumer.
    let steps = crate::parse_script(indoc! {r#"
        IMPORT [STD]
        LET $s: STRING = "2048"
        LET $x: STRING = FORMAT_BYTE_COUNT($s)
    "#})
    .expect("parse ok");
    let fs: Box<dyn WorkspaceFs> = Box::new(MockFs::new());
    let err = run_expect_err(fs, &steps, MockProcessManager::default());
    assert!(
        format!("{err:#}").contains("expected INT"),
        "names the slot: {err:#}"
    );
}

#[test]
fn union_nests_inside_generics() {
    // Unions compose inside every generic, record, and alias
    // position through the shared resolve path: spellings derive
    // from the members, and identical spellings intern once.
    use oxdock_parser::render_structural;
    let fs = MockFs::new();
    let state = create_exec_state(fs);
    for (spelling, rendered) in [
        ("LIST<INT|STRING>", "LIST<INT|STRING>"),
        ("MAP<k: STRING|PATH>", "MAP<k: STRING|PATH>"),
        ("LIST<MAP<name: STRING|INT>>", "LIST<MAP<name: STRING|INT>>"),
        (
            "MAP<field: INT|FLOAT|STRING>",
            "MAP<field: INT|FLOAT|STRING>",
        ),
        ("LIST<LIST<INT|BOOL>>", "LIST<LIST<INT|BOOL>>"),
        ("LIST<STRING>|MAP<ANY>", "LIST<STRING>|MAP<ANY>"),
    ] {
        let tag = state.resolve_tag(spelling).expect("union nest resolves");
        assert_eq!(render_structural(&tag), rendered, "{spelling}");
        let again = state.resolve_tag(spelling).expect("resolves");
        assert!(
            super::static_check::tags_equal(&tag, &again),
            "shared interned entry for {spelling}"
        );
    }
    // Alias-indirected unions compose too.
    let mut aliased = create_exec_state(MockFs::new());
    aliased
        .type_aliases
        .insert("U".to_string(), "INT|STRING".to_string());
    let tag = aliased.resolve_tag("LIST<U>").expect("alias leaf composes");
    assert_eq!(render_structural(&tag), "LIST<INT|STRING>");
    // Value shapes compose at the constructor level: a union member
    // inside `MAP<...>` renders structurally and checks element-wise.
    let valued = TypeTag::MapOf(std::sync::Arc::new(oxdock_parser::union_tag!(Int, String)));
    assert_eq!(render_structural(&valued), "MAP<INT|STRING>");
    oxdock_parser::check_value(
        &valued,
        &Value::map([("k".to_string(), Value::int(1))].into_iter().collect()),
    )
    .expect("member admits");
}

#[test]
fn union_generics_check_values_elementwise() {
    // `LET` into a union-in-generic admits members element-wise and
    // rejects the rest naming the full structural shape.
    let steps = crate::parse_script(indoc! {r#"
        LET $l: LIST<INT|STRING> = [1, "a"]
        ASSERT_EQ $l.0 1
        LET $m: MAP<name: STRING|INT> = {name: 7}
        ASSERT_EQ $m.name 7
        WRITE out.txt "done"
    "#})
    .expect("parse ok");
    let (_, files) = run_with_mock_fs(&steps);
    assert_eq!(file_content(&files, "out.txt"), b"done");
    let steps = crate::parse_script(indoc! {r#"
        LET $l: LIST<INT|STRING> = [1, true]
    "#})
    .expect("parse ok");
    let fs: Box<dyn WorkspaceFs> = Box::new(MockFs::new());
    let err = run_expect_err(fs, &steps, MockProcessManager::default());
    assert!(
        format!("{err:#}").contains("expected INT|STRING"),
        "names the member set: {err:#}"
    );
}

#[test]
fn builtin_union_signatures_are_honest() {
    // Converters widen through `#[union]`; consumers stay strict.
    // Metadata and extractor agree by construction, so the signature
    // is the check.
    use oxdock_parser::render_structural;
    let metas = super::native::builtin_function_metas();
    let signature = |name: &str| {
        metas
            .iter()
            .find(|meta| meta.name == name)
            .unwrap_or_else(|| panic!("builtin {name} exists"))
            .params
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|param| {
                param
                    .param_type
                    .as_ref()
                    .map(render_structural)
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    assert_eq!(signature("STD::INT"), "INT|FLOAT|STRING|BOOL");
    assert_eq!(signature("STD::FLOAT"), "FLOAT|INT|STRING");
    assert_eq!(signature("STD::FORMAT_BYTE_COUNT"), "INT");
    assert_eq!(signature("STD::GLOB"), "STRING|PATH");
    assert_eq!(signature("STD::TO_JSON"), "STRING|INT|FLOAT|BOOL|LIST|MAP");
}

#[test]
fn converters_accept_words_end_to_end() {
    // Explicit conversion is the one place strings parse: the
    // converter union admits the word, the body parses it.
    let steps = crate::parse_script(indoc! {r#"
        IMPORT [STD]
        LET $i: INT = INT("12")
        ASSERT_EQ $i 12
        LET $whole: INT = INT(12.0)
        ASSERT_EQ $whole 12
        LET $yes: INT = INT(true)
        ASSERT_EQ $yes 1
        LET $no: INT = INT(false)
        ASSERT_EQ $no 0
        LET $f: FLOAT = FLOAT("2.5")
        ASSERT_EQ $f 2.5
        LET $pretty: STRING = FORMAT_BYTE_COUNT(INT("2048"))
        ASSERT_EQ $pretty "2KiB"
        LET $doc: STRING = TO_JSON([1, "a"])
        ASSERT_CONTAINS $doc "a"
        WRITE out.txt "done"
    "#})
    .expect("parse ok");
    let (_, files) = run_with_mock_fs(&steps);
    assert_eq!(file_content(&files, "out.txt"), b"done");
}

#[test]
fn host_return_slot_rejects_lying_body() {
    // A host body returning a word outside its declared slot fails
    // at the call boundary naming the function: no coercion rescues
    // an implementation bug.
    fn lying_module() -> HostModule<MockProcessManager> {
        let lying: PureFn = Arc::new(|_| Ok(Value::string("x".to_string())));
        HostModule {
            name: "T".to_string(),
            funcs: vec![HostRegistration::Pure {
                name: "LIAR".to_string(),
                meta: FuncMeta {
                    name: "T::LIAR".to_string(),
                    module: "T".to_string(),
                    kind: FuncKind::HostPure,
                    params: None,
                    returns: Some(TypeTag::Int),
                    rpn: false,
                    summary: "test-only",
                    docs: "test-only",
                    examples: &[],
                },
                func: lying,
            }],
            types: vec![],
            record_schemas: vec![],
            states: Vec::new(),
        }
    }
    let mut engine = super::Engine::<MockProcessManager>::new_custom();
    engine.register_module(lying_module());
    let fs: Box<dyn WorkspaceFs> = Box::new(MockFs::new());
    let err = engine
        .run_script_on(
            fs,
            "IMPORT [STD, T]\nLET $v: ANY = LIAR()\n",
            MockProcessManager::default(),
        )
        .expect_err("lying return must fail");
    let text = format!("{err:#}");
    assert!(text.contains("LIAR"), "names the function: {text}");
    assert!(text.contains("expected INT"), "names the slot: {text}");
}

#[test]
fn func_loop_only_return_fails() {
    // Loops may trip zero times, so a `RETURN` inside a loop body
    // never satisfies the contract: the trailing path must return.
    let steps = crate::parse_script(indoc! {r#"
        FUNC F($xs: LIST<INT>) {
            FOR $x: INT IN $xs {
                RETURN $x
            }
        }
    "#})
    .expect("parse ok");
    let fs: Box<dyn WorkspaceFs> = Box::new(MockFs::new());
    let err = run_expect_err(fs, &steps, MockProcessManager::default());
    assert!(
        format!("{err:#}").contains("no RETURN on some path"),
        "loop body cannot close: {err:#}"
    );
}

#[test]
fn func_branch_missing_return_fails() {
    // An `ELSE`-less `IF` leaves the fallthrough path open: a
    // `RETURN` on the taken branch alone fails validation.
    let steps = crate::parse_script(indoc! {r#"
        FUNC F($flag: BOOL) {
            IF $flag {
                RETURN 1
            }
        }
    "#})
    .expect("parse ok");
    let fs: Box<dyn WorkspaceFs> = Box::new(MockFs::new());
    let err = run_expect_err(fs, &steps, MockProcessManager::default());
    assert!(
        format!("{err:#}").contains("no RETURN on some path"),
        "open branch fails: {err:#}"
    );
}

#[test]
fn func_trailing_return_after_loop_passes() {
    // The same loop with a trailing `RETURN` closes every path and
    // runs end to end.
    let steps = crate::parse_script(indoc! {r#"
        FUNC FIRST($xs: LIST<INT>) {
            FOR $x: INT IN $xs {
                RETURN $x
            }
            RETURN 0
        }
        LET $v: INT = FIRST([1, 2])
        ASSERT_EQ $v 1
        WRITE out.txt "done"
    "#})
    .expect("parse ok");
    let (_, files) = run_with_mock_fs(&steps);
    assert_eq!(file_content(&files, "out.txt"), b"done");
}

#[test]
fn describe_spells_union_sets() {
    // Introspection renders the same admission sets the boundary
    // enforces: converters name their inputs, path funcs name
    // `STRING|PATH`, strict consumers name `INT`.
    let steps = crate::parse_script(indoc! {r#"
        IMPORT [STD]
        LET $int: MAP<ANY> = DESCRIBE("STD::INT")
        ASSERT_EQ $int.params.0.param_type "INT|FLOAT|STRING|BOOL"
        LET $glob: MAP<ANY> = DESCRIBE("STD::GLOB")
        ASSERT_EQ $glob.params.0.param_type "STRING|PATH"
        LET $fmt: MAP<ANY> = DESCRIBE("STD::FORMAT_BYTE_COUNT")
        ASSERT_EQ $fmt.params.0.param_type "INT"
        WRITE out.txt "done"
    "#})
    .expect("parse ok");
    let (_, files) = run_with_mock_fs(&steps);
    assert_eq!(file_content(&files, "out.txt"), b"done");
}

#[test]
fn func_bare_return_fails_non_string_slot() {
    // A bare `RETURN` yields the empty string: it type-checks only
    // against a `STRING` slot and fails every other contract.
    let steps = crate::parse_script(indoc! {r#"
        FUNC F($flag: BOOL) {
            IF $flag {
                RETURN 1
            }
            RETURN
        }
        LET $v: INT = F(true)
    "#})
    .expect("parse ok");
    let fs: Box<dyn WorkspaceFs> = Box::new(MockFs::new());
    let err = run_expect_err(fs, &steps, MockProcessManager::default());
    assert!(
        format!("{err:#}").contains("cannot unify"),
        "bare return mismatches INT: {err:#}"
    );
}

#[test]
fn resolve_tag_composes_schema_and_alias_leaves() {
    use oxdock_parser::Field;
    let fs = MockFs::new();
    let mut state = create_exec_state(fs);
    let schema_fields: std::sync::Arc<[Field]> = std::sync::Arc::from([Field {
        name: "name".to_string(),
        ty: TypeTag::String,
        docs: "",
        optional: false,
    }]);
    state.register_record_schema("EMP", schema_fields);
    state
        .type_aliases
        .insert("PERSON".to_string(), "MAP<name: STRING>".to_string());
    let tag = state
        .resolve_tag("LIST<PERSON>")
        .expect("alias leaf composes");
    assert!(matches!(tag, TypeTag::ListOf(_)));
    let tag = state
        .resolve_tag("LIST<EMP>")
        .expect("schema leaf composes");
    assert!(matches!(tag, TypeTag::ListOf(_)));
    let err = state
        .resolve_tag("LIST<MISSING>")
        .expect_err("unknown leaf must fail");
    assert!(
        format!("{err:#}").contains("unknown type 'MISSING'"),
        "{err:#}"
    );
}

#[test]
fn resolve_tag_rejects_deep_alias_chains() {
    // Alias hops share the syntactic depth budget: a chain longer
    // than the cap bails instead of recursing.
    let fs = MockFs::new();
    let mut state = create_exec_state(fs);
    for n in 0..12 {
        state.type_aliases.insert(
            format!("A{n}"),
            if n == 11 {
                "MAP".to_string()
            } else {
                format!("LIST<A{}>", n + 1)
            },
        );
    }
    let err = state.resolve_tag("A0").expect_err("deep chain must fail");
    assert!(
        format!("{err:#}").contains("maximum generic depth"),
        "{err:#}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "adversarial scaling probe (200 LETs plus 80 nested IFs through parse and the full static pass); interpreter slowdown stalls it for hours with no unsafe code exercised"
)]
fn static_pass_scales_linearly_on_adversarial_input() {
    // Deterministic linearity enforcement (no wall-time bench):
    // visits must stay proportional to input size on deep nesting,
    // wide sequences, loops, math, and nested functions. A fixpoint
    // or exponential blowup trips this like any regression.
    let mut script = String::from("IMPORT [STD]\nFUNC DEEP($x: INT) {\n");
    for index in 0..200 {
        script.push_str(&format!("    LET $v{index}: INT = $x + {index}\n"));
    }
    for depth in 0..80 {
        script.push_str(&format!(
            "    IF $v0 == {depth} {{\n        RETURN $v0\n    }}\n"
        ));
    }
    script.push_str("    FOR $i: INT IN [1, 2, 3] {\n        LET $w: INT = $i * 2\n    }\n");
    script.push_str("    FUNC INNER($y: INT) {\n        RETURN $y\n    }\n");
    script.push_str("    RETURN $v0\n}\n");
    let steps = crate::parse_script(&script).expect("adversarial script parses");
    let fs = MockFs::new();
    let state = create_exec_state(fs);
    let visits = super::static_check::validate_script_types_counted(&steps, &state)
        .expect("deep but well-formed script passes");
    assert!(
        visits <= script.len() as u64,
        "visits {visits} exceed input bytes {}",
        script.len()
    );
}

#[test]
fn merge_maps_rejects_unknown_policy_at_boundary() {
    // Closed-set enforcement lives in the generated extractor: an
    // unknown policy fails before the body runs, naming the allowed
    // values from the same attribute tokens that render the
    // signature. Valid policies run end to end through the docs-gen
    // render suite on every pipeline run.
    let steps = crate::parse_script(indoc! {r#"
        IMPORT [STD]
        LET $m: MAP<ANY> = MERGE_MAPS([{a: 1}], "merge")
    "#})
    .expect("parse ok");
    let fs = MockFs::new();
    let err = run_expect_err(Box::new(fs), &steps, MockProcessManager::default());
    let text = format!("{err:#}");
    assert!(
        text.contains("must be one of"),
        "boundary names the rule: {text}"
    );
    assert!(
        text.contains("fail_on_duplicate") && text.contains("overwrite"),
        "boundary lists the allowed values: {text}"
    );

    // The LIST shape is boundary-checked too: a non-list never
    // reaches the merge.
    let steps = crate::parse_script(indoc! {r#"
        IMPORT [STD]
        LET $m: MAP<ANY> = MERGE_MAPS("nope", "overwrite")
    "#})
    .expect("parse ok");
    let fs = MockFs::new();
    let err = run_expect_err(Box::new(fs), &steps, MockProcessManager::default());
    assert!(
        format!("{err:#}").contains("expected LIST"),
        "boundary enforces the LIST shape: {err:#}"
    );

    // Element shapes walk the same boundary: gating a heterogeneous
    // list through `LET` reaches the runtime walk, which names the
    // offending position through the one shape implementation.
    let steps = crate::parse_script(indoc! {r#"
        IMPORT [STD]
        LET $ms: LIST<MAP<ANY>> = [{a: 1}, "nope"]
        LET $m: MAP<ANY> = MERGE_MAPS($ms, "overwrite")
    "#})
    .expect("parse ok");
    let fs = MockFs::new();
    let err = run_expect_err(Box::new(fs), &steps, MockProcessManager::default());
    let text = format!("{err:#}");
    assert!(
        text.contains("[1]") && text.contains("expected MAP"),
        "boundary names the element position and shape: {text}"
    );
}

#[test]
fn check_options_enforces_present_values() {
    // Declared option values check through the single shape path:
    // unknown keys fail as before, present wrong-typed values fail
    // naming func, key, expected, and actual, and absent keys pass
    // for downstream defaults.
    let options = [
        ParamOption {
            name: "timeout",
            value: oxdock_parser::union_tag!(Duration, String),
            required: false,
            default: None,
        },
        ParamOption {
            name: "retries",
            value: TypeTag::Int,
            required: false,
            default: None,
        },
    ];
    crate::check_options(&std::collections::BTreeMap::new(), &options, "F").expect("absent passes");
    let mut bad: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    bad.insert("retries".to_string(), Value::string("3".to_string()));
    let err = crate::check_options(&bad, &options, "F").expect_err("must fail");
    assert!(
        format!("{err:#}").contains("option 'retries': expected INT, got STRING"),
        "names func, key, and shape: {err:#}"
    );
    let mut ok: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    ok.insert("timeout".to_string(), Value::string("10s".to_string()));
    crate::check_options(&ok, &options, "F").expect("union member passes");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "blind sampling draws OS randomness through unseeded RAND, outside Miri isolation"
)]
fn returns_gate_covers_std_builtins() {
    // Every builtin honors its declared `returns`: sampled calls that
    // reach `Ok` satisfy the slot, and anything sampling cannot reach
    // must carry executable examples. A lying slot fails here, never
    // in generated docs.
    let proc = MockProcessManager::default();
    super::assert_module_returns_conformance(
        "STD",
        &super::native::FunctionRegistry::<MockProcessManager>::builtin_registrations(),
        Vec::new(),
        proc,
    );
}

#[test]
fn run_start_rejects_unknown_type_tags() {
    // Typo'd `returns` and `params` labels fail before the first step
    // runs, listing every known type. Previously a bad `returns` label
    // lived forever as a lie in DESCRIBE output and generated docs:
    // nothing read it but introspection.
    /// Opaque handle minted here and never registered.
    #[oxdock_func_macro::oxdock_type(name = "TEST_UNREGISTERED")]
    #[derive(Debug, Clone, PartialEq)]
    struct UnregisteredTag(String);
    impl std::fmt::Display for UnregisteredTag {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "unregistered:{}", self.0)
        }
    }
    fn bad_module(
        return_type: Option<TypeTag>,
        param_type: Option<TypeTag>,
    ) -> HostModule<MockProcessManager> {
        let noop: PureFn = Arc::new(|_| Ok(Value::string("x".to_string())));
        HostModule {
            name: "T".to_string(),
            funcs: vec![HostRegistration::Pure {
                name: "BAD".to_string(),
                meta: FuncMeta {
                    name: "T::BAD".to_string(),
                    module: "T".to_string(),
                    kind: FuncKind::HostPure,
                    params: Some(vec![FuncParam {
                        name: "arg".to_string(),
                        param_type,
                        allowed: None,
                        docs: "",
                        options: None,
                        optional: false,
                    }]),
                    returns: return_type,
                    rpn: false,
                    summary: "test-only",
                    docs: "test-only",
                    examples: &[],
                },
                func: noop,
            }],
            types: vec![],
            record_schemas: vec![],
            states: Vec::new(),
        }
    }
    // A custom descriptor whose name was never registered: tags make the
    // reference well-typed in Rust, but the run directory cannot resolve
    // it, so run-start validation fails naming the function and type.
    let unregistered = TypeTag::Custom(UnregisteredTag::descriptor());
    let mut state = create_exec_state(MockFs::new());
    state.register_module(bad_module(
        Some(unregistered.clone()),
        Some(TypeTag::String),
    ));
    let err = state
        .validate_function_type_tags()
        .expect_err("unknown return must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("T::BAD") && msg.contains("TEST_UNREGISTERED"),
        "unexpected: {msg}"
    );
    assert!(msg.contains("STRING"), "must list known types: {msg}");

    let mut state = create_exec_state(MockFs::new());
    state.register_module(bad_module(Some(TypeTag::String), Some(unregistered)));
    let err = state
        .validate_function_type_tags()
        .expect_err("unknown param must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("TEST_UNREGISTERED") && msg.contains("arg"),
        "unexpected: {msg}"
    );

    let mut state = create_exec_state(MockFs::new());
    state.register_module(bad_module(Some(TypeTag::String), Some(TypeTag::Map)));
    state
        .validate_function_type_tags()
        .expect("valid tags must pass");
}

#[test]
fn run_start_rejects_misplaced_optional_params() {
    // Omission fills an empty MAP, so only trailing MAP params may be
    // optional: a required param after an optional one, or an optional
    // non-MAP, fails run-start validation naming the offender.
    fn optional_module(params: Vec<(&str, TypeTag, bool)>) -> HostModule<MockProcessManager> {
        let noop: PureFn = Arc::new(|_| Ok(Value::string("x".to_string())));
        HostModule {
            name: "T".to_string(),
            funcs: vec![HostRegistration::Pure {
                name: "OPT".to_string(),
                meta: FuncMeta {
                    name: "T::OPT".to_string(),
                    module: "T".to_string(),
                    kind: FuncKind::HostPure,
                    params: Some(
                        params
                            .into_iter()
                            .map(|(name, tag, optional)| FuncParam {
                                name: name.to_string(),
                                param_type: Some(tag),
                                allowed: None,
                                docs: "",
                                options: None,
                                optional,
                            })
                            .collect(),
                    ),
                    returns: Some(TypeTag::String),
                    rpn: false,
                    summary: "test-only",
                    docs: "test-only",
                    examples: &[],
                },
                func: noop,
            }],
            types: vec![],
            record_schemas: vec![],
            states: Vec::new(),
        }
    }
    // Optional MAP before a required param: rejected.
    let mut state = create_exec_state(MockFs::new());
    state.register_module(optional_module(vec![
        ("first", TypeTag::Map, true),
        ("second", TypeTag::String, false),
    ]));
    let err = state
        .validate_function_type_tags()
        .expect_err("non-trailing optional must fail");
    assert!(
        format!("{err:#}").contains("optional before a required param"),
        "{err:#}"
    );
    // Optional non-MAP in trailing position: rejected.
    let mut state = create_exec_state(MockFs::new());
    state.register_module(optional_module(vec![("only", TypeTag::String, true)]));
    let err = state
        .validate_function_type_tags()
        .expect_err("optional non-MAP must fail");
    assert!(
        format!("{err:#}").contains("optional without a MAP type"),
        "{err:#}"
    );
    // Trailing optional MAP: accepted.
    let mut state = create_exec_state(MockFs::new());
    state.register_module(optional_module(vec![
        ("url", TypeTag::String, false),
        ("options", TypeTag::Map, true),
    ]));
    state
        .validate_function_type_tags()
        .expect("trailing optional MAP must pass");
}

#[cfg(not(miri))]
#[test]
fn os_rebind_take_twice_bails_with_fresh_declaration_remedy() {
    // No rebind rule exists by design: a second take on one OS end bails
    // loudly naming the remedy (fresh `LET`), never a silent fresh pair.
    // OS needs a task-local declaration (the worker promotes only pipes it
    // declared itself). `RUN` takes defer to spawn conversion (the mock
    // never spawns), so the consuming takes come from DSL bindings, which
    // take at resolve time: the first bridged take wins deterministically
    // and the second fails with the remedy.
    let steps = crate::parse_script(indoc! {r#"
        LET $t1: HANDLE = ASYNC {
            LET $p: PIPE
            WITH_IO [stdout=$p] RUN ["stub-producer"]
            WITH_IO [stdin=$p] ECHO "first"
            WITH_IO [stdin=$p] ECHO "second"
        }
        AWAIT $t1
    "#})
    .expect("parse ok");
    let err = run_mock_steps(&steps, vec![]).expect_err("second take must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("already been consumed"),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("fresh LET $x: PIPE"),
        "error must name the remedy: {msg}"
    );
}

#[cfg(not(miri))]
#[test]
fn host_take_twice_on_os_bails() {
    // Take-once applies to host bridged takes exactly like DSL consumers:
    // the first `pipe_reader` takes, the repeat bails with the same
    // remedy. The host surfaces the message as a string so the script
    // asserts it without depending on error-wrapping paths. The pipe is
    // task-local (declared inside the worker, the only shape that
    // promotes to OS) and travels out through an explicit RETURN.
    fn test_meta(name: &str) -> FuncMeta {
        FuncMeta {
            name: name.to_string(),
            module: "T".to_string(),
            kind: FuncKind::HostCtx,
            params: None,
            returns: Some(TypeTag::String),
            rpn: false,
            summary: "test-only host take-twice probe",
            docs: "test-only",
            examples: &[],
        }
    }
    let probe: NativeFn<MockProcessManager> = Arc::new(|cx, vals| {
        let _first = cx.pipe_reader(&vals[0])?;
        match cx.pipe_reader(&vals[0]) {
            Ok(_) => anyhow::bail!("second take must fail"),
            Err(err) => Ok(Value::string(format!("{err:#}"))),
        }
    });
    let module = HostModule {
        name: "T".to_string(),
        funcs: vec![HostRegistration::Stateful {
            name: "TAKE_TWICE".to_string(),
            meta: test_meta("TAKE_TWICE"),
            func: probe,
        }],
        types: vec![],
        record_schemas: vec![],
        states: Vec::new(),
    };
    let table = oxdock_parser::ModuleTable {
        modules: std::collections::HashMap::from([(
            "T".to_string(),
            Some(oxdock_parser::ModuleFuncs {
                functions: std::collections::HashSet::from(["TAKE_TWICE".to_string()]),
            }),
        )]),
    };
    let steps = crate::parse_script_with_modules(
        indoc! {r#"
        LET $t1: HANDLE = ASYNC {
            LET $p: PIPE
            WITH_IO [stdout=$p] RUN ["stub"]
            RETURN $p
        }
        LET $o: PIPE = AWAIT $t1
        LET $report: STRING = T::TAKE_TWICE($o)
        WRITE out.txt "{{ $report }}"
    "#},
        table,
    )
    .expect("parse ok");
    let fs = MockFs::new();
    let mut state = create_exec_state(fs.clone());
    state.register_module(module);
    let mut proc = MockProcessManager::default();
    execute_steps(
        &mut state,
        &mut proc,
        &steps,
        CommandStdin::Null,
        false,
        None,
        None,
        None,
        None,
        true,
    )
    .expect("host take-twice probe runs");
    let files = fs.snapshot();
    let report = String::from_utf8(file_content(&files, "out.txt")).expect("utf8");
    assert!(
        report.contains("already been consumed"),
        "unexpected report: {report}"
    );
    assert!(
        report.contains("fresh LET $x: PIPE"),
        "report must name the remedy: {report}"
    );
}

#[test]
fn bare_let_pipe_mints_distinct_unspellable_names() {
    // Transitional name backing: every bare declaration mints a key no
    // `pipe:` literal can spell (it contains a space), so anonymous
    // backends never collide with user-named pipes.
    let steps = crate::parse_script(indoc! {r#"
        LET $a: PIPE
        LET $b: PIPE
    "#})
    .expect("parse ok");
    let fs = MockFs::new();
    let mut state = create_exec_state(fs.clone());
    let mut proc = MockProcessManager::default();
    execute_steps(
        &mut state,
        &mut proc,
        &steps,
        CommandStdin::Null,
        false,
        None,
        None,
        None,
        None,
        true,
    )
    .expect("bare declarations run");
    let handle_a = state
        .get_var("a")
        .expect("var a")
        .as_pipe_handle()
        .expect("pipe a");
    let handle_b = state
        .get_var("b")
        .expect("var b")
        .as_pipe_handle()
        .expect("pipe b");
    assert!(
        !handle_a.ptr_eq(&handle_b),
        "bare declarations must mint distinct backends"
    );
    // Handles are opaque cells, not names: nothing textual can alias them.
    for handle in [&handle_a, &handle_b] {
        let info = state.io.inspect_pipe(handle);
        assert_eq!(
            info.kind.as_str(),
            "unbound",
            "never-bound handles inspect as unbound"
        );
    }
}

#[test]
fn stdin_resolve_carries_script_backend() {
    use oxdock_parser::Value;
    let io = ExecIo::new();
    let handle = Value::pipe_fresh().as_pipe_handle().expect("fresh handle");
    io.ensure_handle(&handle, false).expect("ensure");
    let (stdin, backend) = io.resolve_stdin(0, &handle, false, false).expect("resolve");
    assert!(matches!(stdin, CommandStdin::Stream(_)));
    assert!(
        backend.is_some(),
        "script stdin resolve carries the backend for timeout-bounded bridge reads"
    );
}

#[test]
fn force_close_eofs_despite_writer_and_keeper() {
    use oxdock_parser::Value;
    let io = ExecIo::new();
    let handle = Value::pipe_fresh().as_pipe_handle().expect("fresh handle");
    io.ensure_handle(&handle, false).expect("ensure");
    // Attach a live writer and pin a keeper: without force_close neither
    // lets readers observe EOF.
    let (stdin, _) = io.resolve_stdin(0, &handle, false, false).expect("resolve");
    let CommandStdin::Stream(reader) = stdin else {
        panic!("expected stream stdin");
    };
    let (stdout, backend) = io
        .resolve_stdout(0, &handle, false, false)
        .expect("resolve");
    // The OS-writer variant is compiled out under Miri, making the
    // destructure irrefutable there (a match would trip single-match).
    #[cfg(not(miri))]
    let super::io::StreamHandle::Stream(writer) = stdout else {
        panic!("expected stream stdout")
    };
    #[cfg(miri)]
    let super::io::StreamHandle::Stream(writer) = stdout;
    let _keeper = io.pin_keeper(&handle).expect("pin").expect("keeper");
    let backend = backend.expect("script backend");
    backend.force_close();
    let mut buf = [0u8; 8];
    let n = reader
        .lock()
        .expect("lock")
        .read(&mut buf)
        .expect("read after force_close");
    assert_eq!(n, 0, "closed pipe reads EOF with writer and keeper live");
    drop(writer);
}

#[test]
#[cfg_attr(
    miri,
    ignore = "spawns real OS processes and kernel pipes, which die under Miri isolation"
)]
fn script_pipe_fresh_handles_across_sessions() {
    // Two sequential sessions sharing an outer-declared pipe with a RUN
    // task: the pipe stays script (outer scope never promotes), so the
    // producer's bytes land in the shared backend and the main-flow read
    // observes them. The second session mints a fresh handle, so it never
    // observes the first session's consumed descriptors. No timing
    // involved: AWAIT joins each session before the next begins.
    // (Handle identity replaces name-based recycling: rebinding a spent
    // handle is a loud error, so per-session freshness comes from
    // declaring a fresh `LET` per run.)
    use oxdock_fs::PathResolver;
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let steps = crate::parse_script(indoc! {r#"
        LET $x: PIPE
        WITH_IO [stdout=$x] ASYNC RUN "echo one"
        WITH_IO [stdin=$x] WRITE f1.txt
    "#})
    .expect("parse ok");
    let io = ExecIo::new();
    for _ in 0..2 {
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");
        let fs: Box<dyn WorkspaceFs> = Box::new(resolver);
        run_steps_with_manager(
            fs,
            &steps,
            oxdock_process::default_process_manager(),
            io.clone(),
        )
        .unwrap_or_else(|err| panic!("session runs: {err:#}"));
        let check = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
        let content = check
            .read_to_string(&root.join("f1.txt").expect("join"))
            .expect("read file");
        // `cmd` emits CRLF; the proof is handle freshness, not line endings.
        assert_eq!(content.replace("\r\n", "\n"), "one\n");
    }
}

/// Run a script with the recording mock over a fresh tempdir root,
/// returning the tempdir (kept alive) and the mock for env assertions.
/// Recorded envs are exactly the script env each spawn would receive.
fn run_isolated_case(script: &str) -> (oxdock_fs::GuardedTempDir, MockProcessManager) {
    let steps = crate::parse_script(script).expect("isolated case parses");
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver"));
    let mock = MockProcessManager::default();
    run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new())
        .unwrap_or_else(|err| panic!("isolated case runs: {err:#}"));
    (temp, mock)
}

#[test]
fn isolated_scrubs_outer_env_for_children() {
    let (_temp, mock) = run_isolated_case(indoc! {r#"
        ENV OUTER=topsecret
        ISOLATED {
            RUN "echo hi"
        }
    "#});
    let runs = mock.recorded_runs();
    assert_eq!(runs.len(), 1);
    assert!(
        !runs[0].envs.contains_key("OUTER"),
        "scrubbed name must not reach children: {:?}",
        runs[0].envs
    );
}

#[test]
fn isolated_allowlist_reads_outer_scope() {
    let (_temp, mock) = run_isolated_case(indoc! {r#"
        ENV KEEP=yes
        ENV DROP=no
        ISOLATED {
            INHERIT_ENV [KEEP]
            RUN "echo hi"
        }
    "#});
    let runs = mock.recorded_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].envs.get("KEEP").map(String::as_str), Some("yes"));
    assert!(
        !runs[0].envs.contains_key("DROP"),
        "unlisted names stay out: {:?}",
        runs[0].envs
    );
}

#[test]
fn isolated_allowlist_prefers_outer_override_over_seed() {
    // The discriminator for snapshot-copy vs host-re-mirroring: the
    // parent overrides a seeded name, and the block allowlist must
    // copy the enclosing override, not reseed the host value.
    let (_temp, mock) = run_isolated_case(indoc! {r#"
        ENV PATH=/custom/bin
        ISOLATED {
            INHERIT_ENV [PATH]
            RUN "echo hi"
        }
    "#});
    let runs = mock.recorded_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].envs.get("PATH").map(String::as_str),
        Some("/custom/bin"),
        "allowlist must copy the enclosing override, not the host seed: {:?}",
        runs[0].envs
    );
}

#[test]
fn isolated_absent_names_stay_absent() {
    // Listing a name the outer scope lacks is not an error: the block
    // simply runs without it, matching top-level INHERIT_ENV.
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let steps = crate::parse_script(indoc! {r#"
        ISOLATED {
            INHERIT_ENV [OXDOC_DEFINITELY_ABSENT_12345]
            WRITE ok.txt done
        }
        LET $b: STRING = READ ok.txt
        ASSERT_EQ $b "done"
    "#})
    .expect("absent allowlist parses");
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver"));
    run_steps_with_manager(fs, &steps, MockProcessManager::default(), ExecIo::new())
        .unwrap_or_else(|err| panic!("absent allowlist runs: {err:#}"));
}

#[test]
fn isolated_nesting_narrows_monotonically() {
    let (_temp, mock) = run_isolated_case(indoc! {r#"
        ENV A=1
        ENV B=2
        ENV C=3
        ISOLATED {
            INHERIT_ENV [A, B, C]
            ISOLATED {
                INHERIT_ENV [A, B]
                ISOLATED {
                    INHERIT_ENV [A]
                    RUN "echo deep"
                }
                RUN "echo mid"
            }
            RUN "echo outer"
        }
    "#});
    let runs = mock.recorded_runs();
    assert_eq!(runs.len(), 3);
    let keys = |i: usize| {
        let mut names: Vec<&str> = runs[i]
            .envs
            .keys()
            .filter(|k| ["A", "B", "C"].contains(&k.as_str()))
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        names
    };
    assert_eq!(keys(0), vec!["A"], "deepest level keeps only A");
    assert_eq!(keys(1), vec!["A", "B"], "middle level keeps A and B");
    assert_eq!(keys(2), vec!["A", "B", "C"], "outer level keeps all three");
}

#[test]
fn isolated_restores_env_on_exit() {
    let (_temp, mock) = run_isolated_case(indoc! {r#"
        ENV M=outer
        ISOLATED {
            ENV M=inner
            RUN "echo hi"
        }
        RUN "echo hi"
    "#});
    let runs = mock.recorded_runs();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].envs.get("M").map(String::as_str), Some("inner"));
    assert_eq!(runs[1].envs.get("M").map(String::as_str), Some("outer"));
}

#[test]
fn isolated_shares_vars_and_files() {
    // Only env: is scrubbed. Bindings, pipes, and files flow through.
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let steps = crate::parse_script(indoc! {r#"
        LET $who: STRING = "world"
        ENV MODE=outer
        ISOLATED {
            ENV MODE=inner
            WRITE hello.txt "hi {{ $who }} {{ env:MODE }}"
        }
        LET $back: STRING = "{{ env:MODE }}"
        ASSERT_EQ $back "outer"
    "#})
    .expect("sharing case parses");
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver"));
    run_steps_with_manager(fs, &steps, MockProcessManager::default(), ExecIo::new())
        .unwrap_or_else(|err| panic!("sharing case runs: {err:#}"));
    let check = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    let body = check
        .read_to_string(&root.join("hello.txt").expect("join"))
        .expect("read file");
    assert_eq!(body, "hi world inner");
}

#[test]
fn isolated_works_inside_func() {
    let (_temp, mock) = run_isolated_case(indoc! {r#"
        ENV SECRET=topsecret
        FUNC PROBE() {
            ISOLATED {
                RUN "echo hi"
            }
        }
        PROBE()
    "#});
    let runs = mock.recorded_runs();
    assert_eq!(runs.len(), 1);
    assert!(
        !runs[0].envs.contains_key("SECRET"),
        "function-contained block scrubs too: {:?}",
        runs[0].envs
    );
}

#[test]
fn isolated_async_task_inherits_scrub() {
    let (_temp, mock) = run_isolated_case(indoc! {r#"
        ENV SECRET=topsecret
        ENV KEEP=yes
        ISOLATED {
            INHERIT_ENV [KEEP]
            LET $t: HANDLE = ASYNC { RUN "echo hi" }
            AWAIT $t
        }
    "#});
    let spawns = mock.spawn_log();
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].envs.get("KEEP").map(String::as_str), Some("yes"));
    assert!(
        !spawns[0].envs.contains_key("SECRET"),
        "async child of a block stays scrubbed: {:?}",
        spawns[0].envs
    );
}

#[test]
fn isolated_inside_async_scopes_to_task() {
    let (_temp, mock) = run_isolated_case(indoc! {r#"
        ENV SECRET=topsecret
        LET $t: HANDLE = ASYNC {
            ISOLATED {
                RUN "echo hi"
            }
        }
        AWAIT $t
    "#});
    let spawns = mock.spawn_log();
    assert_eq!(spawns.len(), 1);
    assert!(
        !spawns[0].envs.contains_key("SECRET"),
        "block inside a task scrubs that task: {:?}",
        spawns[0].envs
    );
}

#[test]
fn plain_block_keeps_outer_env_for_children() {
    // Control for the scrub tests below: an ordinary scoped block is
    // transparent to envs, so only ISOLATED drops outer names.
    let (_temp, mock) = run_isolated_case(indoc! {r#"
        ENV OUTER=topsecret
        [bool:true] {
            RUN "echo hi"
        }
    "#});
    let runs = mock.recorded_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].envs.get("OUTER").map(String::as_str),
        Some("topsecret"),
        "plain blocks must not scrub: {:?}",
        runs[0].envs
    );
}

#[test]
fn isolated_allowlist_prefers_outer_scope_over_host() {
    // The discriminating proof: when host and outer scope disagree on a
    // listed name, the child sees the OUTER value. A host-reading
    // implementation would answer "from-host" here.
    // RAII-guarded invented probe (not a real env var, so no constant).
    let _host = TestEnvGuard::set("OXDOCK_OUTER_WINS_PROBE", "from-host");
    let (_temp, mock) = run_isolated_case(indoc! {r#"
        ENV OXDOCK_OUTER_WINS_PROBE=from-outer
        ISOLATED {
            INHERIT_ENV [OXDOCK_OUTER_WINS_PROBE]
            RUN "echo hi"
        }
    "#});
    let runs = mock.recorded_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0]
            .envs
            .get("OXDOCK_OUTER_WINS_PROBE")
            .map(String::as_str),
        Some("from-outer"),
        "allowlist must read the outer scope, never the host: {:?}",
        runs[0].envs
    );
}

#[test]
fn isolated_allowlist_cannot_reach_host_only_names() {
    // Airtightness: a name the host exports but no outer scope routed
    // stays absent even when the inner allowlist names it. Listing is
    // necessary but not sufficient; outer-scope presence decides.
    let _host = oxdock_sys_test_utils::TestEnvGuard::set("OXDOC_HOST_ONLY_PROBE", "host");
    let (_temp, mock) = run_isolated_case(indoc! {r#"
        ENV ELSEWHERE=yes
        ISOLATED {
            INHERIT_ENV [OXDOC_HOST_ONLY_PROBE]
            RUN "echo hi"
        }
    "#});
    let runs = mock.recorded_runs();
    assert_eq!(runs.len(), 1);
    assert!(
        !runs[0].envs.contains_key("OXDOC_HOST_ONLY_PROBE"),
        "host-only names must not cross the boundary: {:?}",
        runs[0].envs
    );
    assert!(
        !runs[0].envs.contains_key("ELSEWHERE"),
        "unlisted outer names stay out too: {:?}",
        runs[0].envs
    );
}

/// Run a script with the real process manager over a fresh tempdir root,
/// returning the tempdir (kept alive). Real spawns prove tier injection.
fn run_seeded_case(script: &str) -> oxdock_fs::GuardedTempDir {
    use oxdock_fs::PathResolver;
    let steps = crate::parse_script(script).expect("tiered case parses");
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver"));
    run_steps_with_manager(
        fs,
        &steps,
        oxdock_process::default_process_manager(),
        ExecIo::new(),
    )
    .unwrap_or_else(|err| panic!("tiered case runs: {err:#}"));
    temp
}

fn read_case_file(temp: &oxdock_fs::GuardedTempDir, name: &str) -> String {
    use oxdock_fs::PathResolver;
    let root = temp.as_guarded_path().clone();
    let reader = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    reader
        .read_to_string(&root.join(name).expect("join"))
        .expect("case file readable")
}

#[test]
#[cfg_attr(
    miri,
    ignore = "spawns real OS processes, which die under Miri isolation"
)]
fn seeded_host_path_runs_shell_without_prelude() {
    // No INHERIT_ENV at all: the mirrored host PATH still resolves the
    // shell (`sh` or `cmd` per platform).
    run_seeded_case(indoc! {r#"
        RUN "echo tier-ok"
    "#});
}

#[test]
#[cfg_attr(
    miri,
    ignore = "spawns real OS processes, which die under Miri isolation"
)]
fn seeded_path_visible_to_expansion() {
    // No INHERIT_ENV at all: the mirrored host PATH is visible to
    // expansion when the host defines it, empty otherwise.
    let temp = run_seeded_case(indoc! {r#"
        WRITE path.txt "{{ env:PATH }}"
    "#});
    let expected = std::env::var(oxdock_fs::env::os::PATH).unwrap_or_default();
    assert_eq!(read_case_file(&temp, "path.txt"), expected);
}

#[test]
#[cfg_attr(
    miri,
    ignore = "spawns real OS processes, which die under Miri isolation"
)]
fn seeded_explicit_path_wins_over_seed() {
    let temp = run_seeded_case(indoc! {r#"
        ENV PATH=/custom/bin
        WRITE path.txt "{{ env:PATH }}"
    "#});
    assert_eq!(read_case_file(&temp, "path.txt"), "/custom/bin");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "spawns real OS processes, which die under Miri isolation"
)]
fn seeded_tmpdir_mirrors_or_absent() {
    // TMPDIR mirrors the host when set, absents when not. Unix-family:
    // Windows scripts needing it route `INHERIT_ENV`.
    let temp = run_seeded_case(indoc! {r#"
        WRITE tmpdir.txt "{{ env:TMPDIR }}"
    "#});
    let expected = std::env::var(oxdock_fs::env::os::unix::TMPDIR).unwrap_or_default();
    assert_eq!(read_case_file(&temp, "tmpdir.txt"), expected);
}

#[test]
#[cfg_attr(
    miri,
    ignore = "spawns real OS processes, which die under Miri isolation"
)]
fn seeded_term_mirrors_or_absent() {
    let temp = run_seeded_case(indoc! {r#"
        WRITE term.txt "{{ env:TERM }}"
    "#});
    let expected = std::env::var(oxdock_fs::env::os::unix::TERM).unwrap_or_default();
    assert_eq!(read_case_file(&temp, "term.txt"), expected);
}

#[test]
#[cfg_attr(
    miri,
    ignore = "spawns real OS processes, which die under Miri isolation"
)]
fn isolated_reseeds_factory_defaults() {
    // The discriminating parity proof: top level and block see the
    // identical seed, and outer customs stay out of the block. The
    // script spawns nothing (WRITE/ENV only), so the proof runs on
    // every platform; the PATH expectation mirrors the host or absents.
    let temp = run_seeded_case(indoc! {r#"
        ENV OUTER_CUSTOM=outer-val
        WRITE top-path.txt "{{ env:PATH }}"
        WRITE top-tmp.txt "{{ env:TMPDIR }}"
        ISOLATED {
            WRITE inner-path.txt "{{ env:PATH }}"
            WRITE inner-tmp.txt "{{ env:TMPDIR }}"
            WRITE inner-custom.txt "{{ env:OUTER_CUSTOM }}"
        }
    "#});
    let expected_path = std::env::var(oxdock_fs::env::os::PATH).unwrap_or_default();
    assert_eq!(read_case_file(&temp, "top-path.txt"), expected_path);
    assert_eq!(
        read_case_file(&temp, "inner-path.txt"),
        expected_path,
        "ISOLATED reseeds the identical seed"
    );
    assert_eq!(
        read_case_file(&temp, "inner-tmp.txt"),
        read_case_file(&temp, "top-tmp.txt"),
        "seeded temp matches across the block boundary"
    );
    assert_eq!(
        read_case_file(&temp, "inner-custom.txt"),
        "",
        "outer customs stay out of ISOLATED"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "spawns real OS processes, which die under Miri isolation"
)]
fn seeded_no_prelude_child_env_holds_boundary() {
    // The closedness regression proof: a script declaring NOTHING
    // spawns a child whose environment carries the host-mirrored
    // essentials, never ambient secrets. Past failure mode:
    // the operator's `$SHELL` (zsh) re-polluted scrubbed children with
    // host vars via `/etc/zshenv` and `getpwuid` synthesis. Batch
    // execution is pinned to `sh`/`cmd`, so the dump below is
    // deterministic on every host, every platform.
    let probe = TestEnvGuard::set("OXDOCK_SEED_AUDIT_PROBE", "host-secret");
    let _probe = probe;
    let temp = run_seeded_case(indoc! {r#"
        [family:unix] RUN "env > env.txt"
        [family:windows] RUN "set > env.txt"
    "#});
    let body = read_case_file(&temp, "env.txt");
    let mut got = HashMap::new();
    for line in body.lines() {
        if let Some((k, v)) = line.split_once('=') {
            got.insert(k.to_string(), v.to_string());
        }
    }
    // Mirror or absent: the child carries the host value when the
    // host defines the name, and nothing otherwise. Never invented.
    // Read-only: parallel tests cannot skew these.
    fn assert_seeded(got: &HashMap<String, String>, name: &str) {
        match std::env::var(name).ok() {
            Some(host) => assert_eq!(
                got.get(name).map(String::as_str),
                Some(host.as_str()),
                "{name} must mirror the host: {got:?}"
            ),
            None => assert!(
                !got.contains_key(name),
                "{name} must stay absent when the host lacks it: {got:?}"
            ),
        }
    }
    // Seed-derived assertions: every declared row mirrors the host or
    // stays absent. Only the engine-owned target dir binds scratch
    // (proven below). Adding a seed row needs no edit here.
    for entry in oxdock_process::env_policy::current_platform_seed() {
        let name = entry.name;
        if name == oxdock_fs::env::cargo::TARGET_DIR {
            continue;
        }
        assert_seeded(&got, name);
    }
    // No ambient secrets anywhere, on any platform.
    assert!(
        !got.contains_key("OXDOCK_SEED_AUDIT_PROBE"),
        "host probe leaked: {got:?}"
    );
    assert!(
        !got.contains_key("CARGO_HOME")
            && !got.contains_key("GNUPGHOME")
            && !got.contains_key("SSH_AUTH_SOCK"),
        "credential-adjacent host names leaked: {got:?}"
    );
    // Substrate present: scratch target dir.
    let target = got
        .get("CARGO_TARGET_DIR")
        .cloned()
        .unwrap_or_default()
        .replace('\\', "/");
    assert!(
        target.ends_with("/target") && target.contains("oxdock-"),
        "CARGO_TARGET_DIR must bind the scratch target dir: {got:?}"
    );
}

#[test]
fn inherit_env_description_names_no_seed_rows() {
    // The INHERIT_ENV description must point at the contract, never
    // enumerate seeded names: any backticked baseline name here fails,
    // forcing authors back to the pointer. Pure metadata, no spawns.
    let description = crate::all_metadata()
        .into_iter()
        .find(|meta| meta.name == "INHERIT_ENV")
        .expect("INHERIT_ENV registered")
        .description;
    // Normalized: source line-breaks must never break the pointer check.
    let flat = description.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("Environment Variable Contracts"),
        "INHERIT_ENV description must point at the contract"
    );
    for entry in oxdock_process::env_policy::describe_baseline_defaults() {
        assert!(
            !description.contains(&format!("`{}`", entry.name)),
            "INHERIT_ENV description enumerates seeded name `{}`; point at the contract instead",
            entry.name
        );
    }
}
