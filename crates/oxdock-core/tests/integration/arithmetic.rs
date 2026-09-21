//! DSL arithmetic (#112): infix ops with promotion, ordering comparisons,
//! and `INT()`/`FLOAT()` conversions over `LET`-captured strings.

use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, GuardedTempDir};

fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
    temp.as_guarded_path().clone()
}

fn run_script(root: &GuardedPath, script: &str) -> Result<(), anyhow::Error> {
    // File-local scripts call `STD` builtins; the import is fixture,
    // not subject: `IMPORT` semantics are covered in `import.rs`.
    let steps =
        oxdock_core::parse_script(&format!("IMPORT [STD]\n{script}")).expect("parse script");
    run_steps_with_context_result_with_io(root, root, &steps, ExecIo::new()).map(|_| ())
}

#[test]
fn arithmetic_over_captured_strings() {
    // The #112 bridge: capture yields `"41\n"`, `INT()` trims, `+ 1` promotes.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $size_str: STRING = ECHO 41
        LET $total: INT = INT($size_str) + 1
        ASSERT_EQ $total 42
    "#};
    run_script(&root, script).expect("captured arithmetic");
}

#[test]
fn logical_operators_short_circuit() {
    // `||`/`&&` never evaluate the skipped side: `$missing` is undefined,
    // yet neither errors.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $a: BOOL = true || $missing
        LET $b: BOOL = false && $missing
        LET $c: BOOL = $a && $b == false
        ASSERT_EQ $a true
        ASSERT_EQ $b false
        ASSERT_EQ $c true
    "#};
    run_script(&root, script).expect("short-circuit");
}

#[test]
fn float_equality_is_exact_without_epsilon() {
    // Same gotcha as the LET reference documents: exact binary fractions
    // compare true, decimal fractions may compare false. Dynamic operands
    // exercise the RPN comparator rather than the parse-time fold.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $a: FLOAT = 0.1
        LET $b: FLOAT = 0.2
        LET $c: FLOAT = 0.3
        LET $sum: FLOAT = $a + $b
        IF $sum == $c {
            WRITE unexpected.txt no
        }
        IF $sum > 0.299999 && $sum < 0.300001 {
            WRITE bounded.txt yes
        }
        LET $e: FLOAT = 0.5
        LET $f: FLOAT = 0.25
        LET $g: FLOAT = 0.75
        IF $e + $f == $g {
            WRITE exact.txt yes
        }
        LET $ok: STRING = READ bounded.txt
        LET $exact_ok: STRING = READ exact.txt
        ASSERT_EQ $ok "yes"
        ASSERT_EQ $exact_ok "yes"
        LET $t: STRING = PATH_TYPE("unexpected.txt")
        ASSERT_EQ $t "absent"
    "#};
    run_script(&root, script).expect("float equality");
}

#[test]
fn float_promotion_and_ordering() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $ratio: FLOAT = 1 + 2.5
        ASSERT_EQ $ratio 3.5
        IF $ratio > 3.0 {
            WRITE big.txt yes
        }
        IF 1 == 1.0 {
            WRITE eq.txt yes
        }
        IF 3 < 4.5 {
            WRITE lt.txt yes
        }
        LET $big: STRING = READ big.txt
        LET $eq: STRING = READ eq.txt
        LET $lt: STRING = READ lt.txt
        ASSERT_EQ $big "yes"
        ASSERT_EQ $eq "yes"
        ASSERT_EQ $lt "yes"
    "#};
    run_script(&root, script).expect("float promotion");
}

#[test]
fn running_total_accumulates() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $total: INT = 0
        LET $a: STRING = ECHO 7
        $total = $total + INT($a)
        LET $b: STRING = ECHO 8
        $total = $total + INT($b)
        ASSERT_EQ $total 15
    "#};
    run_script(&root, script).expect("running total");
}

#[test]
fn int_float_conversions_valid() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $n: INT = INT("  123  ")
        LET $f: FLOAT = FLOAT($n)
        LET $g: FLOAT = FLOAT("2.5")
        ASSERT_EQ $n 123
        ASSERT_EQ $f 123.0
        ASSERT_EQ $g 2.5
    "#};
    run_script(&root, script).expect("conversions");
}

