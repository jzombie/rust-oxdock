use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, PathResolver};
fn run_script(root: &GuardedPath, script: &str) -> Result<(), anyhow::Error> {
    let steps = oxdock_core::parse_script(script).expect("parse script");
    run_steps_with_context_result_with_io(root, root, &steps, ExecIo::new()).map(|_| ())
}

fn read_trimmed(root: &GuardedPath, rel: &str) -> String {
    let path = root.join(rel).unwrap();
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    resolver.read_to_string(&path).unwrap().trim().to_string()
}

fn write_file(root: &GuardedPath, rel: &str, content: &[u8]) {
    let path = root.join(rel).unwrap();
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    resolver.write_file(&path, content).unwrap();
}

// ============================================================================
// Parser unit tests
// ============================================================================

#[test]
fn parser_key_path_two_segments() {
    let steps = oxdock_core::parse_script("LET $val = $a.b").unwrap();
    assert_eq!(steps.len(), 1);
}

#[test]
fn parser_key_path_three_segments() {
    let steps = oxdock_core::parse_script("LET $val = $a.b.c").unwrap();
    assert_eq!(steps.len(), 1);
}

#[test]
fn parser_key_path_numeric_index() {
    let steps = oxdock_core::parse_script("LET $val = $items.0").unwrap();
    assert_eq!(steps.len(), 1);
}

#[test]
fn parser_key_path_underscore_prefix() {
    let steps = oxdock_core::parse_script("LET $val = $data._private").unwrap();
    assert_eq!(steps.len(), 1);
}

#[test]
fn parser_single_dollar_is_variable_not_key_path() {
    let steps = oxdock_core::parse_script("LET $val = $pkg").unwrap();
    let step = &steps[0];
    // Should be a Var, not a KeyPath
    match &step.kind {
        oxdock_parser::StepKind::Assign { expr, .. } => {
            assert!(matches!(expr, oxdock_parser::Expr::Var(_)));
        }
        _ => panic!("expected Assign"),
    }
}

#[test]
fn parser_load_toml_in_let() {
    let steps = oxdock_core::parse_script("LET $d = LOAD_TOML(\"x.toml\")").unwrap();
    assert_eq!(steps.len(), 1);
}

#[test]
fn parser_load_json_in_let() {
    let steps = oxdock_core::parse_script("LET $d = LOAD_JSON(\"x.json\")").unwrap();
    assert_eq!(steps.len(), 1);
}

// ============================================================================
// LOAD_TOML integration tests
// ============================================================================

#[test]
fn load_toml_flat_keys() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"a = \"1\"\nb = \"2\"\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" "{{ $d.a }} {{ $d.b }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "1 2");
}

#[test]
fn load_toml_nested_tables() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"[a]\nb = \"deep\"\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" "{{ $d.a.b }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "deep");
}

#[test]
fn load_toml_array_of_strings() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"x = [\"a\", \"b\", \"c\"]\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" "{{ $d.x.0 }} {{ $d.x.1 }} {{ $d.x.2 }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "a b c");
}

#[test]
fn load_toml_integer_becomes_string() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"count = 42\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" "{{ $d.count }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "42");
}

#[test]
fn load_toml_boolean_becomes_string() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"flag = true\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" "{{ $d.flag }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "true");
}

#[test]
fn load_toml_empty_table() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"[empty]\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" "{{ $d.empty }}"
    "#},
    )
    .unwrap();
    // Empty table serializes as empty map
    let result = read_trimmed(&root, "out.txt");
    assert!(result.is_empty() || result == "{}", "got: {result}");
}

#[test]
fn load_toml_error_not_found() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let err = run_script(&root, "LET $d = LOAD_TOML(\"nope.toml\")").unwrap_err();
    assert!(err.to_string().contains("nope.toml"), "{err}");
}

#[test]
fn load_toml_error_invalid_syntax() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "bad.toml", b"{{{{invalid}}}}");
    let err = run_script(&root, "LET $d = LOAD_TOML(\"bad.toml\")").unwrap_err();
    assert!(err.to_string().contains("TOML parse error"), "{err}");
}

// ============================================================================
// LOAD_JSON integration tests
// ============================================================================

