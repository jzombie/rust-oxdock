use crate::common::mock_lower;

use oxdock_parser::ast::{Expr, StepKind, Value};
use oxdock_parser::parse_script;

fn parse_assign_expr(script: &str) -> Expr {
    let steps = parse_script(script, mock_lower).expect("parse arithmetic");
    assert_eq!(steps.len(), 1, "expected one step, got {steps:?}");
    match steps.into_iter().next().unwrap().kind {
        StepKind::Assign { expr, .. } => expr,
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
#[allow(clippy::approx_constant)]
fn int_and_float_literals_bind() {
    assert_eq!(
        parse_assign_expr("LET $a: INT = 42\n"),
        Expr::Literal(Value::Int(42))
    );
    assert_eq!(
        parse_assign_expr("LET $f: FLOAT = 3.14\n"),
        Expr::Literal(Value::Float(3.14))
    );
    assert_eq!(
        parse_assign_expr("LET $f: FLOAT = 2.0\n"),
        Expr::Literal(Value::Float(2.0))
    );
}

#[test]
fn bare_word_boundaries_keep_literal_reading() {
    // Durations, paths, versions, and flags must not split on digits/`-`/`/`.
    for word in ["30s", "100ms", "123/456", "1.0.0", "-f"] {
        assert_eq!(
            parse_assign_expr(&format!("LET $x: STRING = {word}\n")),
            Expr::Literal(Value::String(word.to_string())),
            "bare word {word:?} must stay a string"
        );
    }
}

#[test]
fn precedence_folds_constants() {
    // `2 + 3 * 4` folds fully; `==` folds the comparison too.
    assert_eq!(
        parse_assign_expr("LET $x: INT = 2 + 3 * 4\n"),
        Expr::Literal(Value::Int(14))
    );
    assert_eq!(
        parse_assign_expr("LET $x: BOOL = 2+3*4==14\n"),
        Expr::Literal(Value::Bool(true))
    );
    assert_eq!(
        parse_assign_expr("LET $x: INT = (2 + 3) * 4\n"),
        Expr::Literal(Value::Int(20))
    );
}

#[test]
fn nesting_folds_at_any_depth() {
    // Parens nest arbitrarily; everything folds when operands are literals.
    assert_eq!(
        parse_assign_expr("LET $x: INT = 2 * (2 * (2 + 3)) * 4\n"),
        Expr::Literal(Value::Int(80))
    );
    assert_eq!(
        parse_assign_expr("LET $x: INT = ((2 + 3) * (4 - 1))\n"),
        Expr::Literal(Value::Int(15))
    );
    assert_eq!(
        parse_assign_expr("LET $x: INT = (1 + 2) * (3 + 4) - (10 / (2 + 3))\n"),
        Expr::Literal(Value::Int(19))
    );
}

#[test]
fn additive_and_multiplicative_chains_are_left_associative() {
    // `100 - 30 - 5` is `(100 - 30) - 5`, not `100 - (30 - 5)`.
    assert_eq!(
        parse_assign_expr("LET $x: INT = 100 - 30 - 5\n"),
        Expr::Literal(Value::Int(65))
    );
    assert_eq!(
        parse_assign_expr("LET $x: INT = 100 / 10 / 2\n"),
        Expr::Literal(Value::Int(5))
    );
    assert_eq!(
        parse_assign_expr("LET $x: INT = 2 + 3 * 4 - 10 / 2\n"),
        Expr::Literal(Value::Int(9))
    );
    assert_eq!(
        parse_assign_expr("LET $x: INT = 2 * (3 + 4)\n"),
        Expr::Literal(Value::Int(14))
    );
}

#[test]
fn unary_minus_binds_tighter_than_mul() {
    assert_eq!(
        parse_assign_expr("LET $x: INT = 2 * -3\n"),
        Expr::Literal(Value::Int(-6))
    );
    assert_eq!(
        parse_assign_expr("LET $x: INT = -2 * 3\n"),
        Expr::Literal(Value::Int(-6))
    );
    assert_eq!(
        parse_assign_expr("LET $x: INT = -(2 + 3)\n"),
        Expr::Literal(Value::Int(-5))
    );
    assert_eq!(
        parse_assign_expr("LET $x: INT = -9223372036854775808 + 1\n"),
        Expr::Literal(Value::Int(i64::MIN + 1))
    );
}

#[test]
fn comparisons_fold_over_arithmetic() {
    assert_eq!(
        parse_assign_expr("LET $x: BOOL = 2 + 3 > 4\n"),
        Expr::Literal(Value::Bool(true))
    );
    assert_eq!(
        parse_assign_expr("LET $x: BOOL = 2 * 3 <= 6\n"),
        Expr::Literal(Value::Bool(true))
    );
    assert_eq!(
        parse_assign_expr("LET $x: BOOL = 10 - 4 >= 7\n"),
        Expr::Literal(Value::Bool(false))
    );
    assert_eq!(
        parse_assign_expr("LET $x: BOOL = 1 + 1 == 2\n"),
        Expr::Literal(Value::Bool(true))
    );
}

#[test]
fn float_chains_and_int_division() {
    assert_eq!(
        parse_assign_expr("LET $x: FLOAT = 0.5 * 4 + 1.5\n"),
        Expr::Literal(Value::Float(3.5))
    );
    assert_eq!(
        parse_assign_expr("LET $x: FLOAT = 10.0 / 4\n"),
        Expr::Literal(Value::Float(2.5))
    );
    // Integer division truncates toward zero; it never promotes.
    assert_eq!(
        parse_assign_expr("LET $x: INT = 7 / 2\n"),
        Expr::Literal(Value::Int(3))
    );
}

#[test]
fn deep_dynamic_subtrees_compile_to_single_rpn() {
    // `$a * ($b + $c) - $d / $e`: 5 loads + 4 ops, root op last.
    match parse_assign_expr("LET $x: INT = $a * ($b + $c) - $d / $e\n") {
        Expr::CompiledMath(ops) => {
            assert_eq!(ops.len(), 9, "got {ops:?}");
            assert!(matches!(ops[8], oxdock_parser::ast::MathOp::Sub));
        }
        other => panic!("expected CompiledMath, got {other:?}"),
    }
    // Right-nested parens still flatten into one vector.
    match parse_assign_expr("LET $x: INT = $a * ($b * ($c + $d))\n") {
        Expr::CompiledMath(ops) => {
            assert_eq!(ops.len(), 7, "got {ops:?}");
        }
        other => panic!("expected CompiledMath, got {other:?}"),
    }
}

#[test]
fn dynamic_subtrees_compile_to_rpn() {
    // `$total + INT($size_str)` is the #112 bridge: vars/calls stay dynamic.
    match parse_assign_expr("LET $t: INT = $total + INT($size_str)\n") {
        Expr::CompiledMath(ops) => {
            assert_eq!(ops.len(), 4, "two loads + call + add, got {ops:?}");
            assert!(matches!(&ops[3], oxdock_parser::ast::MathOp::Add));
        }
        other => panic!("expected CompiledMath, got {other:?}"),
    }
}

#[test]
#[allow(clippy::approx_constant)]
fn unary_minus_folds_literals() {
    assert_eq!(
        parse_assign_expr("LET $x: INT = -5\n"),
        Expr::Literal(Value::Int(-5))
    );
    assert_eq!(
        parse_assign_expr("LET $x: INT = --5\n"),
        Expr::Literal(Value::Int(5))
    );
    assert_eq!(
        parse_assign_expr("LET $x: FLOAT = -3.14\n"),
        Expr::Literal(Value::Float(-3.14))
    );
    assert_eq!(
        parse_assign_expr("LET $x: INT = -9223372036854775808\n"),
        Expr::Literal(Value::Int(i64::MIN))
    );
    match parse_assign_expr("LET $x: INT = -$v\n") {
        Expr::CompiledMath(ops) => {
            assert!(matches!(
                ops.as_slice(),
                [
                    oxdock_parser::ast::MathOp::LoadVar(_),
                    oxdock_parser::ast::MathOp::Neg
                ]
            ));
        }
        other => panic!("expected CompiledMath Neg, got {other:?}"),
    }
}

#[test]
fn integer_boundary_overflow_bails() {
    // Bare `2^63` exceeds `i64::MAX` and is only valid under unary `-`.
    parse_script("LET $x: INT = 9223372036854775808\n", mock_lower)
        .expect_err("bare 2^63 must overflow");
    parse_script("LET $x: INT = 9223372036854775808 + 1\n", mock_lower)
        .expect_err("boundary in arithmetic must overflow");
    parse_script("LET $x: INT = 99999999999999999999999\n", mock_lower)
        .expect_err("huge literal must overflow");
}

#[test]
fn promotion_and_ordering_fold() {
    assert_eq!(
        parse_assign_expr("LET $x: FLOAT = 1 + 2.5\n"),
        Expr::Literal(Value::Float(3.5))
    );
    assert_eq!(
        parse_assign_expr("LET $x: BOOL = 3 < 4.5\n"),
        Expr::Literal(Value::Bool(true))
    );
    assert_eq!(
        parse_assign_expr("LET $x: BOOL = 2 >= 2\n"),
        Expr::Literal(Value::Bool(true))
    );
    assert_eq!(
        parse_assign_expr("LET $x: BOOL = 1 == 1.0\n"),
        Expr::Literal(Value::Bool(true))
    );
    // Exact float equality, no epsilon: binary fractions compare cleanly,
    // decimal fractions may not (0.1 + 0.2 is 0.30000000000000004).
    assert_eq!(
        parse_assign_expr("LET $x: BOOL = 0.5 + 0.25 == 0.75\n"),
        Expr::Literal(Value::Bool(true))
    );
    assert_eq!(
        parse_assign_expr("LET $x: BOOL = 0.1 + 0.2 == 0.3\n"),
        Expr::Literal(Value::Bool(false))
    );
}

#[test]
fn runtime_errors_stay_unfolded() {
    // Div-zero / overflow / non-finite must surface at runtime, not fold.
    for script in [
        "LET $x: INT = 1 / 0\n",
        "LET $x: INT = 9223372036854775807 + 1\n",
        "LET $x: FLOAT = 1.0 / 0.0\n",
    ] {
        match parse_assign_expr(script) {
            Expr::CompiledMath(_) => {}
            other => panic!("expected CompiledMath for {script:?}, got {other:?}"),
        }
    }
}

#[test]
fn int_float_conversions_parse() {
    assert!(matches!(
        parse_assign_expr("LET $n: INT = INT($s)\n"),
        Expr::CompiledMath(_) | Expr::Call { .. }
    ));
    assert!(matches!(
        parse_assign_expr("LET $f: FLOAT = FLOAT($s)\n"),
        Expr::CompiledMath(_) | Expr::Call { .. }
    ));
}

#[test]
fn call_arg_order_pushes_left_to_right() {
    // Lowering pushes `arg_0` before `arg_1`; the evaluator reverses pops.
    match parse_assign_expr("LET $x: STRING = FOO($a, $b)\n") {
        Expr::CompiledMath(ops) => {
            assert!(matches!(
                ops.as_slice(),
                [
                    oxdock_parser::ast::MathOp::LoadVar(a),
                    oxdock_parser::ast::MathOp::LoadVar(b),
                    oxdock_parser::ast::MathOp::Call { name, arity: 2 }
                ] if a == "a" && b == "b" && name == "FOO"
            ));
        }
        Expr::Call { name, args } => {
            assert_eq!(name, "FOO");
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected call RPN, got {other:?}"),
    }
}

#[test]
fn inspect_compiles_to_name_preserving_op() {
    // `INSPECT($p)` must not pre-evaluate `$p` to a Value on the stack:
    // inside a comparison it lowers to `Inspect("p")` operands.
    match parse_assign_expr("LET $ok: BOOL = INSPECT($p) == INSPECT($p)\n") {
        Expr::CompiledMath(ops) => {
            assert!(matches!(
                ops.as_slice(),
                [
                    oxdock_parser::ast::MathOp::Inspect(a),
                    oxdock_parser::ast::MathOp::Inspect(b),
                    oxdock_parser::ast::MathOp::Eq
                ] if a == "p" && b == "p"
            ));
        }
        other => panic!("expected Inspect RPN, got {other:?}"),
    }
}

#[test]
fn compiled_math_display_round_trips() {
    for script in [
        "LET $t: INT = $total + INT($size_str)\n",
        "LET $x: INT = 2 * -$v + 1\n",
        "LET $x: BOOL = $a < $b\n",
        "LET $x: INT = $a * ($b * ($c + $d))\n",
        "LET $x: INT = 2 * (2 * (2 + 3)) * 4\n",
    ] {
        let expr = parse_assign_expr(script);
        let rendered = expr.to_string();
        let again = parse_assign_expr(&format!("LET $t: INT = {rendered}\n"));
        assert_eq!(expr, again, "round-trip failed for {script:?}");
    }
}

#[test]
fn right_nested_division_keeps_operand_order() {
    // Non-commutative ops must not evaluate out of order: `100 / (10 / 2)`
    // is 20, while `(100 / 10) / 2` is 5.
    assert_eq!(
        parse_assign_expr("LET $x: INT = 100 / (10 / (4 / 2))\n"),
        Expr::Literal(Value::Int(20))
    );
    assert_eq!(
        parse_assign_expr("LET $x: INT = (100 / 10) / (4 / 2)\n"),
        Expr::Literal(Value::Int(5))
    );
}

#[test]
fn unary_negation_interleaved_with_nested_parens() {
    // `-2 * -(3 + -(4 * 5))` folds prefix chains through parens to -34.
    assert_eq!(
        parse_assign_expr("LET $x: INT = -2 * -(3 + -(4 * 5))\n"),
        Expr::Literal(Value::Int(-34))
    );
}

#[test]
fn mixed_promotion_across_subtrees() {
    // Integer division at the leaf, Float promotion outward: 4.0.
    assert_eq!(
        parse_assign_expr("LET $x: FLOAT = 1 + (2 * (3.5 - (4 / 2)))\n"),
        Expr::Literal(Value::Float(4.0))
    );
}

#[test]
fn complex_subtrees_on_both_sides_of_ordering() {
    // RPN must emit post-order ops for both sides: 8 <= 8 is true.
    assert_eq!(
        parse_assign_expr("LET $x: BOOL = (2 * (3 + 1)) <= (10 - (1 * 2))\n"),
        Expr::Literal(Value::Bool(true))
    );
}

#[test]
fn function_call_embedded_in_nested_arithmetic() {
    // Calls are runtime values, so `INT(..)` blocks literal folding: the
    // whole tree compiles to one RPN vector (5 pushes + call + 3 ops).
    // The value 52 is verified end-to-end in oxdock-core's arithmetic tests.
    match parse_assign_expr("LET $x: INT = 10 + (3 * (INT(\"  12  \") + 2))\n") {
        Expr::CompiledMath(ops) => {
            assert_eq!(ops.len(), 8, "got {ops:?}");
            assert!(matches!(ops[7], oxdock_parser::ast::MathOp::Add));
        }
        other => panic!("expected CompiledMath, got {other:?}"),
    }
}

#[test]
fn boundary_negation_across_parens() {
    // Staged `UnsignedIntBoundary` unwinds through parens: -(MIN + 1) folds
    // to MAX instead of tripping a premature overflow.
    assert_eq!(
        parse_assign_expr("LET $x: INT = -(-9223372036854775808 + 1)\n"),
        Expr::Literal(Value::Int(i64::MAX))
    );
}

#[test]
fn chained_comparisons_are_rejected() {
    // Ordering and equality take exactly one optional operator: `a < b < c`
    // must not silently parse as `(a < b) < c` (BOOL < number) the way C
    // would, nor evaluate `b` twice the way Python chaining would. Write
    // the conjunction explicitly instead.
    for script in [
        "LET $x: BOOL = $a < $b < $c\n",
        "LET $x: BOOL = $a <= $b >= $c\n",
        "LET $x: BOOL = $a == $b == $c\n",
        "LET $x: BOOL = $a != $b != $c\n",
    ] {
        parse_script(script, mock_lower).expect_err("chained comparison must fail");
    }
    // The explicit form parses to a conjunction of two comparisons.
    match parse_assign_expr("LET $x: BOOL = $a < $b && $b < $c\n") {
        Expr::Logical { op, left, right } => {
            assert_eq!(op, oxdock_parser::ast::LogicalOp::And);
            assert!(matches!(left.as_ref(), Expr::CompiledMath(_)));
            assert!(matches!(right.as_ref(), Expr::CompiledMath(_)));
        }
        other => panic!("expected conjunction, got {other:?}"),
    }
}

#[test]
fn chained_logical_operators_with_spaces() {
    // Repetition-level gaps (not just the first): `||`/`&&` chains route
    // through every tier, proving no level is skipped during lowering.
    assert_eq!(
        parse_assign_expr("LET $x: BOOL = false || false || true\n"),
        Expr::Logical {
            op: oxdock_parser::ast::LogicalOp::Or,
            left: Box::new(Expr::Logical {
                op: oxdock_parser::ast::LogicalOp::Or,
                left: Box::new(Expr::Literal(Value::Bool(false))),
                right: Box::new(Expr::Literal(Value::Bool(false))),
            }),
            right: Box::new(Expr::Literal(Value::Bool(true))),
        }
    );
    assert_eq!(
        parse_assign_expr("LET $x: BOOL = true && true && false\n"),
        Expr::Logical {
            op: oxdock_parser::ast::LogicalOp::And,
            left: Box::new(Expr::Logical {
                op: oxdock_parser::ast::LogicalOp::And,
                left: Box::new(Expr::Literal(Value::Bool(true))),
                right: Box::new(Expr::Literal(Value::Bool(true))),
            }),
            right: Box::new(Expr::Literal(Value::Bool(false))),
        }
    );
}
