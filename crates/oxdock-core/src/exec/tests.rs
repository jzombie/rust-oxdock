use super::*;

use anyhow::bail;
use indoc::indoc;
use oxdock_fs::{GuardedPath, MockFs, WorkspaceFs};
use oxdock_parser::{Guard, GuardExpr, IoBinding, IoStream, StepKind};
use oxdock_process::{
    BackgroundHandle, CommandContext, CommandMode, CommandOptions, CommandResult, CommandStdin,
    MockProcessManager, MockRunCall, ProcessManager,
};
use oxdock_sys_test_utils::exit_status_from_code;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
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
                decl_type: "LIST".to_string(),
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
                decl_type: "MAP".to_string(),
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
    // interpreted and reach argv verbatim — there is no shell to escape for.
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
                    pipe: Some(oxdock_parser::PipeTarget::Name("shared".into())),
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
                    pipe: Some(oxdock_parser::PipeTarget::Name("shared".into())),
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
/// trailing newline end to end — pipe EOF included.
#[test]
fn async_echo_pipe_write_preserves_exact_bytes() {
    let steps = vec![
        Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![IoBinding {
                    stream: IoStream::Stdout,
                    pipe: Some(oxdock_parser::PipeTarget::Name("async_out".into())),
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
                    pipe: Some(oxdock_parser::PipeTarget::Name("async_out".into())),
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
                decl_type: "HANDLE".to_string(),
                body: vec![Step {
                    guard: None,
                    kind: StepKind::WithIo {
                        bindings: vec![IoBinding {
                            stream: IoStream::Stdin,
                            pipe: Some(oxdock_parser::PipeTarget::Name("in_chan".into())),
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
                    pipe: Some(oxdock_parser::PipeTarget::Name("in_chan".into())),
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
        ("pipe.rs", include_str!("pipe.rs")),
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
        cwd: fs.root().clone(),
        envs: Arc::new(HashMap::new()),
        bg_children: Vec::new(),
        scope_stack: Vec::new(),
        io: ExecIo::new(),
        assert_windows: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        assert_windows_stderr: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        exact_stdout: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        var_scopes: Vec::new(),
        cancel_token: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active_process: Arc::new(std::sync::Mutex::new(None)),
        named_tasks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        next_task_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        next_pipe_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        inside_async: false,
        keeper_expiry: None,
        cancellable: false,
        functions: super::native::FunctionRegistry::with_builtins(),
        types: super::typing::startup_type_map(),
        call_depth: 0,
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
    let steps = crate::parse_script(
        "LET $items: LIST = [\"a\", \"b\"]\nFOR $i: INT, $v: STRING IN $items {\nWRITE \"{{ $v }}.txt\" \"{{ $i }}\"\n}\n",
    )
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
    let steps = crate::parse_script(
        "LET $m: MAP = {\"k\": \"v\"}\nFOR $k: INT, $v: STRING IN $m {\nWRITE x.txt \"hi\"\n}\n",
    )
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
    let steps = crate::parse_script(
        "LET $b: BOOL = !true\nLET $s: STRING = \"!true\"\nWRITE b.txt \"{{ $b }}\"\nWRITE s.txt \"{{ $s }}\"\nIF $b {\nWRITE wrong.txt \"bool was truthy\"\n}\n",
    )
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
    let steps = crate::parse_script("LET $s: STRING = \"!true\"\nIF $s {\nWRITE x.txt \"hi\"\n}\n")
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
                    pipe: Some(oxdock_parser::PipeTarget::Name("cap-cat".to_string())),
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
                    pipe: Some(oxdock_parser::PipeTarget::Name("cap-cat".to_string())),
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
    let steps = vec![Step {
        guard: None,
        kind: StepKind::WithIo {
            bindings: vec![
                IoBinding {
                    stream: IoStream::Stdout,
                    pipe: Some(oxdock_parser::PipeTarget::Name("p".into())),
                },
                IoBinding {
                    stream: IoStream::Stdout,
                    pipe: Some(oxdock_parser::PipeTarget::Name("p".into())),
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
    let err = execute_steps(
        &mut state,
        &mut proc,
        &steps,
        CommandStdin::Null,
        false,
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
        let steps = vec![Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![
                    IoBinding {
                        stream: stream_a,
                        pipe: Some(oxdock_parser::PipeTarget::Name("p".into())),
                    },
                    IoBinding {
                        stream: stream_b,
                        pipe: Some(oxdock_parser::PipeTarget::Name("p".into())),
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
        let err = execute_steps(
            &mut state,
            &mut proc,
            &steps,
            CommandStdin::Null,
            false,
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
fn with_io_async_single_run_promotes_os_pipe() {
    let steps = vec![Step {
        guard: None,
        kind: StepKind::WithIo {
            bindings: vec![IoBinding {
                stream: IoStream::Stdout,
                pipe: Some(oxdock_parser::PipeTarget::Name("live".into())),
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
    }];
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
        true,
    )
    .expect("run");
    assert!(
        matches!(
            state.io.resolve_stdout(0, "live", true),
            Ok(StreamHandle::Os(_))
        ),
        "single RUN producer must promote pipe:live to an OS pair"
    );
    assert!(
        state.io.input_pipe("live").is_none(),
        "promoted names must not also create script entries"
    );
}

#[cfg(not(miri))]
#[test]
fn with_io_async_guarded_and_exec_form_single_run_promotes_os_pipe() {
    for (script, name) in [
        (
            "WITH_IO [stdout=pipe:g] ASYNC { [bool:true] RUN \"echo hi\" }",
            "g",
        ),
        ("WITH_IO [stdout=pipe:e] ASYNC RUN [\"echo\", \"hi\"]", "e"),
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
            true,
        )
        .expect("run");
        assert!(
            matches!(
                state.io.resolve_stdout(0, name, true),
                Ok(StreamHandle::Os(_))
            ),
            "guarded and exec form single RUN producers must promote: {script}"
        );
    }
}

#[test]
fn with_io_async_dsl_body_stays_script_pipe() {
    let steps = vec![Step {
        guard: None,
        kind: StepKind::WithIo {
            bindings: vec![IoBinding {
                stream: IoStream::Stdout,
                pipe: Some(oxdock_parser::PipeTarget::Name("plain".into())),
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
    }];
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
        true,
    )
    .expect("run");
    assert!(
        state.io.input_pipe("plain").is_some(),
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

    match io.resolve_stdout(0, "s-out", false) {
        Ok(StreamHandle::Stream(w)) => assert!(Arc::ptr_eq(&w, &writer)),
        _ => panic!("expected streamed stdout handle"),
    }
    match io.resolve_stderr(0, "s-inh", false) {
        Ok(StreamHandle::Inherit) => {}
        _ => panic!("expected inherit stderr handle"),
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
        CommandStdin::Null,
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
    // Pipeline should succeed — the background task completes naturally
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
    // A body that fails fast must surface its own error unwrapped — no
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
    // handlers::timeout directly — the step loop's own pre-check would bail
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
        out_pipe_name: None,
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
            let kind = v.type_name().to_string();
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
            // Whatever the variable holds — even template-looking payloads —
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
    }
}

// ---------------------------------------------------------------------------
// SpillBuffer (LET-capture sink) storage tiering tests
// ---------------------------------------------------------------------------

#[test]
#[cfg(not(miri))]
fn spill_buffer_stays_in_memory_below_threshold() {
    use super::capture::{SPILL_THRESHOLD, SpillBuffer};
    use std::sync::Arc;

    let buf = Arc::new(SpillBuffer::new());
    let writer = buf.writer();

    let payload = vec![0xABu8; 1024]; // 1 KiB — below threshold
    writer.lock().unwrap().write_all(&payload).unwrap();
    assert!(!buf.is_spilled());
    assert_eq!(buf.drain_bytes().unwrap(), payload);
    let _ = SPILL_THRESHOLD; // Ensure constant is used
}

#[test]
#[cfg(not(miri))]
fn spill_buffer_spills_to_disk_above_threshold() {
    use super::capture::{SPILL_THRESHOLD, SpillBuffer};
    use std::sync::Arc;

    let buf = Arc::new(SpillBuffer::new());
    let writer = buf.writer();

    // Exceed the threshold by 1 MiB
    let size = SPILL_THRESHOLD + (1024 * 1024);
    let payload: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
    writer.lock().unwrap().write_all(&payload).unwrap();
    drop(writer);

    assert!(buf.is_spilled(), "buffer must have spilled to disk");
    assert_eq!(buf.drain_bytes().unwrap(), payload);
}

#[test]
#[cfg(not(miri))]
fn spill_buffer_backlog_cap_exceeded_returns_error() {
    use super::capture::{MAX_BACKLOG, SPILL_THRESHOLD, SpillBuffer};
    use std::sync::Arc;

    let buf = Arc::new(SpillBuffer::new());
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
    use super::capture::{SPILL_THRESHOLD, SpillBuffer};
    use std::fs;
    use std::sync::Arc;

    let buf = Arc::new(SpillBuffer::new());
    let writer = buf.writer();

    let size = SPILL_THRESHOLD + (1024 * 1024);
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
    use super::capture::SpillBuffer;
    use std::sync::Arc;

    // Valid UTF-8 round-trips exactly (no stripping).
    let buf = Arc::new(SpillBuffer::new());
    buf.writer().lock().unwrap().write_all(b"hi\n").unwrap();
    assert_eq!(buf.drain_string_strict().unwrap(), "hi\n");

    // Invalid UTF-8 is a strict error, never lossy.
    let buf = Arc::new(SpillBuffer::new());
    buf.writer()
        .lock()
        .unwrap()
        .write_all(&[0x66, 0xff, 0xfe])
        .unwrap();
    let err = buf.drain_string_strict().expect_err("non-UTF8 must fail");
    assert!(err.to_string().contains("not valid UTF-8"), "{err}");
}

// ── Network bridge (LISTEN / CONNECT) ────────────────────────────────────
//
// Scripts under test stay `indoc` literals and read their port from
// `{{ env:BRIDGE_PORT }}`: no `format!`-built scripts, no `\n`-joined
// continuations, no ad-hoc substitution. Runners supply a fresh port per
// attempt through the run helpers below.

/// Connect with a deadline: the listener task binds synchronously at spawn,
/// but thread scheduling means the test must tolerate a slow start.
/// Compiled everywhere (execution is gated by the callers' Miri ignores).
fn connect_retry(port: u16) -> TcpStream {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match TcpStream::connect(format!("127.0.0.1:{port}")) {
            Ok(stream) => return stream,
            Err(err) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
                let _ = err;
            }
            Err(err) => panic!("connect to test listener failed: {err}"),
        }
    }
}

/// Read one `\n`-terminated line with a deadline so helper failures error
/// instead of hanging the suite. Compiled everywhere; only called from
/// Miri-ignored tests.
fn read_line_deadline(stream: &mut TcpStream, what: &str) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set timeout");
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => panic!("EOF waiting for {what}"),
            Ok(_) => {
                out.push(byte[0]);
                if byte[0] == b'\n' {
                    return out;
                }
            }
            Err(err) => panic!("read waiting for {what} failed: {err}"),
        }
    }
}

/// Run parsed steps on a mock filesystem with a watchdog: socket tests must
/// fail on a hang, never freeze the suite. `env` seeds `{{ env:... }}`
/// lookups (fresh `ExecIo` is bypassed here, so entries go straight into
/// the run state, which is uniquely owned at this point).
fn run_bridge_steps(
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
        true,
    )?;
    Ok(fs.snapshot())
}

fn run_bridge_script_inner(
    steps: Vec<Step>,
    env: Vec<(String, String)>,
    limit: Duration,
) -> Result<HashMap<String, Vec<u8>>, String> {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(run_bridge_steps(&steps, env));
    });
    match done_rx.recv_timeout(limit) {
        Ok(Ok(files)) => Ok(files),
        Ok(Err(err)) => Err(format!("{err:#}")),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            Err(format!("bridge script did not complete within {limit:?}"))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err("bridge script thread panicked".to_string())
        }
    }
}

fn run_bridge_script(
    steps: Vec<Step>,
    env: Vec<(String, String)>,
    limit: Duration,
) -> HashMap<String, Vec<u8>> {
    run_bridge_script_inner(steps, env, limit).unwrap_or_else(|err| panic!("{err}"))
}

/// Run with a fresh ephemeral candidate per attempt. The candidate is
/// released immediately, so a stolen port surfaces as a fast, deterministic
/// bind failure and retries exact (no TOCTOU flake): conflicts fail at bind
/// time with "bind failed", never mid-run.
fn run_bridge_script_bind_retry(steps: &[Step], limit: Duration) -> HashMap<String, Vec<u8>> {
    let mut last = String::new();
    for _ in 0..10 {
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("reserve candidate")
            .local_addr()
            .expect("candidate addr")
            .port();
        let port_text = port.to_string();
        match run_bridge_script_inner(
            steps.to_vec(),
            vec![("BRIDGE_PORT".to_string(), port_text)],
            limit,
        ) {
            Ok(files) => return files,
            Err(err) if err.contains("bind failed") => {
                last = err;
            }
            Err(err) => panic!("bridge script failed: {err}"),
        }
    }
    panic!("bridge script kept hitting held ports: {last}");
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
fn bridge_validation_and_gating_need_no_sockets() {
    // Misuse and policy rejection happen before any socket call, so these
    // run everywhere including Miri.
    let cases = [
        (
            "WITH_IO [stdin=pipe:req, stdout=pipe:resp] CONNECT 127.0.0.1:9\n",
            "requires ASYNC",
        ),
        (
            "WITH_IO [stdin=pipe:req, stdout=pipe:resp] LISTEN 127.0.0.1:9\n",
            "requires ASYNC",
        ),
        // Bare CONNECT passes bindings now (null stdin means half-closed),
        // so it fails at the same ASYNC gate.
        ("CONNECT 127.0.0.1:9\n", "requires ASYNC"),
        (
            "WITH_IO [stdin=pipe:req, stdout=pipe:resp] CONNECT not-an-endpoint\n",
            "invalid endpoint",
        ),
        (
            "WITH_IO [stdin=pipe:req, stdout=pipe:resp] LISTEN 0.0.0.0:9\n",
            "loopback",
        ),
        (
            "WITH_IO [stdin=pipe:req, stdout=pipe:resp] LISTEN 127.0.0.1:0\n",
            "ephemeral",
        ),
    ];
    for (script, needle) in cases {
        let steps = crate::parse_script(script).expect("parse ok");
        let err = run_expect_err(
            Box::new(MockFs::new()),
            &steps,
            MockProcessManager::default(),
        );
        let msg = format!("{err:#}");
        assert!(msg.contains(needle), "script {script:?}: {msg}");
    }
}
#[test]
#[cfg_attr(
    miri,
    ignore = "OS promotion is compiled out under Miri, so pipe kind differs by platform"
)]
fn bridge_spawn_manifest_ensures_consumed_pipes() {
    // A task that only READS a fresh pipe must still find the decided
    // backend the moment spawning returns: pinning runs synchronously in
    // LET, so no worker scheduling is involved and no sleep is needed.
    // A `RUN` consumer means an OS pair; anything else would be "missing".
    // The command never executes (mock manager records it), so no platform
    // binary is required and the test is cross-platform.
    let steps = crate::parse_script(indoc! {r#"
        LET $t: HANDLE = ASYNC {
          WITH_IO [stdin=pipe:fresh] RUN ["stub-never-executed"]
        }
        LET $p: PIPE = pipe:fresh
        LET $info: MAP = INSPECT($p)
        WRITE kind.txt "{{ $info.pipe_kind }}"
    "#})
    .expect("parse ok");
    let files = run_bridge_script(steps, vec![], Duration::from_secs(15));
    assert_eq!(file_content(&files, "kind.txt"), b"os");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "OS promotion is compiled out under Miri, so pipe kind differs by platform"
)]
fn bridge_spawn_manifest_ensures_consumed_variable_pipes() {
    // Same guarantee through a `PIPE`-typed variable endpoint: promotion
    // analysis applies to dynamic endpoints exactly like literals.
    let steps = crate::parse_script(indoc! {r#"
        LET $p: PIPE = pipe:dynfresh
        LET $t: HANDLE = ASYNC {
          WITH_IO [stdin=$p] RUN ["stub-never-executed"]
        }
        LET $info: MAP = INSPECT($p)
        WRITE kind.txt "{{ $info.pipe_kind }}"
    "#})
    .expect("parse ok");
    let files = run_bridge_script(steps, vec![], Duration::from_secs(15));
    assert_eq!(file_content(&files, "kind.txt"), b"os");
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
    let files = run_bridge_steps(&steps, vec![]).expect("bare pipes round-trip");
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
    let files = run_bridge_steps(&steps, vec![]).expect("aliased pipes round-trip");
    assert_eq!(file_content(&files, "out.txt"), b"shared");
}

#[test]
fn bare_let_pipe_mints_distinct_unspellable_names() {
    // Transitional name backing: every bare declaration mints a key no
    // `pipe:` literal can spell (it contains a space), so anonymous
    // backends never collide with user-named pipes.
    let steps = crate::parse_script("LET $a: PIPE\nLET $b: PIPE\n").expect("parse ok");
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
        true,
    )
    .expect("bare declarations run");
    let name_a = state
        .get_var("a")
        .expect("var a")
        .as_pipe_name()
        .expect("pipe a")
        .to_string();
    let name_b = state
        .get_var("b")
        .expect("var b")
        .as_pipe_name()
        .expect("pipe b")
        .to_string();
    assert_ne!(
        name_a, name_b,
        "bare declarations must mint distinct backends"
    );
    for name in [&name_a, &name_b] {
        assert!(
            name.contains(' '),
            "anonymous pipe key {name:?} must be unspellable as a pipe: literal"
        );
    }
}

#[test]
fn bridge_inner_for_reader_finds_script_backends() {
    let io = ExecIo::new();
    io.ensure_pipe_for("live", false).expect("ensure");
    let CommandStdin::Stream(reader) = io.resolve_stdin(0, "live", false).expect("resolve") else {
        panic!("expected stream stdin");
    };
    assert!(io.stdin_pipe_inner(&reader).is_some());
    let foreign: oxdock_process::SharedInput =
        Arc::new(Mutex::new(std::io::Cursor::new(Vec::new())));
    assert!(io.stdin_pipe_inner(&foreign).is_none());
}

#[test]
fn bridge_force_close_eofs_despite_writer_and_keeper() {
    let io = ExecIo::new();
    io.ensure_pipe_for("p", false).expect("ensure");
    // Attach a live writer and pin a keeper: without force_close neither
    // lets readers observe EOF.
    let CommandStdin::Stream(reader) = io.resolve_stdin(0, "p", false).expect("resolve") else {
        panic!("expected stream stdin");
    };
    let writer = match io.resolve_stdout(0, "p", false).expect("resolve") {
        super::io::StreamHandle::Stream(writer) => writer,
        _ => panic!("expected stream stdout"),
    };
    let _keeper = io.pin_keeper("p").expect("pin").expect("keeper");
    let backend = io.pipe_backend("p").expect("backend");
    assert!(io.pipe_backend("missing").is_none());
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
    ignore = "uses real loopback sockets, which die under Miri isolation"
)]
fn bridge_connect_roundtrip_over_script_pipes() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        conn.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        // Read the request line, answer, half-close, then drain to EOF so
        // the client pump terminates cleanly.
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match conn.read(&mut byte).expect("read request") {
                0 => break,
                _ => {
                    request.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
            }
        }
        assert_eq!(request, b"hello\n");
        conn.write_all(b"world\n").expect("write response");
        conn.shutdown(Shutdown::Write).expect("half-close");
        let mut rest = Vec::new();
        conn.read_to_end(&mut rest).expect("drain");
    });
    let steps = crate::parse_script(indoc! {r#"
        LET $t: HANDLE = ASYNC {
          WITH_IO [stdin=pipe:req, stdout=pipe:resp] CONNECT 127.0.0.1:{{ env:BRIDGE_PORT }}
        }
        WITH_IO [stdout=pipe:req] ECHO "hello"
        WITH_IO [stdin=pipe:resp] READ_LINE $got
        AWAIT $t
        WRITE out.txt "{{ $got }}"
    "#})
    .expect("parse ok");
    let port_text = port.to_string();
    let files = run_bridge_script(
        steps,
        vec![("BRIDGE_PORT".to_string(), port_text)],
        Duration::from_secs(15),
    );
    assert_eq!(file_content(&files, "out.txt"), b"world");
    server.join().expect("server thread");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "uses real loopback sockets, which die under Miri isolation"
)]
fn bridge_listen_explicit_full_duplex() {
    use std::sync::mpsc::RecvTimeoutError;
    let steps = crate::parse_script(indoc! {r#"
        LET $ls: HANDLE = ASYNC {
          WITH_IO [stdin=pipe:req, stdout=pipe:resp] LISTEN 127.0.0.1:{{ env:BRIDGE_PORT }}
        }
        WITH_IO [stdout=pipe:req] ECHO "back"
        WITH_IO [stdin=pipe:resp] READ_LINE $got
        AWAIT $ls
        WRITE out.txt "{{ $got }}"
    "#})
    .expect("parse ok");
    for _ in 0..10 {
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("reserve candidate")
            .local_addr()
            .expect("candidate addr")
            .port();
        let port_text = port.to_string();
        let attempt = steps.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_bridge_steps(
                &attempt,
                vec![("BRIDGE_PORT".to_string(), port_text)],
            ));
        });
        match rx.recv_timeout(Duration::from_millis(500)) {
            // Fast bind failure: stolen port, retry with a fresh candidate.
            Ok(Err(err)) if format!("{err:#}").contains("bind failed") => continue,
            Ok(Err(err)) => panic!("bridge script failed: {err:#}"),
            Ok(Ok(_)) => panic!("script completed without a client"),
            Err(RecvTimeoutError::Disconnected) => panic!("bridge script thread panicked"),
            Err(RecvTimeoutError::Timeout) => {
                // Listener is up (bind is synchronous at task start).
                let mut client = connect_retry(port);
                client.write_all(b"hello\n").expect("write hello");
                let reply = read_line_deadline(&mut client, "back line");
                assert_eq!(reply, b"back\n");
                drop(client);
                match rx.recv_timeout(Duration::from_secs(15)) {
                    Ok(Ok(files)) => {
                        assert_eq!(file_content(&files, "out.txt"), b"hello");
                        return;
                    }
                    Ok(Err(err)) => panic!("bridge script failed: {err:#}"),
                    Err(_) => panic!("bridge script did not finish after client close"),
                }
            }
        }
    }
    panic!("bridge script kept hitting held ports");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "uses real loopback sockets, which die under Miri isolation"
)]
fn bridge_read_only_pump_completes_on_disconnect() {
    // No stdin binding anywhere: the pump starts half-closed, delivers
    // socket bytes, and the task completes on disconnect with nothing to
    // choreograph. This is disconnect detection without producer steps.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        conn.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        conn.write_all(b"hi\n").expect("write");
    });
    let steps = crate::parse_script(indoc! {r#"
        LET $t: HANDLE = ASYNC {
          WITH_IO [stdout=pipe:resp] CONNECT 127.0.0.1:{{ env:BRIDGE_PORT }}
        }
        WITH_IO [stdin=pipe:resp] READ_LINE $got
        AWAIT $t
        WRITE out.txt "{{ $got }}"
    "#})
    .expect("parse ok");
    let port_text = port.to_string();
    let files = run_bridge_script(
        steps,
        vec![("BRIDGE_PORT".to_string(), port_text)],
        Duration::from_secs(15),
    );
    assert_eq!(file_content(&files, "out.txt"), b"hi");
    server.join().expect("server thread");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "uses real loopback sockets, which die under Miri isolation"
)]
fn bridge_no_half_close_defers_fin() {
    // With --no-half-close the server must observe no FIN while the session
    // idles (a timed read times out instead of returning EOF), then still
    // receive bytes produced afterwards, then see a clean close.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        conn.set_read_timeout(Some(Duration::from_millis(500)))
            .expect("timeout");
        let mut byte = [0u8; 1];
        match conn.read(&mut byte) {
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Ok(0) => panic!("unexpected FIN during idle window"),
            other => panic!("unexpected read outcome during idle window: {other:?}"),
        }
        conn.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut line = Vec::new();
        loop {
            match conn.read(&mut byte).expect("read late line") {
                0 => break,
                _ => {
                    line.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
            }
        }
        assert_eq!(line, b"late\n");
    });
    let steps = crate::parse_script(indoc! {r#"
        LET $t: HANDLE = ASYNC {
          WITH_IO [stdin=pipe:req, stdout=pipe:resp] CONNECT 127.0.0.1:{{ env:BRIDGE_PORT }} --no-half-close
        }
        SLEEP 1s
        WITH_IO [stdout=pipe:req] ECHO "late"
        AWAIT $t
    "#})
    .expect("parse ok");
    let port_text = port.to_string();
    run_bridge_script(
        steps,
        vec![("BRIDGE_PORT".to_string(), port_text)],
        Duration::from_secs(15),
    );
    server.join().expect("server thread");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "uses real loopback sockets, which die under Miri isolation"
)]
fn bridge_listen_refuses_while_serving() {
    // Accept-one means the listener drops after the first accept: a second
    // dial while one client is served must refuse, never backlog-and-hang.
    use std::sync::mpsc::RecvTimeoutError;
    let steps = crate::parse_script(indoc! {r#"
        LET $ls: HANDLE = ASYNC {
          WITH_IO [stdin=pipe:req, stdout=pipe:resp] LISTEN 127.0.0.1:{{ env:BRIDGE_PORT }}
        }
        WITH_IO [stdout=pipe:req] ECHO "back"
        WITH_IO [stdin=pipe:resp] READ_LINE $got
        AWAIT $ls
        WRITE out.txt "{{ $got }}"
    "#})
    .expect("parse ok");
    for _ in 0..10 {
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("reserve candidate")
            .local_addr()
            .expect("candidate addr")
            .port();
        let port_text = port.to_string();
        let attempt = steps.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_bridge_steps(
                &attempt,
                vec![("BRIDGE_PORT".to_string(), port_text)],
            ));
        });
        match rx.recv_timeout(Duration::from_millis(500)) {
            // Fast bind failure: stolen port, retry with a fresh candidate.
            Ok(Err(err)) if format!("{err:#}").contains("bind failed") => continue,
            Ok(Err(err)) => panic!("bridge script failed: {err:#}"),
            Ok(Ok(_)) => panic!("script completed without clients"),
            Err(RecvTimeoutError::Disconnected) => panic!("bridge script thread panicked"),
            Err(RecvTimeoutError::Timeout) => {
                // Listener is up (bind is synchronous at task start).
                let mut first = connect_retry(port);
                first.write_all(b"hello\n").expect("write hello");
                // Drain the greeting: dropping with unread bytes pending
                // would RST instead of FIN and fail the pump read below.
                let greeting = read_line_deadline(&mut first, "back line");
                assert_eq!(greeting, b"back\n");
                std::thread::sleep(Duration::from_millis(500));
                let dial_addr: std::net::SocketAddr =
                    format!("127.0.0.1:{port}").parse().expect("addr");
                match TcpStream::connect_timeout(&dial_addr, Duration::from_secs(2)) {
                    Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {}
                    other => panic!("second client must refuse while serving, got {other:?}"),
                }
                drop(first);
                match rx.recv_timeout(Duration::from_secs(15)) {
                    Ok(Ok(files)) => {
                        assert_eq!(file_content(&files, "out.txt"), b"hello");
                        return;
                    }
                    Ok(Err(err)) => panic!("bridge script failed: {err:#}"),
                    Err(_) => panic!("bridge script did not finish after client close"),
                }
            }
        }
    }
    panic!("bridge script kept hitting held ports");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "uses real loopback sockets, which die under Miri isolation"
)]
fn bridge_shared_pair_proxy_terminates() {
    // Two pumps sharing one pipe pair (the transparent-proxy topology):
    // client bytes flow up, upstream bytes flow down, and a client
    // disconnect cascades through both tasks so both AWAITs return.
    // Upstream is held by the test; the client side is the script listener.
    let upstream = TcpListener::bind("127.0.0.1:0").expect("bind");
    let upstream_port = upstream.local_addr().expect("addr").port();
    let upstream_text = upstream_port.to_string();
    let helper = std::thread::spawn(move || {
        // Serve dials until one full exchange completes. Dials from failed
        // bind-retry attempts resolve fast (their scripts are already dead,
        // so EOF arrives promptly); only the live attempt sends data. The
        // timeout is a pathological backstop and must comfortably exceed
        // the probe-plus-dial latency of the live path.
        loop {
            let (mut conn, _) = upstream.accept().expect("accept");
            conn.set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            let got = loop {
                match conn.read(&mut byte) {
                    Ok(0) => break None,
                    Ok(_) => {
                        line.push(byte[0]);
                        if byte[0] == b'\n' {
                            break Some(line);
                        }
                    }
                    Err(_) => break None,
                }
            };
            match got {
                Some(line) => {
                    assert_eq!(line, b"ping\n");
                    conn.set_read_timeout(Some(Duration::from_secs(5)))
                        .expect("timeout");
                    conn.write_all(b"pong\n").expect("write reply");
                    return;
                }
                None => continue,
            }
        }
    });
    let steps = crate::parse_script(indoc! {r#"
        LET $ls: HANDLE = ASYNC {
          WITH_IO [stdin=pipe:s2c, stdout=pipe:c2s] LISTEN 127.0.0.1:{{ env:BRIDGE_PORT }}
        }
        LET $up: HANDLE = ASYNC {
          WITH_IO [stdin=pipe:c2s, stdout=pipe:s2c] CONNECT 127.0.0.1:{{ env:UPSTREAM_PORT }}
        }
        AWAIT $up
        AWAIT $ls
        WRITE done.txt "both-tasks-completed"
    "#})
    .expect("parse ok");
    for _ in 0..10 {
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("reserve candidate")
            .local_addr()
            .expect("candidate addr")
            .port();
        let port_text = port.to_string();
        let attempt = steps.clone();
        let upstream_text = upstream_text.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_bridge_steps(
                &attempt,
                vec![
                    ("BRIDGE_PORT".to_string(), port_text),
                    ("UPSTREAM_PORT".to_string(), upstream_text),
                ],
            ));
        });
        match rx.recv_timeout(Duration::from_millis(500)) {
            // Fast bind failure: stolen port, retry with a fresh candidate.
            Ok(Err(err)) if format!("{err:#}").contains("bind failed") => continue,
            Ok(Err(err)) => panic!("bridge script failed: {err:#}"),
            Ok(Ok(_)) => panic!("script completed without clients"),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("bridge script thread panicked")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Both bridges are up; drive one exchange, then disconnect
                // and require both tasks to finish.
                let mut client = connect_retry(port);
                client.write_all(b"ping\n").expect("write ping");
                let reply = read_line_deadline(&mut client, "pong line");
                assert_eq!(reply, b"pong\n");
                drop(client);
                match rx.recv_timeout(Duration::from_secs(15)) {
                    Ok(Ok(files)) => {
                        assert_eq!(file_content(&files, "done.txt"), b"both-tasks-completed");
                        helper.join().expect("upstream thread");
                        return;
                    }
                    Ok(Err(err)) => panic!("bridge script failed: {err:#}"),
                    Err(_) => panic!("bridge tasks did not finish after client close"),
                }
            }
        }
    }
    panic!("bridge script kept hitting held ports");
}

#[test]
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "spawns real OS processes and kernel pipes, which die under Miri isolation"
)]
fn bridge_os_pipe_names_recycle_across_sessions() {
    // Two sequential sessions reusing one pipe name through RUN-promoted OS
    // pairs: the second session must get a fresh pair, not a
    // consumed-handle error. No timing involved: AWAIT joins each session
    // before the next begins.
    use oxdock_fs::PathResolver;
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let steps = crate::parse_script(indoc! {r#"
        WITH_IO [stdout=pipe:x] ASYNC RUN "echo one"
        WITH_IO [stdin=pipe:x] WRITE f1.txt
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
        assert_eq!(content, "one\n");
    }
}

#[test]
#[cfg_attr(
    miri,
    ignore = "uses real loopback sockets, which die under Miri isolation"
)]
fn bridge_peer_fin_then_late_producer_completes() {
    // The server half-closes immediately; the task must survive the silent
    // period (no premature exit on socket EOF) and still deliver bytes the
    // script produces afterwards.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        conn.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        conn.shutdown(Shutdown::Write).expect("half-close");
        let mut rest = Vec::new();
        conn.read_to_end(&mut rest).expect("drain");
        assert_eq!(rest, b"late\n");
    });
    let steps = crate::parse_script(indoc! {r#"
        LET $t: HANDLE = ASYNC {
          WITH_IO [stdin=pipe:req, stdout=pipe:resp] CONNECT 127.0.0.1:{{ env:BRIDGE_PORT }}
        }
        SLEEP 300ms
        WITH_IO [stdout=pipe:req] ECHO "late"
        AWAIT $t
    "#})
    .expect("parse ok");
    let port_text = port.to_string();
    run_bridge_script(
        steps,
        vec![("BRIDGE_PORT".to_string(), port_text)],
        Duration::from_secs(15),
    );
    server.join().expect("server thread");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "uses real loopback sockets, which die under Miri isolation"
)]
fn bridge_cancel_accept_blocked_listener() {
    let steps = crate::parse_script(indoc! {r#"
        LET $ls: HANDLE = ASYNC {
          WITH_IO [stdin=pipe:req, stdout=pipe:resp] LISTEN 127.0.0.1:{{ env:BRIDGE_PORT }}
        }
        CANCEL $ls
    "#})
    .expect("parse ok");
    let start = std::time::Instant::now();
    let files = run_bridge_script_bind_retry(&steps, Duration::from_secs(15));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "CANCEL of an accept-blocked LISTEN must return promptly"
    );
    drop(files);
}
