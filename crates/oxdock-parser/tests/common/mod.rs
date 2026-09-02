use anyhow::Result;
use oxdock_parser::{Arg, StepKind};

/// Mock lowering for parser integration tests.
/// Delegates to the shared `test_lower_mock` in the parser crate.
pub fn mock_lower(name: &str, args: Vec<Arg>) -> Result<StepKind> {
    oxdock_parser::test_lower_mock::lower(name, args)
}
