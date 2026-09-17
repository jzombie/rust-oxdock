use std::collections::{HashMap, HashSet};

use oxdock_parser::{Arg, ModuleFuncs, ModuleTable, ParseResult, Step, StepKind};

/// Mock lowering for parser integration tests.
/// Delegates to the shared `test_lower_mock` in the parser crate.
pub fn mock_lower(name: &str, args: Vec<Arg>) -> ParseResult<StepKind> {
    oxdock_parser::test_lower_mock::lower(name, args)
}

/// Fictional `MATH` surface for expression-shape tests (RPN order, folding,
/// captures): member names are arbitrary and carry no builtin meaning.
/// Resolution logic itself is covered in `import.rs`; real builtin
/// membership lives in `oxdock-core` (`reference_examples` test).
pub fn math_table() -> ModuleTable {
    let functions: HashSet<String> = ["INT", "FLOAT", "FOO", "GLOB"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    ModuleTable {
        modules: HashMap::from([("MATH".to_string(), Some(ModuleFuncs { functions }))]),
    }
}

/// Parse with `IMPORT [MATH]` pre-seeded so shape tests can call the
/// fictional surface without restating the import in every script. Tests
/// about `IMPORT` itself live in `import.rs` and stay explicit.
pub fn parse_with_math(
    script: &str,
    lower: impl Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<Vec<Step>> {
    oxdock_parser::parse_script_with_modules(
        &format!("IMPORT [MATH]\n{script}"),
        lower,
        HashSet::new(),
        math_table(),
    )
}
