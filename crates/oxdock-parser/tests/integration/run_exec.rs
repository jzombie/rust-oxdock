use indoc::indoc;
use oxdock_parser::ast::{Arg, Expr, StepKind, Value};
use oxdock_parser::{lower_command, parse_script};

use crate::common::mock_lower;

fn parse_prod(script: &str) -> Vec<oxdock_parser::Step> {
    parse_script(script, lower_command).expect("parse with production lower")
}

fn single_kind(script: &str) -> StepKind {
    let steps = parse_prod(script);
    assert_eq!(steps.len(), 1, "expected one step for {script:?}");
    steps.into_iter().next().unwrap().kind
}

#[test]
fn run_exec_form_parses_quoted_list() {
    match single_kind(r#"RUN ["echo", "hi"]"#) {
        StepKind::RunExec { argv } => {
            assert_eq!(argv.len(), 2);
            assert_eq!(
                argv[0],
                Arg::Expr(Expr::Literal(Value::String("echo".to_string())))
            );
            assert_eq!(
                argv[1],
                Arg::Expr(Expr::Literal(Value::String("hi".to_string())))
            );
        }
        other => panic!("expected RunExec, got {other:?}"),
    }
}

#[test]
fn run_exec_form_allows_no_spaces_and_single_quotes() {
    match single_kind(r#"RUN ["echo","hi"]"#) {
        StepKind::RunExec { argv } => assert_eq!(argv.len(), 2),
        other => panic!("expected RunExec, got {other:?}"),
    }
    match single_kind("RUN ['echo', 'hi']") {
        StepKind::RunExec { argv } => {
            assert_eq!(
                argv[0],
                Arg::Expr(Expr::Literal(Value::String("echo".to_string())))
            );
        }
        other => panic!("expected RunExec, got {other:?}"),
    }
}

#[test]
fn run_exec_form_supports_typed_elements() {
    // Bare words stay strings (only `true`/`false` become bools);
    // numeric literals bind Int/Float; `$var`
    // and templates keep their typed/deferred forms for runtime coercion.
    match single_kind(r#"RUN ["prog", $name, 3, true, "{{ env:FOO }}"]"#) {
        StepKind::RunExec { argv } => {
            assert_eq!(argv.len(), 5);
            assert!(matches!(&argv[1], Arg::Expr(Expr::Var(name)) if name == "name"));
            assert_eq!(argv[2], Arg::Expr(Expr::Literal(Value::Int(3))));
            assert_eq!(argv[3], Arg::Expr(Expr::Literal(Value::Bool(true))));
            assert_eq!(
                argv[4],
                Arg::Expr(Expr::Literal(Value::String("{{ env:FOO }}".to_string())))
            );
        }
        other => panic!("expected RunExec, got {other:?}"),
    }
}

#[test]
fn run_exec_form_supports_keypath_and_call_elements() {
    match single_kind(r#"RUN ["prog", $a.b, GLOB("*.txt")]"#) {
        StepKind::RunExec { argv } => {
            assert_eq!(argv.len(), 3);
            assert!(
                matches!(&argv[1], Arg::Expr(Expr::KeyPath { base, keys }) if base == "a" && keys == &["b".to_string()])
            );
            assert!(matches!(&argv[2], Arg::Expr(Expr::Call { name, .. }) if name == "GLOB"));
        }
        other => panic!("expected RunExec, got {other:?}"),
    }
}

