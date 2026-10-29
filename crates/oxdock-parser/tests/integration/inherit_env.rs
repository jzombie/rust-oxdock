use crate::common::mock_lower;

use indoc::indoc;
use oxdock_parser::{StepKind, parse_script};

#[test]
fn inherit_env_step_parses_leading_directive() {
    let script = indoc! {r#"
        INHERIT_ENV [FOO, BAR]
        ENV BAZ=qux"#};
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
    let script = indoc! {r#"
        ENV FOO=1
        INHERIT_ENV [BAR]"#};
    let err = parse_script(script, mock_lower).expect_err("INHERIT_ENV after commands must fail");
    assert!(err.to_string().contains("before any other commands"));
    // The span names the offending directive, not the end of the file.
    assert!(
        err.to_string().contains("line 2"),
        "error must point at the late INHERIT_ENV, got: {err}"
    );
}

#[test]
fn inherit_env_may_repeat_in_prelude() {
    // Several prelude directives merge: platform-specific names ride
    // guarded lines next to the common allowlist.
    let script = indoc! {r#"
        INHERIT_ENV [FOO]
        INHERIT_ENV [BAR]"#};
    let steps = parse_script(script, mock_lower).expect("repeated INHERIT_ENV parses");
    assert_eq!(steps.len(), 2);
    assert!(matches!(&steps[0].kind, StepKind::InheritEnv { .. }));
    assert!(matches!(&steps[1].kind, StepKind::InheritEnv { .. }));
}

#[test]
fn inherit_env_may_be_guarded_in_prelude() {
    // OS-specific names ride an explicit platform guard instead of
    // relying on absent-skips-silently.
    let script = indoc! {r#"
        INHERIT_ENV [FOO]
        [family:windows] INHERIT_ENV [BAR]"#};
    let steps = parse_script(script, mock_lower).expect("guarded INHERIT_ENV parses");
    assert_eq!(steps.len(), 2);
    assert!(steps[1].guard.is_some(), "guard must survive lowering");
    assert!(matches!(&steps[1].kind, StepKind::InheritEnv { .. }));
}

#[test]
fn inherit_env_cannot_appear_inside_with_io() {
    let script = "WITH_IO [stdout=$cap] INHERIT_ENV [BAR]";
    let err = parse_script(script, mock_lower).expect_err("nested INHERIT_ENV must fail");
    assert!(err.to_string().contains("cannot be nested"));
}

#[test]
fn inherit_env_inside_guard_block_braces_runs_conditionally() {
    // Braced guard blocks are conditional execution like guard lines:
    // allowed in the prelude position, rejected anywhere else.
    let script = "[env:FOO] { INHERIT_ENV [BAR] }";
    let steps = parse_script(script, mock_lower).expect("guard-block INHERIT_ENV parses");
    assert_eq!(steps.len(), 1);
}

#[test]
fn inherit_env_cannot_be_inside_with_io_block_braces() {
    let script = "WITH_IO [stdout=$cap] { INHERIT_ENV [BAR] }";
    let err =
        parse_script(script, mock_lower).expect_err("INHERIT_ENV inside WITH_IO block must fail");
    assert!(err.to_string().contains("cannot be nested"));
}

#[test]
fn inherit_env_cannot_be_deeply_nested_under_guards_and_io() {
    let script = "[env:FOO] { WITH_IO [stdout=$cap] { INHERIT_ENV [BAR] } }";
    let err = parse_script(script, mock_lower).expect_err("deeply nested INHERIT_ENV must fail");
    assert!(
        err.to_string().contains("cannot be guarded")
            || err.to_string().contains("cannot be nested")
    );
}
