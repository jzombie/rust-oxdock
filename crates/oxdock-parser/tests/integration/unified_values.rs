//! Unified string-value parsing: every command's free-text value flows through
//! the single grammar-level `assignment` rule plus `lower_command_value`.
//! These tests pin the matrix from the unification plan.

use oxdock_parser::ast::{Arg, ArgPart, Expr};
use oxdock_parser::commands::lower_command;
use oxdock_parser::{StepKind, parse_script};

fn parse_one(script: &str) -> StepKind {
    let steps = parse_script(script, lower_command).expect("parse script");
    assert_eq!(steps.len(), 1, "expected one step for {script}");
    steps.into_iter().next().unwrap().kind
}

fn parse_err(script: &str) -> String {
    parse_script(script, lower_command)
        .expect_err("script must fail")
        .to_string()
}

fn round_trip(script: &str) -> String {
    let kind = parse_one(script);
    let rendered = kind.to_string();
    let reparsed = parse_one(&rendered);
    assert_eq!(
        reparsed.to_string(),
        rendered,
        "Display must be stable for {script}"
    );
    rendered
}

#[test]
fn env_quoted_value_with_spaces() {
    match parse_one(r#"ENV SET_FORTH="outer scope""#) {
        StepKind::Env { key, value } => {
            assert_eq!(key, "SET_FORTH");
            assert_eq!(value, Arg::String("outer scope".to_string(), true));
        }
        other => panic!("expected Env, saw {other:?}"),
    }
    match parse_one("ENV SET_FORTH='outer scope'") {
        StepKind::Env { key, value } => {
            assert_eq!(key, "SET_FORTH");
            assert_eq!(value, Arg::String("outer scope".to_string(), true));
        }
        other => panic!("expected Env, saw {other:?}"),
    }
}

#[test]
fn env_quoted_whitespace_is_byte_exact() {
    match parse_one(r#"ENV K="a   b""#) {
        StepKind::Env { value, .. } => {
            assert_eq!(value, Arg::String("a   b".to_string(), true));
        }
        other => panic!("expected Env, saw {other:?}"),
    }
    match parse_one("ENV K=\"a\tb\"") {
        StepKind::Env { value, .. } => {
            assert_eq!(value, Arg::String("a\tb".to_string(), true));
        }
        other => panic!("expected Env, saw {other:?}"),
    }
}

#[test]
fn env_unquoted_multiword_joins_with_space() {
    match parse_one("ENV FOO=outer scope") {
        StepKind::Env { key, value } => {
            assert_eq!(key, "FOO");
            assert_eq!(value, Arg::String("outer scope".to_string(), false));
        }
        other => panic!("expected Env, saw {other:?}"),
    }
}

#[test]
fn env_equals_in_value_and_empty() {
    match parse_one("ENV A=b=c") {
        StepKind::Env { value, .. } => {
            assert_eq!(value, Arg::String("b=c".to_string(), false));
        }
        other => panic!("expected Env, saw {other:?}"),
    }
    match parse_one("ENV EMPTY=") {
        StepKind::Env { value, .. } => {
            assert_eq!(value, Arg::String(String::new(), false));
        }
        other => panic!("expected Env, saw {other:?}"),
    }
}

#[test]
fn env_bare_variable_stays_typed() {
    match parse_one("ENV SOME_ENV=$x") {
        StepKind::Env { value, .. } => {
            assert_eq!(value, Arg::Expr(Expr::Var("x".to_string())));
        }
        other => panic!("expected Env, saw {other:?}"),
    }
    match parse_one("ENV K=$a.b") {
        StepKind::Env { value, .. } => {
            assert!(matches!(value, Arg::Expr(Expr::KeyPath { .. })));
        }
        other => panic!("expected Env, saw {other:?}"),
    }
}

#[test]
fn env_template_and_tail_is_literal_text() {
    match parse_one(r#"ENV K="{{ $x }} other stuff""#) {
        StepKind::Env { value, .. } => {
            assert_eq!(value, Arg::String("{{ $x }} other stuff".to_string(), true));
        }
        other => panic!("expected Env, saw {other:?}"),
    }
}

#[test]
fn env_quoted_dollar_is_literal() {
    // Only `{{ }}` interpolates inside quoted values; bare `$` stays literal.
    match parse_one(r#"ENV PATH="/usr/bin:$HOME""#) {
        StepKind::Env { value, .. } => {
            assert_eq!(value, Arg::String("/usr/bin:$HOME".to_string(), true));
        }
        other => panic!("expected Env, saw {other:?}"),
    }
}

#[test]
fn env_rejects_malformed() {
    assert!(parse_err("ENV FOO").contains("KEY=value"));
    assert!(parse_err("ENV =v").contains("KEY=value"));
}

#[test]
fn expand_override_with_spaces_is_one_override() {
    match parse_one(r#"EXPAND FILE="testing 1 2 3""#) {
        StepKind::Expand { path, overrides } => {
            assert_eq!(path, None);
            assert_eq!(overrides.len(), 1);
            assert_eq!(overrides[0].0, "FILE");
            assert_eq!(
                overrides[0].1,
                Arg::String("testing 1 2 3".to_string(), true)
            );
        }
        other => panic!("expected Expand, saw {other:?}"),
    }
}