#[test]
fn int_float_conversions_invalid_fail() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    for script in [
        "LET $n: INT = INT(\"abc\")\n",
        "LET $n: INT = INT(\"3.5\")\n",
        "LET $f: FLOAT = FLOAT(\"abc\")\n",
        "LET $f: FLOAT = FLOAT(\"nan\")\n",
        "LET $f: FLOAT = FLOAT(\"inf\")\n",
    ] {
        run_script(&root, script).expect_err("invalid conversion must fail");
    }
}

#[test]
fn div_zero_and_overflow_fail() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    for (script, needle) in [
        (
            "LET $x: INT = 1 / 0\n",
            "arithmetic error: integer overflow or division by zero",
        ),
        (
            "LET $a: INT = 5\nLET $x: INT = $a / 0\n",
            "arithmetic error: integer overflow or division by zero",
        ),
        (
            "LET $x: FLOAT = 1.0 / 0.0\n",
            "arithmetic error: float division by zero",
        ),
        (
            "LET $a: INT = 9223372036854775807\nLET $x: INT = $a + 1\n",
            "arithmetic error: integer overflow or division by zero",
        ),
    ] {
        let err = run_script(&root, script).expect_err("arithmetic error must fail");
        assert!(
            err.to_string().contains(needle),
            "wrong error for {script:?}, got: {err:#}"
        );
    }
}

#[test]
fn every_int_op_overflows_loudly() {
    // Add is pinned above; Sub/Mul/Div/Neg share the checked path but each
    // gets its own pin so a future shortcut on one op cannot slip through.
    // All operands ride variables so the overflow surfaces at runtime.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    for (script, needle) in [
        (
            "LET $m: INT = -9223372036854775808\nLET $x: INT = $m - 1\n",
            "arithmetic error: integer overflow or division by zero",
        ),
        (
            "LET $a: INT = 9223372036854775807\nLET $x: INT = $a * 2\n",
            "arithmetic error: integer overflow or division by zero",
        ),
        (
            "LET $m: INT = -9223372036854775808\nLET $n: INT = 0 - 1\nLET $x: INT = $m / $n\n",
            "arithmetic error: integer overflow or division by zero",
        ),
        (
            "LET $m: INT = -9223372036854775808\nLET $x: INT = -$m\n",
            "arithmetic error: integer overflow",
        ),
    ] {
        let err = run_script(&root, script).expect_err("int overflow must fail");
        assert!(
            err.to_string().contains(needle),
            "wrong error for {script:?}, got: {err:#}"
        );
    }
}

#[test]
fn float_overflow_to_infinity_fails() {
    // Numeric literals have no exponent syntax, so enter through
    // `FLOAT("1e308")`: ten times that is +inf, which bails instead of
    // storing a non-finite value.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = "LET $big: FLOAT = FLOAT(\"1e308\")\nLET $x: FLOAT = $big * 10.0\n";
    let err = run_script(&root, script).expect_err("float overflow must fail");
    assert!(
        err.to_string()
            .contains("arithmetic error: non-finite float result"),
        "wrong error, got: {err:#}"
    );
}

#[test]
fn string_operands_never_convert_implicitly() {
    // The reference promises this: `"100" + 1` is a Type Error, not 101.
    // Cross the boundary explicitly with INT() / FLOAT() instead.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    run_script(&root, "LET $x: INT = \"100\" + 1\n").expect_err("string math must fail");
    run_script(&root, "LET $x: BOOL = \"3.14\" > 2.0\n").expect_err("string ordering must fail");
}

#[test]
fn ordering_on_non_numerics_fails() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    run_script(&root, "LET $x: BOOL = \"a\" < \"b\"\n").expect_err("ordering strings must fail");
}

#[test]
fn inspect_equality_through_rpn() {
    // `INSPECT($p)` inside a comparison lowers to name-preserving RPN ops.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $p: PIPE
        IF INSPECT($p) == INSPECT($p) {
            WRITE same.txt yes
        }
        LET $ok: STRING = READ same.txt
        ASSERT_EQ $ok "yes"
    "#};
    run_script(&root, script).expect("inspect compare");
}

#[test]
fn subtraction_operand_order() {
    // Non-commutative ops must restore left-to-right order (`5 - 3 == 2`).
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $a: INT = 5
        LET $b: INT = 3
        LET $d: INT = $a - $b
        ASSERT_EQ $d 2
    "#};
    run_script(&root, script).expect("subtraction order");
}

