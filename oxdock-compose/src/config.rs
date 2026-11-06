//! Pipeline config surface: filename default plus plugin registry.
//!
//! Rust owns only the filename edge (flag, env, neutral default) and
//! reading the plugin path list out of JSON. Lifecycle defaults and
//! all sequencing stay in the DSL (`compose.oxfile`).

use anyhow::{Context, Result};
use oxdock_fs::{GuardedPath, PathResolver};

/// The engine's default lifecycle provider: the generic plugin
/// file. Used when the config names no plugins of its own.
pub(crate) const GENERIC_PLUGIN: &str = "oxdock-compose/src/lifecycle/generic.oxfile";

/// Lifecycle plugin paths from the config, or the generic default
/// when the config names none. Entries are plain paths: every
/// plugin file exposes the same lifecycle methods, so there is
/// nothing else to declare.
pub(crate) fn configured_plugins(
    resolver: &PathResolver,
    root: &GuardedPath,
    config_name: &str,
) -> Result<Vec<String>> {
    let path = root.join(config_name)?;
    let raw = resolver
        .read_to_string(&path)
        .with_context(|| format!("read {config_name} for plugin list"))?;
    let cfg: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse {config_name} for plugin list"))?;
    let files = match cfg.get("lifecycle").and_then(|lc| lc.get("files")) {
        None => return Ok(vec![GENERIC_PLUGIN.to_string()]),
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

/// Config filename: explicit `COMPOSE_CONFIG` wins, else the one
/// neutral name. Anything else fails closed naming what was looked
/// for; the DSL never carries a filename default.
pub(crate) fn resolve_config_name(resolver: &PathResolver, root: &GuardedPath) -> Result<String> {
    if let Ok(name) = std::env::var("COMPOSE_CONFIG") {
        return Ok(name);
    }
    let neutral = root.join("compose.json")?;
    if resolver.metadata(&neutral).is_ok() {
        return Ok("compose.json".to_string());
    }
    anyhow::bail!(
        "no compose.json under {}; pass --config or set COMPOSE_CONFIG",
        root.as_path().to_string_lossy()
    )
}
