use anyhow::{Context, Result, ensure};
#[allow(clippy::disallowed_types)]
use oxdock_fs::UnguardedPath;
use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver, WorkspaceFs, command_path};
use oxdock_process::CommandBuilder;
use std::collections::BTreeMap;
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value};

/// Builder that materializes a Cargo project template inside a temporary,
/// auto-cleaned directory for integration tests.
#[allow(clippy::disallowed_types)]
pub struct FixtureBuilder {
    template: UnguardedPath,
    replacements: BTreeMap<String, DependencyReplacement>,
    workspace_root_env: Option<PathBuf>,
    workspace_manifest_root: Option<PathBuf>,
}

#[allow(clippy::disallowed_types)]
enum DependencyReplacement {
    Path(PathBuf),
    Version(String),
}

/// Handle to a copied fixture workspace.
#[allow(clippy::disallowed_types)]
pub struct FixtureInstance {
    #[allow(dead_code)]
    tempdir: GuardedTempDir,
    root: GuardedPath,
    workspace_root_env: Option<PathBuf>,
}

impl FixtureBuilder {
    /// Create a builder from an on-disk template directory.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub fn new(template: impl AsRef<Path>) -> Result<Self> {
        let path = template.as_ref();
        ensure!(
            path.exists(),
            "fixture template {} does not exist",
            path.display()
        );
        Ok(Self {
            template: UnguardedPath::external(path),
            replacements: BTreeMap::new(),
            workspace_root_env: None,
            workspace_manifest_root: None,
        })
    }

    /// Force the named dependency to use an absolute path inside the instantiated project.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub fn with_path_dependency(mut self, name: impl Into<String>, path: impl AsRef<Path>) -> Self {
        self.replacements.insert(
            name.into(),
            DependencyReplacement::Path(path.as_ref().to_path_buf()),
        );
        self
    }

    /// Force the named dependency to use a literal version string.
    pub fn with_version_dependency(
        mut self,
        name: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        self.replacements
            .insert(name.into(), DependencyReplacement::Version(version.into()));
        self
    }

    /// Override the workspace root that should be exposed to commands spawned via [`FixtureInstance`].
    /// When present, the spawned process receives `OXDOCK_WORKSPACE_ROOT`, enabling DSL scripts
    /// to reference the original repository while working against the copied fixture.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub fn with_workspace_root(mut self, root: impl AsRef<Path>) -> Self {
        self.workspace_root_env = Some(root.as_ref().to_path_buf());
        self
    }

    /// Provide a workspace root used only to resolve workspace dependency versions
    /// when patching fixture manifests.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub fn with_workspace_manifest_root(mut self, root: impl AsRef<Path>) -> Self {
        self.workspace_manifest_root = Some(root.as_ref().to_path_buf());
        self
    }

    /// Copy the template into a guarded temporary directory, returning a handle
    /// that cleans up automatically on drop.
    pub fn instantiate(self) -> Result<FixtureInstance> {
        let tempdir = GuardedPath::tempdir()?;
        let root = tempdir.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone())?;

        copy_fixture_template(&resolver, &self.template, &root)?;

        if !self.replacements.is_empty() || self.workspace_manifest_root.is_some() {
            patch_manifest(
                &resolver,
                &root,
                &self.replacements,
                self.workspace_manifest_root.as_deref(),
            )?;
        }

        let workspace_root_env = self.workspace_root_env;

        Ok(FixtureInstance {
            tempdir,
            root,
            workspace_root_env,
        })
    }
}

impl FixtureInstance {
    /// Guarded path to the fixture root directory.
    pub fn root(&self) -> &GuardedPath {
        &self.root
    }

    /// Guarded path to `Cargo.toml` inside the copied fixture.
    pub fn manifest_path(&self) -> Result<GuardedPath> {
        self.root.join("Cargo.toml")
    }

