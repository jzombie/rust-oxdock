use std::collections::{HashMap, HashSet};

use crate::common::mock_lower;

use oxdock_parser::{Expr, ModuleFuncs, ModuleTable, StepKind, parse_script_with_modules};

/// Fictional modules exercising resolution logic: `ALPHA` (several
/// functions), `BETA` (overlapping `GLOB`), and opaque `OPAQ` (membership
/// unknown, the compile-time `modules:` prefix shape). Names are arbitrary;
/// real builtin membership lives in `oxdock-core` and is covered by its
/// `reference_examples` test, so nothing here can drift from it.
fn table() -> ModuleTable {
    let std_functions: HashSet<String> = ["GLOB", "LOAD_JSON", "INT"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let docs_functions: HashSet<String> = ["SYNC", "GLOB"].iter().map(|s| s.to_string()).collect();
    ModuleTable {
        modules: HashMap::from([
            (
                "ALPHA".to_string(),
                Some(ModuleFuncs {
                    functions: std_functions,
                }),
            ),
            (
                "BETA".to_string(),
                Some(ModuleFuncs {
                    functions: docs_functions,
                }),
            ),
            ("OPAQ".to_string(), None),
        ]),
    }
}

fn parse(script: &str) -> Result<Vec<oxdock_parser::Step>, oxdock_parser::ParseError> {
    parse_script_with_modules(script, mock_lower, HashSet::new(), table())
}

fn call_name(steps: &[oxdock_parser::Step], idx: usize) -> String {
    match &steps[idx].kind {
        StepKind::Call { name, .. } => name.clone(),
        other => panic!("expected Call, got {other:?}"),
    }
}

#[test]
fn import_brings_bare_calls_into_scope() {
    let steps = parse("IMPORT [ALPHA]\nGLOB(\"*.txt\")\n").expect("import parses");
    // IMPORT emits zero runtime steps.
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(call_name(&steps, 0), "ALPHA::GLOB");
}

#[test]
fn import_bare_module_form_works() {
    let steps = parse("IMPORT ALPHA\nGLOB(\"*.txt\")\n").expect("bare import parses");
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(call_name(&steps, 0), "ALPHA::GLOB");
}

#[test]
fn import_in_let_rhs_resolves() {
    let steps =
        parse("IMPORT [ALPHA]\nLET $files: LIST = GLOB(\"*.txt\")\n").expect("import parses");
    assert_eq!(steps.len(), 1, "{steps:?}");
    let StepKind::Assign { expr, .. } = &steps[0].kind else {
        panic!("expected Assign, got {:?}", steps[0].kind);
    };
    assert!(
        matches!(expr, Expr::Call { name, .. } if name == "ALPHA::GLOB"),
        "{expr:?}"
    );
}

#[test]
fn qualified_call_checks_membership() {
    let steps = parse("ALPHA::GLOB(\"*.txt\")\n").expect("qualified parses");
    assert_eq!(call_name(&steps, 0), "ALPHA::GLOB");
    let err = parse("ALPHA::NOPE(\"*.txt\")\n").expect_err("missing function must fail");
    assert!(
        err.to_string().contains("unknown function `ALPHA::NOPE`"),
        "{err}"
    );
    let err = parse("NOPE::GLOB(\"*.txt\")\n").expect_err("unknown module must fail");
    assert!(err.to_string().contains("unknown module `NOPE`"), "{err}");
}

#[test]
fn unimported_bare_call_suggests_import() {
    // LOAD_JSON lives in ALPHA alone, so the suggestion names one module.
    let err = parse("LOAD_JSON(\"a.json\")\n").expect_err("unimported call must fail");
    let text = err.to_string();
    assert!(text.contains("unknown function `LOAD_JSON`"), "{text}");
    assert!(text.contains("IMPORT [ALPHA]"), "{text}");
}

#[test]
fn unknown_bare_call_has_no_suggestion() {
    let err = parse("FROBNICATE(\"x\")\n").expect_err("unknown call must fail");
    let text = err.to_string();
    assert!(text.contains("unknown function `FROBNICATE`"), "{text}");
    assert!(!text.contains("IMPORT"), "{text}");
}

#[test]
fn ambiguous_bare_call_across_modules_fails() {
    // Both ALPHA and BETA export GLOB: importing both leaves bare GLOB
    // ambiguous instead of shadowing silently.
    let err = parse("IMPORT [ALPHA, BETA]\nGLOB(\"*.txt\")\n").expect_err("ambiguous must fail");
    let text = err.to_string();
    assert!(text.contains("ambiguous function `GLOB`"), "{text}");
    assert!(text.contains("ALPHA::GLOB"), "{text}");
}

#[test]
fn import_reverts_on_block_exit() {
    let err = parse("[bool:true] {\nIMPORT [ALPHA]\nWRITE a.txt x\n}\nGLOB(\"*.txt\")\n")
        .expect_err("call after block must fail");
    assert!(err.to_string().contains("unknown function `GLOB`"), "{err}");
}

#[test]
fn import_inside_block_applies_within() {
    let steps =
        parse("[bool:true] {\nIMPORT [ALPHA]\nGLOB(\"*.txt\")\n}\n").expect("block import parses");
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(call_name(&steps, 0), "ALPHA::GLOB");
}

#[test]
fn script_definitions_win_over_opaque_imports() {
    // Opaque membership is unknown, so a SCRIPT definition deterministically
    // wins: no ambiguity to report. (Known-module names stay reserved, see
    // func_shadow_native and func_cannot_shadow_module_function.)
    let steps = parse("FUNC FOO($p: STRING) {\nRETURN $p\n}\nIMPORT [OPAQ]\nFOO(\"x\")\n")
        .expect("script def parses");
    assert_eq!(call_name(&steps, 1), "SCRIPT::FOO");
}

#[test]
fn func_cannot_shadow_module_function() {
    let err = parse("IMPORT [ALPHA]\nFUNC GLOB($p: STRING) {\nRETURN $p\n}\n")
        .expect_err("shadow must fail");
    assert!(
        err.to_string()
            .contains("cannot shadow reserved function `GLOB`"),
        "{err}"
    );
}

#[test]
fn export_is_reserved() {
    let err = parse("EXPORT FOO\n").expect_err("export must fail");
    assert!(err.to_string().contains("reserved"), "{err}");
}

#[test]
fn import_cannot_be_guarded() {
    let err = parse("[env:FOO]\nIMPORT [ALPHA]\n").expect_err("guarded import must fail");
    assert!(err.to_string().contains("cannot be guarded"), "{err}");
}

#[test]
fn lone_opaque_import_determines_target() {
    let steps = parse("IMPORT [OPAQ]\nFOO(\"x\")\n").expect("opaque import parses");
    assert_eq!(call_name(&steps, 0), "OPAQ::FOO");
}

#[test]
fn several_opaque_imports_are_ambiguous() {
    let table = ModuleTable {
        modules: HashMap::from([("OPAQ".to_string(), None), ("OPAQ2".to_string(), None)]),
    };
    let err = parse_script_with_modules(
        "IMPORT [OPAQ, OPAQ2]\nFOO(\"x\")\n",
        mock_lower,
        HashSet::new(),
        table,
    )
    .expect_err("two opaques must fail");
    assert!(
        err.to_string().contains("ambiguous function `FOO`"),
        "{err}"
    );
}

#[test]
fn lowercase_qualified_parts_are_rejected() {
    // Fully lowercase heads never reach lowering: the uppercase-only
    // `func_call_head` rule rejects them at lex time.
    let err = parse("std::glob(\"*.txt\")\n").expect_err("lowercase must fail");
    assert!(!err.to_string().is_empty(), "{err}");
    // A lowercase tail parses as a name, then fails the UPPERCASE check
    // with a span.
    let err = parse("ALPHA::glob(\"*.txt\")\n").expect_err("lowercase tail must fail");
    assert!(err.to_string().contains("UPPERCASE"), "{err}");
}

#[test]
fn deep_paths_are_rejected() {
    let err = parse_script_with_modules("A::B::F(\"x\")\n", mock_lower, HashSet::new(), table())
        .expect_err("deep path must fail");
    assert!(!err.to_string().is_empty(), "{err}");
}

#[test]
fn module_qualified_inspect_is_rejected() {
    let err = parse("LET $v: STRING = \"x\"\nLET $m: STRING = ALPHA::INSPECT($v)\n")
        .expect_err("qualified INSPECT must fail");
    assert!(
        err.to_string().contains("cannot be module-qualified"),
        "{err}"
    );
}

#[test]
fn bare_inspect_needs_no_import() {
    let steps =
        parse("LET $v: STRING = \"x\"\nLET $m: MAP = INSPECT($v)\n").expect("bare INSPECT parses");
    assert_eq!(steps.len(), 2, "{steps:?}");
}

#[test]
fn import_directive_is_not_a_function() {
    let err = parse("IMPORT(\"ALPHA\")\n").expect_err("call-shaped IMPORT must fail");
    assert!(
        err.to_string().contains("directive, not a function"),
        "{err}"
    );
}
