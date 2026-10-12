//! Numeric value lowering under mutation: grouped and plain spellings
//! must stage identical outcomes, and malformed numerics must error,
//! never panic.
//!
//! Companion to `fuzz_parity` (syntax/provenance fixpoints over
//! `Display`): this target asserts VALUE equivalence through the
//! `numeric_literal` lowering surface instead. Outcomes normalize to
//! `(StepKind, ParseErrorKind)` pairs so overflow and boundary behavior
//! compare bilaterally: separators are cosmetic, so both spellings must
//! agree whether they stage a value or fail.

use oxdock_parser::ast::Expr;
use oxdock_parser::commands::lower_command;
use oxdock_parser::{ParseErrorKind, StepKind, parse_script};
use proptest::prelude::*;

#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Staged(StepKind),
    Failed(ParseErrorKind),
}

/// Lower one `LET` initializer through the production dispatcher,
/// normalizing success and failure into one comparable outcome.
fn lower_initializer(spelling: &str) -> Outcome {
    let script = format!("LET $x: ANY = {spelling}");
    match parse_script(&script, lower_command) {
        Ok(steps) => Outcome::Staged(steps[0].kind.clone()),
        Err(e) => Outcome::Failed(e.kind().clone()),
    }
}

/// Insert `_` separators at valid interior positions (between digits
/// only): sorted, deduplicated, boundary-clamped.
fn grouped(digits: &str, cuts: Vec<usize>) -> String {
    let mut cuts: Vec<usize> = cuts
        .into_iter()
        .map(|c| c % (digits.len() + 1))
        .filter(|c| *c > 0 && *c < digits.len())
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = digits.to_string();
    for (i, c) in cuts.iter().enumerate() {
        out.insert(c + i, '_');
    }
    out
}

fn arb_cuts() -> impl Strategy<Value = Vec<usize>> {
    prop::collection::vec(any::<usize>(), 0..6)
}

fn arb_digit_run() -> BoxedStrategy<String> {
    prop::collection::vec(0..10u8, 1..25)
        .prop_map(|ds| ds.iter().map(|d| (b'0' + d) as char).collect())
        .boxed()
}

/// Numerics broken by construction: leading/trailing/doubled
/// separators, interior letters (which also cover exponent shapes like
/// `1e10`: the DSL has no float exponents, so these must stay bare
/// words), and stray dots at the ends. Every shape is invalid *as a
/// number*, so staging one would be a misread.
fn arb_invalid_numeric() -> impl Strategy<Value = String> {
    let run = arb_digit_run();
    prop_oneof![
        run.clone().prop_map(|d| format!("_{d}")),
        run.clone().prop_map(|d| format!("{d}_")),
        (run.clone(), 0..26usize).prop_map(|(d, p)| {
            let mut s = d;
            let at = p.min(s.len());
            s.insert_str(at, "__");
            s
        }),
        (
            run.clone(),
            0..26usize,
            prop_oneof![Just('e'), Just('s'), Just('g'), Just('k'), Just('m'),],
        )
            .prop_map(|(d, p, c)| {
                let mut s = d;
                let at = p.min(s.len());
                s.insert(at, c);
                s
            }),
        run.clone().prop_map(|d| format!(".{d}")),
        run.clone().prop_map(|d| format!("{d}.")),
    ]
}