    /// Build a `CommandBuilder` scoped to the fixture root.
    pub fn cargo(&self) -> CommandBuilder {
        let mut builder = CommandBuilder::new("cargo");
        let cwd = command_path(self.root()).into_owned();
        builder.current_dir(cwd);
        if let Some(root) = &self.workspace_root_env {
            builder.env("OXDOCK_WORKSPACE_ROOT", root);
        }
        builder
    }
}

#[allow(clippy::disallowed_types)]
fn patch_manifest(
    resolver: &PathResolver,
    root: &GuardedPath,
    replacements: &BTreeMap<String, DependencyReplacement>,
    workspace_manifest_root: Option<&Path>,
) -> Result<()> {
    let manifest_path = root.join("Cargo.toml")?;
    let contents = resolver
        .read_to_string(&manifest_path)
        .context("failed to read fixture manifest")?;
    let mut doc = contents
        .parse::<DocumentMut>()
        .context("failed to parse fixture manifest")?;
    let workspace_versions = load_workspace_dependencies(resolver, workspace_manifest_root)?;

    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        let Some(item) = doc.get_mut(section) else {
            continue;
        };
        if let Item::Table(table) = item {
            patch_dependency_table(table, replacements, &workspace_versions);
        }
    }

    resolver.write_file(&manifest_path, doc.to_string().as_bytes())?;
    Ok(())
}

