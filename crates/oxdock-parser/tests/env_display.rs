mod common;
use common::mock_lower;

use oxdock_parser::ast::{Guard, GuardExpr};
use oxdock_parser::parse_script;

#[test]
fn env_equals_display_uses_functional_syntax() {
    let guard = Guard::EnvEquals {
        key: "A".into(),
        value: "1".into(),
    };

    assert_eq!(guard.to_string(), "eq(env:A, 1)");

    // not(eq(...)) for negation
    let expr = GuardExpr::Not(Box::new(GuardExpr::Predicate(guard)));
    assert_eq!(expr.to_string(), "not(eq(env:A, 1))");

    // Verify round-trip through parser with WRITE command.
    let rendered = "[not(eq(env:A, 1))] WRITE a";
    let parsed = parse_script(rendered, mock_lower).expect("round-trip parse");
    assert_eq!(parsed.len(), 1);
    let parsed_guard = parsed[0].guard.as_ref().expect("missing guard");
    assert_eq!(parsed_guard.to_string(), "not(eq(env:A, 1))");
}
