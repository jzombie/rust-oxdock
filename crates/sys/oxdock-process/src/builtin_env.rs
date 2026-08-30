use oxdock_fs::{GuardedPath, current_head_commit};
use std::collections::HashMap;

/// Built-in environment variables injected into the OxDock execution context.
#[derive(Debug, Default)]
pub struct BuiltinEnv {
    values: HashMap<String, String>,
}

impl BuiltinEnv {
    pub const WORKSPACE_GIT_COMMIT: &'static str = "WORKSPACE_GIT_COMMIT";

    pub fn collect(build_context: &GuardedPath) -> Self {
        let mut values = HashMap::new();
        if let Ok(Some(commit)) = current_head_commit(build_context) {
            values.insert(Self::WORKSPACE_GIT_COMMIT.to_string(), commit);
        }

        {
            // Propagate Cargo-provided feature/cfg envs into the OxDock environment.
            // Note: proc-macro processes do NOT receive these by default; the
            // oxdock-build build script emits them via rustc-env.
            // These are already namespaced by Cargo (CARGO_FEATURE_*, CARGO_CFG_*).
            for (key, value) in std::env::vars() {
                if key.starts_with("CARGO_FEATURE_") || key.starts_with("CARGO_CFG_") {
                    values.entry(key).or_insert(value);
                }
            }
            // Derive feature flags from CARGO_CFG_FEATURE when only the feature list is present.
            for feature in cargo_features_from_env() {
                let key = format!("CARGO_FEATURE_{}", normalize_feature_key(&feature));
                values.entry(key).or_insert_with(|| "1".to_string());
            }
        }

        Self { values }
    }

    pub fn into_envs(self) -> HashMap<String, String> {
        self.values
    }
}

fn cargo_features_from_env() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(cfg_features) = std::env::var("CARGO_CFG_FEATURE") {
        out.extend(split_feature_list(&cfg_features));
    }
    out
}

fn split_feature_list(value: &str) -> Vec<String> {
    value
        .split([',', ' '])
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .collect()
}

fn normalize_feature_key(name: &str) -> String {
    name.replace('-', "_").to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial_cargo_env::manifest_env_guard;
    use oxdock_fs::GuardedPath;
    use oxdock_sys_test_utils::TestEnvGuard;

    #[test]
    fn collect_includes_cargo_feature_and_cfg_envs() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let _guard = manifest_env_guard(&root, true);
        let _env_guard_a = TestEnvGuard::set("CARGO_FEATURE_OXDOCK_TEST", "1");
        let _env_guard_b = TestEnvGuard::set("CARGO_CFG_OXDOCK_TEST", "enabled");
        let env = BuiltinEnv::collect(&root).into_envs();
        assert_eq!(env.get("CARGO_FEATURE_OXDOCK_TEST"), Some(&"1".into()));
        assert_eq!(env.get("CARGO_CFG_OXDOCK_TEST"), Some(&"enabled".into()));
    }

    #[test]
    fn collect_derives_feature_keys_from_cfg_feature_list() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let _guard = manifest_env_guard(&root, true);
        let _cfg = TestEnvGuard::set("CARGO_CFG_FEATURE", "my-feat, other_feat,,  spaced ");
        let env = BuiltinEnv::collect(&root).into_envs();

        // Dashes become underscores; names uppercase; empty segments dropped.
        assert_eq!(env.get("CARGO_FEATURE_MY_FEAT"), Some(&"1".to_string()));
        assert_eq!(env.get("CARGO_FEATURE_OTHER_FEAT"), Some(&"1".to_string()));
        assert_eq!(env.get("CARGO_FEATURE_SPACED"), Some(&"1".to_string()));
    }

    #[test]
    fn collect_prefers_existing_feature_env_over_derived_value() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let _guard = manifest_env_guard(&root, true);
        let _cfg = TestEnvGuard::set("CARGO_CFG_FEATURE", "my-feat");
        let _existing = TestEnvGuard::set("CARGO_FEATURE_MY_FEAT", "custom");
        let env = BuiltinEnv::collect(&root).into_envs();

        assert_eq!(
            env.get("CARGO_FEATURE_MY_FEAT"),
            Some(&"custom".to_string()),
            "derived '1' must not overwrite an explicit env value"
        );
    }
}
