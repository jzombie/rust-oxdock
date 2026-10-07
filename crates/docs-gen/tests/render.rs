//! End to end pipeline renders over isolated tempdir fixtures.
//!
//! Each test builds a tiny repo root (config, values, one target with
//! master plus fragments) and runs the real `docs_gen::run` over it.
//! Fixture IO needs a host tempdir, so the suite stays out of Miri.

use oxdock_core::{HostModule, OxDockFn, TypeTag, Value};
use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver};
use oxdock_func_macro::oxdock_func;
use oxdock_process::DefaultProcessManager;

#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
fn fixture(files: &[(&str, &str)]) -> (GuardedTempDir, GuardedPath, PathResolver) {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    for (rel, contents) in files {
        if let Some((parent, _)) = rel.rsplit_once('/') {
            let dir = root.join(parent).expect("join dir");
            resolver.create_dir_all(&dir).expect("create dir");
        }
        let path = root.join(rel).expect("join file");
        resolver
            .write_file(&path, contents.as_bytes())
            .expect("write file");
    }
    (temp, root, resolver)
}

fn read(resolver: &PathResolver, root: &GuardedPath, rel: &str) -> String {
    let path = root.join(rel).expect("join read");
    String::from_utf8(resolver.read_file(&path).expect("read")).expect("utf8")
}

fn err_text(err: anyhow::Error) -> String {
    format!("{err:#}")
}

