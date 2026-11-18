use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, GuardedTempDir};

fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
    temp.as_guarded_path().clone()
}

fn run_script(root: &GuardedPath, script: &str) -> Result<(), anyhow::Error> {
    let steps = oxdock_core::parse_script(script).expect("parse script");
    run_steps_with_context_result_with_io(root, root, &steps, ExecIo::new()).map(|_| ())
}

fn run_err(root: &GuardedPath, script: &str) -> String {
    let steps = oxdock_core::parse_script(script).expect("parse script");
    run_steps_with_context_result_with_io(root, root, &steps, ExecIo::new())
        .unwrap_err()
        .to_string()
}

#[test]
fn parse_args_pair_fills_float_and_int_defaults() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        # The pair coerces through TYPE FLAGS in one declaration;
        # absent defaulted flags fill, including FLOAT defaults.
        TYPE FLAGS = MAP<ratio: FLOAT = 2.5, retries: INT = -2, model: STRING>
        TYPE POSITIONS = LIST<STRING>
        TYPE PARSED = MAP<flags: FLAGS, positionals?: POSITIONS>
        LET $parsed: PARSED = PARSE_ARGS(["--model=x", "serve"])
        LET $flags: FLAGS = $parsed.flags
        ASSERT_EQ $flags.model "x"
        LET $ratio_ok: BOOL = $flags.ratio == 2.5
        ASSERT_EQ $ratio_ok true
        LET $retries_ok: BOOL = $flags.retries == 0 - 2
        ASSERT_EQ $retries_ok true
        ASSERT_EQ $parsed.positionals.0 "serve"
    "#};
    run_script(&root, script).expect("pair fills defaults");
}

#[test]
fn parse_flags_returns_map_directly_with_float_default() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        # Flags only returns the flags MAP directly: no pair
        # wrapper, no positionals.
        TYPE FLAGS = MAP<ratio: FLOAT = 2.5, model: STRING>
        LET $flags: FLAGS = PARSE_FLAGS(["--model=x"])
        ASSERT_EQ $flags.model "x"
        LET $ratio_ok: BOOL = $flags.ratio == 2.5
        ASSERT_EQ $ratio_ok true
    "#};
    run_script(&root, script).expect("flags only binds directly");
}

#[test]
fn parse_positionals_needs_no_schema() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        # Positionals are always strings in order: no schema, no
        # TYPE FLAGS needed.
        TYPE POSITIONS = LIST<STRING>
        LET $pos: POSITIONS = PARSE_POSITIONALS(["serve", "--", "--not-a-flag"])
        ASSERT_EQ $pos.0 "serve"
        ASSERT_EQ $pos.1 "--not-a-flag"
    "#};
    run_script(&root, script).expect("positionals need no schema");
}

#[test]
fn parse_args_flags_only_shape_omits_positionals() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        # Empty positionals are omitted, so a flags only shape
        # binds clean.
        TYPE FLAGS = MAP<model: STRING>
        TYPE PARSED = MAP<flags: FLAGS>
        LET $parsed: PARSED = PARSE_ARGS(["--model=x"])
        LET $flags: FLAGS = $parsed.flags
        ASSERT_EQ $flags.model "x"
    "#};
    run_script(&root, script).expect("flags only shape binds clean");
}

#[test]
fn let_inline_defaults_fill() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        # LET annotations take the same `= literal` tails as TYPE
        # aliases: the assignment `=` is the depth-zero one.
        TYPE FLAGS = MAP<k: STRING = "d">
        LET $m: MAP<k: STRING = "d", n: INT = 3> = PARSE_FLAGS([])
        ASSERT_EQ $m.n 3
        LET $flags: FLAGS = PARSE_FLAGS([])
        ASSERT_EQ $flags.k "d"
    "#};
    run_script(&root, script).expect("inline defaults fill");
}

#[test]
fn parse_args_missing_required_and_unknown_flag_fail() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let missing = indoc! {r#"
        IMPORT [STD]
        TYPE FLAGS = MAP<model: STRING>
        TYPE PARSED = MAP<flags: FLAGS>
        LET $parsed: PARSED = PARSE_ARGS([])
    "#};
    let err = run_err(&root, missing);
    assert!(err.contains("missing required flag --model"), "{err}");
    let unknown = indoc! {r#"
        IMPORT [STD]
        TYPE FLAGS = MAP<model: STRING>
        TYPE PARSED = MAP<flags: FLAGS>
        LET $parsed: PARSED = PARSE_ARGS(["--model=x", "--nope=1"])
    "#};
    let err = run_err(&root, unknown);
    assert!(err.contains("unknown flag --nope"), "{err}");
}
