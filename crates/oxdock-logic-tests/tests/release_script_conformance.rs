use anyhow::{Context, Result};
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, PathResolver};

fn repo_root() -> Result<String> {
    // Same layout derivation as docs_conformance: normalize separators
    // first since Windows CARGO_MANIFEST_DIR uses backslashes.
    let manifest_dir = std::env::var(oxdock_fs::env::CARGO_MANIFEST_DIR)
        .context("CARGO_MANIFEST_DIR missing")?
        .replace('\\', "/");
    Ok(manifest_dir
        .strip_suffix("crates/oxdock-logic-tests")
        .context("test must live under crates/oxdock-logic-tests")?
        .trim_end_matches('/')
        .to_string())
}

fn load_release_script() -> Result<String> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let script_path = resolver.root().join("gha-rust-release.oxfile")?;
    resolver.read_to_string(&script_path)
}

/// The release script imports MARKDOWN, so it parses against a module
/// table carrying the plugin, mirroring the CLI `--features markdown`
/// build the release workflow runs.
#[cfg(feature = "markdown")]
fn parse_release_script(text: &str) -> Result<Vec<oxdock_parser::Step>> {
    let mut engine = oxdock_core::Engine::new();
    engine.register_module(oxdock_markdown_plugin::module());
    oxdock_core::parse_script_with_modules(text, engine.module_table())
        .map_err(|err| anyhow::anyhow!("gha-rust-release.oxfile failed to parse: {err}"))
}

/// The release script must always parse: a breaking language change that
/// invalidates it fails the PR that makes the change, never release day.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(
    miri,
    ignore = "reads gha-rust-release.oxfile from the repository checkout layout"
)]
fn release_script_parses() -> Result<()> {
    let text = load_release_script()?;
    parse_release_script(&text)?;
    // Iron rule, pinned in test: exactly the two workspace publish
    // commands, no per-crate flags, no hand-managed crate list. Exec
    // form spells args comma separated, so match that shape.
    assert_eq!(
        text.matches("\"publish\", \"--workspace\"").count(),
        2,
        "release publishes via workspace commands only"
    );
    assert!(
        text.contains("\"publish\", \"--workspace\", \"--dry-run\""),
        "dry-run publish comes first"
    );
    assert!(
        !text.contains("\"publish\", \"-p\""),
        "no per-crate publish flags"
    );
    // Resume hardening: an existing release is deleted first so
    // creation stays unconditional across re-dispatches.
    assert!(
        text.contains("\"gh\", \"release\", \"create\""),
        "release creation step exists"
    );
    // Library products skip binaries: a notes-only create with no
    // asset paths must exist alongside the asset-carrying one.
    assert!(
        text.contains("\"--notes-file\", \"target/release-notes.md\"]"),
        "notes-only creation step exists"
    );
    // Tag identity gate: nothing destructive runs unless the tag
    // points at HEAD, so wrong-version binaries can never replace a
    // release under an old tag.
    assert!(
        text.contains("git rev-list -n 1"),
        "tag identity probe dereferences annotated tags"
    );
    assert!(
        text.contains("ASSERT_EQ $head $tagged"),
        "tag identity assertion exists"
    );
    assert!(
        text.contains("gh release delete"),
        "stale release cleanup exists"
    );
    Ok(())
}

/// The tree version always has extractable notes: the same extraction
/// the release script runs, asserted on every PR, so a missing
/// CHANGELOG section fails long before release day.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(
    miri,
    ignore = "reads the repository manifest and changelog from the checkout"
)]
fn release_notes_extract_for_tree_version() -> Result<()> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let manifest_path = resolver.root().join("Cargo.toml")?;
    let manifest_text = resolver.read_to_string(&manifest_path)?;
    let document: toml_edit::DocumentMut = manifest_text
        .parse()
        .map_err(|err| anyhow::anyhow!("Cargo.toml must parse: {err}"))?;
    let version = document["workspace"]["package"]["version"]
        .as_str()
        .context("workspace.package.version must exist")?;
    let log_path = resolver.root().join("CHANGELOG.md")?;
    let log = resolver.read_to_string(&log_path)?;
    let notes = oxdock_markdown_plugin::markdown::extract_section(&log, &format!("[{version}]"))
        .map_err(|err| anyhow::anyhow!("CHANGELOG must hold the tree version: {err}"))?;
    assert!(
        notes.starts_with(&format!("## [{version}]")),
        "notes open with the version heading"
    );
    assert!(
        notes.lines().count() > 1,
        "notes carry body beyond the heading"
    );
    Ok(())
}

/// A mismatched confirmation fails at the gate with no side effects:
/// version read plus assert run before any publish, tag, or release step.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(miri, ignore = "reads the repository Cargo.toml plus host environment")]
fn release_gate_rejects_mismatched_confirmation() -> Result<()> {
    let text = load_release_script()?;
    let steps = parse_release_script(&text)?;
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let mut io = ExecIo::new();
    io.insert_inherit_env("RELEASE_CONFIRM", "0.0.0-nope");
    io.insert_inherit_env("RELEASE_DRY_RUN", "true");
    let err = run_steps_with_context_result_with_io(&root, &root, &steps, io)
        .expect_err("mismatched confirmation must fail");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("ASSERT_EQ mismatch"),
        "gate names the mismatch: {rendered}"
    );
    Ok(())
}