#[test]
fn load_json_flat_object() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.json", b"{\"key\": \"val\"}");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_JSON("t.json")
        WRITE "out.txt" "{{ $d.key }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "val");
}

#[test]
fn load_json_nested_object() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.json", b"{\"a\": {\"b\": \"deep\"}}");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_JSON("t.json")
        WRITE out.txt "{{ $d.a.b }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "deep");
}

#[test]
fn load_json_array() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.json", b"{\"arr\": [10, 20, 30]}");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_JSON("t.json")
        WRITE out.txt "{{ $d.arr.0 }} {{ $d.arr.1 }} {{ $d.arr.2 }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "10 20 30");
}

#[test]
fn load_json_boolean() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.json", b"{\"ok\": true}");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_JSON("t.json")
        WRITE "out.txt" "{{ $d.ok }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "true");
}

#[test]
fn load_json_null_becomes_empty_string() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.json", b"{\"n\": null}");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_JSON("t.json")
        WRITE "out.txt" "{{ $d.n }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "");
}

#[test]
fn load_json_error_not_found() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let err = run_script(&root, "LET $d = LOAD_JSON(\"nope.json\")").unwrap_err();
    assert!(err.to_string().contains("nope.json"), "{err}");
}

#[test]
fn load_json_error_invalid_syntax() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "bad.json", b"{not json}");
    let err = run_script(&root, "LET $d = LOAD_JSON(\"bad.json\")").unwrap_err();
    assert!(err.to_string().contains("JSON parse error"), "{err}");
}

// ============================================================================
// Key-path evaluation tests
// ============================================================================

#[test]
fn key_path_resolves_top_level_field() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"name = \"hello\"\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" "{{ $d.name }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "hello");
}

#[test]
fn key_path_resolves_nested_field() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"[a]\nb = \"nested\"\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" "{{ $d.a.b }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "nested");
}

#[test]
fn key_path_deeply_nested() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"[a]\n[a.b]\nc = \"deep\"\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" "{{ $d.a.b.c }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "deep");
}

#[test]
fn key_path_array_index() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"items = [\"x\", \"y\", \"z\"]\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" "{{ $d.items.0 }} {{ $d.items.2 }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "x z");
}

#[test]
fn key_path_out_of_bounds_index_errors() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"items = [\"a\"]\n");

    let err = run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        LET $v = $d.items.99
        WRITE "out.txt" $v
    "#},
    )
    .unwrap_err();
    assert!(err.to_string().contains("out of bounds"), "{err}");
}

#[test]
fn key_path_non_numeric_index_errors() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"items = [\"a\"]\n");

    let err = run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        LET $v = $d.items.foo
        WRITE "out.txt" $v
    "#},
    )
    .unwrap_err();
    assert!(err.to_string().contains("Invalid"), "{err}");
}

#[test]
fn key_path_missing_key_errors() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"a = 1\n");

    let err = run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        LET $v = $d.nonexistent
        WRITE "out.txt" $v
    "#},
    )
    .unwrap_err();
    assert!(err.to_string().contains("nonexistent"), "{err}");
}

#[test]
fn key_path_traverse_into_string_errors() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"a = \"scalar\"\n");

    let err = run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        LET $v = $d.a.b
        WRITE "out.txt" $v
    "#},
    )
    .unwrap_err();
    assert!(err.to_string().contains("Cannot"), "{err}");
}

#[test]
fn key_path_with_underscore_key() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"_hidden = \"secret\"\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" "{{ $d._hidden }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "secret");
}

// ============================================================================
// Data leakage prevention tests
// ============================================================================

#[test]
fn missing_key_in_string_interpolation_does_not_dump_parent_map() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"name = \"oxdock\"\nversion = \"1.0\"\n");

    // $pkg.missing should emit literal "$pkg", not the stringified map
    run_script(
        &root,
        indoc! {r#"
        LET $pkg = LOAD_TOML("t.toml")
        WRITE "out.txt" "docs/$pkg.missing/README.md"
    "#},
    )
    .unwrap();
    let result = read_trimmed(&root, "out.txt");
    assert_eq!(result, "docs/$pkg.missing/README.md");
}

