//! Static input discovery for `cargo:rerun-if-changed` emission.
//!
//! Walks the parsed DSL AST and derives watchable inputs:
//! - fully literal path arguments become `cargo:rerun-if-changed` entries,
//! - templated paths contribute their literal directory head (conservative)
//!   plus `cargo:rerun-if-env-changed` entries for every referenced env key,
//! - keys assigned by `ENV` steps inside the same script are excluded, since
//!   the script itself controls those values.
//!
//! Outputs are manifest-dir-relative forward-slash strings; cargo normalizes
//! them per-host.

use std::collections::{BTreeSet, HashSet};

use oxdock_parser::{StepKind, TemplateString};

fn template_text(t: &TemplateString) -> &str {
    &t.0
}

/// Extract `{{ env:KEY }}` placeholder names from a template string.
fn env_placeholders(template: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        if let Some(end) = after.find("}}") {
            let inner = after[..end].trim();
            if let Some(key) = inner.strip_prefix("env:") {
                keys.push(key.trim().to_string());
            }
            rest = &after[end + 2..];
        } else {
            break;
        }
    }
    keys
}

fn push_path_entry(entries: &mut BTreeSet<String>, template: &TemplateString) {
    let mut text = template_text(template).trim();
    // Quoted DSL paths keep their quotes through parsing.
    while text.starts_with('"') && text.ends_with('"') && text.len() >= 2 {
        text = text[1..text.len() - 1].trim();
    }
    if text.is_empty() {
        return;
    }
    if !text.contains("{{") {
        entries.insert(text.replace('\\', "/"));
    } else {
        // Conservative fallback: watch the literal directory head so edits
        // near the dynamic name still invalidate the build.
        let head = text
            .split("{{")
            .next()
            .unwrap_or_default()
            .trim_matches('/');
        if !head.is_empty() {
            let head = match head.rfind('/') {
                Some(idx) => &head[..idx],
                None => head,
            };
            if !head.is_empty() && !head.contains("{{") {
                entries.insert(head.replace('\\', "/"));
            }
        }
    }
}

/// Compute the ordered, de-duplicated input directives for a parsed script.
///
/// `assigned_keys` are the variables the script sets itself (`ENV` steps);
/// placeholders referencing them are skipped because their values never come
/// from the host environment.
pub fn plan_input_directives(steps: &[oxdock_parser::Step]) -> (Vec<String>, Vec<String>) {
    let mut changed: BTreeSet<String> = BTreeSet::new();
    let mut env_changed: BTreeSet<String> = BTreeSet::new();
    let mut assigned: HashSet<String> = HashSet::new();

    for step in steps {
        if let StepKind::Env { key, .. } = &step.kind {
            assigned.insert(key.clone());
        }
    }

    for step in steps {
        match &step.kind {
            StepKind::Copy { from, .. } => {
                push_path_entry(&mut changed, from);
                collect_env_keys(&mut env_changed, template_text(from), &assigned);
            }
            StepKind::CopyGit { from, .. } => {
                push_path_entry(&mut changed, from);
                collect_env_keys(&mut env_changed, template_text(from), &assigned);
            }
            StepKind::Symlink { from, .. } => {
                push_path_entry(&mut changed, from);
                collect_env_keys(&mut env_changed, template_text(from), &assigned);
            }
            StepKind::HashSha256 { path } => {
                push_path_entry(&mut changed, path);
                collect_env_keys(&mut env_changed, template_text(path), &assigned);
            }
            StepKind::Read(Some(path)) => {
                push_path_entry(&mut changed, path);
                collect_env_keys(&mut env_changed, template_text(path), &assigned);
            }
            _ => {}
        }
    }

    (
        changed.into_iter().collect(),
        env_changed.into_iter().collect(),
    )
}

fn collect_env_keys(out: &mut BTreeSet<String>, template: &str, assigned: &HashSet<String>) {
    let template = template.trim().trim_matches('"');
    for key in env_placeholders(template) {
        if !assigned.contains(&key) {
            out.insert(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxdock_parser::{Step, parse_script};

    fn script_steps(script: &str) -> Vec<Step> {
        parse_script(script).expect("parse")
    }

    #[test]
    fn literal_copy_sources_are_watched() {
        let steps = script_steps("COPY src/a.txt dist\nSYMLINK lnk target");
        let (changed, env) = plan_input_directives(&steps);
        assert_eq!(changed, vec!["lnk".to_string(), "src/a.txt".to_string()]);
        assert!(env.is_empty());
    }

    #[test]
    fn templated_sources_fall_back_to_directory_head_and_env_watch() {
        let steps = script_steps("ENV BASE=b\nCOPY \"{{ env:BASE }}/file.txt\" dist");
        // ENV assignment must exclude BASE from rerun-if-env-changed.
        let (_, env) = plan_input_directives(&steps);
        assert!(env.is_empty(), "script-assigned keys must be excluded");
    }

    #[test]
    fn unassigned_env_placeholders_are_watched() {
        let steps = script_steps("COPY \"{{ env:ASSET_DIR }}/blob.bin\" out");
        let (changed, env) = plan_input_directives(&steps);
        assert!(
            changed.is_empty(),
            "placeholder-first path has no static head to watch"
        );
        assert_eq!(env, vec!["ASSET_DIR".to_string()]);
    }

    #[test]
    fn write_targets_are_not_inputs() {
        let steps = script_steps("WRITE out/generated.txt body");
        let (changed, env) = plan_input_directives(&steps);
        assert!(changed.is_empty());
        assert!(env.is_empty());
    }
}
