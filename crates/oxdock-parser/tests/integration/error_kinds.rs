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
    let err = parse_script(
        indoc! {r#"
        WITH_IO [stdout]
        ECHO hi
    "#},
        mock_lower,
    )
    .expect_err("must fail");
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
    let err = parse_script(
        indoc! {r#"
            FOR $k: BOOL, $v: STRING IN $m {
            ECHO hi
            }
        "#},
        mock_lower,
    )
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
    let err = parse_script(
        indoc! {r#"
        WITH_IO [stdout]
        ECHO hi
    "#},
        mock_lower,
    )
    .expect_err("must fail");
    assert_eq!(
        (err.line(), err.col_start(), err.col_end()),
        (1, Some(1), Some(16))
    );
    let text = err_text(&err);
    assert!(text.contains("^^^^^^^^^^^^^^^^"), "got: {text}");
}

#[test]
fn multi_line_unknown_reports_exact_line() {
    let err = parse_script(
        indoc! {r#"
            ECHO one
            FROBNICATE hi
            ECHO three
        "#},
        mock_lower,
    )
    .expect_err("must fail");
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
                invalid syntax for command LET: LET assigns a variable, e.g. `LET $name: STRING = <expr>`, `LET $t: HANDLE = ASYNC ...`, `LET $out: STRING = <command>` (capture), `LET $out: STRING = AWAIT $t`, or `LET $var: TYPE = { ... }` (inline block); got `$x = 1`.
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
                invalid syntax for command WITH_IO: WITH_IO needs `WITH_IO [bindings] <command>` or `WITH_IO [bindings] { <commands> }`: invalid binding `stdout=discard`; bindings are `stdin`, `stdout`, `stderr`, or `<stream>=$var` with a PIPE-typed variable (e.g. `[stdout=$p]`, `[stdin=$p]`); got `[stdout=discard] ECHO hi`.
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

#[test]
fn block_error_inside_if_reports_inner_failure() {
    // Issue #215 acceptance: an unbalanced block inside `IF` must not
    // report the enclosing `IF` head. The diagnostic names the unclosed
    // block and its opener line, past the head, with no grammar internals
    // leaking into the text.
    let err = parse_script(
        indoc! {r#"
            LET $go: BOOL = true
            IF $go {
                WHILE $go {
                    $go = false
                ECHO "done"
            }
        "#},
        lower_command,
    )
    .expect_err("unbalanced WHILE must fail");
    assert!(
        matches!(err.kind(), ParseErrorKind::PestParse),
        "unexpected kind: {:?}",
        err.kind()
    );
    assert!(
        err.line() > 2,
        "error must land past the IF head (line 2), got line {}: {err}",
        err.line()
    );
    assert!(
        !err.expected().is_empty(),
        "inner failure must name its expectations: {err}"
    );
    let text = err_text(&err);
    assert!(
        !text.contains("invalid syntax for command IF"),
        "inner span must not blame IF syntax: {text}"
    );
    assert!(
        !text.contains("expected:"),
        "no grammar rule dump in user text: {text}"
    );
    assert!(
        text.contains("unclosed `IF` block opened on line 2"),
        "got: {text}"
    );
}

#[test]
fn block_error_inside_while_reports_inner_failure() {
    // Same shape at top level: a missing `}` reports the unclosed block
    // past the `WHILE` head, not invalid `WHILE` syntax.
    let err = parse_script(
        indoc! {r#"
            LET $go: BOOL = true
            WHILE $go {
                $go = false
            ECHO "done"
        "#},
        lower_command,
    )
    .expect_err("unbalanced WHILE must fail");
    assert!(
        matches!(err.kind(), ParseErrorKind::PestParse),
        "unexpected kind: {:?}",
        err.kind()
    );
    assert!(
        err.line() > 2,
        "error must land past the WHILE head (line 2), got line {}: {err}",
        err.line()
    );
    let text = err_text(&err);
    assert!(
        !text.contains("invalid syntax for command WHILE"),
        "inner span must not blame WHILE syntax: {text}"
    );
    assert!(
        text.contains("unclosed `WHILE` block opened on line 2"),
        "got: {text}"
    );
}

#[test]
fn fragmented_block_matrix_reports_inner_failure() {
    // Issue #215 property sweep: every keyword-led block statement whose
    // body breaks reports a genuine inner grammar failure past its head
    // line, never the head itself. Each entry gives its head line.
    let cases: &[(&str, &str, usize)] = &[
        (
            "if-while",
            indoc! {r#"
                LET $go: BOOL = true
                IF $go {
                    WHILE $go {
                        $go = false
                    ECHO "done"
                }
            "#},
            2,
        ),
        (
            "if-for",
            indoc! {r#"
                LET $go: BOOL = true
                IF $go {
                    FOR $x: STRING IN ["a"] {
                        ECHO hi
                    ECHO "done"
                }
            "#},
            2,
        ),
        (
            "while-if",
            indoc! {r#"
                LET $go: BOOL = true
                WHILE $go {
                    IF $go {
                        ECHO hi
                    $go = false
                }
            "#},
            2,
        ),
        (
            "while-top",
            indoc! {r#"
                LET $go: BOOL = true
                WHILE $go {
                    $go = false
                ECHO "done"
            "#},
            2,
        ),
        (
            "for-top",
            indoc! {r#"
                FOR $x: STRING IN ["a"] {
                    ECHO hi
            "#},
            1,
        ),
        (
            "func",
            indoc! {r#"
                FUNC GREET($n: STRING) {
                    ECHO hi
            "#},
            1,
        ),
        (
            "timeout",
            indoc! {r#"
                TIMEOUT 30s {
                    ECHO hi
            "#},
            1,
        ),
        (
            "remote",
            indoc! {r#"
                REMOTE prod {
                    ECHO hi
            "#},
            1,
        ),
        (
            "async",
            indoc! {r#"
                ASYNC {
                    ECHO hi
            "#},
            1,
        ),
        (
            "if-missing-close",
            indoc! {r#"
                LET $go: BOOL = true
                IF $go {
                    ECHO hi
            "#},
            2,
        ),
        (
            "if-else",
            indoc! {r#"
                IF true {
                    ECHO hi
                } ELSE {
                    ECHO broken
            "#},
            3,
        ),
    ];
    for (name, script, head_line) in cases {
        let err = match parse_script(script, lower_command) {
            Err(err) => err,
            Ok(_) => panic!("case {name}: unbalanced block must fail"),
        };
        assert!(
            matches!(err.kind(), ParseErrorKind::PestParse),
            "case {name}: expected an inner grammar failure, got {:?}: {err}",
            err.kind()
        );
        assert!(
            err.line() > *head_line,
            "case {name}: error must land past the head (line {head_line}), got line {}: {err}",
            err.line()
        );
        assert!(
            err.col_start().is_some() && err.col_end().is_some(),
            "case {name}: error must carry a column span: {err}"
        );
        assert!(
            err.source_line().is_some(),
            "case {name}: error must carry its source line: {err}"
        );
        let text = err_text(&err);
        assert!(
            !text.contains("expected:"),
            "case {name}: no grammar rule dump in user text: {text}"
        );
        assert!(
            text.contains("block opened on line"),
            "case {name}: message must name the block and its opener: {text}"
        );
        assert!(
            text.contains("--> line") && text.contains('^'),
            "case {name}: error must render a caret block: {text}"
        );
    }
}

#[test]
fn triple_nested_block_reports_past_innermost_head() {
    // Depth check: with three nested blocks and the innermost unclosed,
    // the span must clear not just the outer `IF` but the innermost
    // `FOR` head (line 4) as well.
    let err = parse_script(
        indoc! {r#"
            LET $go: BOOL = true
            IF $go {
                WHILE $go {
                    FOR $x: STRING IN ["a"] {
                        ECHO hi
                    $go = false
                }
            }
        "#},
        lower_command,
    )
    .expect_err("unbalanced FOR must fail");
    assert!(
        matches!(err.kind(), ParseErrorKind::PestParse),
        "unexpected kind: {:?}",
        err.kind()
    );
    assert!(
        err.line() > 4,
        "error must land past the innermost FOR head (line 4), got line {}: {err}",
        err.line()
    );
}

#[test]
fn fragmented_block_errors_match_snapshots() {
    // Hand-rolled snapshots (the crate carries no snapshot dependency):
    // exact outputs for the representative shapes, so any drift in
    // position or wording fails here first. (`trim_end` drops indoc
    // formatting whitespace, never parser output. The `N | ` rows carry
    // a significant trailing space: the terminal line is empty.)
    let cases: &[(&str, &str)] = &[
        (
            indoc! {r#"
                LET $go: BOOL = true
                IF $go {
                    WHILE $go {
                        $go = false
                    ECHO "done"
                }
            "#},
            indoc! {"
                unclosed `IF` block opened on line 2
                  --> line 7, col 1-1
                  7 | 
                    | ^
            "},
        ),
        (
            indoc! {r#"
                LET $go: BOOL = true
                WHILE $go {
                    $go = false
                ECHO "done"
            "#},
            indoc! {r#"
                unclosed `WHILE` block opened on line 2
                  --> line 5, col 1-1
                  5 | 
                    | ^
            "#},
        ),
        (
            indoc! {r#"
                IF true {
                    ECHO hi
                } ELSE {
                    ECHO broken
            "#},
            indoc! {"
                unclosed `ELSE` block opened on line 3
                  --> line 5, col 1-1
                  5 | 
                    | ^
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
fn missing_block_opener_keeps_head_span() {
    // A head with no `{` on its line is a head error, not a fragmented
    // block: the fault lies on line 1 and the span stays there.
    let err =
        parse_script("IF $go\n    ECHO hi\n", lower_command).expect_err("missing opener must fail");
    assert!(
        matches!(
            err.kind(),
            ParseErrorKind::InvalidSyntax { command } if command == "IF"
        ),
        "unexpected kind: {:?}",
        err.kind()
    );
    assert_eq!(err.line(), 1);
    let text = err_text(&err);
    assert!(
        text.contains("invalid syntax for command IF"),
        "got: {text}"
    );
}

#[test]
fn comment_brace_keeps_head_span() {
    // A `{` inside a comment is not a block opener: the opener check
    // reads the lexer token stream, so comment text cannot trick it
    // into refinement and the head span stands.
    let err = parse_script(
        indoc! {r#"
            IF $go # {
                ECHO hi
            }
        "#},
        lower_command,
    )
    .expect_err("missing opener must fail");
    assert_eq!(err.line(), 1, "comment brace must not move the span: {err}");
}

#[test]
fn dangling_else_if_keeps_head_span() {
    // A complete but dangling `ELSE IF` (no enclosing `IF`) is a head
    // error: the clause matches as a prefix, the sibling clause fails on
    // line 1, and no bogus deep error is selected.
    let err = parse_script(
        indoc! {r#"
            ELSE IF $c {
                ECHO hi
            }
        "#},
        lower_command,
    )
    .expect_err("dangling ELSE must fail");
    assert!(
        matches!(
            err.kind(),
            ParseErrorKind::InvalidSyntax { command } if command == "ELSE"
        ),
        "unexpected kind: {:?}",
        err.kind()
    );
    assert_eq!(err.line(), 1);
}

#[test]
fn dangling_unclosed_else_if_reports_inner_failure() {
    // The unclosed variant refines past the head to the terminal line,
    // naming the `ELSE` block and its opener.
    let err = parse_script(
        indoc! {r#"
            ELSE IF $c {
                ECHO broken
        "#},
        lower_command,
    )
    .expect_err("unclosed ELSE must fail");
    assert!(
        matches!(err.kind(), ParseErrorKind::PestParse),
        "unexpected kind: {:?}",
        err.kind()
    );
    assert!(
        err.line() > 1,
        "error must land past the ELSE head (line 1), got line {}: {err}",
        err.line()
    );
    let text = err_text(&err);
    assert!(
        text.contains("unclosed `ELSE` block opened on line 1"),
        "got: {text}"
    );
}

#[test]
fn inner_malformed_head_keeps_inner_span() {
    // A malformed head nested in a balanced outer block is blamed
    // precisely: the `FOR` without a variable fails on its own line
    // through the ordinary lowering path, untouched by refinement.
    let err = parse_script(
        indoc! {r#"
            LET $go: BOOL = true
            IF $go {
                FOR foo {
                    ECHO hi
                }
            }
        "#},
        lower_command,
    )
    .expect_err("malformed FOR must fail");
    assert!(
        matches!(
            err.kind(),
            ParseErrorKind::InvalidSyntax { command } if command == "FOR"
        ),
        "unexpected kind: {:?}",
        err.kind()
    );
    assert_eq!(err.line(), 3);
}

#[test]
fn broken_head_with_brace_keeps_head_span() {
    // A brace on the head line does not force refinement: when the head
    // itself is malformed, the structural rules fail on line 1 and the
    // head span stands.
    for script in ["FOR foo {\n    ECHO hi\n}\n", "LET FOO {\n    ECHO hi\n}\n"] {
        let err = parse_script(script, lower_command).expect_err("must fail");
        assert_eq!(
            err.line(),
            1,
            "bad head must stay on line 1 for {script:?}: {err}"
        );
    }
}

#[test]
fn inner_command_errors_keep_inner_span() {
    // Untouched path: a balanced outer block with a bad inner command
    // still blames the inner command on its own line.
    let err = parse_script(
        indoc! {r#"
            IF $go {
                ECHO hi
            }
            FROBNICATE x
        "#},
        lower_command,
    )
    .expect_err("must fail");
    assert!(
        matches!(
            err.kind(),
            ParseErrorKind::UnknownCommand { name } if name == "FROBNICATE"
        ),
        "unexpected kind: {:?}",
        err.kind()
    );
    assert_eq!(err.line(), 4);
}

#[test]
fn single_line_keyword_errors_keep_head_span() {
    // The refinement only fires across lines: single-line keyword
    // failures keep their exact head spans byte-for-byte.
    let cases: &[(&str, &str)] = &[
        (
            "FOR foo\n",
            indoc! {"
                invalid syntax for command FOR: FOR loops need `FOR $item: TYPE IN <expr> { ... }` (or `FOR $key: STRING, $value: TYPE IN <expr> { ... }`); got `foo`.
                  --> line 1, col 1-7
                  1 | FOR foo
                    | ^^^^^^^
            "},
        ),
        (
            "WHILE\n",
            indoc! {"
                invalid syntax for command WHILE: WHILE needs a Bool condition and a block, e.g. `WHILE !$done { ... }`; got nothing.
                  --> line 1, col 1-5
                  1 | WHILE
                    | ^^^^^
            "},
        ),
        (
            "ELSE\n",
            indoc! {"
                invalid syntax for command ELSE: ELSE must directly follow an `IF ... { ... }` block, e.g. `IF true { ECHO yes } ELSE { ECHO no }`; got nothing.
                  --> line 1, col 1-4
                  1 | ELSE
                    | ^^^^
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
fn valid_nested_blocks_still_parse() {
    // The fixed repro with its brace restored prints `done`: valid
    // behavior is unchanged, and the balanced nesting lowers to two steps.
    let steps = parse_script(
        indoc! {r#"
            LET $go: BOOL = true
            IF $go {
                WHILE $go {
                    $go = false
                }
                ECHO "done"
            }
        "#},
        lower_command,
    )
    .expect("balanced nesting must parse");
    assert_eq!(steps.len(), 2);
}
