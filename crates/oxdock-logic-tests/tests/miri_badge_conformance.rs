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

fn load_badge_script() -> Result<String> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let script_path = resolver.root().join("scripts/gha/gha-miri-badge.oxfile")?;
    resolver.read_to_string(&script_path)
}

/// The badge script must always parse: a breaking language change that
/// invalidates it fails the PR that makes the change, never the nightly.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads scripts/gha/gha-miri-badge.oxfile from the repository checkout layout"
)]
fn badge_script_parses_and_covers_all_surfaces() -> Result<()> {
    let text = load_badge_script()?;
    oxdock_core::parse_script(&text).map_err(|err| {
        anyhow::anyhow!("scripts/gha/gha-miri-badge.oxfile failed to parse: {err}")
    })?;
    assert!(
        text.contains("miri-coverage.json"),
        "badge script writes the badge JSON"
    );
    assert!(
        text.contains("GITHUB_OUTPUT") && text.contains("GITHUB_STEP_SUMMARY"),
        "badge script bridges both runner files"
    );
    Ok(())
}
