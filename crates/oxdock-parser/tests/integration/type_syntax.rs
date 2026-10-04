use crate::common::mock_lower;

use oxdock_parser::ast::StepKind;
use oxdock_parser::parse_script;

fn decl_of(script: &str) -> String {
    let steps = parse_script(script, mock_lower).expect("parses");
    assert_eq!(steps.len(), 1, "{script}");
    match &steps[0].kind {
        StepKind::Assign { decl_type, .. } => decl_type.clone(),
        StepKind::TypeAlias { target, .. } => target.clone(),
        other => panic!("expected declaration, got {other:?}"),
    }
}

#[test]
fn generic_spellings_parse_verbatim() {
    assert_eq!(decl_of("LET $x: LIST<MAP> = []\n"), "LIST<MAP>");
    assert_eq!(
        decl_of("LET $x: MAP<name: STRING, age: INT> = {}\n"),
        "MAP<name: STRING, age: INT>"
    );
    assert_eq!(decl_of("LET $x: LIST<LIST<INT>> = []\n"), "LIST<LIST<INT>>");
}

#[test]
fn spaced_generic_spellings_parse() {
    // Whitespace is insignificant; canonicalization (not rejection)
    // unifies the spellings at resolution.
    for script in [
        "LET $x: LIST< MAP > = []\n",
        "LET $x: MAP<name : STRING> = {}\n",
        "TYPE TEAM = LIST< PERSON >\n",
    ] {
        parse_script(script, mock_lower).expect("spaced spelling parses");
    }
}

#[test]
fn type_alias_parses_to_name_and_target() {
    let steps = parse_script("TYPE PERSON = MAP<name: STRING>\n", mock_lower).expect("parses");
    assert_eq!(steps.len(), 1);
    match &steps[0].kind {
        StepKind::TypeAlias { name, target } => {
            assert_eq!(name, "PERSON");
            assert_eq!(target, "MAP<name: STRING>");
        }
        other => panic!("expected TypeAlias, got {other:?}"),
    }
    let rendered = steps[0].to_string();
    assert_eq!(rendered, "TYPE PERSON = MAP<name: STRING>");
    let reparsed = parse_script(&rendered, mock_lower).expect("round-trip parses");
    assert_eq!(reparsed[0].kind, steps[0].kind);
}

#[test]
fn type_alias_rejects_nested_and_guarded_positions() {
    assert!(parse_script("FUNC F() {\nTYPE X = MAP\n}\n", mock_lower).is_err());
    assert!(parse_script("IF true {\nTYPE X = MAP\n}\n", mock_lower).is_err());
    assert!(parse_script("[bool:true]\nTYPE X = MAP\n", mock_lower).is_err());
}

#[test]
fn generic_for_key_still_hits_the_pin() {
    // Loop keys stay `INT` (default) or `STRING`: a shaped key
    // fails with the pre-existing pin error, not a shape error.
    let err = parse_script(
        "FOR $k: LIST<MAP<ANY>>, $v: MAP<name: STRING> IN [] {\n}\n",
        mock_lower,
    )
    .expect_err("generic FOR key must fail");
    assert!(format!("{err:#}").contains("INT"), "{err:#}");
}

#[test]
fn func_params_accept_generic_spellings() {
    let steps = parse_script("FUNC F($p: LIST<MAP>) {\n}\n", mock_lower).expect("parses");
    match &steps[0].kind {
        StepKind::FuncDef { params, .. } => {
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].1, "LIST<MAP>");
        }
        other => panic!("expected FuncDef, got {other:?}"),
    }
}
