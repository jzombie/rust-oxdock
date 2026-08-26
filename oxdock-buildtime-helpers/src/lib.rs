pub mod assets;
mod manifest_paths;
mod track;

pub use assets::{
    DslSource, EmbedSpec, FINGERPRINT_SALT_ENV, FORCE_REBUILD_ENV, PrepareSpec,
    asset_input_fingerprint, embed_assets, embed_debug_enabled, embed_force_rebuild,
    embed_force_rebuild_from, execution_is_skipped, execution_is_skipped_with, prepare_assets,
    stage_materialize, sync_tree, try_embed_assets, try_prepare_assets,
};
pub use track::{collect_env_references, plan_input_directives};

use anyhow::Result;
use oxdock_process::CommandBuilder;

/// Emit `cargo:rustc-env=...` directives for enabled Cargo features.
pub fn emit_feature_envs() -> Result<()> {
    let lines = feature_env_lines();
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

fn feature_env_lines() -> Vec<String> {
    let mut features = Vec::new();
    let mut lines = Vec::new();
    for (key, value) in std::env::vars() {
        if key.starts_with("CARGO_FEATURE_") && value == "1" {
            lines.push(format!("cargo:rustc-env={key}={value}"));
            if let Some(name) = key.strip_prefix("CARGO_FEATURE_") {
                features.push(name.to_ascii_lowercase());
            }
        }
    }
    if !features.is_empty() {
        lines.push(format!(
            "cargo:rustc-env=CARGO_CFG_FEATURE={}",
            features.join(",")
        ));
    }
    lines
}

/// Emit `cargo:rustc-env=...` directives for cfg keys from `rustc --print cfg`.
pub fn emit_cfg_envs() -> Result<()> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    for line in collect_cfg_lines(&rustc)? {
        println!("{line}");
    }
    Ok(())
}

/// Arguments handed to the rustc invocation that produces the cfg listing.
/// Extracted for tests so TARGET forwarding is pinned without spawning.
fn cfg_command_args(target: Option<&str>) -> Vec<String> {
    let mut args = vec!["--print".to_string(), "cfg".to_string()];
    if let Some(target) = target {
        args.push("--target".to_string());
        args.push(target.to_string());
    }
    args
}

/// Invoke rustc and translate its `--print cfg` output into directives.
///
/// A rustc that runs but exits nonzero is swallowed into an empty list so
/// build scripts survive exotic toolchains; a rustc that cannot spawn at all
/// still surfaces as an error.
fn collect_cfg_lines(rustc: &str) -> Result<Vec<String>> {
    let mut cmd = CommandBuilder::new(rustc);
    let target = std::env::var("TARGET").ok();
    for arg in cfg_command_args(target.as_deref()) {
        cmd.arg(arg);
    }
    let output = cmd.output()?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(cfg_env_lines(&stdout))
}

/// Emit both feature and cfg env directives for downstream proc-macros.
pub fn emit_feature_and_cfg_envs() -> Result<()> {
    emit_feature_envs()?;
    emit_cfg_envs()?;
    Ok(())
}

fn trim_cfg_quotes(value: &str) -> &str {
    let value = value.trim();
    let value = value
        .strip_prefix("\\\"")
        .and_then(|s| s.strip_suffix("\\\""))
        .unwrap_or(value);
    let value = value
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(value);
    value
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(value)
}

