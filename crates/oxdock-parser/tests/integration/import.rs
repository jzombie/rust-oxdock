use std::collections::{HashMap, HashSet};

use crate::common::mock_lower;

use indoc::indoc;
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
    let steps = parse(indoc! {r#"
        IMPORT [ALPHA]
        GLOB("*.txt")
    "#})
    .expect("import parses");
    // IMPORT emits zero runtime steps.
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(call_name(&steps, 0), "ALPHA::GLOB");
}

#[test]
fn import_bare_module_form_works() {
    let steps = parse(indoc! {r#"
        IMPORT ALPHA
        GLOB("*.txt")
    "#})
    .expect("bare import parses");
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(call_name(&steps, 0), "ALPHA::GLOB");
}

#[test]
fn import_in_let_rhs_resolves() {
    let steps = parse(indoc! {r#"
        IMPORT [ALPHA]
        LET $files: LIST = GLOB("*.txt")
    "#})
    .expect("import parses");
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
    let err = parse(indoc! {r#"
        IMPORT [ALPHA, BETA]
        GLOB("*.txt")
    "#})
    .expect_err("ambiguous must fail");
    let text = err.to_string();
    assert!(text.contains("ambiguous function `GLOB`"), "{text}");
    assert!(text.contains("ALPHA::GLOB"), "{text}");
}

#[test]
fn import_reverts_on_block_exit() {
    let err = parse(indoc! {r#"
        [bool:true] {
        IMPORT [ALPHA]
        WRITE a.txt x
        }
        GLOB("*.txt")
    "#})
    .expect_err("call after block must fail");
    assert!(err.to_string().contains("unknown function `GLOB`"), "{err}");
}

#[test]
fn import_inside_block_applies_within() {
    let steps = parse(indoc! {r#"
        [bool:true] {
        IMPORT [ALPHA]
        GLOB("*.txt")
        }
    "#})
    .expect("block import parses");
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(call_name(&steps, 0), "ALPHA::GLOB");
}

#[test]
fn script_definitions_win_over_opaque_imports() {
    // Opaque membership is unknown, so a SCRIPT definition deterministically
    // wins: no ambiguity to report. (Known-module names stay reserved, see
    // func_shadow_native and func_cannot_shadow_module_function.)
    let steps = parse(indoc! {r#"
        FUNC FOO($p: STRING) {
        RETURN $p
        }
        IMPORT [OPAQ]
        FOO("x")
    "#})
    .expect("script def parses");
    assert_eq!(call_name(&steps, 1), "SCRIPT::FOO");
}

#[test]
fn func_cannot_shadow_module_function() {
    let err = parse(indoc! {r#"
        IMPORT [ALPHA]
        FUNC GLOB($p: STRING) {
        RETURN $p
        }
    "#})
    .expect_err("shadow must fail");
    assert!(
        err.to_string()
            .contains("cannot shadow reserved function `GLOB`"),
        "{err}"
    );
}

#[test]
fn export_single_and_list_forms() {
    use oxdock_parser::parse_script_with_modules_and_exports;
    let parsed = parse_script_with_modules_and_exports(
        indoc! {r#"
            FUNC FOO() {
                RETURN "x"
            }
            EXPORT FOO
        "#},
        mock_lower,
        HashSet::new(),
        table(),
    )
    .expect("single export parses");
    assert_eq!(parsed.exports, vec!["FOO".to_string()]);
    assert_eq!(parsed.steps.len(), 1);

    let parsed = parse_script_with_modules_and_exports(
        indoc! {r#"
            FUNC FOO() {
                RETURN "x"
            }
            FUNC BAR() {
                RETURN "y"
            }
            EXPORT [FOO, BAR]
        "#},
        mock_lower,
        HashSet::new(),
        table(),
    )
    .expect("list export parses");
    assert_eq!(parsed.exports, vec!["FOO".to_string(), "BAR".to_string()]);
}

#[test]
fn export_forward_reference_allowed() {
    use oxdock_parser::parse_script_with_modules_and_exports;
    let parsed = parse_script_with_modules_and_exports(
        indoc! {r#"
            EXPORT FOO
            FUNC FOO() {
                RETURN "x"
            }
        "#},
        mock_lower,
        HashSet::new(),
        table(),
    )
    .expect("forward export parses");
    assert_eq!(parsed.exports, vec!["FOO".to_string()]);
}

#[test]
fn export_unknown_name_fails() {
    use oxdock_parser::parse_script_with_modules_and_exports;
    let err = parse_script_with_modules_and_exports(
        "EXPORT MISSING\n",
        mock_lower,
        HashSet::new(),
        table(),
    )
    .expect_err("unknown export must fail");
    assert!(
        err.to_string()
            .contains("does not match any top-level FUNC"),
        "{err}"
    );
}

#[test]
fn export_qualified_host_name_passes() {
    use oxdock_parser::parse_script_with_modules_and_exports;
    let parsed = parse_script_with_modules_and_exports(
        indoc! {r#"
            FUNC FOO() {
                RETURN "x"
            }
            EXPORT [FOO, ALPHA::GLOB]
        "#},
        mock_lower,
        HashSet::new(),
        table(),
    )
    .expect("qualified export parses");
    assert_eq!(
        parsed.exports,
        vec!["FOO".to_string(), "ALPHA::GLOB".to_string()]
    );
}

#[test]
fn export_qualified_script_name_passes() {
    use oxdock_parser::parse_script_with_modules_and_exports;
    let parsed = parse_script_with_modules_and_exports(
        indoc! {r#"
            FUNC FOO() {
                RETURN "x"
            }
            EXPORT SCRIPT::FOO
        "#},
        mock_lower,
        HashSet::new(),
        table(),
    )
    .expect("SCRIPT-qualified export parses");
    assert_eq!(parsed.exports, vec!["SCRIPT::FOO".to_string()]);
}

#[test]
fn export_qualified_unknown_member_fails() {
    use oxdock_parser::parse_script_with_modules_and_exports;
    let err = parse_script_with_modules_and_exports(
        "EXPORT ALPHA::MISSING\n",
        mock_lower,
        HashSet::new(),
        table(),
    )
    .expect_err("unknown member export must fail");
    assert!(err.to_string().contains("ALPHA::MISSING"), "{err}");
}

#[test]
fn export_qualified_unknown_module_fails() {
    use oxdock_parser::parse_script_with_modules_and_exports;
    let err = parse_script_with_modules_and_exports(
        "EXPORT NOPE::FOO\n",
        mock_lower,
        HashSet::new(),
        table(),
    )
    .expect_err("unknown module export must fail");
    assert!(err.to_string().contains("NOPE::FOO"), "{err}");
}

#[test]
fn export_qualified_opaque_module_passes() {
    use oxdock_parser::parse_script_with_modules_and_exports;
    let parsed = parse_script_with_modules_and_exports(
        "EXPORT OPAQ::ANYTHING\n",
        mock_lower,
        HashSet::new(),
        table(),
    )
    .expect("opaque module export parses; the registry resolves it or fails serving");
    assert_eq!(parsed.exports, vec!["OPAQ::ANYTHING".to_string()]);
}

#[test]
fn export_duplicate_fails() {
    use oxdock_parser::parse_script_with_modules_and_exports;
    let err = parse_script_with_modules_and_exports(
        indoc! {r#"
            FUNC FOO() {
                RETURN "x"
            }
            EXPORT [FOO, FOO]
        "#},
        mock_lower,
        HashSet::new(),
        table(),
    )
    .expect_err("duplicate export must fail");
    assert!(err.to_string().contains("duplicate EXPORT"), "{err}");
}

#[test]
fn export_cannot_be_guarded() {
    let err = parse(indoc! {r#"
        [env:FOO]
        EXPORT FOO
    "#})
    .expect_err("guarded export must fail");
    assert!(err.to_string().contains("cannot be guarded"), "{err}");
}

#[test]
fn export_docs_collected_from_hash_comments() {
    use oxdock_parser::parse_script_with_modules_and_exports;
    let parsed = parse_script_with_modules_and_exports(
        indoc! {r#"
            # Adds one.
            # Second line.
            FUNC FOO() {
                RETURN "x"
            }
            EXPORT FOO
        "#},
        mock_lower,
        HashSet::new(),
        table(),
    )
    .expect("export with docs parses");
    let (summary, docs) = parsed.func_docs.get("FOO").expect("docs collected");
    assert_eq!(summary, "Adds one.");
    assert!(docs.contains("Second line"), "{docs}");
}

#[test]
fn export_docs_absent_without_comments() {
    use oxdock_parser::parse_script_with_modules_and_exports;
    let parsed = parse_script_with_modules_and_exports(
        indoc! {r#"
            FUNC FOO() {
                RETURN "x"
            }
            EXPORT FOO
        "#},
        mock_lower,
        HashSet::new(),
        table(),
    )
    .expect("export without docs parses");
    assert!(
        !parsed.func_docs.contains_key("FOO"),
        "no docs without comments"
    );
}

#[test]
fn export_docs_ignore_slash_comments() {
    use oxdock_parser::parse_script_with_modules_and_exports;
    let parsed = parse_script_with_modules_and_exports(
        indoc! {r#"
            // Adds one.
            // Second line.
            FUNC FOO() {
                RETURN "x"
            }
            EXPORT FOO
        "#},
        mock_lower,
        HashSet::new(),
        table(),
    )
    .expect("export with slash comments parses");
    assert!(
        !parsed.func_docs.contains_key("FOO"),
        "slash comments are not docs"
    );
}

#[test]
fn import_cannot_be_guarded() {
    let err = parse(indoc! {r#"
        [env:FOO]
        IMPORT [ALPHA]
    "#})
    .expect_err("guarded import must fail");
    assert!(err.to_string().contains("cannot be guarded"), "{err}");
}

#[test]
fn lone_opaque_import_determines_target() {
    let steps = parse(indoc! {r#"
        IMPORT [OPAQ]
        FOO("x")
    "#})
    .expect("opaque import parses");
    assert_eq!(call_name(&steps, 0), "OPAQ::FOO");
}

#[test]
fn several_opaque_imports_are_ambiguous() {
    let table = ModuleTable {
        modules: HashMap::from([("OPAQ".to_string(), None), ("OPAQ2".to_string(), None)]),
    };
    let err = parse_script_with_modules(
        indoc! {r#"
            IMPORT [OPAQ, OPAQ2]
            FOO("x")
        "#},
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
    let err = parse(indoc! {r#"
        LET $v: STRING = "x"
        LET $m: STRING = ALPHA::INSPECT($v)
    "#})
    .expect_err("qualified INSPECT must fail");
    assert!(
        err.to_string().contains("cannot be module-qualified"),
        "{err}"
    );
}

#[test]
fn bare_inspect_needs_no_import() {
    let steps = parse(indoc! {r#"
        LET $v: STRING = "x"
        LET $m: MAP = INSPECT($v)
    "#})
    .expect("bare INSPECT parses");
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

#[test]
fn file_import_bracket_form_emits_step() {
    let steps = parse(indoc! {r#"
        IMPORT ["./lib.oxdock" AS LIB]
    "#})
    .expect("file import parses");
    assert_eq!(steps.len(), 1, "{steps:?}");
    let StepKind::ImportFile { path, alias } = &steps[0].kind else {
        panic!("expected ImportFile, got {:?}", steps[0].kind);
    };
    assert_eq!(path, "./lib.oxdock");
    assert_eq!(alias, "LIB");
}

#[test]
fn file_import_bare_form_emits_step() {
    let steps = parse("IMPORT \"./lib.oxdock\" AS LIB\n").expect("bare file import parses");
    assert_eq!(steps.len(), 1, "{steps:?}");
    let StepKind::ImportFile { path, alias } = &steps[0].kind else {
        panic!("expected ImportFile, got {:?}", steps[0].kind);
    };
    assert_eq!(path, "./lib.oxdock");
    assert_eq!(alias, "LIB");
}

#[test]
fn file_import_registers_alias_for_calls() {
    let steps = parse(indoc! {r#"
        IMPORT ["./lib.oxdock" AS LIB]
        LIB::FOO("a")
    "#})
    .expect("qualified call through alias parses");
    assert_eq!(steps.len(), 2, "{steps:?}");
    assert_eq!(call_name(&steps, 1), "LIB::FOO");
}

#[test]
fn file_import_alias_enables_bare_calls() {
    let steps = parse(indoc! {r#"
        IMPORT ["./lib.oxdock" AS LIB]
        FOO("a")
    "#})
    .expect("bare call through alias parses");
    assert_eq!(call_name(&steps, 1), "LIB::FOO");
}

#[test]
fn file_import_unimported_alias_fails() {
    let err = parse("LIB::FOO(\"a\")\n").expect_err("unknown module must fail");
    assert!(err.to_string().contains("unknown module `LIB`"), "{err}");
}

#[test]
fn file_import_missing_alias_fails() {
    let err = parse("IMPORT \"./lib.oxdock\"\n").expect_err("missing AS must fail");
    assert!(err.to_string().contains("IMPORT"), "{err}");
}

#[test]
fn file_import_lowercase_alias_fails() {
    let err = parse("IMPORT [\"./lib.oxdock\" AS lib]\n").expect_err("lowercase alias must fail");
    assert!(err.to_string().contains("UPPERCASE"), "{err}");
}

#[test]
fn file_import_alias_colliding_with_module_fails() {
    let err = parse("IMPORT [\"./lib.oxdock\" AS ALPHA]\n").expect_err("collision must fail");
    assert!(
        err.to_string().contains("collides with module `ALPHA`"),
        "{err}"
    );
}

#[test]
fn file_import_duplicate_alias_fails() {
    let err = parse(indoc! {r#"
        IMPORT ["./a.oxdock" AS LIB]
        IMPORT ["./b.oxdock" AS LIB]
    "#})
    .expect_err("duplicate alias must fail");
    assert!(
        err.to_string().contains("duplicate IMPORT alias `LIB`"),
        "{err}"
    );
}

#[test]
fn file_import_template_path_fails() {
    let err =
        parse("IMPORT [\"./{{ $x }}.oxdock\" AS LIB]\n").expect_err("template path must fail");
    assert!(err.to_string().contains("literal"), "{err}");
}

#[test]
fn file_import_remote_url_defers_to_270() {
    let err = parse("IMPORT [\"https://example.com/lib.oxdock\" AS LIB]\n")
        .expect_err("remote URL must fail");
    assert!(err.to_string().contains("#270"), "{err}");
}

#[test]
fn file_import_in_let_captures_return() {
    let steps = parse(indoc! {r#"
        IMPORT ["./lib.oxdock" AS LIB]
        LET $r: STRING = IMPORT ["./other.oxdock" AS OTHER]
    "#})
    .expect("LET IMPORT parses");
    assert_eq!(steps.len(), 2, "{steps:?}");
    let StepKind::AssignCapture { cmd, .. } = &steps[1].kind else {
        panic!("expected AssignCapture, got {:?}", steps[1].kind);
    };
    assert!(
        matches!(cmd.as_ref(), StepKind::ImportFile { alias, .. } if alias == "OTHER"),
        "{cmd:?}"
    );
}

#[test]
fn file_import_in_let_async_spawns_task() {
    let steps = parse(indoc! {r#"
        IMPORT ["./lib.oxdock" AS LIB]
        LET $t: HANDLE = ASYNC IMPORT ["./other.oxdock" AS OTHER]
    "#})
    .expect("LET ASYNC IMPORT parses");
    assert_eq!(steps.len(), 2, "{steps:?}");
    let StepKind::AssignAsync { body, .. } = &steps[1].kind else {
        panic!("expected AssignAsync, got {:?}", steps[1].kind);
    };
    assert_eq!(body.len(), 1, "{body:?}");
    assert!(
        matches!(&body[0].kind, StepKind::ImportFile { alias, .. } if alias == "OTHER"),
        "{:?}",
        body[0].kind
    );
}

#[test]
fn file_import_reexport_fails_explicitly() {
    let err = oxdock_parser::parse_script_with_modules_and_exports(
        indoc! {r#"
            IMPORT ["./lib.oxdock" AS LIB]
            EXPORT LIB::FOO
        "#},
        mock_lower,
        HashSet::new(),
        table(),
    )
    .expect_err("re-export must fail");
    assert!(err.to_string().contains("re-exports"), "{err}");
}

#[test]
fn file_import_multiple_files_emit_ordered_steps() {
    let steps =
        parse("IMPORT [\"a.oxdock\" AS A, \"b.oxdock\" AS B]\n").expect("multi-file import parses");
    assert_eq!(steps.len(), 2, "{steps:?}");
    for (idx, (path, alias)) in [("a.oxdock", "A"), ("b.oxdock", "B")].iter().enumerate() {
        let StepKind::ImportFile { path: p, alias: a } = &steps[idx].kind else {
            panic!("expected ImportFile, got {:?}", steps[idx].kind);
        };
        assert_eq!((p.as_str(), a.as_str()), (*path, *alias));
    }
}

#[test]
fn file_import_mixed_modules_and_files() {
    let steps = parse(indoc! {r#"
        IMPORT [ALPHA, "./lib.oxdock" AS LIB]
        GLOB("*.txt")
        LIB::FOO("a")
    "#})
    .expect("mixed import parses");
    // Module entry emits no step; the file entry emits one.
    assert_eq!(steps.len(), 3, "{steps:?}");
    assert!(matches!(&steps[0].kind, StepKind::ImportFile { alias, .. } if alias == "LIB"));
    assert_eq!(call_name(&steps, 1), "ALPHA::GLOB");
    assert_eq!(call_name(&steps, 2), "LIB::FOO");
}

#[test]
fn file_import_multiline_mixed_form() {
    let steps = parse(indoc! {r#"
        IMPORT [ALPHA,
                "./lib.oxdock" AS LIB, "./nested.oxdock" AS NEST]
    "#})
    .expect("multiline mixed import parses");
    assert_eq!(steps.len(), 2, "{steps:?}");
    let StepKind::ImportFile { alias: a0, .. } = &steps[0].kind else {
        panic!("expected ImportFile, got {:?}", steps[0].kind);
    };
    let StepKind::ImportFile { alias: a1, .. } = &steps[1].kind else {
        panic!("expected ImportFile, got {:?}", steps[1].kind);
    };
    assert_eq!((a0.as_str(), a1.as_str()), ("LIB", "NEST"));
}

#[test]
fn file_import_bare_module_still_works() {
    let steps = parse(indoc! {r#"
        IMPORT ALPHA
        GLOB("*.txt")
    "#})
    .expect("bare module import still parses");
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(call_name(&steps, 0), "ALPHA::GLOB");
}

#[test]
fn file_import_let_rejects_multi() {
    let err = parse("LET $r: STRING = IMPORT [\"a.oxdock\" AS A, \"b.oxdock\" AS B]\n")
        .expect_err("multi-file LET must fail");
    assert!(err.to_string().contains("single file import"), "{err}");
}

#[test]
fn file_import_let_rejects_modules() {
    let err = parse("LET $r: STRING = IMPORT [ALPHA]\n").expect_err("module LET must fail");
    assert!(err.to_string().contains("single file import"), "{err}");
}

#[test]
fn file_import_mixed_guarded_top_level_fails() {
    let err = parse(indoc! {r#"
        [env:FOO]
        IMPORT [ALPHA, "a.oxdock" AS A]
    "#})
    .expect_err("guarded mixed top-level must fail");
    assert!(err.to_string().contains("cannot be guarded"), "{err}");
}

#[test]
fn file_import_bad_entry_fails() {
    let err = parse("IMPORT [ALPHA, \"a.oxdock\"]\n").expect_err("bad entry must fail");
    assert!(err.to_string().contains("IMPORT"), "{err}");
}

#[test]
fn file_import_duplicate_alias_in_one_statement_fails() {
    let err = parse("IMPORT [\"a.oxdock\" AS A, \"b.oxdock\" AS A]\n")
        .expect_err("duplicate alias in one statement must fail");
    assert!(
        err.to_string().contains("duplicate IMPORT alias `A`"),
        "{err}"
    );
}

#[test]
fn file_import_from_bare_form_harvests() {
    let steps = parse("IMPORT [[FOO, BAR FROM \"lib.oxdock\"]]\n").expect("FROM parses");
    assert_eq!(steps.len(), 1, "{steps:?}");
    let StepKind::ImportFuncs { path, alias, names } = &steps[0].kind else {
        panic!("expected ImportFuncs, got {:?}", steps[0].kind);
    };
    assert_eq!(path, "lib.oxdock");
    assert_eq!(alias, &None);
    assert_eq!(names, &["FOO".to_string(), "BAR".to_string()]);
}

#[test]
fn file_import_from_aliased_form() {
    let steps = parse("IMPORT [[FOO FROM \"lib.oxdock\" AS H]]\n").expect("aliased FROM parses");
    assert_eq!(steps.len(), 1, "{steps:?}");
    let StepKind::ImportFuncs { path, alias, names } = &steps[0].kind else {
        panic!("expected ImportFuncs, got {:?}", steps[0].kind);
    };
    assert_eq!(path, "lib.oxdock");
    assert_eq!(alias, &Some("H".to_string()));
    assert_eq!(names, &["FOO".to_string()]);
}

#[test]
fn file_import_from_mixed_with_modules() {
    let steps = parse(indoc! {r#"
        IMPORT [[FUNC_A, FUNC_B FROM "some_file.xdock"], ALPHA]
        FUNC_A("x")
        GLOB("*.txt")
    "#})
    .expect("mixed FROM parses");
    assert_eq!(steps.len(), 3, "{steps:?}");
    assert!(matches!(&steps[0].kind, StepKind::ImportFuncs { .. }));
    assert_eq!(call_name(&steps, 1), "SCRIPT::FUNC_A");
    assert_eq!(call_name(&steps, 2), "ALPHA::GLOB");
}

#[test]
fn file_import_from_multiline() {
    let steps = parse(indoc! {r#"
        IMPORT [[FUNC_A,
                 FUNC_B FROM "some_file.xdock"]]
    "#})
    .expect("multiline FROM parses");
    assert_eq!(steps.len(), 1, "{steps:?}");
    let StepKind::ImportFuncs { names, .. } = &steps[0].kind else {
        panic!("expected ImportFuncs, got {:?}", steps[0].kind);
    };
    assert_eq!(names, &["FUNC_A".to_string(), "FUNC_B".to_string()]);
}

#[test]
fn file_import_from_lowercase_name_fails() {
    let err = parse("IMPORT [[foo FROM \"x.oxdock\"]]\n").expect_err("lowercase must fail");
    assert!(err.to_string().contains("UPPERCASE"), "{err}");
}

#[test]
fn file_import_from_reserved_name_fails() {
    let err = parse_script_with_modules(
        "IMPORT [[GLOB FROM \"x.oxdock\"]]\n",
        mock_lower,
        HashSet::from(["GLOB".to_string()]),
        table(),
    )
    .expect_err("reserved must fail");
    assert!(
        err.to_string()
            .contains("cannot shadow reserved function `GLOB`"),
        "{err}"
    );
}

#[test]
fn file_import_from_missing_names_fails() {
    let err = parse("IMPORT [[FROM \"x.oxdock\"]]\n").expect_err("empty names must fail");
    assert!(err.to_string().contains("IMPORT"), "{err}");
}

#[test]
fn file_import_from_duplicate_bare_name_fails() {
    let err = parse("IMPORT [[F FROM \"a.oxdock\"], [F FROM \"b.oxdock\"]]\n")
        .expect_err("duplicate harvest must fail");
    assert!(
        err.to_string()
            .contains("duplicate function `F` in same scope"),
        "{err}"
    );
}

#[test]
fn file_import_from_remote_defers_to_270() {
    let err = parse("IMPORT [[F FROM \"https://example.com/x.oxdock\"]]\n")
        .expect_err("remote must fail");
    assert!(err.to_string().contains("#270"), "{err}");
}

#[test]
fn file_import_from_rejected_in_let() {
    let err = parse("LET $r: STRING = IMPORT [[F FROM \"x.oxdock\"]]\n")
        .expect_err("FROM in LET must fail");
    assert!(err.to_string().contains("single file import"), "{err}");
}

#[test]
fn file_import_from_bare_unbracketed_misses() {
    let err = parse("IMPORT FOO FROM \"x.oxdock\"\n").expect_err("unbracketed FROM must fail");
    assert!(err.to_string().contains("IMPORT"), "{err}");
}

#[test]
fn file_import_from_reexport_denied_like_alias() {
    use oxdock_parser::parse_script_with_modules_and_exports;
    let err = parse_script_with_modules_and_exports(
        indoc! {r#"
            IMPORT [[FOO FROM "lib.oxdock" AS LIB]]
            EXPORT LIB::FOO
        "#},
        mock_lower,
        HashSet::new(),
        table(),
    )
    .expect_err("re-export must fail");
    assert!(err.to_string().contains("re-exports"), "{err}");
}

#[test]
fn file_import_mixed_entries_emit_in_source_order() {
    let steps = parse("IMPORT [\"b.oxdock\" AS B, [F FROM \"f.oxdock\"], \"a.oxdock\" AS A]\n")
        .expect("mixed entries parse");
    assert_eq!(steps.len(), 3, "{steps:?}");
    assert!(matches!(&steps[0].kind, StepKind::ImportFile { alias, .. } if alias == "B"));
    assert!(
        matches!(&steps[1].kind, StepKind::ImportFuncs { names, .. } if names == &["F".to_string()])
    );
    assert!(matches!(&steps[2].kind, StepKind::ImportFile { alias, .. } if alias == "A"));
}

#[test]
fn file_import_from_single_bracket_harvests() {
    // Single harvests need no inner brackets: the FROM keyword scopes
    // the entry. Multi-name harvests keep [[F, G FROM ...]] so the
    // comma stays grouped.
    let steps = parse("IMPORT [FOO FROM \"lib.oxdock\"]\n").expect("single FROM parses");
    assert_eq!(steps.len(), 1, "{steps:?}");
    let StepKind::ImportFuncs { path, alias, names } = &steps[0].kind else {
        panic!("expected ImportFuncs, got {:?}", steps[0].kind);
    };
    assert_eq!(path, "lib.oxdock");
    assert_eq!(alias, &None);
    assert_eq!(names, &["FOO".to_string()]);
}

#[test]
fn file_import_from_single_bracket_aliased() {
    let steps =
        parse("IMPORT [FOO FROM \"lib.oxdock\" AS H]\n").expect("aliased single FROM parses");
    assert_eq!(steps.len(), 1, "{steps:?}");
    let StepKind::ImportFuncs { path, alias, names } = &steps[0].kind else {
        panic!("expected ImportFuncs, got {:?}", steps[0].kind);
    };
    assert_eq!(path, "lib.oxdock");
    assert_eq!(alias, &Some("H".to_string()));
    assert_eq!(names, &["FOO".to_string()]);
}

#[test]
fn file_import_from_single_bracket_mixed() {
    let steps = parse("IMPORT [ALPHA, FOO FROM \"lib.oxdock\", \"b.oxdock\" AS B]\n")
        .expect("mixed single FROM parses");
    assert_eq!(steps.len(), 2, "{steps:?}");
    assert!(matches!(&steps[0].kind, StepKind::ImportFuncs { .. }));
    assert!(matches!(&steps[1].kind, StepKind::ImportFile { .. }));
}

#[test]
fn file_import_from_single_bracket_value_position_rejected() {
    let err = parse("LET $r: STRING = IMPORT [FOO FROM \"x.oxdock\"]\n")
        .expect_err("harvest in value position must fail");
    assert!(err.to_string().contains("single file import"), "{err}");
}