#[test]
fn run_exec_form_preserves_backslash_escapes_verbatim() {
    // Escape processing is deferred to runtime: the AST keeps source bytes.
    match single_kind(r#"RUN ["a\"b\\c"]"#) {
        StepKind::RunExec { argv } => {
            assert_eq!(
                argv[0],
                Arg::Expr(Expr::Literal(Value::String("a\\\"b\\\\c".to_string())))
            );
        }
        other => panic!("expected RunExec, got {other:?}"),
    }
    // ...so Display round-trips escaped sources byte-identical.
    let kind = single_kind(r#"RUN ["a\"b\\c"]"#);
    assert_eq!(single_kind(&kind.to_string()), kind);
}

#[test]
fn run_exec_form_keeps_shell_metachars_literal() {
    // `;` and `//` inside list elements must not split steps or comments.
    let steps = parse_prod(r#"RUN ["echo", "a; b // c"]"#);
    assert_eq!(steps.len(), 1, "semicolon must stay inside the element");
    match &steps[0].kind {
        StepKind::RunExec { argv } => {
            assert_eq!(
                argv[1],
                Arg::Expr(Expr::Literal(Value::String("a; b // c".to_string())))
            );
        }
        other => panic!("expected RunExec, got {other:?}"),
    }
}

#[test]
fn run_exec_form_rejects_empty_and_bare_run() {
    let err = parse_script("RUN []", lower_command).expect_err("RUN [] must fail");
    assert!(
        format!("{err:#}").contains("RUN requires"),
        "unexpected error: {err:#}"
    );
    let err = parse_script("RUN", lower_command).expect_err("bare RUN must fail");
    assert!(
        format!("{err:#}").contains("requires argument"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn run_shell_form_is_unchanged() {
    match single_kind("RUN echo hello") {
        StepKind::Run(arg) => assert_eq!(arg.as_str(), "echo hello"),
        other => panic!("expected shell Run, got {other:?}"),
    }
    // POSIX test brackets stay on the shell path, never exec form.
    match single_kind("RUN [ -f /etc/passwd ]") {
        StepKind::Run(arg) => assert_eq!(arg.as_str(), "[ -f /etc/passwd ]"),
        other => panic!("expected shell Run, got {other:?}"),
    }
}

#[test]
fn run_exec_interception_yields_typed_list_with_mock_lower() {
    // The grammar routes exec form to `lower` as one typed list arg instead
    // of a stringified bracket blob, even for the grammar-only mock.
    let steps = parse_script(r#"RUN ["echo", "hi"]"#, mock_lower).expect("parse");
    match &steps[0].kind {
        StepKind::Run(arg) => match arg {
            Arg::Expr(Expr::List(elems)) => assert_eq!(elems.len(), 2),
            other => panic!("expected Expr::List, got {other:?}"),
        },
        other => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn run_exec_partial_span_is_rejected_not_truncated() {
    // A second bracket group after the list must fail the whole script —
    // the grammar engine requires total span consumption, so `["hi"]`
    // can never be silently discarded.
    parse_script(r#"RUN ["echo"] ["hi"]"#, lower_command).expect_err("trailing span must fail");
}

#[test]
fn run_exec_accepts_guards() {
    let steps = parse_prod(indoc! {r#"
        ENV FOO="1"
        [env:FOO] RUN ["echo", "hi"]
    "#});
    assert_eq!(steps.len(), 2);
    assert!(steps[1].guard.is_some(), "guard must attach to exec form");
    assert!(
        matches!(&steps[1].kind, StepKind::RunExec { .. }),
        "expected RunExec, got {:?}",
        steps[1].kind
    );
}

#[test]
fn run_exec_display_round_trips() {
    for script in [
        r#"RUN ["echo", "hi"]"#,
        r#"RUN ["echo", "a; b // c"]"#,
        r#"RUN ["prog", $name, "3", true]"#,
    ] {
        let kind = single_kind(script);
        let rendered = kind.to_string();
        assert!(
            rendered.starts_with("RUN ["),
            "exec Display must keep bracket form, got {rendered:?}"
        );
        let again = single_kind(&rendered);
        assert_eq!(kind, again, "round-trip failed for {script:?}");
    }
}

#[test]
fn run_exec_shell_display_round_trips() {
    let kind = single_kind("RUN echo hello");
    let rendered = kind.to_string();
    assert_eq!(rendered, "RUN echo hello");
    assert_eq!(single_kind(&rendered), kind);
}

#[test]
fn run_exec_wraps_in_structural_commands() {
    let script = indoc! {r#"
        WITH_IO [stdout=pipe:cap] RUN ["cargo", "--version"]
    "#};
    let steps = parse_prod(script);
    match &steps[0].kind {
        StepKind::WithIo { cmd, .. } => {
            assert!(matches!(cmd.as_ref(), StepKind::RunExec { .. }));
        }
        other => panic!("expected WITH_IO wrapper, got {other:?}"),
    }

    let steps = parse_prod(r#"ASYNC RUN ["cargo", "--version"]"#);
    match &steps[0].kind {
        StepKind::AsyncBlock { body } => {
            assert!(matches!(&body[0].kind, StepKind::RunExec { .. }));
        }
        other => panic!("expected ASYNC wrapper, got {other:?}"),
    }

    let steps = parse_prod(r#"TIMEOUT 10s RUN ["cargo", "--version"]"#);
    match &steps[0].kind {
        StepKind::Timeout { body, .. } => {
            assert!(matches!(&body[0].kind, StepKind::RunExec { .. }));
        }
        other => panic!("expected TIMEOUT wrapper, got {other:?}"),
    }
}
