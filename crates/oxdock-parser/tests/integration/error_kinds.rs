//! Regression tests for issue #143: syntax errors must never surface
//! as a bare `unknown command`, and every string path error carries a
//! line, column span, source line, and caret block.
use crate::common::mock_lower;
use indoc::indoc;
use oxdock_parser::{ParseErrorKind, lower_command, parse_script};

fn err_text(err: &oxdock_parser::ParseError) -> String {
    err.to_string()
}

#[test]
fn truly_unknown_command_reports_kind_with_span() {
    let err = parse_script("FROBNICATE hi\n", mock_lower).expect_err("must fail");
    assert!(
        matches!(err.kind(), ParseErrorKind::UnknownCommand { name } if name == "FROBNICATE"),
        "unexpected kind: {:?}",
        err.kind()
    );
    assert_eq!(err.line(), 1);
    assert!(err.col_start().is_some() && err.col_end().is_some());
    assert!(err.source_line().is_some());
    let text = err_text(&err);
    assert!(text.contains("unknown command: FROBNICATE"), "got: {text}");
    assert!(text.contains("--> line 1"), "got: {text}");
    assert!(text.contains('^'), "got: {text}");
}

#[test]
fn malformed_with_io_is_invalid_syntax_never_unknown() {
    // Strict anti-pattern: WITH_IO must always explain the binding
    // failure, never claim the command does not exist.
    let err =
        parse_script("WITH_IO [stdout=discard] ECHO hi\n", lower_command).expect_err("must fail");
    assert!(
        matches!(
            err.kind(),
            ParseErrorKind::InvalidSyntax { command } if command == "WITH_IO"
        ),
        "unexpected kind: {:?}",
        err.kind()
    );
    let text = err_text(&err);
    assert!(
        text.contains("invalid syntax for command WITH_IO"),
        "got: {text}"
    );
    assert!(!text.contains("unknown command"), "got: {text}");
    assert!(text.contains("--> line 1"), "got: {text}");
}

#[test]
fn with_io_missing_bracket_hint_is_invalid_syntax() {
    let err =
        parse_script("WITH_IO [stdout=pipe:log ECHO hi\n", lower_command).expect_err("must fail");
    assert!(
        matches!(
            err.kind(),
            ParseErrorKind::InvalidSyntax { command } if command == "WITH_IO"
        ),
        "unexpected kind: {:?}",
        err.kind()
    );
    let text = err_text(&err);
    assert!(text.contains("missing closing `]`"), "got: {text}");
}

#[test]
fn bare_structural_keywords_are_invalid_syntax() {
    for (script, command) in [
        ("LET $x = 1\n", "LET"),
        ("FOR foo\n", "FOR"),
        ("AWAIT ECHO hi\n", "AWAIT"),
    ] {
        let err = parse_script(script, lower_command).expect_err("must fail");
        assert!(
            matches!(
                err.kind(),
                ParseErrorKind::InvalidSyntax { command: c } if c == command
            ),
            "script {script:?} gave kind {:?}",
            err.kind()
        );
        let text = err_text(&err);
        assert!(!text.contains("unknown command"), "got: {text}");
    }
}

#[test]
fn lowercase_command_is_parse_error_with_uppercase_note() {
    let err = parse_script("echo hi\n", mock_lower).expect_err("must fail");
    assert!(
        matches!(err.kind(), ParseErrorKind::PestParse),
        "unexpected kind: {:?}",
        err.kind()
    );
    let text = err_text(&err);
    assert!(text.contains("parse error"), "got: {text}");
    assert!(text.contains("command must be uppercase"), "got: {text}");
    assert!(text.contains("expected `ECHO`"), "got: {text}");
}

#[test]
fn with_io_missing_block_brace_is_structural_with_caret() {
    let err = parse_script("WITH_IO [stdout]\nECHO hi\n", mock_lower).expect_err("must fail");
    assert!(
        matches!(err.kind(), ParseErrorKind::Structural { .. }),
        "unexpected kind: {:?}",
        err.kind()
    );
    let text = err_text(&err);
    assert!(
        text.contains("WITH_IO block must be followed"),
        "got: {text}"
    );
    assert!(text.contains("--> line 1"), "got: {text}");
    assert!(text.contains('^'), "got: {text}");
}

