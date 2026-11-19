use crate::common::mock_lower;

use oxdock_parser::ast::{Arg, StepKind};
use oxdock_parser::parse_script;

fn fail_case(script: &str) -> (Option<Arg>, Option<Arg>, Vec<oxdock_parser::ast::Step>) {
    let steps = parse_script(script, mock_lower).expect("parse FAIL");
    assert_eq!(steps.len(), 1, "expected a single FAIL step");
    match &steps[0].kind {
        StepKind::Fail {
            code,
            message,
            body,
        } => (code.clone(), message.clone(), body.clone()),
        other => panic!("expected Fail, got {other:?}"),
    }
}

fn lit(text: &str) -> Arg {
    Arg::String(text.to_string(), false)
}

#[test]
fn fail_message_only() {
    let (code, message, body) = fail_case(r#"FAIL "boom""#);
    assert_eq!(code, None);
    assert_eq!(message, Some(Arg::String("boom".to_string(), true)));
    assert!(body.is_empty());
}

#[test]
fn fail_code_and_message() {
    let (code, message, body) = fail_case(r#"FAIL 2 "boom""#);
    assert_eq!(code, Some(lit("2")));
    assert_eq!(message, Some(Arg::String("boom".to_string(), true)));
    assert!(body.is_empty());
}

#[test]
fn fail_lone_bare_int_is_a_silent_code() {
    // Quoting decides: bare `2` exits silently, quoted `"2"` prints.
    let (code, message, _) = fail_case("FAIL 2");
    assert_eq!(code, Some(lit("2")));
    assert_eq!(message, None);
    let (code, message, _) = fail_case(r#"FAIL "2""#);
    assert_eq!(code, None);
    assert_eq!(message, Some(Arg::String("2".to_string(), true)));
}

#[test]
fn fail_block_only() {
    let (code, message, body) = fail_case("FAIL { ECHO usage }");
    assert_eq!(code, None);
    assert_eq!(message, None);
    assert_eq!(body.len(), 1);
    assert!(matches!(body[0].kind, StepKind::Echo(_)));
}

#[test]
fn fail_message_with_block() {
    let (code, message, body) = fail_case("FAIL \"boom\" { ECHO usage }");
    assert_eq!(code, None);
    assert_eq!(message, Some(Arg::String("boom".to_string(), true)));
    assert_eq!(body.len(), 1);
}

#[test]
fn fail_code_message_with_block() {
    let (code, message, body) = fail_case("FAIL 2 \"boom\" { ECHO usage }");
    assert_eq!(code, Some(lit("2")));
    assert_eq!(message, Some(Arg::String("boom".to_string(), true)));
    assert_eq!(body.len(), 1);
}

#[test]
fn fail_requires_message_or_block() {
    assert!(parse_script("FAIL", mock_lower).is_err());
}

#[test]
fn fail_rejects_three_args() {
    assert!(parse_script("FAIL 1 \"a\" \"b\"", mock_lower).is_err());
}

#[test]
fn fail_display_round_trips() {
    // Canonical forms only: Display normalizes quoting (a safe word
    // renders bare even when written quoted, like ECHO), so the
    // round-trip pin uses spellings that are already canonical.
    for script in [
        "FAIL boom",
        "FAIL 2 boom",
        "FAIL 2",
        "FAIL \"cargo not found\"",
        "FAIL { ECHO usage }",
        "FAIL boom { ECHO usage }",
        "FAIL 2 boom { ECHO usage }",
    ] {
        let steps = parse_script(script, mock_lower).expect("parse");
        let rendered: Vec<String> = steps.iter().map(|s| s.to_string()).collect();
        let reparsed = parse_script(&rendered.join("\n"), mock_lower).expect("reparse");
        assert_eq!(steps, reparsed, "Display round-trip failed for {script}");
    }
}

#[test]
fn fail_display_normalizes_safe_quoting() {
    let steps = parse_script("FAIL \"boom\"", mock_lower).expect("parse");
    assert_eq!(steps[0].to_string(), "FAIL boom");
}

#[test]
fn fail_numeric_message_keeps_quotes_and_round_trips() {
    // Quoting decides: a bare int-like message must stay quoted or
    // the reparse would invert it into a silent code.
    for script in ["FAIL \"2\"", "FAIL 2 \"2\""] {
        let steps = parse_script(script, mock_lower).expect("parse");
        let rendered: Vec<String> = steps.iter().map(|s| s.to_string()).collect();
        let reparsed = parse_script(&rendered.join("\n"), mock_lower).expect("reparse");
        assert_eq!(steps, reparsed, "Display round-trip failed for {script}");
    }
    let quoted = parse_script("FAIL \"2\"", mock_lower).expect("parse");
    assert_eq!(quoted[0].to_string(), "FAIL \"2\"");
}

#[test]
fn fail_empty_block_parses() {
    let (code, message, body) = fail_case("FAIL {}");
    assert_eq!(code, None);
    assert_eq!(message, None);
    assert!(body.is_empty());
    let steps = parse_script("FAIL {}", mock_lower).expect("parse");
    let rendered: Vec<String> = steps.iter().map(|s| s.to_string()).collect();
    assert_eq!(rendered, vec!["FAIL {}".to_string()]);
    let reparsed = parse_script(&rendered.join("\n"), mock_lower).expect("reparse");
    assert_eq!(steps, reparsed);
}

#[test]
fn fail_rejects_non_int_code() {
    let err = parse_script("FAIL banana \"msg\"", mock_lower).expect_err("non-int code");
    assert!(
        err.to_string().contains("int"),
        "expected int error, got: {err}"
    );
}
