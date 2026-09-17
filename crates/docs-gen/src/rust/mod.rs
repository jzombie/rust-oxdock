pub mod cargo;

use anyhow::{Context, Result};
use oxdock_fs::{GuardedPath, PathResolver};

use crate::io::read_text;

/// Resolve a version string: explicit env wins, otherwise fall
/// back to the workspace manifest. The source is always logged so the
/// choice is visible, never magic. Callers pass the script-visible
/// `CRATE_VERSION` (already inherited from the host); `None` or empty
/// means unset and falls back to the manifest.
pub fn crate_version_with_env(
    root: &GuardedPath,
    resolver: &PathResolver,
    env_version: Option<String>,
) -> Result<String> {
    if let Some(value) = env_version
        && !value.is_empty()
    {
        eprintln!("using CRATE_VERSION={value} from the environment");
        return Ok(value);
    }
    let text = read_text(resolver, root, "Cargo.toml")?;
    let doc: toml_edit::DocumentMut = text.parse().context("parse Cargo.toml")?;
    let version = doc
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .context("workspace.package.version not found in Cargo.toml")?;
    eprintln!("CRATE_VERSION unset; using workspace version {version}");
    Ok(version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    fn fixture_root(manifest: &str) -> (oxdock_fs::GuardedTempDir, GuardedPath, PathResolver) {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
        let manifest_path = root.join("Cargo.toml").expect("join");
        resolver
            .write_file(&manifest_path, manifest.as_bytes())
            .expect("write manifest");
        (temp, root, resolver)
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "fixture needs host tempdir and file IO, blocked by Miri isolation"
    )]
    fn env_value_wins_then_falls_back_to_manifest() {
        let (_temp, root, resolver) =
            fixture_root("[workspace.package]\nversion = \"0.10.0-alpha\"\n");
        assert_eq!(
            crate_version_with_env(&root, &resolver, Some("9.9.9-test".to_string()))
                .expect("version"),
            "9.9.9-test"
        );
        assert_eq!(
            crate_version_with_env(&root, &resolver, None).expect("version"),
            "0.10.0-alpha"
        );
        assert_eq!(
            crate_version_with_env(&root, &resolver, Some(String::new())).expect("version"),
            "0.10.0-alpha"
        );
    }
}
