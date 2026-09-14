use crate::common::mock_lower;

use indoc::indoc;
use oxdock_parser::ast::StepKind;
use oxdock_parser::parse_script;

fn cancel_case(script: &str) -> String {
    let steps = parse_script(script, mock_lower).expect("parse CANCEL");
    assert_eq!(steps.len(), 1, "expected a single CANCEL step");
    match &steps[0].kind {
        StepKind::Cancel { var } => var.clone(),
        other => panic!("expected Cancel, got {other:?}"),
    }
}

#[test]
fn cancel_parses_variable() {
    assert_eq!(cancel_case("CANCEL $task"), "task");
}

#[test]
fn cancel_inside_async_single_command() {
    let steps = parse_script("ASYNC CANCEL $task", mock_lower).expect("parse ASYNC CANCEL");
    assert_eq!(steps.len(), 1);
    match &steps[0].kind {
        StepKind::AsyncBlock { body } => {
            assert_eq!(body.len(), 1);
            assert!(matches!(&body[0].kind, StepKind::Cancel { .. }));
        }
        other => panic!("expected AsyncBlock, got {other:?}"),
    }
}

#[test]
fn cancel_inside_async_block() {
    let script = indoc! {r#"
        ASYNC {
            CANCEL $task
            ECHO done
        }
    "#};
    let steps = parse_script(script, mock_lower).expect("parse ASYNC block");
    assert_eq!(steps.len(), 1);
    match &steps[0].kind {
        StepKind::AsyncBlock { body } => {
            assert_eq!(body.len(), 2);
            assert!(matches!(&body[0].kind, StepKind::Cancel { .. }));
        }
        other => panic!("expected AsyncBlock, got {other:?}"),
    }
}

#[test]
fn cancel_inside_timeout() {
    let steps = parse_script("TIMEOUT 5s CANCEL $task", mock_lower).expect("parse TIMEOUT CANCEL");
    assert_eq!(steps.len(), 1);
    match &steps[0].kind {
        StepKind::Timeout { body, .. } => {
            assert_eq!(body.len(), 1);
            assert!(matches!(&body[0].kind, StepKind::Cancel { .. }));
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
}

#[test]
fn cancel_inside_timeout_block() {
    let script = indoc! {r#"
        TIMEOUT 5s {
            CANCEL $task
        }
    "#};
    let steps = parse_script(script, mock_lower).expect("parse TIMEOUT block");
    match &steps[0].kind {
        StepKind::Timeout { body, .. } => {
            assert_eq!(body.len(), 1);
            assert!(matches!(&body[0].kind, StepKind::Cancel { .. }));
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
}

#[test]
fn let_async_wraps_cancel_and_timeout() {
    let steps = parse_script("LET $a: HANDLE = ASYNC CANCEL $b", mock_lower)
        .expect("parse LET ASYNC CANCEL");
    assert_eq!(steps.len(), 1);
    match &steps[0].kind {
        StepKind::AssignAsync { var, body, .. } => {
            assert_eq!(var, "a");
            assert_eq!(body.len(), 1);
            assert!(matches!(&body[0].kind, StepKind::Cancel { .. }));
        }
        other => panic!("expected AssignAsync, got {other:?}"),
    }

    let steps = parse_script(
        "LET $a: HANDLE = ASYNC TIMEOUT 5s RUN \"echo hi\"",
        mock_lower,
    )
    .expect("parse LET ASYNC TIMEOUT");
    match &steps[0].kind {
        StepKind::AssignAsync { var, body, .. } => {
            assert_eq!(var, "a");
            assert_eq!(body.len(), 1);
            assert!(matches!(&body[0].kind, StepKind::Timeout { .. }));
        }
        other => panic!("expected AssignAsync, got {other:?}"),
    }
}

#[test]
fn timeout_wraps_async() {
    let steps =
        parse_script("TIMEOUT 5s ASYNC RUN \"echo hi\"", mock_lower).expect("parse TIMEOUT ASYNC");
    match &steps[0].kind {
        StepKind::Timeout { body, .. } => {
            assert_eq!(body.len(), 1);
            assert!(matches!(&body[0].kind, StepKind::AsyncBlock { .. }));
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
}

#[test]
fn cancel_requires_variable() {
    assert!(parse_script("CANCEL", mock_lower).is_err());
    assert!(parse_script("CANCEL task", mock_lower).is_err());
}

#[test]
fn cancel_display_round_trips() {
    for script in [
        "CANCEL $task",
        "ASYNC CANCEL $task",
        "TIMEOUT 5s CANCEL $task",
    ] {
        let steps = parse_script(script, mock_lower).expect("parse");
        let rendered: Vec<String> = steps.iter().map(|s| s.to_string()).collect();
        let reparsed = parse_script(&rendered.join("\n"), mock_lower).expect("reparse");
        assert_eq!(steps, reparsed, "Display round-trip failed for {script}");
    }
}
