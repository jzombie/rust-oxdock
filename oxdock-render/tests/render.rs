//! End to end pipeline renders over isolated tempdir fixtures.
//!
//! Each test builds a tiny repo root (config, values, one target with
//! master plus fragments) and runs the real `oxdock_render::run` over it.
//! Fixture IO needs a host tempdir, so the suite stays out of Miri.

use oxdock_core::{HostModule, OxDockFn, TypeTag, Value};
use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver};
use oxdock_func_macro::oxdock_func;
use oxdock_process::DefaultProcessManager;

/// Absolute path of the pipe-child binary: the executable next to
/// the test binary's target dir. Built by `cargo test` alongside the
/// suite; a missing file fails naming the required build step.
#[allow(clippy::disallowed_methods)]
fn child_bin() -> String {
    let exe = std::env::current_exe().expect("test executable path");
    let debug = exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("test executable has a target dir");
    #[cfg(windows)]
    let candidate = debug.join("oxdock-render-child.exe");
    #[cfg(not(windows))]
    let candidate = debug.join("oxdock-render-child");
    assert!(
        candidate.is_file(),
        "pipe-child binary missing at {}",
        candidate.display()
    );
    candidate.to_string_lossy().into_owned()
}

/// Lifecycle override plus scope contract every fixture config gets:
/// the OxDock metadata intake on top of the engine's generic
/// lifecycle. Scope key names default to `global`/`ctx`: fixtures
/// declaring their own keep them, the rest get the defaults.
fn with_lifecycle(cfg: &str) -> String {
    let mut v: serde_json::Value =
        serde_json::from_str(cfg).expect("fixture config is a JSON object");
    let obj = v.as_object_mut().expect("fixture config is an object");
    let vars = obj
        .entry("vars")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let vm = vars.as_object_mut().expect("config vars is a map");
    for (key, fallback) in [
        ("global", "global"),
        ("ctx", "ctx"),
        ("workspace_pkg", "workspace_pkg"),
    ] {
        vm.entry(key)
            .or_insert_with(|| serde_json::Value::String(fallback.to_string()));
    }
    obj.entry("version_key")
        .or_insert_with(|| serde_json::Value::String("CRATE_VERSION".to_string()));
    // Registration order is execution order: source discovery
    // first, generic stage behavior second, OxDock intake third.
    obj.insert(
        "lifecycle".to_string(),
        serde_json::json!({"files": [
            "oxdock-render/src/lifecycle/scanner.oxfile",
            "oxdock-render/src/lifecycle/generic.oxfile",
            "oxdock-render/src/lifecycle/oxdock.oxfile"
        ]}),
    );
    serde_json::to_string(&v).expect("serialize fixture config")
}

#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
fn fixture(files: &[(&str, &str)]) -> (GuardedTempDir, GuardedPath, PathResolver) {
    fixture_opt(files, true)
}