fn cfg_env_lines(output: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((key, rest)) = line.split_once('=') {
            let key = key.trim();
            let value = trim_cfg_quotes(rest.trim());
            lines.push(format!(
                "cargo:rustc-env=CARGO_CFG_{}={}",
                key.to_ascii_uppercase(),
                value
            ));
        } else {
            lines.push(format!(
                "cargo:rustc-env=CARGO_CFG_{}=1",
                line.to_ascii_uppercase()
            ));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{cfg_env_lines, emit_feature_envs, feature_env_lines, trim_cfg_quotes};
    use oxdock_sys_test_utils::TestEnvGuard;

    #[test]
    fn trims_cfg_quotes() {
        assert_eq!(trim_cfg_quotes("\"value\""), "value");
        assert_eq!(trim_cfg_quotes("\\\"value\\\""), "value");
        assert_eq!(trim_cfg_quotes("'value'"), "value");
        assert_eq!(trim_cfg_quotes("value"), "value");
    }

    #[test]
    fn feature_env_lines_collects_features() {
        let _a = TestEnvGuard::set("CARGO_FEATURE_ALPHA", "1");
        let _b = TestEnvGuard::set("CARGO_FEATURE_BETA", "1");
        let _c = TestEnvGuard::set("CARGO_FEATURE_IGNORE", "0");
        let lines = feature_env_lines();
        assert!(
            lines
                .iter()
                .any(|line| line == "cargo:rustc-env=CARGO_FEATURE_ALPHA=1")
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "cargo:rustc-env=CARGO_FEATURE_BETA=1")
        );
        let features_line = lines
            .iter()
            .find(|line| line.starts_with("cargo:rustc-env=CARGO_CFG_FEATURE="))
            .expect("features line");
        let features = features_line
            .trim_start_matches("cargo:rustc-env=CARGO_CFG_FEATURE=")
            .split(',')
            .collect::<Vec<_>>();
        assert!(features.contains(&"alpha"));
        assert!(features.contains(&"beta"));
        let _ = emit_feature_envs();
    }

    #[test]
    fn cfg_env_lines_parses_output() {
        let output = r#"
            unix
            target_arch="x86_64"
            target_os='linux'
        "#;
        let lines = cfg_env_lines(output);
        assert!(lines.contains(&"cargo:rustc-env=CARGO_CFG_UNIX=1".to_string()));
        assert!(lines.contains(&"cargo:rustc-env=CARGO_CFG_TARGET_ARCH=x86_64".to_string()));
        assert!(lines.contains(&"cargo:rustc-env=CARGO_CFG_TARGET_OS=linux".to_string()));
    }

    #[test]
    fn feature_env_lines_exclude_disabled_and_unrelated_vars() {
        let _a = TestEnvGuard::set("CARGO_FEATURE_ALPHA", "1");
        let _off = TestEnvGuard::set("CARGO_FEATURE_DISABLED", "0");
        let _unrelated = TestEnvGuard::set("SOME_OTHER_VAR", "1");

        let lines = feature_env_lines();
        assert!(
            lines
                .iter()
                .any(|line| line == "cargo:rustc-env=CARGO_FEATURE_ALPHA=1")
        );
        for line in &lines {
            assert!(!line.contains("CARGO_FEATURE_DISABLED"), "leaked: {line}");
            assert!(!line.contains("SOME_OTHER_VAR"), "leaked: {line}");
        }
        let features = lines
            .iter()
            .find(|line| line.starts_with("cargo:rustc-env=CARGO_CFG_FEATURE="))
            .expect("alpha must produce a CARGO_CFG_FEATURE line");
        assert!(
            features.ends_with("=alpha"),
            "disabled features must not enter the list: {features}"
        );
    }

    #[test]
    fn cfg_env_lines_handles_values_with_equals_and_dashes() {
        // Bare keys are uppercased verbatim (dashes preserved) and flagged =1.
        let lines = cfg_env_lines("target_features=\"+crc,+aes\"\nhas-dash\n\n   \n");
        assert!(
            lines.contains(&"cargo:rustc-env=CARGO_CFG_TARGET_FEATURES=+crc,+aes".to_string()),
            "'=' inside a value must not split the pair: {lines:?}"
        );
        assert!(lines.contains(&"cargo:rustc-env=CARGO_CFG_HAS-DASH=1".to_string()));
        // Whitespace-only lines produce nothing.
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn trim_cfg_quotes_leaves_unmatched_or_mixed_quotes_alone() {
        assert_eq!(trim_cfg_quotes("\"value"), "\"value");
        assert_eq!(trim_cfg_quotes("value\""), "value\"");
        assert_eq!(trim_cfg_quotes("'value\""), "'value\"");
    }

    #[test]
    fn cfg_command_args_forward_target_only_when_present() {
        assert_eq!(
            super::cfg_command_args(None),
            vec!["--print".to_string(), "cfg".to_string()]
        );
        assert_eq!(
            super::cfg_command_args(Some("aarch64-apple-darwin")),
            vec![
                "--print".to_string(),
                "cfg".to_string(),
                "--target".to_string(),
                "aarch64-apple-darwin".to_string()
            ]
        );
    }

    #[cfg(unix)]
    #[cfg_attr(
        miri,
        ignore = "spawns rustc-like processes; Miri does not support process execution"
    )]
    #[test]
    fn collect_cfg_lines_propagates_spawn_failures_but_swallows_bad_status() {
        // An unspawnable rustc surfaces as an error...
        let err = super::collect_cfg_lines("/definitely/not/a/rustc-binary")
            .expect_err("spawn failures must propagate");
        assert!(err.to_string().contains("failed to run"));

        // ...whereas an executable that exits nonzero yields no directives.
        // (`sh` rejects the unknown --print flag, exiting nonzero.)
        let lines = super::collect_cfg_lines("/bin/sh").expect("nonzero status is swallowed");
        assert!(
            lines.is_empty(),
            "no directives expected from a failing rustc"
        );
    }
}
