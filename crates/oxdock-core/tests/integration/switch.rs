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

fn parse_err(script: &str) -> String {
    oxdock_core::parse_script(script).unwrap_err().to_string()
}

#[test]
fn switch_value_first_match_wins_with_comma_groups_and_calls() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # Discriminant "b" hits the second arm: comma groups share
        # one body, and call values compare like literals.
        FUNC CANON($s: STRING): STRING {
            RETURN $s
        }
        LET $cmd: STRING = "b"
        SWITCH $cmd {
            CASE "a" {
                WRITE hit.txt a
            }
            CASE "b", CANON("beta") {
                WRITE hit.txt b-or-beta
            }
            CASE "b" {
                WRITE hit.txt second-b
            }
            DEFAULT {
                WRITE hit.txt fallback
            }
        }
        LET $hit: STRING = READ hit.txt
        ASSERT_EQ $hit "b-or-beta"
    "#};
    run_script(&root, script).expect("first matching arm wins");
}

#[test]
fn switch_no_match_without_default_is_noop() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # Nothing matches and there is no DEFAULT: the switch is a
        # no-op and the script continues.
        LET $cmd: STRING = "zzz"
        SWITCH $cmd {
            CASE "a" {
                WRITE hit.txt a
            }
            CASE "b" {
                WRITE hit.txt b
            }
        }
        WRITE done.txt done
        LET $done: STRING = READ done.txt
        ASSERT_EQ $done "done"
        LET $t: STRING = STD::PATH_TYPE("hit.txt")
        ASSERT_EQ $t "absent"
    "#};
    run_script(&root, script).expect("no match without default is a no-op");
}

#[test]
fn switch_default_only_always_runs() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # A lone DEFAULT arm always runs.
        LET $cmd: STRING = "anything"
        SWITCH $cmd {
            DEFAULT {
                WRITE hit.txt fallback
            }
        }
        LET $hit: STRING = READ hit.txt
        ASSERT_EQ $hit "fallback"
    "#};
    run_script(&root, script).expect("default-only switch runs");
}

#[test]
fn switch_strict_cross_type_values_never_match() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # Strict ==: the STRING "1" never matches the INT 1, so the
        # DEFAULT arm runs instead.
        LET $cmd: STRING = "1"
        SWITCH $cmd {
            CASE 1 {
                EXIT 1
            }
            DEFAULT {
                WRITE hit.txt fallback
            }
        }
        LET $hit: STRING = READ hit.txt
        ASSERT_EQ $hit "fallback"
    "#};
    run_script(&root, script).expect("cross-type values never match");
}

#[test]
fn switch_numeric_values_compare_across_int_and_float() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # Numerics compare across INT and FLOAT like == does.
        LET $n: INT = 1
        SWITCH $n {
            CASE 1.0 {
                WRITE hit.txt numeric
            }
            DEFAULT {
                EXIT 1
            }
        }
        LET $hit: STRING = READ hit.txt
        ASSERT_EQ $hit "numeric"
    "#};
    run_script(&root, script).expect("numeric values match across types");
}

#[test]
fn switch_later_case_values_never_evaluate_after_a_hit() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # Evaluation stops at the first hit: the division by zero in
        # the later arm never runs. A variable divisor keeps the
        # expression from folding at parse time.
        LET $z: INT = 0
        LET $n: INT = 1
        SWITCH $n {
            CASE 2 {
                EXIT 1
            }
            CASE 1 {
                WRITE hit.txt one
            }
            CASE 1 / $z {
                EXIT 1
            }
        }
        LET $hit: STRING = READ hit.txt
        ASSERT_EQ $hit "one"
    "#};
    run_script(&root, script).expect("later case values never evaluate");
}

