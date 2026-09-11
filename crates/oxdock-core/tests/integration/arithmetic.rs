//! DSL arithmetic (#112): infix ops with promotion, ordering comparisons,
//! and `INT()`/`FLOAT()` conversions over `LET`-captured strings.

use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver};

fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
    temp.as_guarded_path().clone()
}

fn run_script(root: &GuardedPath, script: &str) -> Result<(), anyhow::Error> {
    let steps = oxdock_core::parse_script(script).expect("parse script");
    run_steps_with_context_result_with_io(root, root, &steps, ExecIo::new()).map(|_| ())
}

fn read_file(path: &GuardedPath) -> String {
    let resolver = PathResolver::new(path.root(), path.root()).unwrap();
    resolver.read_to_string(path).unwrap()
}

#[test]
fn arithmetic_over_captured_strings() {
    // The #112 bridge: capture yields `"41\n"`, `INT()` trims, `+ 1` promotes.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $size_str: STRING = ECHO 41
        LET $total: INT = INT($size_str) + 1
        WRITE total.txt "{{ $total }}"
        ASSERT_FILE total.txt "42"
    "#};
    run_script(&root, script).expect("captured arithmetic");
    assert_eq!(read_file(&root.join("total.txt").unwrap()), "42");
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
        WRITE out.txt "{{ $a }}|{{ $b }}|{{ $c }}"
        ASSERT_FILE out.txt "true|false|true"
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
        ASSERT_FILE bounded.txt "yes"
        ASSERT_FILE exact.txt "yes"
        ASSERT_ABSENT unexpected.txt
    "#};
    run_script(&root, script).expect("float equality");
}

#[test]
fn float_promotion_and_ordering() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $ratio: FLOAT = 1 + 2.5
        WRITE ratio.txt "{{ $ratio }}"
        ASSERT_FILE ratio.txt "3.5"
        IF $ratio > 3.0 {
            WRITE big.txt yes
        }
        IF 1 == 1.0 {
            WRITE eq.txt yes
        }
        IF 3 < 4.5 {
            WRITE lt.txt yes
        }
        ASSERT_FILE big.txt "yes"
        ASSERT_FILE eq.txt "yes"
        ASSERT_FILE lt.txt "yes"
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
        WRITE total.txt "{{ $total }}"
        ASSERT_FILE total.txt "15"
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
        WRITE out.txt "{{ $n }}|{{ $f }}|{{ $g }}"
        ASSERT_FILE out.txt "123|123|2.5"
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
    for script in [
        "LET $x: INT = 1 / 0\n",
        "LET $a: INT = 5\nLET $x: INT = $a / 0\n",
        "LET $x: FLOAT = 1.0 / 0.0\n",
        "LET $a: INT = 9223372036854775807\nLET $x: INT = $a + 1\n",
    ] {
        run_script(&root, script).expect_err("arithmetic error must fail");
    }
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
        LET $p: PIPE = pipe:log
        IF INSPECT($p) == INSPECT($p) {
            WRITE same.txt yes
        }
        ASSERT_FILE same.txt "yes"
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
        WRITE d.txt "{{ $d }}"
        ASSERT_FILE d.txt "2"
    "#};
    run_script(&root, script).expect("subtraction order");
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
        WRITE x.txt "{{ $x }}"
        ASSERT_FILE x.txt "20"
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
        WRITE q.txt "{{ $q }}"
        ASSERT_FILE q.txt "20"
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
        WRITE x.txt "{{ $x }}"
        ASSERT_FILE x.txt "52"
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
        WRITE x.txt "{{ $x }}"
        ASSERT_FILE x.txt "4"
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
        ASSERT_FILE ok.txt "yes"
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
        WRITE x.txt "{{ $x }}"
        ASSERT_FILE x.txt "34"
    "#};
    run_script(&root, script).expect("unary interleaved");
}
