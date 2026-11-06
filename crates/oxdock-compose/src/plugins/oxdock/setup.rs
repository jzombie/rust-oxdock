//! Rust-docs run-once intake as an `OXDOCK::SETUP` host call.
//!
//! Refreshes generated inputs, snapshots workspace version plus
//! package into staging for downstream stages to read as plain JSON,
//! and syncs per-member values files. Idempotent: reruns rewrite
//! identical bytes. Called as the SETUP stage by docs pipelines;
//! generic pipelines register no SETUP provider and skip this work.

use anyhow::{Context, Result};
use oxdock_core::{StepCtx, TypeTag, Value, json_to_value};
use oxdock_fs::PathResolver;
use oxdock_func_macro::oxdock_func;
use oxdock_process::ProcessManager;
use std::collections::BTreeMap;

use super::generated_text;
use crate::plugins::rust::{cargo, package_values_json, version};

/// Run-once intake over the pipeline CTX map, returned unchanged.
/// File layout mirrors the old hook byte for byte: generated outputs
/// land under the workspace root, the snapshot under staging, values
/// files under each member's template dir.
#[oxdock_func(returns = TypeTag::MapOf(std::sync::Arc::new(TypeTag::Any)))]
fn setup<P: ProcessManager>(cx: &mut StepCtx<P>, ctx: Value) -> Result<Value> {
    let root = cx.workspace_root().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path())?;
    let map = ctx.as_map().context("OXDOCK::SETUP needs its CTX map")?;
    let field = |key: &str| -> Result<String> {
        map.get(key)
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .with_context(|| format!("OXDOCK::SETUP needs CTX.{key}"))
    };
    let config = field("config")?;
    let staging = field("staging")?;
    let cfg_path = root.join(&config)?;
    let cfg_raw = resolver
        .read_to_string(&cfg_path)
        .with_context(|| format!("OXDOCK::SETUP reads config '{config}'"))?;
    let cfg: serde_json::Value =
        serde_json::from_str(&cfg_raw).context("OXDOCK::SETUP parses config")?;

    // Generated inputs: config keys dispatch through the same
    // key-to-call table templates resolve.
    let generated = cfg
        .get("generated")
        .and_then(|entries| entries.as_array())
        .context("OXDOCK::SETUP needs a 'generated' array")?;
    for entry in generated {
        let key = entry
            .get("key")
            .and_then(|key| key.as_str())
            .context("OXDOCK::SETUP generated entry needs a 'key'")?;
        let out = entry
            .get("out")
            .and_then(|out| out.as_str())
            .context("OXDOCK::SETUP generated entry needs an 'out'")?;
        let text = generated_text(key)?;
        let text = text.as_str().context("generated text")?.to_string();
        let dest = root.join(out)?;
        resolver
            .write_file(&dest, text.as_bytes())
            .with_context(|| format!("OXDOCK::SETUP writes '{out}'"))?;
    }

    // Workspace snapshot for downstream stages (plain JSON, no host
    // calls past this point).
    let crate_version =
        version::crate_version_with_env(&root, &resolver, cx.get_env("CRATE_VERSION"))?;
    let package = cargo::workspace_package(&root, &resolver)?;
    let snapshot = serde_json::json!({
        "version": crate_version,
        "package": {
            "author": package.author,
            "author_given": package.author_given,
            "author_family": package.author_family,
            "author_email": package.author_email,
            "license": package.license,
            "repository": package.repository,
        },
    });
    let staging_path = root.join(&staging)?;
    resolver
        .create_dir_all(&staging_path)
        .context("OXDOCK::SETUP stages")?;
    let snapshot_path = staging_path.join("_workspace.json")?;
    resolver
        .write_file(
            &snapshot_path,
            serde_json::to_string(&snapshot)
                .context("OXDOCK::SETUP encodes snapshot")?
                .as_bytes(),
        )
        .context("OXDOCK::SETUP writes snapshot")?;

    // Per-member values sync: manifest name and description with the
    // committed `name` override winning.
    let members = cargo::workspace_members(&root, &resolver)?;
    for member in members {
        let (name, description) =
            cargo::cargo_package(&root, &resolver, &member)?.unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let rel = format!("{member}/.oxdock/template/values.json");
        let dest = root.join(&rel)?;
        let existing = match resolver.read_to_string(&dest) {
            Ok(raw) => {
                let json: serde_json::Value = serde_json::from_str(&raw)
                    .with_context(|| format!("OXDOCK::SETUP parses '{rel}'"))?;
                json_to_value(json)
                    .as_map()
                    .context("OXDOCK::SETUP values stay a map")?
                    .clone()
            }
            Err(_) => BTreeMap::new(),
        };
        let mut pkg = BTreeMap::new();
        pkg.insert("name".to_string(), Value::string(name));
        pkg.insert("description".to_string(), Value::string(description));
        let rendered = package_values_json(pkg, existing)?;
        let rendered = rendered
            .as_str()
            .context("OXDOCK::SETUP renders values")?
            .to_string();
        resolver
            .write_file(&dest, rendered.as_bytes())
            .with_context(|| format!("OXDOCK::SETUP writes '{rel}'"))?;
    }
    Ok(ctx)
}
