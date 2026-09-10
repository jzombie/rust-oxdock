//! End-to-end unified string values: ENV/EXPAND overrides resolve exactly like
//! every other command's free text (bare `$var` evaluates, `{{ }}`
//! interpolates, quoted whitespace is exact).

use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, PathResolver};

fn run_script(root: &GuardedPath, script: &str) -> Result<(), anyhow::Error> {
    let steps = oxdock_core::parse_script(script).expect("parse script");
    run_steps_with_context_result_with_io(root, root, &steps, ExecIo::new()).map(|_| ())
}

fn read_trimmed(root: &GuardedPath, rel: &str) -> String {
    let path = root.join(rel).unwrap();
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    resolver.read_to_string(&path).unwrap().trim().to_string()
}

#[test]
fn env_quoted_spaces_round_trip() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
            ENV SET_FORTH="outer scope"
            WRITE out.txt "{{ env:SET_FORTH }}"
        "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "outer scope");
}

#[test]
fn env_bare_variable_evaluates() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
            LET $who: STRING = "Alice"
            ENV GREETING=$who
            WRITE out.txt "{{ env:GREETING }}"
        "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "Alice");
}

#[test]
fn env_template_value_interpolates() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
            LET $who: STRING = "Alice Smith"
            ENV GREETING="{{ $who }}!"
            WRITE out.txt "{{ env:GREETING }}"
        "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "Alice Smith!");
}

#[test]
fn env_preserves_non_string_expr_types() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
            LET $pair: LIST = [1, 2]
            ENV PAIR=$pair
            WRITE out.txt "{{ env:PAIR }}"
        "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "1 2");
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
            ASSERT_STDOUT "Alice hello"
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
            ASSERT_STDOUT "Hello Alice Smith!"
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
            ASSERT_STDOUT "Hi Bob!"
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
            ASSERT_STDOUT "Hi Carol!!"
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
            ASSERT_STDOUT "Hi Ada Lovelace!"
        "#},
    )
    .unwrap();
}
