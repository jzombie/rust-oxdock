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
use oxdock_core::{HostModule, OxDockFn, StepCtx, TypeTag, Value};
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
/// display name, license, and repository. Citation rendering must use
/// this map instead of copying manifest values into templates or
/// checked-in values files.
#[oxdock_func(returns = TypeTag::Map)]
fn workspace_package<P: ProcessManager>(cx: &mut StepCtx<P>) -> Result<Value> {
    let (root, resolver) = docs_resolver(cx)?;
    let package = cargo::workspace_package(&root, &resolver)?;
    let mut entries = BTreeMap::new();
    entries.insert("author".to_string(), Value::string(package.author));
    entries.insert("license".to_string(), Value::string(package.license));
    entries.insert("repository".to_string(), Value::string(package.repository));
    Ok(Value::map(entries))
}

/// Package name and description from a member manifest. Returns empty
/// strings when there is no manifest or no package name, so the script
/// can skip with `IF $pkg.name != ""` instead of catching an error.
#[oxdock_func(returns = TypeTag::Map)]
fn cargo_package<P: ProcessManager>(cx: &mut StepCtx<P>, member: String) -> Result<Value> {
    let (root, resolver) = docs_resolver(cx)?;
    let (name, description) = cargo::cargo_package(&root, &resolver, &member)?.unwrap_or_default();
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
fn package_values_json(pkg: Value, existing: Value) -> Result<Value> {
    let pkg_map = pkg.as_map().ok_or_else(|| {
        anyhow::anyhow!(
            "PACKAGE_VALUES_JSON expects a MAP as its pkg argument, got {}",
            pkg.type_name()
        )
    })?;
    let name = pkg_map
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("PACKAGE_VALUES_JSON pkg misses name"))?
        .to_string();
    let description = pkg_map
        .get("description")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("PACKAGE_VALUES_JSON pkg misses description"))?
        .to_string();
    let existing_map = existing.as_map().ok_or_else(|| {
        anyhow::anyhow!(
            "PACKAGE_VALUES_JSON expects a MAP as its existing argument, got {}",
            existing.type_name()
        )
    })?;
    let mut final_name = name;
    if let Some(override_name) = existing_map.get("name").and_then(|v| v.as_str()) {
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
        let out = package_values_json(pkg, existing)
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
        let out = package_values_json(pkg, Value::map(BTreeMap::new()))
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
        let out = package_values_json(pkg, Value::map(BTreeMap::new()))
            .expect("encode")
            .as_str()
            .expect("string")
            .to_string();
        assert_eq!(
            out,
            "{\"name\": \"demo\", \"description\": \"says \\\"hi\\\"\"}\n"
        );
    }
}