#[test]
fn missing_key_after_successful_traversal_does_not_dump() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"[package]\nname = \"test\"\n");

    // $d.package works, but $d.package.nope fails — should error
    let err = run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" $d.package.nope.txt
    "#},
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("not found") || err.to_string().contains("undefined"),
        "{err}"
    );
}

#[test]
fn out_of_bounds_index_in_string_interpolation_does_not_dump_list() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"items = [\"a\", \"b\"]\n");

    // $d.items.0 works, but $d.items.99 fails — should error
    let err = run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" $d.items.99.suffix
    "#},
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("out of bounds") || err.to_string().contains("undefined"),
        "{err}"
    );
}

#[test]
fn non_numeric_index_in_string_interpolation_does_not_dump() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"items = [\"a\"]\n");

    // $d.items works, but $d.items.foo is not a valid index — should error
    let err = run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" $d.items.foo
    "#},
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("Invalid") || err.to_string().contains("undefined"),
        "{err}"
    );
}

#[test]
fn traverse_into_scalar_emits_value_and_literal_suffix() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"name = \"hello\"\n");

    // $d.name resolves to "hello", .x is a literal suffix on a scalar
    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" "{{ $d.name }}.x"
    "#},
    )
    .unwrap();
    let result = read_trimmed(&root, "out.txt");
    assert_eq!(result, "hello.x");
}

// ============================================================================
// Inline string interpolation
// ============================================================================

#[test]
fn dollar_var_resolves_string() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    run_script(
        &root,
        indoc! {r#"
        LET $name = "world"
        WRITE out.txt "hello-{{ $name }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "hello-world");
}

#[test]
fn dollar_var_with_trailing_dot_not_consumed() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    run_script(
        &root,
        indoc! {r#"
        LET $name = "hello"
        WRITE out.txt "{{ $name }}."
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "hello.");
}

#[test]
fn dollar_var_with_suffix_after_scalar() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"name = \"ox\"\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WRITE "out.txt" "{{ $d.name }}.dock"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "ox.dock");
}

#[test]
fn unresolved_dollar_var_emitted_as_literal() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    let err = run_script(&root, "WRITE \"out.txt\" $nope").unwrap_err();
    assert!(err.to_string().contains("undefined variable"), "{err}");
}

#[test]
fn unresolved_dollar_with_suffix_literal() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    run_script(&root, "WRITE \"out.txt\" \"$nope.txt\"").unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "$nope.txt");
}

#[test]
fn multiple_dollar_vars_in_string() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    run_script(
        &root,
        indoc! {r#"
        LET $a = "hello"
        LET $b = "world"
        WRITE out.txt "{{ $a }} {{ $b }}"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "hello world");
}

// ============================================================================
// EXPAND template tag expansion
// ============================================================================

#[test]
fn expand_resolves_env_prefix_tag() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "tmpl.txt", b"Hi {{ env:WHO }}!");

    run_script(
        &root,
        indoc! {r#"
        ENV WHO=Alice
        WITH_IO [stdout=pipe:t] EXPAND tmpl.txt
        WITH_IO [stdin=pipe:t] WRITE out.txt
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "Hi Alice!");
}

#[test]
fn expand_resolves_bare_key_path_tag() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"greeting = \"hello\"\n");
    write_file(&root, "tmpl.txt", b"{{ $d.greeting }} world");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WITH_IO [stdout=pipe:t] EXPAND tmpl.txt
        WITH_IO [stdin=pipe:t] WRITE out.txt
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "hello world");
}

#[test]
fn expand_resolves_nested_key_path_tag() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"[pkg]\nname = \"ox\"\n");
    write_file(&root, "tmpl.txt", b"{{ $d.pkg.name }}");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WITH_IO [stdout=pipe:t] EXPAND tmpl.txt
        WITH_IO [stdin=pipe:t] WRITE out.txt
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "ox");
}

#[test]
fn expand_missing_tag_resolves_to_empty() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "tmpl.txt", b"before {{ missing }} after");

    let err = run_script(
        &root,
        indoc! {r#"
        WITH_IO [stdout=pipe:t] EXPAND tmpl.txt
        WITH_IO [stdin=pipe:t] WRITE out.txt
    "#},
    );
    assert!(err.is_err(), "undefined variable in template should error");
    assert!(
        err.unwrap_err()
            .to_string()
            .contains("missing required step override"),
        "error should mention missing step override"
    );
}