#[test]
fn switch_overlapping_arms_run_first_match_only() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # Both value arms and condition arms overlap here: only the
        # first hit runs. CONTINUE cannot reach the next arm; it
        # would continue an enclosing loop instead.
        LET $cmd: STRING = "b"
        SWITCH $cmd {
            CASE "b" {
                WRITE picked.txt first-b
            }
            CASE "b" {
                WRITE picked.txt second-b
            }
            DEFAULT {
                WRITE picked.txt fallback
            }
        }
        LET $n: INT = 10
        SWITCH {
            CASE $n > 1 {
                WRITE band.txt wide
            }
            CASE $n > 5 {
                WRITE band.txt narrow
            }
            DEFAULT {
                WRITE band.txt low
            }
        }
        LET $picked: STRING = READ picked.txt
        ASSERT_EQ $picked "first-b"
        LET $band: STRING = READ band.txt
        ASSERT_EQ $band "wide"
    "#};
    run_script(&root, script).expect("overlapping arms run first match only");
}

#[test]
fn switch_nested_switch_in_arm_body() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # Arms hold full bodies, including another SWITCH.
        LET $a: STRING = "x"
        LET $b: STRING = "y"
        SWITCH $a {
            CASE "x" {
                SWITCH $b {
                    CASE "y" {
                        WRITE hit.txt xy
                    }
                    DEFAULT {
                        WRITE hit.txt x-other
                    }
                }
            }
            DEFAULT {
                WRITE hit.txt other
            }
        }
        LET $hit: STRING = READ hit.txt
        ASSERT_EQ $hit "xy"
    "#};
    run_script(&root, script).expect("nested switch runs");
}

#[test]
fn switch_bare_first_truthy_condition_wins() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # With no discriminant each CASE is its own BOOL condition;
        # commas read as OR inside one arm.
        LET $n: INT = 2
        SWITCH {
            CASE $n > 5 {
                WRITE band.txt high
            }
            CASE false, $n > 1 {
                WRITE band.txt mid
            }
            DEFAULT {
                WRITE band.txt low
            }
        }
        LET $band: STRING = READ band.txt
        ASSERT_EQ $band "mid"
    "#};
    run_script(&root, script).expect("first truthy condition wins");
}

#[test]
fn switch_bare_conditions_short_circuit_after_a_hit() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # Later conditions never evaluate once an arm hits: the
        # division by zero below would fail the script.
        LET $z: INT = 0
        SWITCH {
            CASE true {
                WRITE hit.txt taken
            }
            CASE 1 / $z == 0 {
                EXIT 1
            }
        }
        LET $hit: STRING = READ hit.txt
        ASSERT_EQ $hit "taken"
    "#};
    run_script(&root, script).expect("bare conditions short-circuit");
}

#[test]
fn switch_bare_call_conditions_and_no_match_noop() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # Calls work as conditions; with no hit and no DEFAULT the
        # switch is a no-op.
        FUNC IS_BIG($n: INT): BOOL {
            RETURN $n > 100
        }
        LET $n: INT = 2
        SWITCH {
            CASE IS_BIG($n) {
                EXIT 1
            }
        }
        WRITE done.txt done
        LET $done: STRING = READ done.txt
        ASSERT_EQ $done "done"
    "#};
    run_script(&root, script).expect("call conditions work");
}

#[test]
fn switch_bare_head_tolerates_comment_gap() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # A comment between the bare head and the body brace parses:
        # gaps skip before the brace check, like the discriminant path.
        LET $n: INT = 2
        SWITCH /* pick */ {
            CASE $n > 5 {
                WRITE band.txt high
            }
            CASE $n > 1 {
                WRITE band.txt mid
            }
        }
        LET $band: STRING = READ band.txt
        ASSERT_EQ $band "mid"
    "#};
    run_script(&root, script).expect("comment gap before body parses");
}

