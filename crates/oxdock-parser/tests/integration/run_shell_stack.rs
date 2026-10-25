//! Release-script shell lines must lower to their exact shell text:
//! Docker-style bare words (`&&`, `||`, `>`, quoted paths, `{{ }}`
//! templates) stay literal text instead of compiling to expressions.
//! Lowering errors inside LET-capture also keep their real file line:
//! the removed nested re-parse reported line 1 for everything.
use indoc::indoc;
use oxdock_parser::ast::Arg;
use oxdock_parser::{StepKind, lower_command, parse_script};

fn shell_text(step: &oxdock_parser::Step) -> &str {
    let StepKind::AssignCapture { cmd, .. } = &step.kind else {
        panic!("expected AssignCapture, got {:?}", step.kind);
    };
    let StepKind::Run(arg) = cmd.as_ref() else {
        panic!("expected Run, got {:?}", cmd);
    };
    let Arg::String(text, _) = arg else {
        panic!("shell args must lower to String, got {arg:?}");
    };
    text
}

#[test]
fn release_shell_lines_lower_to_exact_shell_text() {
    let script = indoc! {r#"
        LET $probe: STRING = RUN cd "{{ $probedir }}" && cargo info "{{ $pname }}@{{ $pver }}" >/dev/null 2>&1 || printf OXDOCK_PUBLISH_MISSING
        LET $diag: STRING = RUN cat "{{ $probedir }}/publish-round.log"
        LET $head: STRING = RUN git rev-parse HEAD
        LET $round_out: STRING = RUN cargo publish --workspace{{ $excludes }} >"{{ $probedir }}/publish-round.log" 2>&1 || printf OXDOCK_PUBLISH_ROUND_FAILED
    "#};
    let steps = parse_script(script, lower_command).expect("release lines must parse");
    assert_eq!(
        steps.len(),
        4,
        "got: {:?}",
        steps.iter().map(|s| &s.kind).collect::<Vec<_>>()
    );
    assert_eq!(
        shell_text(&steps[0]),
        r#"cd {{ $probedir }} && cargo info {{ $pname }}@{{ $pver }} >/dev/null 2>&1 || printf OXDOCK_PUBLISH_MISSING"#,
    );
    assert_eq!(
        shell_text(&steps[1]),
        r#"cat {{ $probedir }}/publish-round.log"#
    );
    assert_eq!(shell_text(&steps[2]), "git rev-parse HEAD");
    assert_eq!(
        shell_text(&steps[3]),
        r#"cargo publish --workspace{{ $excludes }} >"{{ $probedir }}/publish-round.log" 2>&1 || printf OXDOCK_PUBLISH_ROUND_FAILED"#,
    );
}

#[test]
fn let_capture_lowering_error_reports_real_line() {
    let script = indoc! {r#"
        ECHO one
        ECHO two
        LET $x: STRING = RUN
    "#};
    let err = parse_script(script, lower_command).expect_err("bare RUN must fail lowering");
    assert_eq!(err.line(), 3, "got: {err}");
    assert!(err.to_string().contains("requires argument"), "got: {err}");
}
