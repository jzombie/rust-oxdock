//! Internal `RUST` plugin: Cargo workspace content for pipelines.
//!
//! Workspace member lists, manifest package metadata, render versions,
//! and per-member values rendering with the committed `name` override
//! winning. Reading lives in `cargo.rs` and `version.rs`; the host
//! wrappers below stay thin. Registered only by `crate::run`;
//! reusable by any Rust project feeding its manifests into a pipeline,
//! registered by no non-Cargo project.

mod cargo;
mod io;
mod version;

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use oxdock_core::{Field, HostModule, OxDockFn, StepCtx, TypeTag, Value};
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_func_macro::oxdock_func;
use oxdock_process::ProcessManager;

fn docs_resolver(cx: &StepCtx<impl ProcessManager>) -> Result<(GuardedPath, PathResolver)> {
    // Pipelines run the script with cwd at the repo root, so stage
    // helpers resolve against it.
    let root = cx.cwd().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path())?;
    Ok((root, resolver))
}

/// List workspace member paths from the root Cargo.toml.
#[oxdock_func(returns = TypeTag::List)]
fn workspace_members<P: ProcessManager>(cx: &mut StepCtx<P>) -> Result<Value> {
    let (root, resolver) = docs_resolver(cx)?;
    let members = cargo::workspace_members(&root, &resolver)?;
    Ok(Value::list(
        members.into_iter().map(Value::string).collect(),
    ))
}

/// Resolve the render version: script-visible `CRATE_VERSION` wins,
/// otherwise fall back to the workspace manifest.
#[oxdock_func(returns = TypeTag::String)]
fn workspace_version<P: ProcessManager>(cx: &mut StepCtx<P>) -> Result<Value> {
    let (root, resolver) = docs_resolver(cx)?;
    let version = version::crate_version_with_env(&root, &resolver, cx.get_env("CRATE_VERSION"))?;
    Ok(Value::string(version))
}

/// Workspace citation metadata from the root manifest: first author
/// display name plus its structured given, family, and email parts,
/// license, and repository. Citation rendering must use this map instead
/// of copying manifest values into templates or checked-in values files.
#[oxdock_func(returns = TypeTag::Record(std::sync::Arc::from([
    Field {
        name: "author".to_string(),
        ty: TypeTag::String,
        docs: "First author display name.",
        optional: false,
    },
    Field {
        name: "author_given".to_string(),
        ty: TypeTag::String,
        docs: "First author given name.",
        optional: false,
    },
    Field {
        name: "author_family".to_string(),
        ty: TypeTag::String,
        docs: "First author family name.",
        optional: false,
    },
    Field {
        name: "author_email".to_string(),
        ty: TypeTag::String,
        docs: "First author email.",
        optional: false,
    },
    Field {
        name: "license".to_string(),
        ty: TypeTag::String,
        docs: "SPDX license expression.",
        optional: false,
    },
    Field {
        name: "repository".to_string(),
        ty: TypeTag::String,
        docs: "Repository URL.",
        optional: false,
    },
])))]
fn workspace_package<P: ProcessManager>(cx: &mut StepCtx<P>) -> Result<Value> {
    let (root, resolver) = docs_resolver(cx)?;
    let package = cargo::workspace_package(&root, &resolver)?;
    let mut entries = BTreeMap::new();
    entries.insert("author".to_string(), Value::string(package.author));
    entries.insert(
        "author_given".to_string(),
        Value::string(package.author_given),
    );
    entries.insert(
        "author_family".to_string(),
        Value::string(package.author_family),
    );
    entries.insert(
        "author_email".to_string(),
        Value::string(package.author_email),
    );
    entries.insert("license".to_string(), Value::string(package.license));
    entries.insert("repository".to_string(), Value::string(package.repository));
    Ok(Value::map(entries))
}