#[test]
fn negative_literal_binds() {
    // Negative literals bind directly. Note: `ASSERT_EQ $neg -5` would
    // parse `- 5` as subtraction of the first argument, so the expected
    // value is bound separately; both routes must yield Int(-5).
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $neg: INT = -5
        LET $e: INT = 0 - 5
        ASSERT_EQ $neg $e
    "#};
    run_script(&root, script).expect("negative literal");
}

#[test]
fn subtraction_yields_negative() {
    // `3 - 8` is -5, pinning signed results (nothing covered these before).
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $d: INT = 3 - 8
        LET $e: INT = 0 - 5
        ASSERT_EQ $d $e
    "#};
    run_script(&root, script).expect("negative subtraction");
}

#[test]
fn negative_multiplication() {
    // Sign handling across multiplication: `-3 * 4` and `3 * -4` are -12.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $a: INT = -3 * 4
        LET $b: INT = 3 * -4
        LET $e: INT = 0 - 12
        ASSERT_EQ $a $e
        ASSERT_EQ $b $e
    "#};
    run_script(&root, script).expect("negative multiplication");
}

#[test]
fn negative_division_truncates_toward_zero() {
    // Integer division truncates toward zero: `-7 / 2` is -3 (not floored -4).
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $x: INT = 0 - 7
        LET $q: INT = $x / 2
        LET $r: INT = 7 / (0 - 2)
        LET $e: INT = 0 - 3
        ASSERT_EQ $q $e
        ASSERT_EQ $r $e
    "#};
    run_script(&root, script).expect("negative division");
}

#[test]
fn negative_float_division() {
    // Float division keeps the sign and fraction: `-7.0 / 2` is -3.5.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $f: FLOAT = -7.0 / 2
        LET $e: FLOAT = 0.0 - 3.5
        ASSERT_EQ $f $e
    "#};
    run_script(&root, script).expect("negative float division");
}

#[test]
fn deep_dynamic_nesting_evaluates() {
    // Right-nested parens through the RPN loop: 2 * (2 * (2 + 3)) = 20.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $a: INT = 2
        LET $b: INT = 2
        LET $c: INT = 2
        LET $d: INT = 3
        LET $x: INT = $a * ($b * ($c + $d))
        ASSERT_EQ $x 20
    "#};
    run_script(&root, script).expect("deep nesting");
}

#[test]
fn right_nested_division_order() {
    // `$x / ($y / $z)` is 20; left-assoc `($x / $y) / $z` would be 5.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $x: INT = 100
        LET $y: INT = 10
        LET $z: INT = 2
        LET $q: INT = $x / ($y / $z)
        ASSERT_EQ $q 20
    "#};
    run_script(&root, script).expect("division order");
}

#[test]
fn call_embedded_in_nested_arithmetic_evaluates() {
    // `MathOp::Call` arg restore inside nested terms: 10 + 3 * (12 + 2) = 52.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $s: STRING = "  12  "
        LET $x: INT = 10 + (3 * (INT($s) + 2))
        ASSERT_EQ $x 52
    "#};
    run_script(&root, script).expect("nested call");
}

#[test]
fn mixed_promotion_dynamic() {
    // Int division at the leaf, Float promotion outward: 4.0.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $h: INT = 1
        LET $i: INT = 2
        LET $j: FLOAT = 3.5
        LET $k: INT = 4
        LET $l: INT = 2
        LET $x: FLOAT = $h + ($i * ($j - ($k / $l)))
        ASSERT_EQ $x 4.0
    "#};
    run_script(&root, script).expect("mixed promotion");
}

#[test]
fn ordering_over_complex_dynamic_subtrees() {
    // Both comparison sides compile to RPN: 8 <= 8 is true.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $a: INT = 2
        LET $b: INT = 3
        LET $c: INT = 1
        LET $d: INT = 10
        LET $e: INT = 1
        LET $f: INT = 2
        IF ($a * ($b + $c)) <= ($d - ($e * $f)) {
            WRITE ok.txt yes
        }
        LET $ok: STRING = READ ok.txt
        ASSERT_EQ $ok "yes"
    "#};
    run_script(&root, script).expect("complex ordering");
}

#[test]
fn unary_interleaved_dynamic() {
    // `-$a * -($b + $c)` = -2 * -17 = 34.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $a: INT = 2
        LET $b: INT = 3
        LET $c: INT = 14
        LET $x: INT = -$a * -($b + $c)
        ASSERT_EQ $x 34
    "#};
    run_script(&root, script).expect("unary interleaved");
}
