//! The `TOOLCHAIN` host module: portable OxDock builder (issue #179).
//!
//! `TOOLCHAIN_ENSURE` provisions the pinned Rust toolchain for a target
//! triple under the dedicated cache group, `TOOLCHAIN_FETCH_SOURCE`
//! snapshots local source into the cache (v1 bundle; pinned remote
//! fetch is follow-up), and `TOOLCHAIN_BUILD` compiles the staged
//! source with the cached toolchain only. Nothing reads the local repo
//! as toolchain source and nothing writes into it.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use oxdock_core::{HostModule, OxDockFn, StepCtx, Value};
use oxdock_func_macro::oxdock_func;
use oxdock_process::ProcessManager;

use crate::build::{build_cached, fetch_source};
use crate::fingerprint::parse_profile;
use crate::provision::provision_toolchain;
use crate::targets::resolve_triple;

/// Read the options MAP for a `TOOLCHAIN_*` function. Must be a MAP.
fn toolchain_options<'a>(options: &'a Value, func: &str) -> Result<&'a BTreeMap<String, Value>> {
    options.as_map().ok_or_else(|| {
        anyhow::anyhow!("{func} options must be a MAP, got {}", options.type_name())
    })
}

/// Ensure the pinned Rust toolchain for `triple` (blank means host) is
/// provisioned under the toolchain cache group. Returns a MAP with
/// `cargo`, `rustc`, `sysroot` (all guarded cache paths), and `triple`.
/// Idempotent: present binaries short-circuit before any network use.
#[oxdock_func(returns = "MAP", summary = "Ensure the cached toolchain for a target triple.")]
fn toolchain_ensure<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    triple: Value,
) -> Result<Value> {
    let raw = match triple.as_str() {
        Some(s) => s.to_string(),
        None => bail!("TOOLCHAIN_ENSURE() argument `$triple` must be a STRING"),
    };
    let resolved = resolve_triple(Some(raw.as_str()))?;
    let anchor = cx.cwd().clone();
    let info = provision_toolchain(&anchor, &resolved)?;
    let mut out = BTreeMap::new();
    out.insert("cargo".to_string(), Value::string(info.cargo));
    out.insert("rustc".to_string(), Value::string(info.rustc));
    out.insert("sysroot".to_string(), Value::string(info.sysroot));
    out.insert("triple".to_string(), Value::string(info.triple));
    Ok(Value::map(out))
}

/// Snapshot local source into the toolchain cache (v1 bundle). `hint`
/// anchors like READ: a leading `/` means the workspace root. Returns a
/// MAP with `manifest_dir` pointing inside the cache, never into the
/// local tree. Content addressed, so repeat calls are free.
#[oxdock_func(returns = "MAP", summary = "Snapshot local source into the toolchain cache.")]
fn toolchain_fetch_source<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    hint: Value,
) -> Result<Value> {
    let raw = match hint.as_str() {
        Some(s) => s.to_string(),
        None => bail!("TOOLCHAIN_FETCH_SOURCE() argument `$hint` must be a STRING"),
    };
    let anchor = cx.cwd().clone();
    let manifest_dir = fetch_source(&anchor, &raw)?;
    let mut out = BTreeMap::new();
    out.insert(
        "manifest_dir".to_string(),
        Value::string(manifest_dir),
    );
    Ok(Value::map(out))
}

/// Build staged source with the cached toolchain only. `triple` blanks
/// to host; `manifest_dir` must live inside the toolchain cache;
/// `features` is a LIST of STRING feature names; `options` is a MAP
/// with optional `profile` (`release` default, explicit `dev` opt-in).
/// Missing toolchain binaries provision automatically. Rebuilds only
/// when the fingerprint (source plus features plus profile plus
/// toolchain versions) is stale. Returns a MAP with `binary`, `profile`,
/// `releasable` (BOOL), and `metadata` (MAP).
#[oxdock_func(returns = "MAP", summary = "Build staged source with the cached toolchain.")]
fn toolchain_build<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    triple: Value,
    manifest_dir: Value,
    features: Value,
    options: Value,
) -> Result<Value> {
    let raw_triple = match triple.as_str() {
        Some(s) => s.to_string(),
        None => bail!("TOOLCHAIN_BUILD() argument `$triple` must be a STRING"),
    };
    let resolved = resolve_triple(Some(raw_triple.as_str()))?;
    let raw_manifest = match manifest_dir.as_str() {
        Some(s) => s.to_string(),
        None => bail!("TOOLCHAIN_BUILD() argument `$manifest_dir` must be a STRING"),
    };
    let list = features.as_list().ok_or_else(|| {
        anyhow::anyhow!(
            "TOOLCHAIN_BUILD() argument `$features` must be a LIST, got {}",
            features.type_name()
        )
    })?;
    let mut feature_names = Vec::with_capacity(list.len());
    for item in list {
        match item.as_str() {
            Some(s) => feature_names.push(s.to_string()),
            None => bail!(
                "TOOLCHAIN_BUILD() `$features` entries must be STRING, got {}",
                item.type_name()
            ),
        }
    }
    let map = toolchain_options(&options, "TOOLCHAIN_BUILD")?;
    for key in map.keys() {
        if key != "profile" {
            bail!("TOOLCHAIN_BUILD() unknown option '{key}' (expected: profile)");
        }
    }
    let profile_raw = match map.get("profile") {
        Some(value) => match value.as_str() {
            Some(s) => Some(s.to_string()),
            None => bail!("TOOLCHAIN_BUILD option 'profile' must be a STRING"),
        },
        None => None,
    };
    let profile = parse_profile(profile_raw.as_deref())?;
    let anchor = cx.cwd().clone();
    let output = build_cached(
        &anchor,
        &resolved,
        &raw_manifest,
        &feature_names,
        profile,
    )?;
    let mut meta = BTreeMap::new();
    for (key, value) in output.metadata {
        meta.insert(key, Value::string(value));
    }
    let mut out = BTreeMap::new();
    out.insert("binary".to_string(), Value::string(output.binary));
    out.insert(
        "profile".to_string(),
        Value::string(match output.profile {
            crate::fingerprint::Profile::Release => "release".to_string(),
            crate::fingerprint::Profile::Dev => "dev".to_string(),
        }),
    );
    out.insert("releasable".to_string(), Value::bool(output.releasable));
    out.insert("metadata".to_string(), Value::map(meta));
    Ok(Value::map(out))
}

/// The `TOOLCHAIN` host module: portable builder funcs. Generic over the
/// process manager like every host module. No shared registry: the
/// toolchain guard derives from the cache identity per call.
pub fn module_with<P: ProcessManager>() -> HostModule<P> {
    HostModule {
        name: "TOOLCHAIN".to_string(),
        funcs: vec![
            ToolchainEnsure::registration(),
            ToolchainFetchSource::registration(),
            ToolchainBuild::registration(),
        ],
        types: vec![],
    }
}
