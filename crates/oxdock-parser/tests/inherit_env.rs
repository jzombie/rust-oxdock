mod common;
use common::mock_lower;

use oxdock_parser::{StepKind, parse_script};

#[test]
fn inherit_env_step_parses_leading_directive() {
    let script = "INHERIT_ENV [FOO, BAR]\nENV BAZ=qux";
    let steps = parse_script(script, mock_lower).expect("parse INHERIT_ENV directive");
    let StepKind::InheritEnv { keys } = &steps[0].kind else {
        panic!("expected INHERIT_ENV step");
    };
    assert_eq!(keys, &vec!["FOO".to_string(), "BAR".to_string()]);
    assert!(
        matches!(steps.get(1).map(|step| &step.kind), Some(StepKind::Env { key, .. }) if key == "BAZ")
    );
}

#[test]
fn inherit_env_must_appear_before_other_commands() {
    let script = "ENV FOO=1\nINHERIT_ENV [BAR]";
    let err = parse_script(script, mock_lower).expect_err("INHERIT_ENV after commands must fail");
    assert!(err.to_string().contains("before any other commands"));
}

#[test]
fn inherit_env_cannot_repeat() {
    let script = "INHERIT_ENV [FOO]\nINHERIT_ENV [BAR]";
    let err =
        parse_script(script, mock_lower).expect_err("multiple INHERIT_ENV directives must fail");
    assert!(err.to_string().contains("only one INHERIT_ENV"));
}

#[test]
fn inherit_env_cannot_be_guarded_or_nested() {
    let script = "[env:FOO]\nINHERIT_ENV [BAR]";
    let err = parse_script(script, mock_lower).expect_err("guarded INHERIT_ENV must fail");
    assert!(err.to_string().contains("cannot be guarded"));
}

#[test]
fn inherit_env_cannot_appear_inside_with_io() {
    let script = "WITH_IO [stdout=pipe:cap] INHERIT_ENV [BAR]";
    let err = parse_script(script, mock_lower).expect_err("nested INHERIT_ENV must fail");
    assert!(err.to_string().contains("cannot be nested"));
}

#[test]
fn inherit_env_cannot_be_inside_guard_block_braces() {
    let script = "[env:FOO] { INHERIT_ENV [BAR] }";
    let err =
        parse_script(script, mock_lower).expect_err("INHERIT_ENV inside guard block must fail");
    assert!(
        err.to_string().contains("cannot be guarded")
            || err.to_string().contains("cannot be nested")
    );
}

#[test]
fn inherit_env_cannot_be_inside_with_io_block_braces() {
    let script = "WITH_IO [stdout=pipe:cap] { INHERIT_ENV [BAR] }";
    let err =
        parse_script(script, mock_lower).expect_err("INHERIT_ENV inside WITH_IO block must fail");
    assert!(err.to_string().contains("cannot be nested"));
}

#[test]
fn inherit_env_cannot_be_deeply_nested_under_guards_and_io() {
    let script = "[env:FOO] { WITH_IO [stdout=pipe:cap] { INHERIT_ENV [BAR] } }";
    let err = parse_script(script, mock_lower).expect_err("deeply nested INHERIT_ENV must fail");
    assert!(
        err.to_string().contains("cannot be guarded")
            || err.to_string().contains("cannot be nested")
    );
}
