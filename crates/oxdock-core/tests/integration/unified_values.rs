//! End-to-end unified string values: ENV/EXPAND overrides resolve exactly like
//! every other command's free text (bare `$var` evaluates, `{{ }}`
//! interpolates, quoted whitespace is exact).

use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::GuardedPath;

fn run_script(root: &GuardedPath, script: &str) -> Result<(), anyhow::Error> {
    let steps = oxdock_core::parse_script(script).expect("parse script");
    run_steps_with_context_result_with_io(root, root, &steps, ExecIo::new()).map(|_| ())
}

fn run_script_captured(root: &GuardedPath, script: &str) -> Result<String, anyhow::Error> {
    let steps = oxdock_core::parse_script(script).expect("parse script");
    let captured: std::sync::Arc<std::sync::Mutex<Vec<u8>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut io_cfg = ExecIo::new();
    io_cfg.set_stdout(Some(captured.clone()));
    run_steps_with_context_result_with_io(root, root, &steps, io_cfg).map(|_| ())?;
    let bytes = captured.lock().unwrap().clone();
    Ok(String::from_utf8(bytes).expect("captured stdout is valid UTF-8"))
}

#[test]
fn env_quoted_spaces_round_trip() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let out = run_script_captured(
        &root,
        indoc! {r#"
            ENV SET_FORTH="outer scope"
            ECHO "{{ env:SET_FORTH }}"
        "#},
    )
    .unwrap();
    assert_eq!(out, "outer scope\n");
}

#[test]
fn env_bare_variable_evaluates() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let out = run_script_captured(
        &root,
        indoc! {r#"
            LET $who: STRING = "Alice"
            ENV GREETING=$who
            ECHO "{{ env:GREETING }}"
        "#},
    )
    .unwrap();
    assert_eq!(out, "Alice\n");
}

#[test]
fn env_template_value_interpolates() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let out = run_script_captured(
        &root,
        indoc! {r#"
            LET $who: STRING = "Alice Smith"
            ENV GREETING="{{ $who }}!"
            ECHO "{{ env:GREETING }}"
        "#},
    )
    .unwrap();
    assert_eq!(out, "Alice Smith!\n");
}

#[test]
fn env_preserves_non_string_expr_types() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let out = run_script_captured(
        &root,
        indoc! {r#"
            LET $pair: LIST = [1, 2]
            ENV PAIR=$pair
            ECHO "{{ env:PAIR }}"
        "#},
    )
    .unwrap();
    assert_eq!(out, "1 2\n");
}

#[test]
fn echo_mixed_variable_keeps_value() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
            LET $who: STRING = "Alice"
            ECHO $who hello
            ASSERT_CONTAINS stdout "Alice hello"
        "#},
    )
    .unwrap();
}

#[test]
fn expand_override_with_spaces() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
            WRITE template.md "Hello \{{ env:NAME }}!"
            EXPAND template.md NAME="Alice Smith"
            ASSERT_CONTAINS stdout "Hello Alice Smith!"
        "#},
    )
    .unwrap();
}

#[test]
fn expand_bare_variable_override() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
            LET $who: STRING = "Bob"
            WRITE template.md "Hi \{{ env:WHO }}!"
            EXPAND template.md WHO=$who
            ASSERT_CONTAINS stdout "Hi Bob!"
        "#},
    )
    .unwrap();
}

#[test]
fn expand_template_override() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
            LET $who: STRING = "Carol"
            WRITE template.md "Hi \{{ env:WHO }}!"
            EXPAND template.md WHO="{{ $who }}!!"
            ASSERT_CONTAINS stdout "Hi Carol!!"
        "#},
    )
    .unwrap();
}

#[test]
fn expand_multi_assignment_overrides_resolve() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
            LET $first: STRING = "Ada"
            LET $last: STRING = "Lovelace"
            WRITE template.md "Hi \{{ env:FIRST }} \{{ env:LAST }}!"
            EXPAND template.md FIRST=$first LAST=$last
            ASSERT_CONTAINS stdout "Hi Ada Lovelace!"
        "#},
    )
    .unwrap();
}
