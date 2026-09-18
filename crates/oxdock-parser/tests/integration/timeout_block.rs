use crate::common::mock_lower;

use indoc::indoc;
use oxdock_parser::ast::{Arg, Expr, StepKind};
use oxdock_parser::commands::lower_command;
use oxdock_parser::parse_script;

fn timeout_case(script: &str) -> (Arg, Vec<oxdock_parser::ast::Step>) {
    let steps = parse_script(script, mock_lower).expect("parse TIMEOUT");
    assert_eq!(steps.len(), 1, "expected a single TIMEOUT step");
    match &steps[0].kind {
        StepKind::Timeout { duration, body } => (duration.clone(), body.clone()),
        other => panic!("expected Timeout, got {other:?}"),
    }
}

fn lit(text: &str) -> Arg {
    Arg::String(text.to_string(), false)
}

#[test]
fn timeout_inline_single_command() {
    let (duration, body) = timeout_case(r#"TIMEOUT 10s RUN "echo hi""#);
    assert_eq!(duration, lit("10s"));
    assert_eq!(body.len(), 1);
    assert!(matches!(body[0].kind, StepKind::Run(_)));
}

#[test]
fn timeout_duration_units() {
    let cases = [
        ("TIMEOUT 500ms ECHO x", lit("500ms")),
        ("TIMEOUT 10s ECHO x", lit("10s")),
        ("TIMEOUT 2m ECHO x", lit("2m")),
        ("TIMEOUT 1h ECHO x", lit("1h")),
        ("TIMEOUT 30 ECHO x", lit("30")),
    ];
    for (script, expected) in cases {
        let (duration, _) = timeout_case(script);
        assert_eq!(duration, expected, "wrong duration for {script}");
    }
}

#[test]
fn timeout_dynamic_forms() {
    // Variables, quoted literals, and templates resolve at runtime
    // instead of freezing at parse.
    let (duration, _) = timeout_case("TIMEOUT $d ECHO x");
    assert_eq!(duration, Arg::Expr(Expr::Var("d".to_string())));
    let (duration, _) = timeout_case("TIMEOUT \"10s\" ECHO x");
    assert_eq!(duration, Arg::String("10s".to_string(), true));
    let (duration, _) = timeout_case("TIMEOUT {{ $d }} ECHO x");
    assert_eq!(duration, lit("{{ $d }}"));
}

#[test]
fn timeout_block_multi_step() {
    let script = indoc! {r#"
        TIMEOUT 2m {
            WRITE a.txt x
            ECHO done
        }
    "#};
    let (duration, body) = timeout_case(script);
    assert_eq!(duration, lit("2m"));
    assert_eq!(body.len(), 2);
}

#[test]
fn timeout_wraps_await() {
    let (duration, body) = timeout_case("TIMEOUT 30s AWAIT $task");
    assert_eq!(duration, lit("30s"));
    assert_eq!(body.len(), 1);
    assert!(matches!(body[0].kind, StepKind::Await { .. }));
}

#[test]
fn timeout_nests_inside_async_and_with_io() {
    let steps = parse_script(
        "WITH_IO [stdout=$p] ASYNC TIMEOUT 5s RUN \"echo x\"",
        mock_lower,
    )
    .expect("parse nested TIMEOUT");
    assert_eq!(steps.len(), 1);
    match &steps[0].kind {
        StepKind::WithIo { cmd, .. } => match cmd.as_ref() {
            StepKind::AsyncBlock { body } => {
                assert_eq!(body.len(), 1);
                assert!(matches!(body[0].kind, StepKind::Timeout { .. }));
            }
            other => panic!("expected AsyncBlock, got {other:?}"),
        },
        other => panic!("expected WithIo, got {other:?}"),
    }
}

#[test]
fn timeout_rejects_bad_durations() {
    for script in [
        "TIMEOUT 0s RUN x",
        "TIMEOUT 0 RUN x",
        "TIMEOUT banana RUN x",
    ] {
        let err = parse_script(script, mock_lower).expect_err("must reject {script}");
        assert!(
            err.to_string().contains("TIMEOUT"),
            "unexpected error for {script}: {err}"
        );
    }
}

#[test]
fn timeout_requires_body() {
    let err = parse_script("TIMEOUT 10s", mock_lower).expect_err("must require a body");
    assert!(!err.to_string().is_empty());
}

#[test]
fn timeout_display_round_trips() {
    for script in [
        "TIMEOUT 500ms ECHO hello",
        "TIMEOUT 30s AWAIT $task",
        // NOTE: inputs use Display-stable quoting (quote_arg quotes dotted
        // paths), so the round-trip comparison is exact.
        "TIMEOUT 2m {\nWRITE \"a.txt\" x\nECHO done\n}",
    ] {
        let steps = parse_script(script, mock_lower).expect("parse");
        let rendered: Vec<String> = steps.iter().map(|s| s.to_string()).collect();
        let reparsed = parse_script(&rendered.join("\n"), mock_lower).expect("reparse");
        assert_eq!(steps, reparsed, "Display round-trip failed for {script}");
    }
}

#[test]
fn sleep_lowers_duration() {
    let steps = parse_script("SLEEP 500ms", lower_command).expect("parse SLEEP");
    assert_eq!(steps.len(), 1);
    match &steps[0].kind {
        StepKind::Sleep { duration } => assert_eq!(*duration, lit("500ms")),
        other => panic!("expected Sleep, got {other:?}"),
    }
}

#[test]
fn sleep_requires_single_duration() {
    assert!(parse_script("SLEEP", lower_command).is_err());
    assert!(parse_script("SLEEP 1s 2s", lower_command).is_err());
    assert!(parse_script("SLEEP banana", lower_command).is_err());
}

#[test]
fn sleep_display_round_trips() {
    let steps = parse_script("SLEEP 2m", lower_command).expect("parse");
    let rendered: Vec<String> = steps.iter().map(|s| s.to_string()).collect();
    assert_eq!(rendered, vec!["SLEEP 2m".to_string()]);
    let reparsed = parse_script(&rendered.join("\n"), lower_command).expect("reparse");
    assert_eq!(steps, reparsed);
}

#[test]
fn timeout_wraps_sleep() {
    let steps = parse_script("TIMEOUT 1s SLEEP 30s", lower_command).expect("parse");
    assert_eq!(steps.len(), 1);
    match &steps[0].kind {
        StepKind::Timeout { duration, body } => {
            assert_eq!(*duration, lit("1s"));
            assert_eq!(body.len(), 1);
            assert!(matches!(body[0].kind, StepKind::Sleep { .. }));
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
}
