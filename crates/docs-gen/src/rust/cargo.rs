use anyhow::{Context, Result};
use oxdock_fs::{GuardedPath, PathResolver};

use crate::io::read_text;

/// Workspace member list from the root manifest.
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
pub fn workspace_members(root: &GuardedPath, resolver: &PathResolver) -> Result<Vec<String>> {
    let text = read_text(resolver, root, "Cargo.toml")?;
    let doc: toml_edit::DocumentMut = text.parse().context("parse Cargo.toml")?;
    let members = doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .context("workspace.members not found or not an array")?;
    Ok(members
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect())
}

/// Package name/description from a member manifest. Returns `None` when
/// there is no manifest or no `[package]` name, in which case the DSL
/// skips the member and the committed values.json is static data left
/// untouched.
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
pub fn cargo_package(
    root: &GuardedPath,
    resolver: &PathResolver,
    member: &str,
) -> Result<Option<(String, String)>> {
    let rel = format!("{member}/Cargo.toml");
    let path = root.join(&rel)?;
    if resolver.entry_kind(&path).is_err() {
        return Ok(None);
    }
    let text = read_text(resolver, root, &rel)?;
    let doc: toml_edit::DocumentMut = text.parse().context("parse Cargo.toml")?;
    let Some(name) = doc
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str())
    else {
        return Ok(None);
    };
    let description = doc
        .get("package")
        .and_then(|p| p.get("description"))
        .and_then(|v| v.as_str())
        .unwrap_or("No description provided.");
    Ok(Some((name.to_string(), description.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    fn fixture_root() -> (oxdock_fs::GuardedTempDir, GuardedPath, PathResolver) {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
        (temp, root, resolver)
    }

    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    fn write_manifest(
        resolver: &PathResolver,
        root: &GuardedPath,
        member: &str,
        description: &str,
    ) {
        let dir = root.join(member).expect("join");
        resolver.create_dir_all(&dir).expect("mkdir");
        let manifest = dir.join("Cargo.toml").expect("join");
        resolver
            .write_file(
                &manifest,
                format!("[package]\nname = \"{member}-pkg\"\ndescription = \"{description}\"\n")
                    .as_bytes(),
            )
            .expect("write manifest");
        let template_dir = root
            .join(&format!("{member}/.oxdock/template"))
            .expect("join");
        resolver.create_dir_all(&template_dir).expect("mkdir");
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "fixture needs host tempdir and file IO, blocked by Miri isolation"
    )]
    fn package_reads_name_and_description() {
        let (_temp, root, resolver) = fixture_root();
        write_manifest(&resolver, &root, "demo", "first description");
        let (name, description) = cargo_package(&root, &resolver, "demo")
            .expect("package")
            .expect("found");
        assert_eq!(name, "demo-pkg");
        assert_eq!(description, "first description");
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "fixture needs host tempdir and file IO, blocked by Miri isolation"
    )]
    fn missing_manifest_returns_none() {
        let (_temp, root, resolver) = fixture_root();
        assert!(
            cargo_package(&root, &resolver, "ghost")
                .expect("package")
                .is_none(),
            "no manifest must mean no package"
        );
    }
}
