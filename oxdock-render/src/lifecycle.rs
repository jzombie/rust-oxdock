//! Embedded pipeline integrity: uniformity check over served files.
//!
//! The binary carries its pipeline files (see [`crate::embedded`]) and
//! serves them from memory, so a bare binary runs anywhere without a
//! source checkout and without writing pipeline files into consumer
//! repos. What runs is the binary's version, never Rust-assembled
//! text and never a stale on-disk copy: Rust only serves embedded
//! bytes and checks every registered plugin exposes the same
//! lifecycle methods. Sequencing stays in the DSL.

use anyhow::{Context, Result};
use oxdock_fs::{GuardedPath, WorkspaceFs};
use oxdock_process::ProcessManager;

use crate::config::{configured_plugins, default_plugins};

/// Every lifecycle plugin must expose the same methods: registration
/// order decides execution order within a stage, so a missing method
/// would silently skew one plugin. Parse each file with the
/// production parser and require identical export sets, failing fast
/// naming the outlier instead of mid-pipeline at first use.
pub(crate) fn check_plugin_uniformity(
    fs: &dyn WorkspaceFs,
    root: &GuardedPath,
    engine: &oxdock_core::Engine<impl ProcessManager>,
    config_name: &str,
) -> Result<()> {
    let mut files = default_plugins();
    for plugin in configured_plugins(fs, root, config_name)? {
        if !files.contains(&plugin) {
            files.push(plugin);
        }
    }
    let table = engine.module_table();
    let mut baseline: Option<Vec<String>> = None;
    for rel in &files {
        let path = root.join(rel)?;
        let text = fs
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

/// Byte-copy one static script out of the served tree: what runs is
/// what the embedded layer serves, never Rust-assembled text.
pub(crate) fn read_support(fs: &dyn WorkspaceFs, root: &GuardedPath, rel: &str) -> Result<String> {
    let path = root.join(rel)?;
    fs.read_to_string(&path)
        .with_context(|| format!("read static script '{rel}'"))
}