/// Package name and description from a member manifest. Returns empty
/// strings when there is no manifest or no package name, so the script
/// can skip with `IF $pkg.name != ""` instead of catching an error.
#[oxdock_func(returns = TypeTag::Record(std::sync::Arc::from([
    Field {
        name: "name".to_string(),
        ty: TypeTag::String,
        docs: "Package name, empty when no manifest names it.",
        optional: false,
    },
    Field {
        name: "description".to_string(),
        ty: TypeTag::String,
        docs: "Package description, empty when absent.",
        optional: false,
    },
])))]
fn cargo_package<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    #[union(String, Path)] member: Value,
) -> Result<Value> {
    let text = match member.as_str() {
        Some(s) => s.to_string(),
        None => member
            .as_path()
            .expect("union extractor admits STRING|PATH")
            .to_string_lossy()
            .to_string(),
    };
    let (root, resolver) = docs_resolver(cx)?;
    let (name, description) = cargo::cargo_package(&root, &resolver, &text)?.unwrap_or_default();
    let mut entries = BTreeMap::new();
    entries.insert("name".to_string(), Value::string(name));
    entries.insert("description".to_string(), Value::string(description));
    Ok(Value::map(entries))
}

/// Render one member values file: manifest name and description with the
/// committed `name` override winning, stable key order, serde escaping,
/// and one trailing newline. `existing` may be an empty map when no
/// values file exists yet; a missing `name` there keeps the manifest
/// name instead of failing.
#[oxdock_func(pure, returns = TypeTag::String)]
fn package_values_json(
    /// Manifest name and description; closed: extras fail at the boundary.
    #[record("name: String", "description: String")]
    pkg: BTreeMap<String, Value>,
    /// Committed values file; open: extra keys pass through untouched.
    existing: BTreeMap<String, Value>,
) -> Result<Value> {
    // The `#[record]` extractor verified the closed shape above, so
    // the reads below narrow infallibly after.
    let name = pkg
        .get("name")
        .expect("record extractor verified name")
        .as_str()
        .expect("record extractor verified STRING")
        .to_string();
    let description = pkg
        .get("description")
        .expect("record extractor verified description")
        .as_str()
        .expect("record extractor verified STRING")
        .to_string();
    let mut final_name = name;
    if let Some(override_name) = existing.get("name").and_then(|v| v.as_str()) {
        final_name = override_name.to_string();
    }
    let merged = [("name", final_name), ("description", description)];
    let mut out = String::from("{");
    for (idx, (key, value)) in merged.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        out.push_str(&serde_json::to_string(key).context("encode values")?);
        out.push_str(": ");
        out.push_str(&serde_json::to_string(value).context("encode values")?);
    }
    out.push_str("}\n");
    Ok(Value::string(out))
}

