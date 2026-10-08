use anyhow::{Context, Result};
use oxdock_fs::{GuardedPath, PathResolver};

fn repo_root() -> Result<String> {
    let manifest_dir = std::env::var(oxdock_fs::env::CARGO_MANIFEST_DIR)
        .context("CARGO_MANIFEST_DIR missing")?
        .replace('\\', "/");
    Ok(manifest_dir
        .strip_suffix("crates/oxdock-logic-tests")
        .context("test must live under crates/oxdock-logic-tests")?
        .trim_end_matches('/')
        .to_string())
}

fn load_settings_script() -> Result<String> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let script_path = resolver.root().join("gha-apply-settings.oxfile")?;
    resolver.read_to_string(&script_path)
}

/// The settings applier must always parse, and it must own every
/// settings surface: environments via `gh api` plus secrets via
/// `gh secret set`, with no per-repo hand steps hiding outside it.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads gha-apply-settings.oxfile from the repository checkout layout"
)]
fn settings_script_parses_and_covers_all_surfaces() -> Result<()> {
    let text = load_settings_script()?;
    oxdock_core::parse_script(&text)
        .map_err(|err| anyhow::anyhow!("gha-apply-settings.oxfile failed to parse: {err}"))?;
    assert!(
        text.contains("environments"),
        "settings script converges environments"
    );
    assert!(
        text.contains("\"gh\", \"secret\", \"set\""),
        "settings script converges secrets"
    );
    assert!(
        !text.contains("--body"),
        "secret values never ride argv: they pipe through stdin"
    );
    Ok(())
}
