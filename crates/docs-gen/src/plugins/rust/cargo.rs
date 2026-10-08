use anyhow::{Context, Result};
use oxdock_fs::{GuardedPath, PathResolver};

use super::io::read_text;

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

/// Workspace citation metadata from the root manifest.
///
/// Returns the first workspace author as a display name (without the
/// `<email>` suffix) plus its structured `Given ... Family <email>`
/// parts, the workspace license, and the repository. Citation rendering
/// must use these values instead of copying them into templates or
/// values files.
pub struct WorkspacePackage {
    pub author: String,
    pub author_given: String,
    pub author_family: String,
    pub author_email: String,
    pub license: String,
    pub repository: String,
}

/// Split one `Given ... Family <email>` author entry into its citation
/// parts. Strict: given plus family names and an email are all required,
/// so a malformed entry fails naming the fix instead of rendering a
/// half empty citation file.
fn split_citation_author(raw: &str) -> Result<(String, String, String)> {
    let (name, email) = raw.rsplit_once('<').ok_or_else(|| {
        anyhow::anyhow!("workspace.package.authors entry {raw:?} needs an `<email>` suffix")
    })?;
    let email = email.strip_suffix('>').ok_or_else(|| {
        anyhow::anyhow!(
            "workspace.package.authors entry {raw:?} needs a closing `>` after the email"
        )
    })?;
    let email = email.trim().to_string();
    if email.is_empty() {
        anyhow::bail!("workspace.package.authors entry {raw:?} has an empty email");
    }
    let mut words: Vec<&str> = name.split_whitespace().collect();
    let Some(family) = words.pop() else {
        anyhow::bail!("workspace.package.authors entry {raw:?} has no display name");
    };
    if words.is_empty() {
        anyhow::bail!(
            "workspace.package.authors entry {raw:?} needs given plus family names (`Given Family <email>`)",
        );
    }
    Ok((words.join(" "), family.to_string(), email))
}

/// Read workspace citation metadata from `Cargo.toml`.
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
pub fn workspace_package(root: &GuardedPath, resolver: &PathResolver) -> Result<WorkspacePackage> {
    let text = read_text(resolver, root, "Cargo.toml")?;
    let doc: toml_edit::DocumentMut = text.parse().context("parse Cargo.toml")?;
    let package = doc
        .get("workspace")
        .and_then(|workspace| workspace.get("package"))
        .context("workspace.package not found in Cargo.toml")?;
    let authors = package
        .get("authors")
        .and_then(|value| value.as_array())
        .context("workspace.package.authors not found in Cargo.toml")?;
    let raw_author = authors
        .iter()
        .find_map(|value| value.as_str())
        .context("workspace.package.authors is empty in Cargo.toml")?;
    let author = raw_author
        .split('<')
        .next()
        .unwrap_or(raw_author)
        .trim()
        .to_string();
    if author.is_empty() {
        anyhow::bail!("workspace.package.authors has no display name in Cargo.toml");
    }
    let license = package
        .get("license")
        .and_then(|value| value.as_str())
        .context("workspace.package.license not found in Cargo.toml")?
        .to_string();
    let repository = package
        .get("repository")
        .and_then(|value| value.as_str())
        .context("workspace.package.repository not found in Cargo.toml")?
        .to_string();
    let (author_given, author_family, author_email) = split_citation_author(raw_author)?;
    Ok(WorkspacePackage {
        author,
        author_given,
        author_family,
        author_email,
        license,
        repository,
    })
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
                indoc::formatdoc! {r#"
                    [package]
                    name = "{member}-pkg"
                    description = "{description}"
                "#}
                .as_bytes(),
            )
            .expect("write manifest");
        let template_dir = root
            .join(&format!("{member}/.oxdock/template"))
            .expect("join");
        resolver.create_dir_all(&template_dir).expect("mkdir");
    }

    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    fn write_root_manifest(resolver: &PathResolver, root: &GuardedPath, manifest: &str) {
        let manifest_path = root.join("Cargo.toml").expect("join");
        resolver
            .write_file(&manifest_path, manifest.as_bytes())
            .expect("write manifest");
    }

    const WORKSPACE_METADATA: &str = indoc::indoc! {r#"
        [workspace.package]
        authors = ["Jeremy Harris <jeremy.harris@zenosmosis.com>"]
        license = "Apache-2.0"
        repository = "https://github.com/jzombie/rust-oxdock"
    "#};

    #[test]
    #[cfg_attr(
        miri,
        ignore = "fixture needs host tempdir and file IO, blocked by Miri isolation"
    )]
    fn workspace_package_reads_manifest_metadata() {
        let (_temp, root, resolver) = fixture_root();
        write_root_manifest(&resolver, &root, WORKSPACE_METADATA);
        let package = workspace_package(&root, &resolver).expect("package");
        assert_eq!(package.author, "Jeremy Harris");
        assert_eq!(package.license, "Apache-2.0");
        assert_eq!(package.repository, "https://github.com/jzombie/rust-oxdock");
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "fixture needs host tempdir and file IO, blocked by Miri isolation"
    )]
    fn workspace_package_splits_citation_author() {
        let (_temp, root, resolver) = fixture_root();
        write_root_manifest(&resolver, &root, WORKSPACE_METADATA);
        let package = workspace_package(&root, &resolver).expect("package");
        assert_eq!(package.author_given, "Jeremy");
        assert_eq!(package.author_family, "Harris");
        assert_eq!(package.author_email, "jeremy.harris@zenosmosis.com");
        for raw in [
            "Jeremy Harris",
            "Jeremy",
            "Jeremy Harris <>",
            "Jeremy Harris <jeremy.harris@zenosmosis.com",
        ] {
            assert!(
                split_citation_author(raw).is_err(),
                "author entry must be `Given Family <email>`: {raw}",
            );
        }
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "fixture needs host tempdir and file IO, blocked by Miri isolation"
    )]
    fn workspace_package_requires_citation_metadata() {
        let (_temp, root, resolver) = fixture_root();
        for manifest in [
            indoc::indoc! {r#"
                [workspace.package]
                license = "Apache-2.0"
                repository = "https://example.test/repo"
            "#},
            indoc::indoc! {r#"
                [workspace.package]
                authors = ["Jeremy Harris"]
                repository = "https://example.test/repo"
            "#},
            indoc::indoc! {r#"
                [workspace.package]
                authors = ["Jeremy Harris"]
                license = "Apache-2.0"
            "#},
        ] {
            write_root_manifest(&resolver, &root, manifest);
            assert!(
                workspace_package(&root, &resolver).is_err(),
                "citation metadata must be complete: {manifest}"
            );
        }
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
