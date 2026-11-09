//! Embedded pipeline distribution: byte-copy, provision, uniformity.
//!
//! The binary carries its pipeline files and provisions them
//! write-if-missing into the repo, so a bare binary runs anywhere
//! without a source checkout and without per-repo vendoring. What
//! runs is what the tree holds, never Rust-assembled text: Rust only
//! byte-copies sources and checks every registered plugin exposes the
//! same lifecycle methods. Sequencing stays in the DSL.

use anyhow::{Context, Result};
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_process::ProcessManager;

use crate::config::{configured_plugins, default_plugins};

/// Pipeline files the binary carries: entry, renderer, stage order,
/// shared types, and the default hook set. Provisioned into the repo
/// (see [`provision_dist`]) so a bare binary runs anywhere without a
/// source checkout and without per-repo vendoring: byte-copy, never
/// Rust-assembled text.
fn embedded_dist() -> Vec<(&'static str, &'static str)> {
    vec![
        ("oxdock-render/src/run.oxfile", include_str!("run.oxfile")),
        (
            "oxdock-render/src/render.oxfile",
            include_str!("render.oxfile"),
        ),
        (
            "oxdock-render/src/stages.oxfile",
            include_str!("stages.oxfile"),
        ),
        (
            "oxdock-render/src/types.oxfile",
            include_str!("types.oxfile"),
        ),
        (
            "oxdock-render/src/lifecycle/scanner.oxfile",
            include_str!("lifecycle/scanner.oxfile"),
        ),
        (
            "oxdock-render/src/lifecycle/generic.oxfile",
            include_str!("lifecycle/generic.oxfile"),
        ),
        (
            "oxdock-render/src/lifecycle/oxdock.oxfile",
            include_str!("lifecycle/oxdock.oxfile"),
        ),
    ]
}

/// Lay the embedded pipeline files into the repo: write-if-missing,
/// no-op when identical, and hands off a customized file untouched
/// (its owner wins; delete it to resync). Keeps binary-only repos
/// working with zero vendored maintenance while preserving every
/// on-disk path the DSL imports resolve.
pub(crate) fn provision_dist(resolver: &PathResolver, root: &GuardedPath) -> Result<()> {
    for (rel, contents) in embedded_dist() {
        let path = root.join(rel)?;
        let current: Option<Vec<u8>> = resolver.read_file(&path).ok();
        match current {
            None => {
                if let Some((parent, _)) = rel.rsplit_once('/') {
                    let dir = root.join(parent)?;
                    resolver
                        .create_dir_all(&dir)
                        .context("provision hooks dir")?;
                }
                resolver
                    .write_file(&path, contents.as_bytes())
                    .with_context(|| format!("provision default '{rel}'"))?;
            }
            Some(bytes) if bytes == contents.as_bytes() => {}
            Some(_) => {
                eprintln!("render: keeping customized '{rel}' (delete it to resync the default)");
            }
        }
    }
    Ok(())
}

/// Every lifecycle plugin must expose the same methods: registration
/// order decides execution order within a stage, so a missing method
/// would silently skew one plugin. Parse each file with the
/// production parser and require identical export sets, failing fast
/// naming the outlier instead of mid-pipeline at first use.
pub(crate) fn check_plugin_uniformity(
    resolver: &PathResolver,
    root: &GuardedPath,
    engine: &oxdock_core::Engine<impl ProcessManager>,
    config_name: &str,
) -> Result<()> {
    let mut files = default_plugins();
    for plugin in configured_plugins(resolver, root, config_name)? {
        if !files.contains(&plugin) {
            files.push(plugin);
        }
    }
    let table = engine.module_table();
    let mut baseline: Option<Vec<String>> = None;
    for rel in &files {
        let path = root.join(rel)?;
        let text = resolver
            .read_to_string(&path)
            .with_context(|| format!("read plugin '{rel}'"))?;
        let parsed = oxdock_core::parse_script_with_modules_and_exports(&text, table.clone())
            .with_context(|| format!("parse plugin '{rel}'"))?;
        let mut names = parsed.func_exports;
        names.sort();
        match &baseline {
            None => baseline = Some(names),
            Some(first) => {
                if first != &names {
                    let missing: Vec<&str> = first
                        .iter()
                        .map(String::as_str)
                        .filter(|name| !names.iter().any(|other| other == name))
                        .collect();
                    let extra: Vec<&str> = names
                        .iter()
                        .map(String::as_str)
                        .filter(|name| !first.iter().any(|other| other == name))
                        .collect();
                    anyhow::bail!(
                        "plugin '{rel}' exposes different lifecycle methods: missing {missing:?}, extra {extra:?}"
                    );
                }
            }
        }
    }
    Ok(())
}

/// Byte-copy one static script out of the repo: what runs is what
/// the provisioned files hold, never Rust-assembled text.
pub(crate) fn read_support(
    resolver: &PathResolver,
    root: &GuardedPath,
    rel: &str,
) -> Result<String> {
    let path = root.join(rel)?;
    resolver
        .read_to_string(&path)
        .with_context(|| format!("read static script '{rel}'"))
}
