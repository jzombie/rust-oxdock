use crate::common::mock_lower;

use indoc::indoc;
use oxdock_parser::ast::StepKind;
use oxdock_parser::parse_script;

fn parse_one(script: &str) -> StepKind {
    let steps = parse_script(script, mock_lower).expect("parse LET capture");
    assert_eq!(steps.len(), 1, "expected one step, got {steps:?}");
    steps.into_iter().next().unwrap().kind
}

#[test]
fn let_capture_sync_command_binds_assign_capture() {
    match parse_one("LET $x = ECHO hi\n") {
        StepKind::AssignCapture { var, cmd } => {
            assert_eq!(var, "x");
            assert!(
                matches!(cmd.as_ref(), StepKind::Echo(_)),
                "expected ECHO body, got {cmd:?}"
            );
        }
        other => panic!("expected AssignCapture, got {other:?}"),
    }
}

#[test]
fn let_capture_run_command() {
    match parse_one("LET $out = RUN \"echo hi\"\n") {
        StepKind::AssignCapture { var, cmd } => {
            assert_eq!(var, "out");
            assert!(matches!(cmd.as_ref(), StepKind::Run(_)));
        }
        other => panic!("expected AssignCapture, got {other:?}"),
    }
}

#[test]
fn let_capture_unknown_lead_stays_expression() {
    // Uppercase-but-unknown leads must not break expression assignment.
    match parse_one("LET $x = MY_VAR\n") {
        StepKind::Assign { var, .. } => assert_eq!(var, "x"),
        other => panic!("expected Assign, got {other:?}"),
    }
    match parse_one("LET $x = foo\n") {
        StepKind::Assign { var, .. } => assert_eq!(var, "x"),
        other => panic!("expected Assign, got {other:?}"),
    }
    match parse_one("LET $d = 30s\n") {
        StepKind::Assign { var, .. } => assert_eq!(var, "d"),
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn let_capture_func_call_stays_expression() {
    // `GLOB("...")` starts with an uppercase lead but the paren form is an
    // expression; the end-guard must route it to let_statement.
    match parse_one("LET $files = GLOB(\"*.txt\")\n") {
        StepKind::Assign { var, .. } => assert_eq!(var, "files"),
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn let_capture_unknown_command_with_args_stays_error() {
    let err = parse_script("LET $x = FROBNICATE hi\n", mock_lower)
        .expect_err("unknown command with args must fail");
    assert!(
        err.to_string().contains("FROBNICATE"),
        "unexpected error: {err}"
    );
}

#[test]
fn let_capture_await_binds_await_capture() {
    match parse_one("LET $o = AWAIT $t\n") {
        StepKind::AwaitCapture { out_var, task_var } => {
            assert_eq!(out_var, "o");
            assert_eq!(task_var, "t");
        }
        other => panic!("expected AwaitCapture, got {other:?}"),
    }
}

#[test]
fn let_capture_timeout_wrapper() {
    match parse_one("LET $x = TIMEOUT 5s ECHO hi\n") {
        StepKind::AssignCapture { var, cmd } => {
            assert_eq!(var, "x");
            assert!(
                matches!(cmd.as_ref(), StepKind::Timeout { .. }),
                "expected TIMEOUT body, got {cmd:?}"
            );
        }
        other => panic!("expected AssignCapture, got {other:?}"),
    }
}

#[test]
fn let_capture_rejects_inline_async() {
    let err = parse_script("LET $x = TIMEOUT 5s ASYNC ECHO hi\n", mock_lower)
        .expect_err("inline ASYNC in capture must fail");
    assert!(err.to_string().contains("ASYNC"), "unexpected error: {err}");
}

#[test]
fn let_async_still_binds_task_handle() {
    // No regression: ASYNC-led lines still produce AssignAsync.
    let steps = parse_script("LET $t = ASYNC ECHO hi\n", mock_lower).expect("parse ASYNC");
    assert!(
        matches!(steps[0].kind, StepKind::AssignAsync { .. }),
        "expected AssignAsync, got {:?}",
        steps[0].kind
    );
}

#[test]
fn let_capture_display_round_trip() {
    for script in [
        "LET $x = ECHO hi\n",
        "LET $o = AWAIT $t\n",
        "LET $x = TIMEOUT 5s ECHO hi\n",
    ] {
        let steps = parse_script(script, mock_lower).expect("parse");
        let rendered = steps
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let reparsed = parse_script(&rendered, mock_lower).expect("reparse");
        assert_eq!(steps, reparsed, "round-trip failed for {script}");
    }
}

#[test]
fn let_capture_example_block() {
    let script = indoc! {r#"
        LET $name = "world"
        LET $out = ECHO hi
        LET $t = ASYNC ECHO done
        LET $o = AWAIT $t
    "#};
    let steps = parse_script(script, mock_lower).expect("parse mixed block");
    assert_eq!(steps.len(), 4);
    assert!(matches!(steps[0].kind, StepKind::Assign { .. }));
    assert!(matches!(steps[1].kind, StepKind::AssignCapture { .. }));
    assert!(matches!(steps[2].kind, StepKind::AssignAsync { .. }));
    assert!(matches!(steps[3].kind, StepKind::AwaitCapture { .. }));
}