/// The internal `RUST` plugin for the render engine. Registered only by
/// `crate::run`; the core language never sees it.
pub fn module<P: ProcessManager>() -> HostModule<P> {
    HostModule {
        name: "RUST".to_string(),
        funcs: vec![
            WorkspaceMembers::registration(),
            WorkspaceVersion::registration(),
            WorkspacePackage::registration(),
            CargoPackage::registration(),
            PackageValuesJson::registration(),
        ],
        types: vec![],
        record_schemas: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    // The bodies take narrowed maps (the `#[record]` extractor owns
    // the boundary): unwrap the test maps the way the extractor does.
    fn record_map(value: Value) -> BTreeMap<String, Value> {
        value.as_map().expect("test map").clone()
    }

    #[test]
    fn package_values_merge_keeps_committed_name() {
        let pkg = map(&[
            ("name", Value::string("demo-pkg".to_string())),
            (
                "description",
                Value::string("second description".to_string()),
            ),
        ]);
        let existing = map(&[
            ("name", Value::string("Display".to_string())),
            ("description", Value::string("stale copy".to_string())),
        ]);
        let out = package_values_json(record_map(pkg), record_map(existing))
            .expect("encode")
            .as_str()
            .expect("string")
            .to_string();
        assert!(
            out.contains("\"description\": \"second description\""),
            "manifest description must flow through, got: {out}"
        );
        assert!(
            out.contains("\"name\": \"Display\""),
            "committed name override must win, got: {out}"
        );
    }

    #[test]
    fn package_values_without_existing_uses_manifest() {
        let pkg = map(&[
            ("name", Value::string("demo-pkg".to_string())),
            ("description", Value::string("fresh".to_string())),
        ]);
        let out = package_values_json(record_map(pkg), BTreeMap::new())
            .expect("encode")
            .as_str()
            .expect("string")
            .to_string();
        assert_eq!(
            out,
            "{\"name\": \"demo-pkg\", \"description\": \"fresh\"}\n"
        );
    }

    #[test]
    fn package_values_escapes_quotes() {
        let pkg = map(&[
            ("name", Value::string("demo".to_string())),
            ("description", Value::string("says \"hi\"".to_string())),
        ]);
        let out = package_values_json(record_map(pkg), BTreeMap::new())
            .expect("encode")
            .as_str()
            .expect("string")
            .to_string();
        assert_eq!(
            out,
            "{\"name\": \"demo\", \"description\": \"says \\\"hi\\\"\"}\n"
        );
    }

    #[test]
    fn package_signatures_name_closed_shapes() {
        // Metadata honesty: the record/union spellings the boundary
        // enforces render through the single structural renderer.
        let metas = module::<oxdock_process::DefaultProcessManager>()
            .funcs
            .into_iter()
            .map(|registration| match registration {
                oxdock_core::HostRegistration::Pure { meta, .. } => meta,
                oxdock_core::HostRegistration::Stateful { meta, .. } => meta,
            })
            .collect::<Vec<_>>();
        fn base(name: &str) -> &str {
            name.rsplit("::").next().unwrap_or(name)
        }
        let signature = |name: &str| {
            metas
                .iter()
                .find(|meta| base(&meta.name) == base(name))
                .unwrap_or_else(|| panic!("fn {name} exists"))
                .params
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|param| {
                    param
                        .param_type
                        .as_ref()
                        .map(oxdock_parser::render_structural)
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        assert_eq!(
            signature("RUST::PACKAGE_VALUES_JSON"),
            "MAP<name: STRING, description: STRING>, MAP<ANY>"
        );
        let returns = |name: &str| {
            metas
                .iter()
                .find(|meta| base(&meta.name) == base(name))
                .unwrap_or_else(|| panic!("fn {name} exists"))
                .returns
                .as_ref()
                .map(oxdock_parser::render_structural)
                .unwrap_or_default()
        };
        assert_eq!(
            returns("RUST::CARGO_PACKAGE"),
            "MAP<name: STRING, description: STRING>"
        );
        assert_eq!(
            returns("RUST::WORKSPACE_PACKAGE"),
            "MAP<author: STRING, author_given: STRING, author_family: STRING, author_email: STRING, license: STRING, repository: STRING>"
        );
    }

    #[test]
    fn package_values_rejects_open_pkg_end_to_end() {
        // The closed `pkg` record fails missing and extra fields at
        // the call boundary; extras-bearing `existing` still passes.
        use indoc::indoc;
        let temp = oxdock_fs::GuardedPath::tempdir().unwrap();
        let root = temp.as_guarded_path().clone();
        let mut engine = oxdock_core::Engine::new();
        engine.register_module(module());
        let err = engine
            .run_script(
                &root,
                indoc! {r#"
                    IMPORT [STD, RUST]
                    LET $pkg: MAP<name: STRING> = {name: "demo"}
                    LET $json: STRING = RUST::PACKAGE_VALUES_JSON($pkg, {})
                "#},
            )
            .expect_err("missing description must fail");
        assert!(
            format!("{err:#}").contains("missing field 'description'"),
            "names the field: {err:#}"
        );
        engine
            .run_script(
                &root,
                indoc! {r#"
                    IMPORT [STD, RUST]
                    LET $pkg: MAP<name: STRING, description: STRING> = RUST::CARGO_PACKAGE(".")
                    LET $existing: MAP<ANY> = {name: "Display", extra: 1}
                    LET $json: STRING = RUST::PACKAGE_VALUES_JSON($pkg, $existing)
                    ASSERT_CONTAINS $json "Display"
                "#},
            )
            .expect("extras-bearing existing passes");
    }
}