#[test]
fn for_key_type_error_points_at_type_tag() {
    // Sub-expression precision: the caret lands on `BOOL`, not the statement.
    let err = parse_script("FOR $k: BOOL, $v: STRING IN $m {\nECHO hi\n}\n", mock_lower)
        .expect_err("must fail");
    assert!(
        matches!(
            err.kind(),
            ParseErrorKind::Validation { command } if command == "FOR"
        ),
        "unexpected kind: {:?}",
        err.kind()
    );
    assert_eq!(
        (err.line(), err.col_start(), err.col_end()),
        (1, Some(9), Some(12))
    );
    let text = err_text(&err);
    assert!(text.contains("^^^"), "got: {text}");
}

#[test]
fn with_io_missing_brace_covers_statement_line() {
    let err = parse_script("WITH_IO [stdout]\nECHO hi\n", mock_lower).expect_err("must fail");
    assert_eq!(
        (err.line(), err.col_start(), err.col_end()),
        (1, Some(1), Some(16))
    );
    let text = err_text(&err);
    assert!(text.contains("^^^^^^^^^^^^^^^^"), "got: {text}");
}

#[test]
fn multi_line_unknown_reports_exact_line() {
    let err =
        parse_script("ECHO one\nFROBNICATE hi\nECHO three\n", mock_lower).expect_err("must fail");
    assert!(
        matches!(err.kind(), ParseErrorKind::UnknownCommand { .. }),
        "unexpected kind: {:?}",
        err.kind()
    );
    assert_eq!(err.line(), 2);
    let text = err_text(&err);
    assert!(text.contains("2 | FROBNICATE hi"), "got: {text}");
}

#[test]
fn documented_outputs_match_byte_for_byte() {
    // The exact outputs shown in the workspace README error handling
    // section. If these change, update the docs to match.
    // (`trim_end` drops indoc formatting whitespace, never parser output.)
    let cases = [
        (
            indoc! {"
                FROBNICATE hi
            "},
            indoc! {"
                unknown command: FROBNICATE
                  --> line 1, col 1-13
                  1 | FROBNICATE hi
                    | ^^^^^^^^^^^^^
            "},
        ),
        (
            indoc! {"
                LET $x = 1
            "},
            indoc! {"
                invalid syntax for command LET: LET assigns a variable, e.g. `LET $name: STRING = <expr>`, `LET $t: HANDLE = ASYNC ...`, `LET $out: STRING = <command>` (capture), or `LET $out: STRING = AWAIT $t`; got `$x = 1`.
                  --> line 1, col 1-10
                  1 | LET $x = 1
                    | ^^^^^^^^^^
            "},
        ),
        (
            indoc! {"
                echo hi
            "},
            indoc! {"
                parse error (expected: script)
                note: command must be uppercase: found `echo`, expected `ECHO`
                  --> line 1, col 1-1
                  1 | echo hi
                    | ^
            "},
        ),
        (
            indoc! {"
                WITH_IO [stdout=discard] ECHO hi
            "},
            indoc! {"
                invalid syntax for command WITH_IO: WITH_IO needs `WITH_IO [bindings] <command>` or `WITH_IO [bindings] { <commands> }`: invalid binding `stdout=discard`; bindings are `stdin`, `stdout`, `stderr`, or `<stream>=$var` with a PIPE-typed variable (e.g. `[stdout=$p]`, `[stdin=$p]`). `pipe:name` was removed; declare LET $x: PIPE and pass $x; got `[stdout=discard] ECHO hi`.
                  --> line 1, col 1-32
                  1 | WITH_IO [stdout=discard] ECHO hi
                    | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            "},
        ),
    ];
    for (script, expected) in cases {
        let err = parse_script(script, lower_command).expect_err("must fail");
        assert_eq!(
            err.to_string(),
            expected.trim_end(),
            "output drift for {script:?}"
        );
    }
}

#[test]
fn direct_lower_command_case_hint_stays_unknown_with_kind() {
    let err = lower_command("echo", vec![]).expect_err("must fail");
    assert!(
        matches!(
            err.kind(),
            ParseErrorKind::UnknownCommand { name } if name == "echo"
        ),
        "unexpected kind: {:?}",
        err.kind()
    );
    let text = err_text(&err);
    assert!(text.contains("did you mean `ECHO`"), "got: {text}");
}
