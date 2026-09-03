mod common;
use common::mock_lower;

use indoc::indoc;
use oxdock_parser::ast::{IoStream, StepKind};
use oxdock_parser::parse_script;

#[test]
fn with_io_block_wraps_commands() {
    let script = indoc! {r#"
        WITH_IO [stdout=pipe:setup] {
            RUN "echo alpha"
            WITH_IO [stderr=pipe:setup] RUN "echo beta"
        }
    "#};

    let steps = parse_script(script, mock_lower).expect("parse WITH_IO block");
    assert_eq!(steps.len(), 2, "expected two commands inside WITH_IO block");

    let first = &steps[0].kind;
    match first {
        StepKind::WithIo { bindings, cmd } => {
            assert_eq!(
                bindings.len(),
                1,
                "stdout default should be applied exactly once"
            );
            assert_eq!(bindings[0].stream, IoStream::Stdout);
            assert_eq!(bindings[0].pipe.as_deref(), Some("setup"));
            match cmd.as_ref() {
                StepKind::Run(rendered) => {
                    assert_eq!(rendered.as_ref(), "echo alpha");
                }
                other => panic!("expected RUN inside WITH_IO, got {other:?}"),
            }
        }
        other => panic!("expected WITH_IO wrapper, got {other:?}"),
    }

    let second = &steps[1].kind;
    match second {
        StepKind::WithIo { bindings, cmd } => {
            assert_eq!(bindings.len(), 2, "default stdout plus stderr override");
            assert_eq!(bindings[0].stream, IoStream::Stdout);
            assert_eq!(bindings[0].pipe.as_deref(), Some("setup"));
            assert_eq!(bindings[1].stream, IoStream::Stderr);
            assert_eq!(bindings[1].pipe.as_deref(), Some("setup"));
            match cmd.as_ref() {
                StepKind::Run(rendered) => {
                    assert_eq!(rendered.as_ref(), "echo beta");
                }
                other => panic!("expected RUN inside WITH_IO, got {other:?}"),
            }
        }
        other => panic!("expected WITH_IO wrapper, got {other:?}"),
    }
}

#[test]
fn with_io_block_requires_brace() {
    let script = "WITH_IO [stdout=pipe:setup]\nRUN \"echo hi\"";
    let err =
        parse_script(script, mock_lower).expect_err("script should reject missing block braces");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("WITH_IO block must be followed"),
        "unexpected error: {msg}"
    );
}