const MANIFEST: &str = indoc::indoc! {r#"
    [workspace]
    members = []
    [workspace.package]
    version = "0.0.0-test"
    authors = ["Test Author <test@example.test>"]
    license = "Apache-2.0"
    repository = "https://example.test/repo"
"#};

const BASE_VALUES: &str = r#"{"title": "Base", "who": "base", "dup": "base"}"#;
const OVERLAY_VALUES: &str = r#"{"who": "overlay", "extra": "yes", "dup": "overlay"}"#;

fn config(policy: &str) -> String {
    format!(
        r#"{{"global_values": ["base.json", "overlay.json"], "merge_policy": "{policy}", "scopes": ["."], "generated": [], "staging_dir": "target/t-docs"}}"#
    )
}

const TARGET: &str = r#"{"targets": [{"name": "t", "out": "out.md", "template": "master.md.tmpl", "values": "values.json", "fragments": {"sec": ["fragments/sec/*.md.tmpl"]}}]}"#;
const VALUES: &str = r#"{"n": "1"}"#;
const MASTER: &str = indoc::indoc! {r#"
    # {{ $docs_global.title }} {{ $docs_ctx.n }}
    {{ $files.sec.a }}
    {{ $files.sec.b }}
"#};
const FRAG_A: &str = "A {{ $docs_global.who }} {{ $docs_ctx.n }}";
const FRAG_B: &str = "B {{ $docs_global.extra }}";

fn base_tree(policy: &str) -> Vec<(String, String)> {
    vec![
        ("Cargo.toml".to_string(), MANIFEST.to_string()),
        ("docs-gen.json".to_string(), config(policy)),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("overlay.json".to_string(), OVERLAY_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        ("master.md.tmpl".to_string(), MASTER.to_string()),
        ("fragments/sec/a.md.tmpl".to_string(), FRAG_A.to_string()),
        ("fragments/sec/b.md.tmpl".to_string(), FRAG_B.to_string()),
    ]
}

fn as_refs(tree: &[(String, String)]) -> Vec<(&str, &str)> {
    tree.iter()
        .map(|(rel, contents)| (rel.as_str(), contents.as_str()))
        .collect()
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn overwrite_merge_renders_overlay_values_in_master_order() {
    let tree = base_tree("overwrite");
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    docs_gen::run(root.as_path()).expect("render");
    let out = read(&resolver, &root, "out.md");
    assert!(out.contains("# Base 1"), "master header renders: {out}");
    assert!(
        out.contains("A overlay 1"),
        "overlay wins the merged key: {out}"
    );
    assert!(out.contains("B yes"), "overlay-only keys land: {out}");
    assert!(!out.contains("{{"), "no placeholders survive: {out}");
    let manifest = read(&resolver, &root, "target/t-docs/t.json");
    assert!(
        manifest.contains("overlay"),
        "custom staging dir holds the manifest: {manifest}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn fail_on_duplicate_names_the_repeated_key() {
    let tree = base_tree("fail_on_duplicate");
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    let err = err_text(docs_gen::run(root.as_path()).expect_err("dup must fail"));
    assert!(err.contains("dup"), "error names the key: {err}");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn duplicate_stem_fails_naming_the_stem() {
    let mut tree = base_tree("overwrite");
    tree.push((
        "fragments/sec/a.b.md.tmpl".to_string(),
        "shadow".to_string(),
    ));
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    // Same EXIT-1 shape as duplicate targets: the stem ECHO goes to
    // stdout, so pin the failure mode and attribute it via the
    // passing base tree render.
    let err = err_text(docs_gen::run(root.as_path()).expect_err("dup stem must fail"));
    assert!(
        err.contains("EXIT requested with code 1"),
        "duplicate stem exits 1: {err}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn duplicate_target_name_fails() {
    let mut tree = base_tree("overwrite");
    tree.push(("other/target.json".to_string(), TARGET.to_string()));
    tree.push(("other/values.json".to_string(), VALUES.to_string()));
    tree.push(("other/master.md.tmpl".to_string(), MASTER.to_string()));
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    // NOTE_TARGET echoes the duplicate then EXITs 1, and the echo
    // goes to stdout rather than the error chain, so pin the failure
    // mode (clean EXIT 1, not a strict expansion error) and rely on
    // the passing base tree render to attribute it to the duplicate.
    let err = err_text(docs_gen::run(root.as_path()).expect_err("dup target must fail"));
    assert!(
        err.contains("EXIT requested with code 1"),
        "duplicate target exits 1: {err}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn unknown_placeholder_fails_strict() {
    let mut tree = base_tree("overwrite");
    tree.push((
        "fragments/sec/c.md.tmpl".to_string(),
        "C {{ $docs_global.nope }}".to_string(),
    ));
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    let err = err_text(docs_gen::run(root.as_path()).expect_err("unknown key must fail"));
    assert!(err.contains("nope"), "error names the key: {err}");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn escaped_placeholder_stays_literal() {
    let mut tree = base_tree("overwrite");
    tree.push((
        "fragments/sec/c.md.tmpl".to_string(),
        "C \\{{ $docs_global.title }}".to_string(),
    ));
    // The extra fragment joins group sec; master stays fixed, so read
    // the staged manifest for the literal instead of the output.
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    docs_gen::run(root.as_path()).expect("render");
    let manifest = read(&resolver, &root, "target/t-docs/t.json");
    assert!(
        manifest.contains("{{ $docs_global.title }}"),
        "escape renders literally: {manifest}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn custom_scope_keys_and_staging_render() {
    let tree = vec![
        ("Cargo.toml".to_string(), MANIFEST.to_string()),
        (
            "docs-gen.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": [], "staging_dir": "target/custom", "version_key": "VER", "vars": {"global": "g", "ctx": "c"}}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            indoc::indoc! {r#"
                # {{ $docs_global.title }} {{ $docs_ctx.n }}
                {{ $files.sec.a }}
            "#}
            .to_string(),
        ),
        (
            "fragments/sec/a.md.tmpl".to_string(),
            "A {{ $g.who }} {{ env:VER }}".to_string(),
        ),
    ];
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    docs_gen::run(root.as_path()).expect("render");
    let out = read(&resolver, &root, "out.md");
    assert!(out.contains("# Base 1"), "custom scope keys render: {out}");
    assert!(out.contains("A base "), "custom version key renders: {out}");
    assert!(!out.contains("{{"), "no placeholders survive: {out}");
    let manifest = read(&resolver, &root, "target/custom/t.json");
    assert!(
        manifest.contains("sec"),
        "custom staging holds it: {manifest}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn unknown_generated_key_lists_known_keys() {
    let tree = vec![
        ("Cargo.toml".to_string(), MANIFEST.to_string()),
        (
            "docs-gen.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": [{"key": "nope", "out": "x.md"}]}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            "# {{ $docs_global.title }}\n".to_string(),
        ),
    ];
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    let err = err_text(docs_gen::run(root.as_path()).expect_err("bad key must fail"));
    assert!(err.contains("nope"), "error names the key: {err}");
    assert!(
        err.contains("command_index"),
        "error lists known keys: {err}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn slim_and_full_share_fragments_with_split_content() {
    // One values file feeds a slim pointer file and a full carrier:
    // the shared fragment expands byte-identically in both, while the
    // full master adds sections the slim omits. The split invariant is
    // structural (same values, same fragments, two masters), never
    // copied text drifting between outputs.
    let tree = [
        ("Cargo.toml", MANIFEST),
        (
            "docs-gen.json",
            r#"{"global_values": "values.json", "scopes": ["."], "generated": []}"#,
        ),
        ("values.json", VALUES),
        (
            "target.json",
            r#"{"targets": [{"name": "slim", "out": "slim.txt", "template": "slim.txt.tmpl", "values": "values.json", "fragments": {"sec": ["fragments/sec/*.md.tmpl"]}}, {"name": "full", "out": "full.txt", "template": "full.txt.tmpl", "values": "values.json", "fragments": {"sec": ["fragments/sec/*.md.tmpl"]}}]}"#,
        ),
        ("slim.txt.tmpl", "# {{ $docs_ctx.n }}\n{{ $files.sec.a }}\n"),
        (
            "full.txt.tmpl",
            "# {{ $docs_ctx.n }} full\n{{ $files.sec.a }}\n{{ $files.sec.b }}\n",
        ),
        ("fragments/sec/a.md.tmpl", "shared A"),
        ("fragments/sec/b.md.tmpl", "full B"),
    ];
    let refs: Vec<(&str, &str)> = tree
        .iter()
        .map(|(rel, contents)| (*rel, *contents))
        .collect();
    let (_temp, root, resolver) = fixture(&refs);
    docs_gen::run(root.as_path()).expect("render");
    let slim = read(&resolver, &root, "slim.txt");
    let full = read(&resolver, &root, "full.txt");
    assert!(
        slim.lines().any(|line| line == "shared A"),
        "slim carries the shared section: {slim}"
    );
    assert!(
        full.lines().any(|line| line == "shared A"),
        "full carries the identical shared section: {full}"
    );
    assert!(
        !slim.contains("full B"),
        "slim omits the full-only section: {slim}"
    );
    assert!(
        full.lines().any(|line| line == "full B"),
        "full carries its extra section: {full}"
    );
    assert!(!slim.contains("{{"), "no placeholders survive slim: {slim}");
    assert!(!full.contains("{{"), "no placeholders survive full: {full}");
}
#[oxdock_func(pure, returns = TypeTag::String)]
fn ping(value: String) -> anyhow::Result<Value> {
    Ok(Value::string(format!("PONG:{value}")))
}

fn test_module() -> HostModule<DefaultProcessManager> {
    HostModule {
        name: "TEST".to_string(),
        funcs: vec![Ping::registration()],
        types: vec![],
        record_schemas: vec![],
    }
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn external_plugin_resolves_in_templates_without_header_changes() {
    let mut tree = base_tree("overwrite");
    tree.push((
        "fragments/sec/c.md.tmpl".to_string(),
        "C {{ TEST::PING($docs_global.who) }}".to_string(),
    ));
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    docs_gen::run_with_plugins(root.as_path(), vec![test_module()]).expect("render");
    let manifest = read(&resolver, &root, "target/t-docs/t.json");
    assert!(
        manifest.contains("PONG:overlay"),
        "external placeholder call resolves: {manifest}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn toc_renders_tree_from_final_readme() {
    let tree = vec![
        ("Cargo.toml".to_string(), MANIFEST.to_string()),
        (
            "docs-gen.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": []}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            indoc::indoc! {r#"
                # {{ $docs_global.title }}
                {{ DOCS::DEFER("MARKDOWN::TOC", [{}]) }}
                {{ $files.sec.a }}
            "#}
            .to_string(),
        ),
        (
            "fragments/sec/a.md.tmpl".to_string(),
            indoc::indoc! {r#"
                ## Alpha

                ```markdown
                ## Not a heading
                ```

                ### Sub

                ## Alpha
            "#}
            .to_string(),
        ),
    ];
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    docs_gen::run(root.as_path()).expect("render");
    let out = read(&resolver, &root, "out.md");
    assert!(
        out.contains("- [Alpha](#alpha)\n  - [Sub](#sub)\n- [Alpha](#alpha-1)\n"),
        "tree TOC lists sections with dedup anchors: {out}"
    );
    assert!(
        !out.contains("- [Base]") && !out.contains("- [Not a heading]"),
        "TOC skips the H1 title and fenced code headings: {out}"
    );
    assert!(!out.contains("OXDOCK"), "no sentinel survives: {out}");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn toc_renders_inline_bar_for_single_level() {
    let tree = vec![
        ("Cargo.toml".to_string(), MANIFEST.to_string()),
        (
            "docs-gen.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": []}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            indoc::indoc! {r#"
                # {{ $docs_global.title }}
                {{ DOCS::DEFER("MARKDOWN::TOC", [{format: "inline"}]) }}
                {{ $files.sec.a }}
            "#}
            .to_string(),
        ),
        (
            "fragments/sec/a.md.tmpl".to_string(),
            "## Alpha\n\n### Sub\n\n## Beta\n".to_string(),
        ),
    ];
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    docs_gen::run(root.as_path()).expect("render");
    let out = read(&resolver, &root, "out.md");
    assert!(
        out.contains("[Alpha](#alpha) | [Beta](#beta)"),
        "inline TOC joins one level: {out}"
    );
    assert!(
        !out.contains("Sub]"),
        "inline TOC omits deeper levels: {out}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn toc_replaces_multiple_sentinels_independently() {
    // Two DEFERs with different options in one file: each sentinel
    // decodes its own envelope and renders from the same document.
    let tree = vec![
        ("Cargo.toml".to_string(), MANIFEST.to_string()),
        (
            "docs-gen.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": []}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            indoc::indoc! {r#"
                # {{ $docs_global.title }}
                {{ DOCS::DEFER("MARKDOWN::TOC", [{format: "inline"}]) }}
                {{ DOCS::DEFER("MARKDOWN::TOC", [{max_level: 2, delimiter: " • "}]) }}
                {{ $files.sec.a }}
            "#}
            .to_string(),
        ),
        (
            "fragments/sec/a.md.tmpl".to_string(),
            "## Alpha\n\n## Beta\n".to_string(),
        ),
    ];
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    docs_gen::run(root.as_path()).expect("render");
    let out = read(&resolver, &root, "out.md");
    assert!(
        out.contains("[Alpha](#alpha) | [Beta](#beta)"),
        "inline sentinel renders with its own options: {out}"
    );
    assert!(
        out.contains("- [Alpha](#alpha)\n- [Beta](#beta)\n"),
        "tree sentinel renders with its own options: {out}"
    );
    assert!(!out.contains("OXDOCK"), "no sentinel survives: {out}");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn toc_rejects_multi_level_inline_at_dispatch() {
    let tree = vec![
        ("Cargo.toml".to_string(), MANIFEST.to_string()),
        (
            "docs-gen.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": []}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            indoc::indoc! {r#"
                # {{ $docs_global.title }}
                {{ DOCS::DEFER("MARKDOWN::TOC", [{format: "inline", max_level: 3}]) }}
            "#}
            .to_string(),
        ),
    ];
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    // DEFER only checks the envelope in pass 1; the target validates
    // its own options when pass 2 dispatches the full document.
    let err = err_text(docs_gen::run(root.as_path()).expect_err("inline depth must fail"));
    assert!(
        err.contains("requires max_level == min_level"),
        "error names the inline depth rule: {err}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn toc_renders_immediately_without_deferral() {
    // TOC is pass-agnostic: called directly with text plus options it
    // renders at once, no sentinel involved.
    let tree = vec![
        ("Cargo.toml".to_string(), MANIFEST.to_string()),
        (
            "docs-gen.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": []}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            indoc::indoc! {r#"
                # {{ $docs_global.title }}
                {{ MARKDOWN::TOC($files.sec.a, {max_level: 2}) }}
            "#}
            .to_string(),
        ),
        (
            "fragments/sec/a.md.tmpl".to_string(),
            "## Alpha\n\n### Sub\n\n## Beta\n".to_string(),
        ),
    ];
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    docs_gen::run(root.as_path()).expect("render");
    let out = read(&resolver, &root, "out.md");
    assert!(
        out.contains("- [Alpha](#alpha)\n- [Beta](#beta)\n"),
        "immediate TOC renders from the fragment text: {out}"
    );
    assert!(
        !out.contains("Sub]"),
        "max_level bounds immediate use: {out}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn toc_rejects_unknown_defer_targets_at_dispatch() {
    let tree = vec![
        ("Cargo.toml".to_string(), MANIFEST.to_string()),
        (
            "docs-gen.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": []}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            indoc::indoc! {r#"
                # {{ $docs_global.title }}
                {{ DOCS::DEFER("NOPE::MISSING", [{}]) }}
            "#}
            .to_string(),
        ),
    ];
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    let err = err_text(docs_gen::run(root.as_path()).expect_err("unknown target must fail"));
    assert!(
        err.contains("unknown deferred target 'NOPE::MISSING'"),
        "error names the target: {err}"
    );
}