#[test]
fn switch_break_exits_arm_and_continue_reaches_loop() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # BREAK ends the arm early (the trailing append never runs)
        # without touching the loop; CONTINUE skips to the next
        # iteration from inside the switch.
        LET $hits: LIST<STRING> = []
        FOR $item: STRING IN ["a", "skip", "b"] {
            SWITCH $item {
                CASE "skip" {
                    CONTINUE
                }
                CASE "a" {
                    LIST_APPEND $hits "a"
                    BREAK
                    LIST_APPEND $hits "unreached"
                }
                DEFAULT {
                    LIST_APPEND $hits "other"
                }
            }
        }
        LET $want: LIST<STRING> = ["a", "other"]
        ASSERT_EQ $hits $want
    "#};
    run_script(&root, script).expect("break and continue interact with loops");
}

#[test]
fn switch_return_per_arm_routes_like_early_if_chain() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # One RETURN per arm replaces the IF/RETURN dispatch chain,
        # including the empty-string case.
        FUNC ROUTE($cmd: STRING): STRING {
            SWITCH $cmd {
                CASE "" {
                    RETURN "empty"
                }
                CASE "help", "h" {
                    RETURN "help"
                }
                DEFAULT {
                    RETURN "other"
                }
            }
        }
        LET $r1: STRING = ROUTE("")
        LET $r2: STRING = ROUTE("h")
        LET $r3: STRING = ROUTE("zzz")
        ASSERT_EQ $r1 "empty"
        ASSERT_EQ $r2 "help"
        ASSERT_EQ $r3 "other"
    "#};
    run_script(&root, script).expect("return per arm routes");
}

#[test]
fn switch_break_at_top_level_of_switch_is_legal() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # BREAK directly in a SWITCH arm is legal with no loop: the
        # switch consumes it.
        SWITCH {
            CASE true {
                BREAK
            }
            DEFAULT {
                EXIT 1
            }
        }
        WRITE done.txt done
    "#};
    run_script(&root, script).expect("break in switch without loop is legal");
}

#[test]
fn switch_continue_without_loop_is_an_error() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # CONTINUE passes through a SWITCH, so with no enclosing loop
        # it fails exactly like a bare CONTINUE.
        SWITCH {
            CASE true {
                CONTINUE
            }
        }
    "#};
    let err = run_script(&root, script).unwrap_err().to_string();
    assert!(
        err.contains("outside loop"),
        "CONTINUE without a loop names the boundary: {err}"
    );
}

#[test]
fn switch_parse_errors_name_the_shape() {
    let cases = [
        (
            "SWITCH $x {\n}",
            "SWITCH needs at least one CASE or DEFAULT arm",
        ),
        (
            "SWITCH $x {\nECHO hi\n}",
            "SWITCH body must hold CASE or DEFAULT arms",
        ),
        (
            "SWITCH $x {\nCASE \"a\" ECHO hi\n}",
            "SWITCH CASE needs a braced block",
        ),
        (
            "SWITCH {\nDEFAULT {\nECHO a\n}\nCASE true {\nECHO b\n}\n}",
            "SWITCH DEFAULT must be the last arm",
        ),
        (
            "SWITCH {\nDEFAULT {\nECHO a\n}\nDEFAULT {\nECHO b\n}\n}",
            "SWITCH allows only one DEFAULT arm",
        ),
        (
            "SWITCH $x",
            "SWITCH needs a value or bare condition list",
        ),
        (
            "CASE \"a\" {\nECHO hi\n}",
            "CASE only appears inside a SWITCH body",
        ),
    ];
    for (script, needle) in cases {
        let err = parse_err(script);
        assert!(
            err.contains(needle),
            "script {script:?} names the shape ({needle}): {err}"
        );
    }
}

#[test]
fn switch_display_round_trips_through_reparse() {
    for script in [
        "SWITCH $cmd {\nCASE \"a\", \"b\" {\nECHO x\n}\nDEFAULT {\nECHO y\n}\n}\n",
        "SWITCH {\nCASE $n > 1 {\nECHO x\n}\n}\n",
    ] {
        let steps = oxdock_core::parse_script(script).expect("parse switch");
        let rendered = steps
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let again = oxdock_core::parse_script(&rendered).expect("reparse switch");
        assert_eq!(
            steps, again,
            "Display round-trips: {rendered}"
        );
    }
}
