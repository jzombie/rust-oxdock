use crate::common::mock_lower;

use indoc::indoc;
use oxdock_parser::ast::{Expr, StepKind};
use oxdock_parser::parse_script;

// Bare `LET $p: PIPE` mints a fresh backend per declaration: the shape
// lowers through the mock exactly like an initialized LET, with no
// registry or runtime involved.
#[test]
fn bare_let_pipe_shape_and_round_trip() {
    let script = indoc! {r#"
        LET $p: PIPE
        WITH_IO [stdout=$p] WRITE "out.txt"
    "#};
    let steps = parse_script(script, mock_lower).expect("bare LET plus variable binding parses");
    assert_eq!(steps.len(), 2);
    match &steps[0].kind {
        StepKind::Assign {
            var,
            decl_type,
            expr,
        } => {
            assert_eq!(var, "p");
            assert_eq!(decl_type, "PIPE");
            assert!(matches!(expr, Expr::FreshPipe));
        }
        other => panic!("expected Assign, got {other:?}"),
    }
    assert_eq!(steps[0].kind.to_string(), "LET $p: PIPE");
}