#[allow(clippy::disallowed_types)]
fn copy_fixture_template(
    resolver: &PathResolver,
    src: &UnguardedPath,
    dst: &GuardedPath,
) -> Result<()> {
    resolver.create_dir_all(dst)?;
    let entries = resolver
        .read_dir_entries_unguarded(src)
        .with_context(|| format!("reading {}", src))?;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "target" {
            continue;
        }
        let src_path = UnguardedPath::external(entry.path());
        let dst_path = dst.join(&name)?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to read entry type for {}", src_path))?;
        if file_type.is_dir() {
            copy_fixture_template(resolver, &src_path, &dst_path)?;
        } else {
            resolver.copy_file_from_unguarded(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn patch_dependency_table(
    table: &mut Table,
    replacements: &BTreeMap<String, DependencyReplacement>,
    workspace_versions: &BTreeMap<String, String>,
) {
    for (name, replacement) in replacements {
        let Some(item) = table.get_mut(name) else {
            continue;
        };
        match replacement {
            DependencyReplacement::Path(path) => apply_path_replacement(item, path),
            DependencyReplacement::Version(version) => apply_version_replacement(item, version),
        }
    }

    for (name, version) in workspace_versions {
        let Some(item) = table.get_mut(name) else {
            continue;
        };
        if has_workspace_flag(item) {
            apply_version_replacement(item, version);
        }
    }
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn apply_path_replacement(item: &mut Item, path: &Path) {
    let path_value = Value::from(path_string(path));
    match item {
        Item::Value(Value::InlineTable(table)) => {
            table.insert("path", path_value.clone());
            table.remove("workspace");
        }
        Item::Table(table) => {
            insert_or_replace(table, "path", path_value.clone());
            table.remove("workspace");
        }
        Item::Value(existing) => {
            let mut inline = InlineTable::new();
            if let Value::String(s) = existing {
                inline.insert("version", Value::from(s.value().to_string()));
            }
            inline.insert("path", path_value);
            *item = Item::Value(Value::InlineTable(inline));
        }
        _ => {}
    }
}

fn apply_version_replacement(item: &mut Item, version: &str) {
    let version_value = Value::from(version.to_string());
    match item {
        Item::Value(Value::InlineTable(table)) => {
            table.insert("version", version_value.clone());
            table.remove("workspace");
        }
        Item::Table(table) => {
            insert_or_replace(table, "version", version_value.clone());
            table.remove("workspace");
        }
        Item::Value(_) => {
            *item = Item::Value(version_value);
        }
        _ => {}
    }
}

fn insert_or_replace(table: &mut Table, key: &str, val: Value) {
    let item = Item::Value(val);
    if let Some(existing) = table.get_mut(key) {
        *existing = item;
    } else {
        table.insert(key, item);
    }
}

#[allow(clippy::disallowed_types)]
fn load_workspace_dependencies(
    resolver: &PathResolver,
    workspace_root: Option<&Path>,
) -> Result<BTreeMap<String, String>> {
    let Some(root) = workspace_root else {
        return Ok(BTreeMap::new());
    };
    let manifest = resolver
        .read_to_string_unguarded(&UnguardedPath::external(root.join("Cargo.toml")))
        .context("read workspace Cargo.toml")?;
    let doc = manifest
        .parse::<DocumentMut>()
        .context("parse workspace Cargo.toml")?;
    let mut out = BTreeMap::new();
    let Some(deps) = doc
        .get("workspace")
        .and_then(|item| item.as_table())
        .and_then(|table| table.get("dependencies"))
        .and_then(|item| item.as_table())
    else {
        return Ok(out);
    };
    for (name, item) in deps.iter() {
        if let Some(version) = item.as_str() {
            out.insert(name.to_string(), version.to_string());
            continue;
        }
        // Workspace deps appear both as standard tables
        // (`name = { version = ".." }` spans multiple lines) and as inline
        // tables; both must contribute their version.
        if let Some(table) = item.as_table()
            && let Some(version) = table.get("version").and_then(|v| v.as_str())
        {
            out.insert(name.to_string(), version.to_string());
            continue;
        }
        if let Some(inline) = item.as_inline_table()
            && let Some(version) = inline.get("version").and_then(|v| v.as_str())
        {
            out.insert(name.to_string(), version.to_string());
        }
    }
    Ok(out)
}

fn has_workspace_flag(item: &Item) -> bool {
    match item {
        Item::Value(Value::InlineTable(table)) => {
            table.get("workspace").and_then(|v| v.as_bool()) == Some(true)
        }
        Item::Table(table) => table.get("workspace").and_then(|v| v.as_bool()) == Some(true),
        _ => false,
    }
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use std::collections::BTreeMap;
    use toml_edit::DocumentMut;

    #[test]
    fn patch_manifest_applies_replacements_to_all_sections() -> Result<()> {
        let tempdir = GuardedPath::tempdir()?;
        let root = tempdir.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone())?;

        let manifest = indoc! {r#"
            [package]
            name = "fixture"
            version = "0.1.0"

            [dependencies]
            foo = { version = "0.1.0", workspace = true }

            [dev-dependencies]
            bar = "0.2.0"

            [build-dependencies]
            baz = { version = "0.3.0", workspace = true }
        "#};

        let manifest_path = root.join("Cargo.toml")?;
        resolver.write_file(&manifest_path, manifest.as_bytes())?;

        let mut replacements = BTreeMap::new();
        let foo_path = root.join("local_dep")?;
        replacements.insert(
            "foo".to_string(),
            DependencyReplacement::Path(foo_path.as_path().to_path_buf()),
        );
        replacements.insert(
            "bar".to_string(),
            DependencyReplacement::Version("1.2.3".to_string()),
        );
        replacements.insert(
            "baz".to_string(),
            DependencyReplacement::Version("4.5.6".to_string()),
        );

        patch_manifest(&resolver, &root, &replacements, None)?;

        let rewritten = resolver.read_to_string(&manifest_path)?;
        let doc = rewritten.parse::<DocumentMut>()?;

        let foo_path_string = path_string(foo_path.as_path());
        assert_eq!(
            doc["dependencies"]["foo"]["path"].as_str(),
            Some(foo_path_string.as_str())
        );
        assert!(doc["dependencies"]["foo"].get("workspace").is_none());
        assert_eq!(doc["dev-dependencies"]["bar"].as_str(), Some("1.2.3"));
        assert_eq!(
            doc["build-dependencies"]["baz"]["version"].as_str(),
            Some("4.5.6")
        );

        Ok(())
    }

    #[cfg_attr(
        miri,
        ignore = "load_workspace_dependencies performs an unguarded host read"
    )]
    #[test]
    fn workspace_version_autofill_replaces_only_flagged_entries() -> Result<()> {
        let tempdir = GuardedPath::tempdir()?;
        let root = tempdir.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone())?;

        // A stand-in workspace manifest whose dependency versions are canonical.
        let ws_manifest = indoc! {r#"
            [workspace.dependencies]
            anyhow = "1.9.9"
            oxdock-fs = { version = "7.7.7", path = "crates/sys/oxdock-fs" }
            unrelated = "3.3.3"
        "#};
        let ws_dir = root.join("ws")?;
        resolver.create_dir_all(&ws_dir)?;
        resolver.write_file(&ws_dir.join("Cargo.toml")?, ws_manifest.as_bytes())?;

        // Fixture manifest: only `workspace = true` entries may be rewritten.
        let manifest = indoc! {r#"
            [dependencies]
            anyhow = "0.0.1-placeholder"
            oxdock-fs = { workspace = true }
        "#};
        let manifest_path = root.join("Cargo.toml")?;
        resolver.write_file(&manifest_path, manifest.as_bytes())?;

        patch_manifest(&resolver, &root, &BTreeMap::new(), Some(ws_dir.as_path()))?;

        let doc = resolver
            .read_to_string(&manifest_path)?
            .parse::<DocumentMut>()?;
        assert_eq!(
            doc["dependencies"]["oxdock-fs"]["version"].as_str(),
            Some("7.7.7"),
            "workspace-flagged dep must adopt the workspace version"
        );
        assert!(doc["dependencies"]["oxdock-fs"].get("workspace").is_none());
        assert_eq!(
            doc["dependencies"]["anyhow"].as_str(),
            Some("0.0.1-placeholder"),
            "unflagged deps must stay untouched even when listed in the workspace"
        );
        Ok(())
    }

    #[test]
    fn path_replacement_converts_plain_string_dep_to_inline_table() -> Result<()> {
        let tempdir = GuardedPath::tempdir()?;
        let root = tempdir.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone())?;

        let manifest_path = root.join("Cargo.toml")?;
        resolver.write_file(
            &manifest_path,
            br#"
[dependencies]
bar = "0.9.0"
"#,
        )?;

        let mut replacements = BTreeMap::new();
        let dep_path = root.join("vendored/bar")?;
        replacements.insert(
            "bar".to_string(),
            DependencyReplacement::Path(dep_path.as_path().to_path_buf()),
        );
        patch_manifest(&resolver, &root, &replacements, None)?;

        let doc = resolver
            .read_to_string(&manifest_path)?
            .parse::<DocumentMut>()?;
        assert_eq!(
            doc["dependencies"]["bar"]["version"].as_str(),
            Some("0.9.0"),
            "existing version is carried into the inline table"
        );
        assert_eq!(
            doc["dependencies"]["bar"]["path"].as_str(),
            Some(path_string(dep_path.as_path()).as_str())
        );
        Ok(())
    }

    #[test]
    fn path_replacement_applies_to_table_form_dependency() -> Result<()> {
        let tempdir = GuardedPath::tempdir()?;
        let root = tempdir.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone())?;

        let manifest_path = root.join("Cargo.toml")?;
        resolver.write_file(
            &manifest_path,
            br#"
[dependencies]
qux = { version = "1.0.0", features = ["full"] }
"#,
        )?;

        let mut replacements = BTreeMap::new();
        let dep_path = root.join("local/qux")?;
        replacements.insert(
            "qux".to_string(),
            DependencyReplacement::Path(dep_path.as_path().to_path_buf()),
        );
        patch_manifest(&resolver, &root, &replacements, None)?;

        let doc = resolver
            .read_to_string(&manifest_path)?
            .parse::<DocumentMut>()?;
        assert_eq!(
            doc["dependencies"]["qux"]["path"].as_str(),
            Some(path_string(dep_path.as_path()).as_str())
        );
        assert_eq!(
            doc["dependencies"]["qux"]["features"][0].as_str(),
            Some("full"),
            "table-form metadata must survive the patch"
        );
        Ok(())
    }

    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    #[cfg_attr(miri, ignore = "copies through unguarded host directories")]
    #[test]
    fn copy_fixture_template_skips_target_and_copies_nested_files() -> Result<()> {
        let tempdir = GuardedPath::tempdir()?;
        let root = tempdir.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone())?;

        // Build a template tree by hand.
        let template = root.join("tpl")?;
        resolver.create_dir_all(&template)?;
        resolver.write_file(&template.join("Cargo.toml")?, b"[package]\n")?;
        resolver.ensure_parent_dir(&template.join("src/lib.rs")?)?;
        resolver.write_file(&template.join("src/lib.rs")?, b"pub fn f() {}\n")?;
        resolver.ensure_parent_dir(&template.join("assets/data.txt")?)?;
        resolver.write_file(&template.join("assets/data.txt")?, b"data\n")?;

        // `target/` is copied via unguarded fs, so plant it directly.
        let target_dir = template.as_path().join("target");
        std::fs::create_dir_all(&target_dir)?;
        std::fs::write(target_dir.join("junk.bin"), b"junk")?;

        let dst = root.join("instance")?;
        copy_fixture_template(
            &resolver,
            &UnguardedPath::external(template.as_path().to_path_buf()),
            &dst,
        )?;

        assert_eq!(
            resolver.read_file(&dst.join("Cargo.toml")?)?,
            b"[package]\n"
        );
        assert_eq!(
            resolver.read_file(&dst.join("src/lib.rs")?)?,
            b"pub fn f() {}\n"
        );
        assert_eq!(
            resolver.read_file(&dst.join("assets/data.txt")?)?,
            b"data\n"
        );
        let junk = dst.join("target/junk.bin")?;
        assert!(
            !resolver.exists(&junk),
            "build-artifact directory must be skipped during template copy"
        );
        Ok(())
    }

    #[cfg_attr(
        miri,
        ignore = "FixtureBuilder::new probes the host filesystem for existence"
    )]
    #[test]
    fn builder_new_reports_missing_template() {
        let err = match FixtureBuilder::new("/definitely/not/a/real/template-dir") {
            Ok(_) => panic!("missing template must be rejected"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("does not exist"),
            "unexpected error: {err}"
        );
    }

    #[cfg_attr(miri, ignore = "instantiation copies a host template directory")]
    #[test]
    fn instantiated_fixture_scopes_cargo_and_injects_workspace_root() -> Result<()> {
        // Template with a single file so instantiation has something to copy.
        let staging = GuardedPath::tempdir()?;
        let stage_root = staging.as_guarded_path().clone();
        let stage_resolver = PathResolver::new_guarded(stage_root.clone(), stage_root.clone())?;
        let template = stage_root.join("template")?;
        stage_resolver.create_dir_all(&template)?;
        stage_resolver.write_file(&template.join("Cargo.toml")?, b"[package]\n")?;

        let instance = FixtureBuilder::new(template.as_path())?
            .with_workspace_root("/original/repo")
            .instantiate()?;

        let snap = instance.cargo().snapshot();
        assert_eq!(snap.program, std::ffi::OsString::from("cargo"));
        let expected_cwd = command_path(instance.root()).into_owned();
        assert_eq!(snap.cwd, Some(expected_cwd));
        assert!(snap.envs.contains(&(
            std::ffi::OsString::from("OXDOCK_WORKSPACE_ROOT"),
            std::ffi::OsString::from("/original/repo")
        )));

        // Without the override, the env var must not appear.
        let bare_template = stage_root.join("template2")?;
        stage_resolver.create_dir_all(&bare_template)?;
        stage_resolver.write_file(&bare_template.join("Cargo.toml")?, b"[package]\n")?;
        let bare = FixtureBuilder::new(bare_template.as_path())?.instantiate()?;
        let bare_snap = bare.cargo().snapshot();
        assert!(
            !bare_snap
                .envs
                .iter()
                .any(|(k, _)| k == std::ffi::OsStr::new("OXDOCK_WORKSPACE_ROOT"))
        );
        Ok(())
    }
}
