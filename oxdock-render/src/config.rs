//! Pipeline config surface: filename default plus plugin registry.
//!
//! Rust owns only the filename edge (flag, env, neutral default) and
//! reading the plugin path list out of JSON. Lifecycle defaults and
//! all sequencing stay in the DSL (`render.oxfile`).

use anyhow::{Context, Result};
use oxdock_fs::{GuardedPath, WorkspaceFs};

/// The engine's default lifecycle providers: the source scanner
/// plus the generic stage owner. Used when the config names no
/// plugins of its own.
pub(crate) const SCANNER_PLUGIN: &str = "oxdock-render/src/lifecycle/scanner.oxfile";
pub(crate) const GENERIC_PLUGIN: &str = "oxdock-render/src/lifecycle/generic.oxfile";

/// Default registry order: discovery plus expansion first,
/// manifest promotion second.
pub(crate) fn default_plugins() -> Vec<String> {
    vec![SCANNER_PLUGIN.to_string(), GENERIC_PLUGIN.to_string()]
}

/// Lifecycle plugin paths from the config, or the generic default
/// when the config names none. Entries are plain paths: every
/// plugin file exposes the same lifecycle methods, so there is
/// nothing else to declare.
pub(crate) fn configured_plugins(
    fs: &dyn WorkspaceFs,
    root: &GuardedPath,
    config_name: &str,
) -> Result<Vec<String>> {
    let path = root.join(config_name)?;
    let raw = fs
        .read_to_string(&path)
        .with_context(|| format!("read {config_name} for plugin list"))?;
    let cfg: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse {config_name} for plugin list"))?;
    let files = match cfg.get("lifecycle").and_then(|lc| lc.get("files")) {
        None => return Ok(default_plugins()),
        Some(files) => files,
    };
    let arr = files
        .as_array()
        .context("lifecycle files must be plugin file paths as strings")?;
    arr.iter()
        .map(|entry| {
            entry
                .as_str()
                .with_context(|| "lifecycle files must be plugin file paths as strings")
        })
        .map(|result| result.map(str::to_string))
        .collect()
}

/// Config filename: explicit `RENDER_CONFIG` wins, else the one
/// neutral name. Anything else fails closed naming what was looked
/// for; the DSL never carries a filename default.
pub(crate) fn resolve_config_name(fs: &dyn WorkspaceFs, root: &GuardedPath) -> Result<String> {
    if let Ok(name) = std::env::var("RENDER_CONFIG") {
        return Ok(name);
    }
    let neutral = root.join("render.json")?;
    if fs.metadata(&neutral).is_ok() {
        return Ok("render.json".to_string());
    }
    anyhow::bail!(
        "no render.json under {}; pass --config or set RENDER_CONFIG",
        root.as_path().to_string_lossy()
    )
}
