use oxdock_parser::{Arg, ParseResult, StepKind};

/// Mock lowering for parser integration tests.
/// Delegates to the shared `test_lower_mock` in the parser crate.
pub fn mock_lower(name: &str, args: Vec<Arg>) -> ParseResult<StepKind> {
    oxdock_parser::test_lower_mock::lower(name, args)
}
