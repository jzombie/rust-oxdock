mod common;
use common::mock_lower;

use indoc::indoc;
use oxdock_parser::ast::{IoStream, StepKind};
use oxdock_parser::parse_script;

#[test]
fn let_async_with_io_single_command_binds_task() {
    let script = indoc! {r#"
        LET $task = WITH_IO [stdin=pipe:in_chan] ASYNC WRITE "inline_direct.txt"
    "#};

    let steps = parse_script(script, mock_lower).expect("parse LET WITH_IO ASYNC");
    assert_eq!(steps.len(), 1, "expected a single AssignAsync step");
    match &steps[0].kind {
        StepKind::AssignAsync { var, body } => {
            assert_eq!(var, "task");
            assert_eq!(body.len(), 1, "expected a single WITH_IO step in the body");
            match &body[0].kind {
                StepKind::WithIo { bindings, cmd } => {
                    assert_eq!(bindings.len(), 1);
                    assert_eq!(bindings[0].stream, IoStream::Stdin);
                    assert_eq!(bindings[0].pipe.as_deref(), Some("in_chan"));
                    assert!(
                        matches!(cmd.as_ref(), StepKind::Write { .. }),
                        "expected WRITE inside WITH_IO, got {cmd:?}"
                    );
                }
                other => panic!("expected WITH_IO body, got {other:?}"),
            }
        }
        other => panic!("expected AssignAsync, got {other:?}"),
    }
}

#[test]
fn let_async_with_io_rejects_non_async_command() {
    let script = indoc! {r#"
        LET $task = WITH_IO [stdin=pipe:in_chan] RUN "echo hi"
    "#};

    let err = parse_script(script, mock_lower).expect_err("non-ASYNC LET WITH_IO must fail");
    assert!(
        err.to_string().contains("requires an ASYNC command"),
        "unexpected error: {err}"
    );
}

#[test]
fn let_async_with_io_rejects_block_body() {
    let script = indoc! {r#"
        LET $task = WITH_IO [stdin=pipe:in_chan] ASYNC {
            WRITE "a.txt"
            WRITE "b.txt"
        }
    "#};

    let err = parse_script(script, mock_lower).expect_err("block body must fail");
    assert!(
        err.to_string().contains("accepts a single command"),
        "unexpected error: {err}"
    );
}