proptest! {
    /// Grouped unsigned spellings stage exactly what plain ones do,
    /// from zero through `u64::MAX` (past `i64::MAX` both sides fail
    /// with the same overflow kind).
    #[test]
    fn grouped_int_spellings_stage_identically(
        n in any::<u64>(),
        cuts in arb_cuts(),
    ) {
        let plain = n.to_string();
        prop_assert_eq!(
            lower_initializer(&grouped(&plain, cuts)),
            lower_initializer(&plain),
        );
    }

    /// Unary minus distributes over grouping: `-` plus grouped digits
    /// stages what `-` plus plain digits does, including the
    /// `i64::MIN` boundary and overflow rejections.
    #[test]
    fn grouped_negative_spellings_stage_identically(
        n in any::<u64>(),
        cuts in arb_cuts(),
    ) {
        let plain = n.to_string();
        let grouped = grouped(&plain, cuts);
        prop_assert_eq!(
            lower_initializer(&format!("-{grouped}")),
            lower_initializer(&format!("-{plain}")),
        );
    }

    /// Grouping rides fractions too, down through denormals: both
    /// sides underflow, round, or fail identically.
    #[test]
    fn grouped_float_spellings_stage_identically(
        int_part in any::<u64>(),
        frac_digits in prop::collection::vec(0..10u8, 1..400),
        int_cuts in arb_cuts(),
        frac_cuts in arb_cuts(),
    ) {
        let frac: String = frac_digits.iter().map(|d| (b'0' + d) as char).collect();
        let plain = format!("{int_part}.{frac}");
        let int_digits = int_part.to_string();
        let grouped = format!(
            "{}.{}",
            grouped(&int_digits, int_cuts),
            grouped(&frac, frac_cuts),
        );
        prop_assert_eq!(
            lower_initializer(&grouped),
            lower_initializer(&plain),
        );
    }

    /// Signed floats distribute over grouping exactly like signed
    /// ints: `-` plus grouped fraction stages what `-` plus the plain
    /// fraction does.
    #[test]
    fn grouped_negative_float_spellings_stage_identically(
        int_part in any::<u64>(),
        frac_digits in prop::collection::vec(0..10u8, 1..400),
        int_cuts in arb_cuts(),
        frac_cuts in arb_cuts(),
    ) {
        let frac: String = frac_digits.iter().map(|d| (b'0' + d) as char).collect();
        let plain = format!("{int_part}.{frac}");
        let int_digits = int_part.to_string();
        let grouped = format!(
            "{}.{}",
            grouped(&int_digits, int_cuts),
            grouped(&frac, frac_cuts),
        );
        prop_assert_eq!(
            lower_initializer(&format!("-{grouped}")),
            lower_initializer(&format!("-{plain}")),
        );
    }

    /// Malformed numerics never panic the lowering, and misplaced
    /// separators or stray letters never stage a number: each shape
    /// either fails closed or lowers to the bare-word string it
    /// spells. The assertion names both accepted outcomes explicitly.
    #[test]
    fn invalid_numeric_shapes_never_stage_numbers(
        spelling in arb_invalid_numeric(),
    ) {
        let benign = match lower_initializer(&spelling) {
            Outcome::Failed(_) => true,
            Outcome::Staged(StepKind::Assign {
                expr: Expr::Literal(v),
                ..
            }) => v.as_str().is_some(),
            _ => false,
        };
        prop_assert!(
            benign,
            "invalid numeric {spelling:?} staged a non-string value"
        );
    }

    /// Token soup over the numeric alphabet must not panic the
    /// lowering: one evaluation per case suffices, since the function
    /// is pure (a panic fails the case on its own). Scope is
    /// deliberately narrow: this guards slicing and indexing safety
    /// only; classification lives in the test above and value
    /// equivalence in the grouped properties and directed tables.
    #[test]
    fn numeric_token_soup_never_panics(
        digits in prop::collection::vec(0..10u8, 1..8),
        mutation in prop_oneof![
            Just("_".to_string()),
            Just("__".to_string()),
            Just("-".to_string()),
            Just("+".to_string()),
            Just(".".to_string()),
            Just("..".to_string()),
            Just("x".to_string()),
            Just(" ".to_string()),
        ],
        position in 0..9usize,
    ) {
        let mut spelling: String =
            digits.iter().map(|d| (b'0' + d) as char).collect();
        let at = position.min(spelling.len());
        spelling.insert_str(at, &mutation);
        let _ = lower_initializer(&spelling);
    }
}
