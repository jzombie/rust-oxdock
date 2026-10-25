//! LET-capture used to re-parse the single instruction line nested inside
//! the whole-file parse, doubling native stack usage and tripping pest's
//! stack guard on small-stack runners (musl static binary) with
//! `stack limit reached`. It now lowers the already-parsed pair directly.
//! Observable proof, with no threads involved: lowering errors carry the
//! real file line. The re-parse reported line 1 for everything.
use indoc::indoc;
use oxdock_parser::{lower_command, parse_script};

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
