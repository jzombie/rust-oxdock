//! Output writer: executable-bit fixup after the render.
//!
//! Fresh files arrive `0644` from the renderer, which silently drops
//! the `0755` executable workflows need. Re-applying here keeps
//! regeneration self-healing instead of relying on a committed mode
//! bit surviving. Pure filesystem post-pass: the DSL owns what gets
//! written, Rust owns only the mode bits the DSL has no primitive for.

use anyhow::{Context, Result};
use oxdock_fs::{EntryKind, GuardedPath, WorkspaceFs};

/// Mode re-applied to rendered outputs whose target declares
/// `"executable": true`.
const EXECUTABLE_MODE: u32 = 0o755;

/// Output paths in one target.json manifest requesting the executable
/// bit: every target entry with `"executable": true` contributes its
/// `out`. Strict so a typo degrades to an error, never to a silently
/// non-executable script: a present-but-not-boolean flag, or `true`
/// without a string `out`, fails naming the target.
fn executable_outs(manifest: &serde_json::Value) -> Result<Vec<String>> {
    let mut outs = Vec::new();
    let targets = manifest
        .get("targets")
        .and_then(|targets| targets.as_array())
        .context("target manifest needs a 'targets' array")?;
    for target in targets {
        let name = target
            .get("name")
            .and_then(|name| name.as_str())
            .unwrap_or("<unnamed>");
        match target.get("executable") {
            None | Some(serde_json::Value::Bool(false)) => {}
            Some(serde_json::Value::Bool(true)) => {
                let out = target
                    .get("out")
                    .and_then(|out| out.as_str())
                    .with_context(|| {
                        format!("target '{name}' sets 'executable' without a string 'out'")
                    })?;
                outs.push(out.to_string());
            }
            Some(_) => {
                anyhow::bail!(
                    "target '{name}' sets 'executable' to a non-boolean; use true or omit it"
                )
            }
        }
    }
    Ok(outs)
}

/// Collect every target.json path under one compose scope, relative
/// to the repo root. Symlinked directories are never descended, so a
/// link cycle cannot loop the walk; a symlinked target.json file
/// itself still resolves through the normal read.
fn collect_target_manifests(
    fs: &dyn WorkspaceFs,
    root: &GuardedPath,
    rel: &str,
    out: &mut Vec<String>,
) -> Result<()> {
    let dir = if rel == "." {
        root.clone()
    } else {
        root.join(rel)?
    };
    let entries = fs
        .read_dir_entries(&dir)
        .with_context(|| format!("list '{rel}' for executable-bit pass"))?;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = if rel == "." {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if name == "target.json" {
            out.push(child);
            continue;
        }
        let guarded = root.join(&child)?;
        if matches!(fs.entry_kind_no_follow(&guarded)?, EntryKind::Dir) {
            collect_target_manifests(fs, root, &child, out)?;
        }
    }
    Ok(())
}

/// Re-apply the executable bit to every rendered output whose target
/// declares `"executable": true`. The bit itself lands through
/// `set_permissions_mode_unix`, so Windows and Miri runs succeed as
/// no-ops and only POSIX trees change.
pub(crate) fn apply_executable_bits(
    fs: &dyn WorkspaceFs,
    root: &GuardedPath,
    config_name: &str,
) -> Result<()> {
    let cfg_raw = fs
        .read_to_string(&root.join(config_name)?)
        .with_context(|| format!("read {config_name} for executable-bit pass"))?;
    let cfg: serde_json::Value = serde_json::from_str(&cfg_raw)
        .with_context(|| format!("parse {config_name} for executable-bit pass"))?;
    let scopes = cfg
        .get("scopes")
        .and_then(|scopes| scopes.as_array())
        .with_context(|| format!("{config_name} needs a 'scopes' array"))?;
    let mut manifests = Vec::new();
    for scope in scopes {
        let scope = scope
            .as_str()
            .with_context(|| format!("{config_name} 'scopes' entries must be strings"))?;
        collect_target_manifests(fs, root, scope, &mut manifests)?;
    }
    for manifest_rel in &manifests {
        let raw = fs
            .read_to_string(&root.join(manifest_rel)?)
            .with_context(|| format!("read '{manifest_rel}' for executable-bit pass"))?;
        let manifest: serde_json::Value = serde_json::from_str(&raw)
            .with_context(|| format!("parse '{manifest_rel}' for executable-bit pass"))?;
        for out in executable_outs(&manifest)? {
            let path = root.join(&out)?;
            fs.set_permissions_mode_unix(&path, EXECUTABLE_MODE)
                .with_context(|| format!("set executable bit on '{out}'"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_outs_selects_flagged_targets() {
        let manifest = serde_json::json!({"targets": [
            {"name": "a", "out": "a.sh", "executable": true},
            {"name": "b", "out": "b.md"},
            {"name": "c", "out": "c.ps1", "executable": false},
        ]});
        assert_eq!(
            executable_outs(&manifest).expect("select"),
            vec!["a.sh".to_string()]
        );
    }

    #[test]
    fn executable_outs_rejects_non_boolean_flag() {
        let manifest = serde_json::json!({"targets": [
            {"name": "a", "out": "a.sh", "executable": "yes"},
        ]});
        let err = format!(
            "{:#}",
            executable_outs(&manifest).expect_err("string flag must fail")
        );
        assert!(err.contains("'a'"), "names the target: {err}");
    }

    #[test]
    fn executable_outs_rejects_flag_without_out() {
        let manifest = serde_json::json!({"targets": [{"name": "a", "executable": true}]});
        let err = format!(
            "{:#}",
            executable_outs(&manifest).expect_err("missing out must fail")
        );
        assert!(err.contains("'a'"), "names the target: {err}");
    }
}