#[test]
fn expand_path_plus_override() {
    match parse_one("EXPAND tmpl.txt FILE=World") {
        StepKind::Expand { path, overrides } => {
            assert_eq!(path, Some(Arg::String("tmpl.txt".to_string(), false)));
            assert_eq!(overrides.len(), 1);
            assert_eq!(overrides[0].0, "FILE");
        }
        other => panic!("expected Expand, saw {other:?}"),
    }
}

#[test]
fn expand_bare_variable_forms() {
    // Override value evaluates like ECHO $x.
    match parse_one("EXPAND tmpl.txt FILE=$x") {
        StepKind::Expand { overrides, .. } => {
            assert_eq!(overrides[0].1, Arg::Expr(Expr::Var("x".to_string())));
        }
        other => panic!("expected Expand, saw {other:?}"),
    }
    // Lone variable path stays a typed path.
    match parse_one("EXPAND $x") {
        StepKind::Expand { path, .. } => {
            assert_eq!(path, Some(Arg::Expr(Expr::Var("x".to_string()))));
        }
        other => panic!("expected Expand, saw {other:?}"),
    }
    // Template override keeps its braces for expand_string.
    match parse_one("EXPAND tmpl.txt FILE={{ $x }}") {
        StepKind::Expand { overrides, .. } => {
            assert_eq!(overrides[0].1, Arg::String("{{ $x }}".to_string(), false));
        }
        other => panic!("expected Expand, saw {other:?}"),
    }
}

#[test]
fn expand_quoted_path_with_equals_is_a_path() {
    match parse_one(r#"EXPAND "my tmpl.txt" FILE=x"#) {
        StepKind::Expand { path, overrides } => {
            assert_eq!(path, Some(Arg::String("my tmpl.txt".to_string(), true)));
            assert_eq!(overrides.len(), 1);
        }
        other => panic!("expected Expand, saw {other:?}"),
    }
}

#[test]
fn expand_rejects_two_paths() {
    assert!(
        parse_err("EXPAND a.txt b.txt").contains("at most one path"),
        "two bare paths must still fail"
    );
}

#[test]
fn expand_multi_assignment_splits_uniformly() {
    // Typed values stay typed across sibling assignments.
    match parse_one("EXPAND K1=$x K2=$y") {
        StepKind::Expand { path, overrides } => {
            assert_eq!(path, None);
            assert_eq!(overrides.len(), 2);
            assert_eq!(overrides[0].0, "K1");
            assert_eq!(overrides[0].1, Arg::Expr(Expr::Var("x".to_string())));
            assert_eq!(overrides[1].0, "K2");
            assert_eq!(overrides[1].1, Arg::Expr(Expr::Var("y".to_string())));
        }
        other => panic!("expected Expand, saw {other:?}"),
    }
    // Plain values split identically : no shape-dependent tokenizing.
    match parse_one("EXPAND K1=1 K2=2") {
        StepKind::Expand { overrides, .. } => {
            assert_eq!(overrides.len(), 2);
            assert_eq!(overrides[0].1, Arg::String("1".to_string(), false));
            assert_eq!(overrides[1].1, Arg::String("2".to_string(), false));
        }
        other => panic!("expected Expand, saw {other:?}"),
    }
    // Quoted multi-word values still hold their spaces beside siblings.
    match parse_one(r#"EXPAND A="x y" B=z"#) {
        StepKind::Expand { overrides, .. } => {
            assert_eq!(overrides.len(), 2);
            assert_eq!(overrides[0].1, Arg::String("x y".to_string(), true));
        }
        other => panic!("expected Expand, saw {other:?}"),
    }
}

#[test]
fn env_rejects_second_assignment() {
    // `A=1 B=2` is two assignments; ENV takes exactly one : loud error,
    // never a silent merge or drop.
    assert!(
        parse_err("ENV A=1 B=2").contains("KEY=value"),
        "second assignment must fail"
    );
}

#[test]
fn echo_mixed_variable_keeps_expression() {
    match parse_one("ECHO $x hello") {
        StepKind::Echo(Arg::Parts(parts)) => {
            assert_eq!(
                parts,
                vec![
                    ArgPart::Expr(Expr::Var("x".to_string())),
                    ArgPart::Text(" ".to_string(), false),
                    ArgPart::Text("hello".to_string(), false),
                ]
            );
        }
        other => panic!("expected Echo Parts, saw {other:?}"),
    }
    // All-literal tails keep the legacy single-String shape.
    match parse_one("ECHO a  b") {
        StepKind::Echo(value) => {
            assert_eq!(value, Arg::String("a b".to_string(), false));
        }
        other => panic!("expected Echo, saw {other:?}"),
    }
}

#[test]
fn display_round_trips_unified_values() {
    for script in [
        r#"ENV SET_FORTH="outer scope""#,
        "ENV SOME_ENV=$x",
        r#"ENV K="{{ $x }} other stuff""#,
        "ENV A=b=c",
        r#"EXPAND FILE="testing 1 2 3""#,
        "EXPAND tmpl.txt FILE=$x",
        "EXPAND $x",
        "ECHO $x hello",
    ] {
        round_trip(script);
    }
}