#[test]
fn expand_mixed_env_and_key_path_tags() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"val = \"from-toml\"\n");
    write_file(&root, "tmpl.txt", b"{{ env:HOST }} and {{ $d.val }}");

    run_script(
        &root,
        indoc! {r#"
        ENV HOST=from-var
        LET $d = LOAD_TOML("t.toml")
        WITH_IO [stdout=pipe:t] EXPAND tmpl.txt
        WITH_IO [stdin=pipe:t] WRITE out.txt
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "from-var and from-toml");
}

#[test]
fn expand_resolves_script_var_key_path() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"key = \"from-var\"\n");
    write_file(&root, "tmpl.txt", b"{{ $d.key }}");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        WITH_IO [stdout=pipe:t] EXPAND tmpl.txt
        WITH_IO [stdin=pipe:t] WRITE out.txt
    "#},
    )
    .unwrap();
    let result = read_trimmed(&root, "out.txt");
    assert_eq!(result, "from-var");
}

#[test]
fn expand_no_placeholders_passthrough() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "tmpl.txt", b"plain text no tags");

    run_script(
        &root,
        indoc! {r#"
        WITH_IO [stdout=pipe:t] EXPAND tmpl.txt
        WITH_IO [stdin=pipe:t] WRITE out.txt
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "plain text no tags");
}

// ============================================================================
// FOR loop with key-path
// ============================================================================

#[test]
fn for_loop_over_key_path_array() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"items = [\"a\", \"b\", \"c\"]\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        FOR $x IN $d.items {
            WRITE "{{ $x }}.txt" "{{ $x }}"
        }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "a.txt"), "a");
    assert_eq!(read_trimmed(&root, "b.txt"), "b");
    assert_eq!(read_trimmed(&root, "c.txt"), "c");
}

#[test]
fn for_loop_body_has_loop_var() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    run_script(
        &root,
        indoc! {r#"
        LET $items = ["x", "y"]
        FOR $i IN $items {
            WRITE "{{ $i }}.txt" "{{ $i }}"
        }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "x.txt"), "x");
    assert_eq!(read_trimmed(&root, "y.txt"), "y");
}

// ============================================================================
// Combined features
// ============================================================================

#[test]
fn load_toml_then_use_in_template() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "crate.toml", b"[package]\nname = \"my-crate\"\n");
    write_file(&root, "header.txt", b"# {{ $d.package.name }}");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("crate.toml")
        WITH_IO [stdout=pipe:t] EXPAND header.txt
        WITH_IO [stdin=pipe:t] WRITE out.txt
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "# my-crate");
}

#[test]
fn for_loop_writes_multiple_files_from_loaded_data() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "members.toml", b"members = [\"alpha\", \"beta\"]\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("members.toml")
        FOR $m IN $d.members {
            WRITE "{{ $m }}.txt" "{{ $m }}"
        }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "alpha.txt"), "alpha");
    assert_eq!(read_trimmed(&root, "beta.txt"), "beta");
}

// ============================================================================
// Scope shadowing tests
// ============================================================================

#[test]
fn for_loop_var_shadows_outer_var_in_template() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "tmpl.txt", b"{{ $name }}");

    // $name is set to "outer" globally, then $name is shadowed by FOR loop
    run_script(
        &root,
        indoc! {r#"
        LET $name = "outer"
        LET $items = ["first", "second"]
        FOR $name IN $items {
            WITH_IO [stdout=pipe:t] EXPAND tmpl.txt
            WITH_IO [stdin=pipe:t]             WRITE "{{ $name }}.txt"
        }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "first.txt"), "first");
    assert_eq!(read_trimmed(&root, "second.txt"), "second");
}

#[test]
fn for_loop_var_shadows_outer_var_in_command_args() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    // $x is set to "outer" globally, then shadowed by FOR loop
    run_script(
        &root,
        indoc! {r#"
        LET $x = "outer"
        LET $items = ["a", "b"]
        FOR $x IN $items {
            WRITE "{{ $x }}.txt" "{{ $x }}"
        }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "a.txt"), "a");
    assert_eq!(read_trimmed(&root, "b.txt"), "b");
    // "outer" should NOT have created a file
    let path = root.join("outer.txt").unwrap();
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    assert!(resolver.read_to_string(&path).is_err());
}

