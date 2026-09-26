//! Host-side remote inventory: target names bound to tokenized transport
//! commands at startup, validated against parsed scripts before any
//! session starts. Same fail-fast posture as the endpoint registry built
//! from `--listen`/`-p`/`--offline`: unknown targets never become
//! mid-run surprises, and every resolution is logged to stderr.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use oxdock_parser::StepKind;

/// One inventory entry: the tokenized transport command. `--remote-serve`
/// is appended at spawn time, never stored here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTarget {
    /// Tokenized command, for example `["ssh", "-i", "key", "host", "oxdock"]`.
    pub argv: Vec<String>,
}

/// Target inventory bound from `--remote` flags before parsing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteInventory {
    targets: BTreeMap<String, RemoteTarget>,
}

impl RemoteInventory {
    /// Build from parsed `(target, command)` pairs. Duplicate targets and
    /// commands tokenizing to zero argv bail; quoting was already validated
    /// at flag parse time.
    pub fn build(pairs: Vec<(String, String)>) -> Result<Self> {
        let mut targets = BTreeMap::new();
        for (target, command) in pairs {
            if targets.contains_key(&target) {
                bail!("duplicate --remote target {target:?}");
            }
            let argv = shell_words::split(&command).map_err(|err| {
                anyhow::anyhow!("invalid --remote {target:?}: cannot tokenize command: {err}")
            })?;
            if argv.is_empty() {
                bail!("invalid --remote {target:?}: command tokenizes to no executable");
            }
            targets.insert(target, RemoteTarget { argv });
        }
        Ok(Self { targets })
    }

    /// Declared target names, sorted, for diagnostics and unknown-target errors.
    pub fn targets(&self) -> Vec<String> {
        self.targets.keys().cloned().collect()
    }

    /// Transport argv for a bound target.
    pub fn argv(&self, target: &str) -> Option<&[String]> {
        self.targets.get(target).map(|entry| entry.argv.as_slice())
    }

    /// Collect every `REMOTE` target named anywhere in parsed steps,
    /// descending through all nested bodies with the uniform walker.
    pub fn collect_targets(steps: &[oxdock_parser::Step]) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        fn visit(kind: &StepKind, out: &mut BTreeSet<String>) {
            if let StepKind::RemoteBlock { target, .. } = kind {
                out.insert(target.clone());
            }
            kind.walk_child_kinds(&mut |child| visit(child, out));
        }
        for step in steps {
            visit(&step.kind, &mut out);
        }
        out
    }

    /// Fail fast before execution: every script target must be bound.
    /// Scripts without remote blocks always pass, so local-only runs stay
    /// hermetic and never consult the network.
    pub fn validate_script(&self, steps: &[oxdock_parser::Step]) -> Result<()> {
        for target in Self::collect_targets(steps) {
            if !self.targets.contains_key(&target) {
                bail!(
                    "unknown remote target {target:?} (declared: [{}]; add --remote {target}=\"<command>\")",
                    self.targets()
                        .iter()
                        .map(|name| format!("{name:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        Ok(())
    }

    /// Log every alias to endpoint resolution to stderr, even for targets
    /// unused by this run, so inventory drift is visible without digging.
    pub fn log_resolutions(&self) {
        for (target, entry) in &self.targets {
            eprintln!("oxdock: remote {target} -> {}", entry.argv.join(" "));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory() -> RemoteInventory {
        RemoteInventory::build(vec![
            ("prod".to_string(), "ssh user@prod oxdock".to_string()),
            ("lab".to_string(), "ssh -i key user@lab /opt/oxdock".to_string()),
        ])
        .unwrap()
    }

    #[test]
    fn build_rejects_duplicates_and_empty_argv() {
        assert!(
            RemoteInventory::build(vec![
                ("a".to_string(), "ssh h oxdock".to_string()),
                ("a".to_string(), "ssh h2 oxdock".to_string()),
            ])
            .is_err()
        );
        assert!(
            RemoteInventory::build(vec![("a".to_string(), "   ".to_string())]).is_err()
        );
    }

    #[test]
    fn validate_passes_clean_scripts_and_empty_inventory() {
        let steps = oxdock_parser::parse_script(
            "ECHO hi\n",
            oxdock_parser::commands::lower_command,
        )
        .unwrap();
        RemoteInventory::default().validate_script(&steps).unwrap();
        inventory().validate_script(&steps).unwrap();
    }

    #[test]
    fn validate_rejects_unknown_targets() {
        let steps = vec![oxdock_parser::Step {
            guard: None,
            kind: StepKind::RemoteBlock {
                target: "missing".to_string(),
                vars: Vec::new(),
                env: Vec::new(),
                body: Vec::new(),
            },
            scope_enter: 0,
            scope_exit: 0,
        }];
        let err = inventory().validate_script(&steps).expect_err("must fail");
        let text = format!("{err:#}");
        assert!(text.contains("unknown remote target"), "{text}");
        assert!(text.contains("missing"), "{text}");
        assert!(text.contains("--remote"), "{text}");
    }

    #[test]
    fn collect_descends_into_nested_bodies() {
        let steps = vec![oxdock_parser::Step {
            guard: None,
            kind: StepKind::Timeout {
                duration: oxdock_parser::Arg::String("1s".to_string(), false),
                body: vec![oxdock_parser::Step {
                    guard: None,
                    kind: StepKind::RemoteBlock {
                        target: "deep".to_string(),
                        vars: Vec::new(),
                        env: Vec::new(),
                        body: Vec::new(),
                    },
                    scope_enter: 0,
                    scope_exit: 0,
                }],
            },
            scope_enter: 0,
            scope_exit: 0,
        }];
        let targets = RemoteInventory::collect_targets(&steps);
        assert!(targets.contains("deep"));
    }
}