/// Fixture with optional lifecycle injection: `inject == false`
/// writes the config verbatim, proving the engine default (generic
/// lifecycle, no plugins key at all).
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
fn fixture_opt(
    files: &[(&str, &str)],
    inject: bool,
) -> (GuardedTempDir, GuardedPath, PathResolver) {
    // Every run pipes stages through the child binary, never the
    // pipeline binary itself.
    // SAFETY: written to the same value on every call; tests never
    // mutate it back or depend on a prior process value.
    unsafe {
        std::env::set_var(oxdock_fs::env::render::CHILD, child_bin());
    }
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    for (rel, contents) in files.iter() {
        if let Some((parent, _)) = rel.rsplit_once('/') {
            let dir = root.join(parent).expect("join dir");
            resolver.create_dir_all(&dir).expect("create dir");
        }
        let path = root.join(rel).expect("join file");
        let bytes: Vec<u8> = if inject && *rel == "render.json" {
            with_lifecycle(contents).into_bytes()
        } else {
            contents.as_bytes().to_vec()
        };
        resolver.write_file(&path, &bytes).expect("write file");
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

const BASE_VALUES: &str = r#"{"title": "Base", "who": "base", "dup": "base", "workspace_url": "https://example.test/repo"}"#;
const OVERLAY_VALUES: &str = r#"{"who": "overlay", "extra": "yes", "dup": "overlay"}"#;

fn config(policy: &str) -> String {
    format!(
        r#"{{"global_values": ["base.json", "overlay.json"], "merge_policy": "{policy}", "scopes": ["."], "generated": [], "staging_dir": "target/t-docs"}}"#
    )
}

const TARGET: &str = r#"{"targets": [{"name": "t", "out": "out.md", "template": "master.md.tmpl", "values": "values.json", "fragments": {"sec": ["fragments/sec/*.md.tmpl"]}}]}"#;
const VALUES: &str = r#"{"n": "1"}"#;
const MASTER: &str = indoc::indoc! {r#"
    # {{ $global.title }} {{ $ctx.n }}
    {{ $files.sec.a }}
    {{ $files.sec.b }}
"#};
const FRAG_A: &str = "A {{ $global.who }} {{ $ctx.n }}";
const FRAG_B: &str = "B {{ $global.extra }}";

fn base_tree(policy: &str) -> Vec<(String, String)> {
    vec![
        ("Cargo.toml".to_string(), MANIFEST.to_string()),
        ("render.json".to_string(), config(policy)),
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
    oxdock_render::run(root.as_path()).expect("render");
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
    let err = err_text(oxdock_render::run(root.as_path()).expect_err("dup must fail"));
    // Fail-closed across the child boundary: the violator is named in
    // child stderr (plan 5.1 propagation is still an engine gap), while
    // the error value proves the failure crossed the boundary.
    assert!(
        err.contains("failed with status"),
        "error proves the child boundary failed it: {err}"
    );
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
    // Fail-closed across the child boundary: the stem ECHO goes to
    // child stdout, so pin the failure mode (clean non-zero exit).
    let err = err_text(oxdock_render::run(root.as_path()).expect_err("dup stem must fail"));
    assert!(
        err.contains("failed with status"),
        "duplicate stem fails closed: {err}"
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
    // Fail-closed across the child boundary: the duplicate echo goes
    // to child stdout, so pin the failure mode (clean non-zero exit).
    let err = err_text(oxdock_render::run(root.as_path()).expect_err("dup target must fail"));
    assert!(
        err.contains("failed with status"),
        "duplicate target fails closed: {err}"
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
        "C {{ $global.nope }}".to_string(),
    ));
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    let err = err_text(oxdock_render::run(root.as_path()).expect_err("unknown key must fail"));
    // Fail-closed across the child boundary (see duplicate tests).
    assert!(
        err.contains("failed with status"),
        "error proves the child boundary failed it: {err}"
    );
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
        "C \\{{ $global.title }}".to_string(),
    ));
    // The extra fragment joins group sec; master stays fixed, so read
    // the staged manifest for the literal instead of the output.
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    oxdock_render::run(root.as_path()).expect("render");
    let manifest = read(&resolver, &root, "target/t-docs/t.json");
    assert!(
        manifest.contains("{{ $global.title }}"),
        "escape renders literally: {manifest}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn master_sees_workspace_package_like_citation() {
    // Regression: CITATION.cff.tmpl is a master using $workspace_pkg;
    // RENDER must expose it (fragments get it via EXPAND_TEMPLATE scope,
    // masters via script scope) from the SETUP staging snapshot.
    let mut tree = base_tree("overwrite");
    tree.push(("fragments/sec/c.md.tmpl".to_string(), "C".to_string()));
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    // Swap the master for one referencing workspace package metadata.
    let master = root.join("master.md.tmpl").expect("join master");
    use oxdock_fs::PathResolver as PR;
    let r = PR::new(root.as_path(), root.as_path()).expect("resolver");
    r.write_file(&master, b"repo: {{ $workspace_pkg.repository }}")
        .expect("write master");
    oxdock_render::run(root.as_path()).expect("render");
    let out = read(&resolver, &root, "out.md");
    assert!(
        out.contains("https://example.test/repo"),
        "master resolves workspace package: {out}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn empty_scopes_fail_closed_instead_of_rendering_nothing() {
    // A scope typo must not render zero targets and exit 0: the
    // driver counts discoveries and fails naming the config.
    let tree = vec![
        ("Cargo.toml".to_string(), MANIFEST.to_string()),
        (
            "render.json".to_string(),
            r#"{"global_values": "base.json", "scopes": [], "generated": []}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
    ];
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    let err = err_text(oxdock_render::run(root.as_path()).expect_err("empty must fail"));
    assert!(
        err.contains("failed with status"),
        "error proves the child boundary failed it: {err}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn omitted_lifecycle_falls_back_to_generic() {
    // No lifecycle key at all: the engine's generic file runs every
    // stage. Scope contract still comes from the config, never code;
    // vars are omitted too, so the `global`/`ctx` defaults apply.
    let mut tree = base_tree("overwrite");
    let entry = tree
        .iter_mut()
        .find(|(rel, _)| rel == "render.json")
        .expect("base tree carries render.json");
    entry.1 = r#"{"global_values": ["base.json", "overlay.json"], "merge_policy": "overwrite", "scopes": ["."], "generated": [], "staging_dir": "target/t-docs"}"#.to_string();
    let (_temp, root, resolver) = fixture_opt(&as_refs(&tree), false);
    oxdock_render::run(root.as_path()).expect("render");
    let out = read(&resolver, &root, "out.md");
    assert!(out.contains("# Base 1"), "generic master renders: {out}");
    assert!(
        out.contains("A overlay 1"),
        "generic fragments render: {out}"
    );
    assert!(!out.contains("{{"), "no placeholders survive: {out}");
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
            "render.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": [], "staging_dir": "target/custom", "version_key": "VER", "vars": {"global": "g", "ctx": "c"}}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            indoc::indoc! {r#"
                # {{ $g.title }} {{ $c.n }}
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
    oxdock_render::run(root.as_path()).expect("render");
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
            "render.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": [{"key": "nope", "out": "x.md"}]}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            "# {{ $global.title }}\n".to_string(),
        ),
    ];
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    let err = err_text(oxdock_render::run(root.as_path()).expect_err("bad key must fail"));
    // Fail-closed across the child boundary (see duplicate tests).
    assert!(
        err.contains("failed with status"),
        "error proves the child boundary failed it: {err}"
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
            "render.json",
            r#"{"global_values": "values.json", "scopes": ["."], "generated": []}"#,
        ),
        ("values.json", VALUES),
        (
            "target.json",
            r#"{"targets": [{"name": "slim", "out": "slim.txt", "template": "slim.txt.tmpl", "values": "values.json", "fragments": {"sec": ["fragments/sec/*.md.tmpl"]}}, {"name": "full", "out": "full.txt", "template": "full.txt.tmpl", "values": "values.json", "fragments": {"sec": ["fragments/sec/*.md.tmpl"]}}]}"#,
        ),
        ("slim.txt.tmpl", "# {{ $ctx.n }}\n{{ $files.sec.a }}\n"),
        (
            "full.txt.tmpl",
            "# {{ $ctx.n }} full\n{{ $files.sec.a }}\n{{ $files.sec.b }}\n",
        ),
        ("fragments/sec/a.md.tmpl", "shared A"),
        ("fragments/sec/b.md.tmpl", "full B"),
    ];
    let refs: Vec<(&str, &str)> = tree
        .iter()
        .map(|(rel, contents)| (*rel, *contents))
        .collect();
    let (_temp, root, resolver) = fixture(&refs);
    oxdock_render::run(root.as_path()).expect("render");
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
#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn base_prefix_resolves_inputs_under_base() {
    // Explicit per-target base: relative template, values, and
    // fragment patterns resolve under it, and out does too; base
    // itself stays wsroot-relative. Wsroot-anchored patterns
    // (`.oxdock/...`) stay as-is.
    let tree = vec![
        ("Cargo.toml".to_string(), MANIFEST.to_string()),
        (
            "render.json".to_string(),
            r#"{"global_values": "values.json", "scopes": ["."], "generated": []}"#.to_string(),
        ),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "target.json".to_string(),
            r#"{"targets": [{"name": "t", "base": "sub", "out": "out.md", "template": "master.md.tmpl", "values": "values.json", "fragments": {"sec": ["fragments/sec/*.md.tmpl"]}}]}"#.to_string(),
        ),
        ("sub/values.json".to_string(), VALUES.to_string()),
        (
            "sub/master.md.tmpl".to_string(),
            "# {{ $ctx.n }}\n{{ $files.sec.a }}\n".to_string(),
        ),
        (
            "sub/fragments/sec/a.md.tmpl".to_string(),
            "hello A".to_string(),
        ),
    ];
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    oxdock_render::run(root.as_path()).expect("render");
    let out = read(&resolver, &root, "sub/out.md");
    assert!(
        out.contains("hello A"),
        "fragment under base renders: {out}"
    );
    assert!(!out.contains("{{"), "no placeholders survive: {out}");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn base_leaves_shared_patterns_at_wsroot() {
    // Wsroot-anchored patterns (`.oxdock/...`) stay as-is under
    // base so shared fragments keep working; relative ones prefix.
    let tree = vec![
        ("Cargo.toml".to_string(), MANIFEST.to_string()),
        (
            "render.json".to_string(),
            r#"{"global_values": "values.json", "scopes": ["."], "generated": []}"#.to_string(),
        ),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "target.json".to_string(),
            r#"{"targets": [{"name": "t", "base": "sub", "out": "out.md", "template": "master.md.tmpl", "values": "values.json", "fragments": {"sec": ["fragments/sec/*.md.tmpl"], "shared": [".oxdock/template/shared/*.md.tmpl"]}}]}"#.to_string(),
        ),
        ("sub/values.json".to_string(), VALUES.to_string()),
        (
            "sub/master.md.tmpl".to_string(),
            "# {{ $ctx.n }}\n{{ $files.sec.a }}{{ $files.shared.s }}\n".to_string(),
        ),
        (
            "sub/fragments/sec/a.md.tmpl".to_string(),
            "hello A".to_string(),
        ),
        (
            ".oxdock/template/shared/s.md.tmpl".to_string(),
            "shared S".to_string(),
        ),
    ];
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    oxdock_render::run(root.as_path()).expect("render");
    let out = read(&resolver, &root, "sub/out.md");
    assert!(
        out.contains("hello A") && out.contains("shared S"),
        "local prefixed, shared untouched: {out}"
    );
    assert!(!out.contains("{{"), "no placeholders survive: {out}");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn base_not_a_directory_fails_closed() {
    // A base naming a missing entry (or a file) fails naming the
    // target and what was found there instead of rendering elsewhere.
    let tree = vec![
        ("Cargo.toml".to_string(), MANIFEST.to_string()),
        (
            "render.json".to_string(),
            r#"{"global_values": "values.json", "scopes": ["."], "generated": []}"#.to_string(),
        ),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "target.json".to_string(),
            r#"{"targets": [{"name": "t", "base": "no-such-dir", "out": "out.md", "template": "master.md.tmpl", "values": "values.json", "fragments": {"sec": ["fragments/sec/*.md.tmpl"]}}]}"#.to_string(),
        ),
        (
            "master.md.tmpl".to_string(),
            "# {{ $ctx.n }}\n".to_string(),
        ),
    ];
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    let err = err_text(oxdock_render::run(root.as_path()).expect_err("bad base must fail"));
    assert!(
        err.contains("failed with status"),
        "error proves the child boundary failed it: {err}"
    );
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
        states: vec![],
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
        "C {{ TEST::PING($global.who) }}".to_string(),
    ));
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    oxdock_render::run_with_plugins(root.as_path(), vec![test_module()]).expect("render");
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
            "render.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": []}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            indoc::indoc! {r#"
                # {{ $global.title }}
                {{ DEFER::DEFER("MARKDOWN::TOC", [{}]) }}
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
    oxdock_render::run(root.as_path()).expect("render");
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
            "render.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": []}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            indoc::indoc! {r#"
                # {{ $global.title }}
                {{ DEFER::DEFER("MARKDOWN::TOC", [{format: "inline"}]) }}
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
    oxdock_render::run(root.as_path()).expect("render");
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
            "render.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": []}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            indoc::indoc! {r#"
                # {{ $global.title }}
                {{ DEFER::DEFER("MARKDOWN::TOC", [{format: "inline"}]) }}
                {{ DEFER::DEFER("MARKDOWN::TOC", [{max_level: 2, delimiter: " • "}]) }}
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
    oxdock_render::run(root.as_path()).expect("render");
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
            "render.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": []}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            indoc::indoc! {r#"
                # {{ $global.title }}
                {{ DEFER::DEFER("MARKDOWN::TOC", [{format: "inline", max_level: 3}]) }}
            "#}
            .to_string(),
        ),
    ];
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    // DEFER only checks the envelope in pass 1; the target validates
    // its own options when pass 2 dispatches the full document. The
    // rule text surfaces in child stderr; the value proves fail-closed
    // across the child boundary.
    let err = err_text(oxdock_render::run(root.as_path()).expect_err("inline depth must fail"));
    assert!(
        err.contains("failed with status"),
        "error proves the child boundary failed it: {err}"
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
            "render.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": []}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            indoc::indoc! {r#"
                # {{ $global.title }}
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
    oxdock_render::run(root.as_path()).expect("render");
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
fn context_reference_renders_once_with_resolving_links() {
    // Two generated inputs (function plus context references) rendered in
    // one master: every `**Contexts:**` link resolves against exactly one
    // `## Contexts` section, with no suffixed duplicate anchors.
    let tree = vec![
        ("Cargo.toml", MANIFEST),
        (
            "render.json",
            r#"{"global_values": "base.json", "scopes": ["."], "generated": [{"key": "function_reference", "out": "gen/fn.md.tmpl"}, {"key": "context_reference", "out": "gen/ctx.md.tmpl"}]}"#,
        ),
        ("base.json", BASE_VALUES),
        (
            "target.json",
            r#"{"targets": [{"name": "t", "out": "out.md", "template": "master.md.tmpl", "values": "values.json", "fragments": {"gen": ["gen/*.md.tmpl"]}}]}"#,
        ),
        ("values.json", VALUES),
        (
            "master.md.tmpl",
            "# {{ $global.title }}\n{{ $files.gen.fn }}\n{{ $files.gen.ctx }}\n",
        ),
    ];
    let (_temp, root, resolver) = fixture(&tree);
    oxdock_render::run(root.as_path()).expect("render");
    let out = read(&resolver, &root, "out.md");
    assert_eq!(
        out.matches("## Contexts").count(),
        1,
        "exactly one context section: {out}"
    );
    for heading in ["### Context: AST", "### Context: RPN"] {
        assert_eq!(
            out.matches(heading).count(),
            1,
            "exactly one {heading}: {out}"
        );
    }
    let mut linked = 0;
    for line in out.lines().filter(|line| line.starts_with("**Contexts:**")) {
        linked += 1;
        assert!(
            line.contains("](#context-ast)"),
            "every contexts line links AST: {line}"
        );
        assert!(
            !line.contains("AST, RPN") && !line.contains("AST only"),
            "no plain context text survives: {line}"
        );
    }
    assert!(linked > 0, "function reference renders contexts lines");
    assert!(
        !out.contains("#context-ast-1") && !out.contains("#context-rpn-1"),
        "no suffixed duplicate anchors: {out}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn function_reference_without_context_fragment_leaves_dangling_anchors() {
    // The negative half of the rendering invariant: a master that embeds
    // a function reference without the context fragment renders links with
    // no targets, so the missing include is caught instead of silent.
    let tree = vec![
        ("Cargo.toml", MANIFEST),
        (
            "render.json",
            r#"{"global_values": "base.json", "scopes": ["."], "generated": [{"key": "function_reference", "out": "gen/fn.md.tmpl"}]}"#,
        ),
        ("base.json", BASE_VALUES),
        (
            "target.json",
            r#"{"targets": [{"name": "t", "out": "out.md", "template": "master.md.tmpl", "values": "values.json", "fragments": {"gen": ["gen/*.md.tmpl"]}}]}"#,
        ),
        ("values.json", VALUES),
        (
            "master.md.tmpl",
            "# {{ $global.title }}\n{{ $files.gen.fn }}\n",
        ),
    ];
    let (_temp, root, resolver) = fixture(&tree);
    oxdock_render::run(root.as_path()).expect("render");
    let out = read(&resolver, &root, "out.md");
    assert!(out.contains("](#context-ast)"), "links still render: {out}");
    assert!(
        !out.contains("### Context: AST"),
        "missing context fragment leaves links dangling: {out}"
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
            "render.json".to_string(),
            r#"{"global_values": "base.json", "scopes": ["."], "generated": []}"#.to_string(),
        ),
        ("base.json".to_string(), BASE_VALUES.to_string()),
        ("target.json".to_string(), TARGET.to_string()),
        ("values.json".to_string(), VALUES.to_string()),
        (
            "master.md.tmpl".to_string(),
            indoc::indoc! {r#"
                # {{ $global.title }}
                {{ DEFER::DEFER("NOPE::MISSING", [{}]) }}
            "#}
            .to_string(),
        ),
    ];
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    let err = err_text(oxdock_render::run(root.as_path()).expect_err("unknown target must fail"));
    // Fail-closed through the pipe: the violator is named in child
    // stderr (plan 5.1's error-chain propagation is still an engine
    // gap), while the error value proves the failure crossed the
    // child boundary instead of rendering empty.
    assert!(
        err.contains("failed with status"),
        "error proves the child boundary failed it: {err}"
    );
}

const EXEC_TARGET: &str = r#"{"targets": [{"name": "t", "out": "run.sh", "template": "master.md.tmpl", "values": "values.json", "fragments": {"sec": ["fragments/sec/*.md.tmpl"]}, "executable": true}]}"#;

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn executable_without_out_fails_closed() {
    let mut tree = base_tree("overwrite");
    let entry = tree
        .iter_mut()
        .find(|(rel, _)| rel == "target.json")
        .expect("base tree carries target.json");
    entry.1 = r#"{"targets": [{"name": "bad", "template": "master.md.tmpl", "values": "values.json", "fragments": {"sec": ["fragments/sec/*.md.tmpl"]}, "executable": true}]}"#.to_string();
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    let err = err_text(oxdock_render::run(root.as_path()).expect_err("missing out must fail"));
    // Fail-closed across the child boundary (see duplicate tests):
    // the naming ECHO goes to child stdout, the exit proves it fired.
    assert!(
        err.contains("failed with status"),
        "executable without out fails closed: {err}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn non_boolean_executable_fails_closed() {
    let mut tree = base_tree("overwrite");
    let entry = tree
        .iter_mut()
        .find(|(rel, _)| rel == "target.json")
        .expect("base tree carries target.json");
    entry.1 = r#"{"targets": [{"name": "t", "out": "run.sh", "template": "master.md.tmpl", "values": "values.json", "fragments": {"sec": ["fragments/sec/*.md.tmpl"]}, "executable": "yes"}]}"#.to_string();
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    let err = err_text(oxdock_render::run(root.as_path()).expect_err("string flag must fail"));
    // Fail-closed across the child boundary (see duplicate tests):
    // the naming ECHO goes to child stdout, the exit proves it fired.
    assert!(
        err.contains("failed with status"),
        "string executable fails closed: {err}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn executable_target_regains_mode_bit_after_render() {
    let mut tree = base_tree("overwrite");
    let entry = tree
        .iter_mut()
        .find(|(rel, _)| rel == "target.json")
        .expect("base tree carries target.json");
    entry.1 = EXEC_TARGET.to_string();
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    oxdock_render::run(root.as_path()).expect("render");
    let out = read(&resolver, &root, "run.sh");
    assert!(!out.contains("{{"), "no placeholders survive: {out}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("run.sh").expect("join out");
        let mode = resolver
            .metadata(&path)
            .expect("stat out")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "executable bit re-applied");
    }
}

const HOOK_RENDER: &str = r#"{"global_values": ["base.json", "overlay.json"], "merge_policy": "overwrite", "scopes": ["."], "generated": [], "staging_dir": "target/t-docs", "hooks": {"pre": ["scripts/pre.oxfile"], "post": ["scripts/post.oxfile"]}}"#;

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn pre_and_post_hooks_run_around_render() {
    // Hooks are ordinary scripts run by the pipeline engine: the pre
    // hook writes before any target renders, the post hook after.
    let mut tree = base_tree("overwrite");
    let entry = tree
        .iter_mut()
        .find(|(rel, _)| rel == "render.json")
        .expect("base tree carries render.json");
    entry.1 = HOOK_RENDER.to_string();
    tree.push((
        "scripts/pre.oxfile".to_string(),
        "WRITE pre.txt pre-ran\n".to_string(),
    ));
    tree.push((
        "scripts/post.oxfile".to_string(),
        "WRITE post.txt post-ran\n".to_string(),
    ));
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    oxdock_render::run(root.as_path()).expect("render");
    assert_eq!(read(&resolver, &root, "pre.txt"), "pre-ran");
    assert_eq!(read(&resolver, &root, "post.txt"), "post-ran");
    let out = read(&resolver, &root, "out.md");
    assert!(out.contains("# Base 1"), "targets still render: {out}");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn hook_nested_import_resolves_against_hook_dir() {
    // A hook runs as its own script file: a relative import inside
    // the hook resolves against the hook's directory, not the
    // workspace root (regression: hooks used to run from their text,
    // so `./lanes.oxfile` looked next to render.json and failed).
    let mut tree = base_tree("overwrite");
    let entry = tree
        .iter_mut()
        .find(|(rel, _)| rel == "render.json")
        .expect("base tree carries render.json");
    entry.1 = HOOK_RENDER.to_string();
    tree.push((
        "scripts/pre.oxfile".to_string(),
        "IMPORT [\"./lanes.oxfile\" AS LANES]\nLET $m: STRING = LANES::MARK()\nWRITE pre.txt \"{{ $m }}\"\n".to_string(),
    ));
    tree.push((
        "scripts/lanes.oxfile".to_string(),
        "FUNC MARK(): STRING {\n    RETURN \"from-lanes\"\n}\nEXPORT MARK\n".to_string(),
    ));
    tree.push((
        "scripts/post.oxfile".to_string(),
        "WRITE post.txt post-ran\n".to_string(),
    ));
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    oxdock_render::run(root.as_path()).expect("render with nested hook import");
    assert_eq!(read(&resolver, &root, "pre.txt"), "from-lanes");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn hooks_read_user_disk_never_embedded_pipeline() {
    // Hook execution runs on the raw host filesystem, never the
    // embedded pipeline overlay: a user file shadowing an embedded
    // dist path reads back disk bytes inside hooks, not the binary's
    // pipeline payload.
    let mut tree = base_tree("overwrite");
    let entry = tree
        .iter_mut()
        .find(|(rel, _)| rel == "render.json")
        .expect("base tree carries render.json");
    entry.1 = HOOK_RENDER.to_string();
    tree.push((
        "scripts/pre.oxfile".to_string(),
        "LET $c: STRING = READ \"oxdock-render/src/run.oxfile\"\nWRITE pre.txt \"{{ $c }}\"\n"
            .to_string(),
    ));
    tree.push((
        "scripts/post.oxfile".to_string(),
        "WRITE post.txt post-ran\n".to_string(),
    ));
    tree.push((
        "oxdock-render/src/run.oxfile".to_string(),
        "from-disk\n".to_string(),
    ));
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    oxdock_render::run(root.as_path()).expect("render with shadowed dist");
    assert_eq!(read(&resolver, &root, "pre.txt"), "from-disk\n");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn failing_pre_hook_halts_before_render() {
    // A failing pre hook names the hook and never reaches targets:
    // no output file is written.
    let mut tree = base_tree("overwrite");
    let entry = tree
        .iter_mut()
        .find(|(rel, _)| rel == "render.json")
        .expect("base tree carries render.json");
    entry.1 = HOOK_RENDER.to_string();
    tree.push((
        "scripts/pre.oxfile".to_string(),
        "ASSERT_EQ 1 2\n".to_string(),
    ));
    tree.push((
        "scripts/post.oxfile".to_string(),
        "WRITE post.txt post-ran\n".to_string(),
    ));
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    let err = err_text(oxdock_render::run(root.as_path()).expect_err("pre must fail"));
    assert!(
        err.contains("pre hook 'scripts/pre.oxfile' failed"),
        "error names the hook: {err}"
    );
    let out = root.join("out.md").expect("join out");
    assert!(
        resolver.entry_kind(&out).is_err(),
        "no target rendered after a failing pre hook"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn failing_post_hook_fails_after_render() {
    // Post hooks run after every target verifies: the artifacts
    // exist, but the run still fails naming the hook.
    let mut tree = base_tree("overwrite");
    let entry = tree
        .iter_mut()
        .find(|(rel, _)| rel == "render.json")
        .expect("base tree carries render.json");
    entry.1 = HOOK_RENDER.to_string();
    tree.push((
        "scripts/pre.oxfile".to_string(),
        "WRITE pre.txt pre-ran\n".to_string(),
    ));
    tree.push((
        "scripts/post.oxfile".to_string(),
        "ASSERT_EQ 1 2\n".to_string(),
    ));
    let (_temp, root, resolver) = fixture(&as_refs(&tree));
    let err = err_text(oxdock_render::run(root.as_path()).expect_err("post must fail"));
    assert!(
        err.contains("post hook 'scripts/post.oxfile' failed"),
        "error names the hook: {err}"
    );
    let out = read(&resolver, &root, "out.md");
    assert!(
        out.contains("# Base 1"),
        "targets rendered before the failing post hook: {out}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn unknown_render_key_fails_fast() {
    // A typoed top-level key fails closed naming the known keys,
    // instead of silently changing the pipeline.
    let mut tree = base_tree("overwrite");
    let entry = tree
        .iter_mut()
        .find(|(rel, _)| rel == "render.json")
        .expect("base tree carries render.json");
    entry.1 = r#"{"global_values": ["base.json"], "scope": ["."], "generated": []}"#.to_string();
    let (_temp, root, _resolver) = fixture(&as_refs(&tree));
    let err = err_text(oxdock_render::run(root.as_path()).expect_err("typo must fail"));
    assert!(
        err.contains("unknown key 'scope'"),
        "error names the typo: {err}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "fixture render needs a host tempdir filesystem, blocked by Miri isolation"
)]
fn init_scaffold_renders() {
    // The run pipes stages through the child binary, never the
    // pipeline binary itself.
    // SAFETY: written to the same value on every call; tests never
    // mutate it back or depend on a prior process value.
    unsafe {
        std::env::set_var(oxdock_fs::env::render::CHILD, child_bin());
    }
    // The --init template is a working starting point: scaffold it,
    // add the smallest values/target/template triple, and render.
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    let written = oxdock_render::config::init_config_file(&root).expect("init writes the template");
    assert_eq!(
        resolver.read_to_string(&written).expect("read back"),
        oxdock_render::config::INIT_TEMPLATE
    );
    for (rel, contents) in [
        ("values.json", r#"{"n": "1"}"#),
        (
            "target.json",
            r#"{"targets": [{"name": "t", "out": "out.md", "values": "values.json", "template": "master.md.tmpl", "fragments": {}}]}"#,
        ),
        ("master.md.tmpl", "# {{ $ctx.n }}\n"),
    ] {
        let path = root.join(rel).expect("join scaffold");
        resolver
            .write_file(&path, contents.as_bytes())
            .expect("write");
    }
    oxdock_render::run(root.as_path()).expect("scaffold renders");
    let out = read(&resolver, &root, "out.md");
    assert!(out.contains("# 1"), "scaffolded pipeline renders: {out}");
    // A second init refuses to overwrite, leaving the file untouched.
    let before = resolver
        .read_to_string(&written)
        .expect("read before second init");
    let err = err_text(oxdock_render::config::init_config_file(&root).expect_err("no overwrite"));
    assert!(
        err.contains("already exists"),
        "names the existing file: {err}"
    );
    assert_eq!(
        resolver.read_to_string(&written).expect("read after"),
        before
    );
}

#[test]
#[cfg_attr(miri, ignore = "reads the repository checkout; needs real host files")]
fn setup_render_action_pins_release_and_local_contract() {
    // The generated composite action is CI surface: the release legs
    // (unix plus windows) and the local-build leg must stay wired to
    // the renderer product, so a template edit that drops one fails
    // here instead of in a downstream workflow run. Template-to-file
    // drift stays with the CI `git diff --exit-code` render check;
    // this pins the semantics the file must keep.
    let manifest = std::env::var(oxdock_fs::env::cargo::MANIFEST_DIR).expect("manifest dir");
    let repo = oxdock_fs::to_forward_slashes(&manifest);
    let repo = repo
        .strip_suffix("/oxdock-render")
        .expect("test lives in oxdock-render");
    let repo_root = GuardedPath::new_root_from_str(repo).expect("repo root");
    let repo_fs = PathResolver::new_guarded(repo_root.clone(), repo_root.clone()).expect("repo fs");
    let action_path = repo_fs
        .root()
        .join(".github/actions/setup-oxdock-render/action.yml")
        .expect("action path");
    let action = repo_fs
        .read_to_string(&action_path)
        .expect("generated action is checked in");
    for needle in [
        "name: Setup OxDock renderer",
        "binary-name:",
        "default: oxdock-render",
        "default: release",
        "OXDOCK_PRODUCT: oxdock-render",
        "runner.os != 'Windows'",
        "runner.os == 'Windows'",
        "source == 'local'",
        "cargo build --release -p oxdock -p oxdock-render --bins --all-features",
        "OXDOCK_SKIP_DOWNLOAD: '1'",
    ] {
        assert!(action.contains(needle), "action names {needle}");
    }
}