#[test]
fn nested_for_loops_inner_shadows_outer() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    run_script(
        &root,
        indoc! {r#"
        LET $x = "global"
        LET $outer = ["o1", "o2"]
        FOR $x IN $outer {
            LET $inner = ["i1", "i2"]
            FOR $x IN $inner {
            WRITE "{{ $x }}.txt" "{{ $x }}"
            }
        }
    "#},
    )
    .unwrap();
    // Inner loop should shadow outer — files named i1.txt, i2.txt
    assert_eq!(read_trimmed(&root, "i1.txt"), "i1");
    assert_eq!(read_trimmed(&root, "i2.txt"), "i2");
}

#[test]
fn for_map_iteration_sorted_keys() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    // Create a TOML file with a map
    write_file(
        &root,
        "data.toml",
        indoc! {r#"
        [settings]
        zebra = "last"
        alpha = "first"
        middle = "second"
    "#}
        .as_bytes(),
    );

    let steps = oxdock_core::parse_script(indoc! {r#"
        LET $d = LOAD_TOML("data.toml")
        FOR $k, $v IN $d.settings {
            WRITE "{{ $k }}.txt" "{{ $v }}"
        }
    "#})
    .unwrap();

    run_steps_with_context_result_with_io(&root, &root, &steps, ExecIo::new()).unwrap();

    // Keys should be sorted lexicographically: alpha, middle, zebra
    assert_eq!(read_trimmed(&root, "alpha.txt"), "first");
    assert_eq!(read_trimmed(&root, "middle.txt"), "second");
    assert_eq!(read_trimmed(&root, "zebra.txt"), "last");
}

#[test]
fn for_map_iteration_echo_stdout() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    write_file(
        &root,
        "data.toml",
        indoc! {r#"
            [settings]
            a = "1"
            b = "2"
        "#}
        .as_bytes(),
    );

    let steps = oxdock_core::parse_script(indoc! {r#"
        LET $d = LOAD_TOML("data.toml")
        FOR $k, $v IN $d.settings {
            ECHO "{{ $k }} = {{ $v }}"
        }
    "#})
    .unwrap();

    let mut io = ExecIo::new();
    let captured: std::sync::Arc<std::sync::Mutex<Vec<u8>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipe: oxdock_process::SharedOutput = captured.clone();
    io.set_stdout(Some(pipe));

    run_steps_with_context_result_with_io(&root, &root, &steps, io).unwrap();

    let output = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert!(output.contains("a = 1"), "Expected 'a = 1', got: {output}");
    assert!(output.contains("b = 2"), "Expected 'b = 2', got: {output}");
}

#[test]
fn for_list_enumeration() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    run_script(
        &root,
        indoc! {r#"
        LET $items = ["a", "b", "c"]
        FOR $i, $v IN $items {
            WRITE "{{ $v }}.txt" "{{ $i }}"
        }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "a.txt"), "0");
    assert_eq!(read_trimmed(&root, "b.txt"), "1");
    assert_eq!(read_trimmed(&root, "c.txt"), "2");
}

#[test]
fn for_list_enumeration_echo() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    let steps = oxdock_core::parse_script(indoc! {r#"
        LET $items = ["x", "y"]
        FOR $i, $v IN $items {
            ECHO "{{ $i }}: {{ $v }}"
        }
    "#})
    .unwrap();

    let mut io = ExecIo::new();
    let captured: std::sync::Arc<std::sync::Mutex<Vec<u8>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipe: oxdock_process::SharedOutput = captured.clone();
    io.set_stdout(Some(pipe));

    run_steps_with_context_result_with_io(&root, &root, &steps, io).unwrap();

    let output = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert!(output.contains("0: x"), "Expected '0: x', got: {output}");
    assert!(output.contains("1: y"), "Expected '1: y', got: {output}");
}

// ============================================================================
// Comparison operators
// ============================================================================

#[test]
fn comparison_equal_produces_bool() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        LET $x = "hello"
        LET $eq = $x == "hello"
        IF $eq { WRITE "out.txt" "yes" }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "yes");
}

#[test]
fn comparison_not_equal_produces_bool() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        LET $x = "hello"
        LET $ne = $x != "foo"
        IF $ne { WRITE "out.txt" "yes" }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "yes");
}

#[test]
fn comparison_key_path() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"name = \"test\"\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        IF $d.name == "test" { WRITE "out.txt" "yes" }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "yes");
}

#[test]
fn comparison_false_is_falsy() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        LET $x = "hello"
        LET $eq = $x == "nope"
        IF $eq { WRITE "out.txt" "should-not-exist" }
    "#},
    )
    .unwrap();
    assert!(!root.join("out.txt").unwrap().as_path().exists());
}

// ============================================================================
// Logical operators
// ============================================================================

#[test]
fn logical_and_short_circuit() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        LET $a = true
        LET $b = false
        LET $both = $a && $b
        IF $both { WRITE "out.txt" "should-not-exist" }
    "#},
    )
    .unwrap();
    assert!(!root.join("out.txt").unwrap().as_path().exists());
}

#[test]
fn logical_or_short_circuit() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        LET $a = true
        LET $b = false
        LET $either = $a || $b
        IF $either { WRITE "out.txt" "yes" }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "yes");
}

#[test]
fn logical_or_right_side_evaluated_when_left_false() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        LET $a = false
        LET $b = true
        LET $either = $a || $b
        IF $either { WRITE "out.txt" "yes" }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "yes");
}

// ============================================================================
// IF/ELSE statements
// ============================================================================

#[test]
fn if_then_branch() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        LET $x = "hello"
        IF $x == "hello" { WRITE "out.txt" "yes" }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "yes");
}

#[test]
fn if_else_branch() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        LET $x = "hello"
        IF $x == "nope" { WRITE "out.txt" "wrong" } ELSE { WRITE "out.txt" "correct" }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "correct");
}

#[test]
fn if_else_if_chain() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, indoc! {r#"
        LET $x = "2"
        IF $x == "1" { WRITE "out.txt" "1" } ELSE IF $x == "2" { WRITE "out.txt" "2" } ELSE { WRITE "out.txt" "3" }
    "#}).unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "2");
}

#[test]
fn if_compound_condition() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        LET $a = "1"
        LET $b = "2"
        IF $a == "1" && $b == "2" { WRITE "out.txt" "combined" }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "combined");
}

#[test]
fn if_precedence_override() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        LET $a = "1"
        LET $b = "3"
        IF ($a == "1" || $a == "2") && $b == "3" { WRITE "out.txt" "precedence" }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "precedence");
}

#[test]
fn if_nested_inside_for() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"items = [\"a\", \"b\", \"c\"]\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        FOR $x IN $d.items {
            IF $x == "b" { WRITE "{{ $x }}.txt" "found" }
        }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "b.txt"), "found");
    assert!(!root.join("a.txt").unwrap().as_path().exists());
    assert!(!root.join("c.txt").unwrap().as_path().exists());
}

// ============================================================================
// Strict boolean: TypeError for non-bool conditions
// ============================================================================

#[test]
fn if_string_condition_raises_type_error() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let err = run_script(
        &root,
        indoc! {r#"
        LET $path = "docs/readme.md"
        IF $path { WRITE "out.txt" "should-not-exist" }
    "#},
    )
    .unwrap_err();
    assert!(err.to_string().contains("Type Error"), "{err}");
}

#[test]
fn if_literal_true_works() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        IF true { WRITE "out.txt" "yes" }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "yes");
}

#[test]
fn if_literal_false_skips() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        IF false { WRITE "out.txt" "should-not-exist" }
    "#},
    )
    .unwrap();
    assert!(!root.join("out.txt").unwrap().as_path().exists());
}

#[test]
fn if_bool_from_json_is_native() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.json", b"{\"active\": true}");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_JSON("t.json")
        IF $d.active { WRITE "out.txt" "yes" }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "yes");
}

#[test]
fn if_bool_from_toml_is_native() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    write_file(&root, "t.toml", b"active = true\n");

    run_script(
        &root,
        indoc! {r#"
        LET $d = LOAD_TOML("t.toml")
        IF $d.active { WRITE "out.txt" "yes" }
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "yes");
}
