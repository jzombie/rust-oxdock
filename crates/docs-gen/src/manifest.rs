use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result, bail};
use oxdock_fs::{GuardedPath, PathResolver};

use crate::io::{read_text, write_text};

/// Directory (under the workspace root) holding per-target `$files`
/// manifests. Cargo already ignores `target/`, so manifests never
/// pollute the tree and `cargo clean` drops them.
const MANIFEST_DIR: &str = "target/oxdock-docs";

/// Build one `$files` manifest per render target and write it for the
/// DSL pipeline to `LOAD_JSON`.
///
/// A manifest maps each `fragments` group to the expanded content of
/// its files keyed by file stem (`intro.md.tmpl` becomes `intro`), so
/// a master template decides order explicitly with
/// `{{ $files.<group>.<stem> }}` placeholders resolved by the stock
/// strict engine. Nothing here invents syntax: discovery is `GLOB`,
/// expansion is `StreamingExpand`, transport is JSON.
///
/// Every fragment expands with exactly the scope it sees today (the
/// global values, its target values, `CRATE_VERSION` and nothing
/// else), then loses one trailing newline; the placeholder line in
/// the master supplies the line structure back. Strict throughout: an
/// unknown group, an unreadable value, or two files sharing a stem
/// fails the run instead of rendering something stale.
pub fn write_manifests(root: &GuardedPath, resolver: &PathResolver, version: &str) -> Result<()> {
    let cfg_text = read_text(resolver, root, "docs-gen.json")?;
    let cfg: serde_json::Value = serde_json::from_str(&cfg_text).context("parse docs-gen.json")?;
    let scopes = cfg
        .get("scopes")
        .and_then(|v| v.as_array())
        .context("docs-gen.json misses scopes array")?;
    let global_rel = cfg
        .get("global_values")
        .and_then(|v| v.as_str())
        .context("docs-gen.json misses global_values")?;
    let global = load_value_map(resolver, root, global_rel)?;
    let mut seen = BTreeSet::new();
    for scope in scopes {
        let scope = scope.as_str().context("scope entry is not a string")?;
        for rel in sorted_glob(root, &format!("{scope}/**/target.json"))? {
            let text = read_text(resolver, root, &rel)?;
            let parsed: serde_json::Value =
                serde_json::from_str(&text).with_context(|| format!("parse {rel}"))?;
            let targets = parsed
                .get("targets")
                .and_then(|v| v.as_array())
                .with_context(|| format!("{rel} misses targets array"))?;
            for target in targets {
                let name = str_field(target, &rel, "name")?;
                if !seen.insert(name.clone()) {
                    bail!("duplicate target name '{name}' (in {rel})");
                }
                if target.get("globs").is_some() {
                    bail!(
                        "target '{name}' still uses 'globs'; declare 'template' and grouped 'fragments' patterns instead"
                    );
                }
                let values_rel = str_field(target, &rel, "values")?;
                let ctx = load_value_map(resolver, root, &values_rel)?;
                let groups = target
                    .get("fragments")
                    .and_then(|v| v.as_object())
                    .with_context(|| format!("target '{name}' misses fragments group map"))?;
                let mut manifest = serde_json::Map::new();
                for (group, patterns) in groups {
                    let patterns = patterns.as_array().with_context(|| {
                        format!("target '{name}' group '{group}' is not a pattern list")
                    })?;
                    let mut group_map = serde_json::Map::new();
                    for pattern in patterns {
                        let pattern = pattern.as_str().with_context(|| {
                            format!("target '{name}' group '{group}' holds a non-string pattern")
                        })?;
                        for file_rel in sorted_glob(root, pattern)? {
                            let stem = file_stem(&file_rel)?;
                            if group_map.contains_key(&stem) {
                                bail!(
                                    "target '{name}': '{stem}' matches more than one file; placeholders must resolve to exactly one"
                                );
                            }
                            let raw = read_text(resolver, root, &file_rel)?;
                            let expanded =
                                expand_fragment(&raw, &global, &ctx, version, &file_rel)?;
                            group_map.insert(stem, serde_json::Value::String(expanded));
                        }
                    }
                    manifest.insert(group.clone(), serde_json::Value::Object(group_map));
                }
                let mut body = serde_json::to_string(&serde_json::Value::Object(manifest))
                    .context("encode manifest")?;
                body.push('\n');
                write_text(
                    resolver,
                    root,
                    &format!("{MANIFEST_DIR}/{name}.json"),
                    &body,
                )?;
            }
        }
    }
    Ok(())
}

/// Sorted sandbox-relative matches, mirroring the DSL `GLOB` order.
fn sorted_glob(root: &GuardedPath, pattern: &str) -> Result<Vec<String>> {
    let root_path = root.as_path().to_path_buf();
    let mut rels: Vec<String> = root
        .glob_paths(pattern)
        .with_context(|| format!("glob {pattern}"))?
        .into_iter()
        .filter_map(|p| {
            p.strip_prefix(&root_path)
                .ok()
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        })
        .collect();
    rels.sort();
    Ok(rels)
}

/// Placeholder key for a fragment path: file name up to the first
/// dot, so `intro.md.tmpl` and `intro.md` both resolve as `intro`.
/// Keys stay `[A-Za-z0-9_-]` so every placeholder stays readable;
/// anything else fails with a rename hint instead of producing a
/// placeholder nobody can type with confidence.
fn file_stem(rel: &str) -> Result<String> {
    let file = rel.rsplit('/').next().unwrap_or(rel);
    let stem = file.split('.').next().unwrap_or("");
    if stem.is_empty()
        || !stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("fragment '{rel}' has no placeholder-safe stem; rename it to [A-Za-z0-9_-] segments");
    }
    Ok(stem.to_string())
}

/// Expand one fragment with the stock streaming expander and the exact
/// scope a per-file pipeline `EXPAND` sees today, then drop one
/// trailing newline: the placeholder line in the master template
/// supplies the line structure back, which keeps master assembly
/// byte-identical to plain concatenation.
fn expand_fragment(
    raw: &str,
    global: &BTreeMap<String, oxdock_parser::Value>,
    ctx: &BTreeMap<String, oxdock_parser::Value>,
    version: &str,
    rel: &str,
) -> Result<String> {
    let mut env = HashMap::new();
    env.insert("CRATE_VERSION".to_string(), version.to_string());
    let mut vars = HashMap::new();
    vars.insert(
        "docs_global".to_string(),
        oxdock_parser::Value::Map(global.clone()),
    );
    vars.insert(
        "docs_ctx".to_string(),
        oxdock_parser::Value::Map(ctx.clone()),
    );
    let mut expander = oxdock_process::StreamingExpand::new(&[], &env).with_vars(&vars);
    let mut out = Vec::with_capacity(raw.len());
    expander
        .process_bytes(raw.as_bytes(), &mut out)
        .with_context(|| format!("expand {rel}"))?;
    expander
        .flush(&mut out)
        .with_context(|| format!("expand {rel}"))?;
    let mut text =
        String::from_utf8(out).with_context(|| format!("expand {rel} produced non-UTF-8"))?;
    if text.ends_with('\n') {
        text.pop();
    }
    Ok(text)
}

/// Load a JSON values file as template variables. Only the JSON
/// shapes templates can interpolate survive the trip; anything else
/// fails here instead of rendering as a silent empty.
fn load_value_map(
    resolver: &PathResolver,
    root: &GuardedPath,
    rel: &str,
) -> Result<BTreeMap<String, oxdock_parser::Value>> {
    let text = read_text(resolver, root, rel)?;
    let parsed: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parse {rel}"))?;
    let map = parsed
        .as_object()
        .with_context(|| format!("{rel} must hold a JSON object"))?;
    map.iter()
        .map(|(key, value)| json_to_value(value).map(|v| (key.clone(), v)))
        .collect::<Result<BTreeMap<_, _>>>()
        .with_context(|| format!("convert {rel} to template variables"))
}

/// Required string field on a target entry.
fn str_field(target: &serde_json::Value, rel: &str, key: &str) -> Result<String> {
    target
        .get(key)
        .and_then(|v| v.as_str())
        .map(String::from)
        .with_context(|| format!("target in {rel} misses {key}"))
}

/// JSON shapes the template engine can interpolate.
fn json_to_value(value: &serde_json::Value) -> Result<oxdock_parser::Value> {
    match value {
        serde_json::Value::String(s) => Ok(oxdock_parser::Value::String(s.clone())),
        serde_json::Value::Bool(b) => Ok(oxdock_parser::Value::Bool(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(oxdock_parser::Value::Int(i))
            } else if let Some(f) = n.as_f64() {
                Ok(oxdock_parser::Value::Float(f))
            } else {
                bail!("number {n} survives into templates as nothing; drop the key")
            }
        }
        serde_json::Value::Array(items) => items
            .iter()
            .map(json_to_value)
            .collect::<Result<Vec<_>>>()
            .map(oxdock_parser::Value::List),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, item)| json_to_value(item).map(|v| (key.clone(), v)))
            .collect::<Result<BTreeMap<_, _>>>()
            .map(oxdock_parser::Value::Map),
        serde_json::Value::Null => bail!("null survives into templates as nothing; drop the key"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_strips_compound_extensions() {
        assert_eq!(file_stem("a/b/intro.md.tmpl").expect("stem"), "intro");
        assert_eq!(file_stem("x/y.md").expect("stem"), "y");
        assert_eq!(file_stem("header.md.tmpl").expect("stem"), "header");
        assert_eq!(file_stem("a/quick-start.md").expect("stem"), "quick-start");
    }

    #[test]
    fn stem_rejects_placeholder_hostile_names() {
        assert!(file_stem("a/usage-(build.rs).md").is_err());
        assert!(file_stem("a/.md").is_err());
    }

    #[test]
    fn fragment_expansion_mirrors_pipeline_scope() {
        let mut global = BTreeMap::new();
        global.insert(
            "workspace".to_string(),
            oxdock_parser::Value::String("OxDock".to_string()),
        );
        let mut ctx = BTreeMap::new();
        ctx.insert(
            "name".to_string(),
            oxdock_parser::Value::String("oxdock".to_string()),
        );
        let out = expand_fragment(
            "# {{ $docs_ctx.name }} {{ $docs_global.workspace }} {{ env:CRATE_VERSION }}\n",
            &global,
            &ctx,
            "1.2.3",
            "test.md",
        )
        .expect("expand");
        assert_eq!(out, "# oxdock OxDock 1.2.3");
    }

    #[test]
    fn fragment_expansion_keeps_doc_examples_literal() {
        let out = expand_fragment(
            "write `\\{{ $var }}` here\n",
            &BTreeMap::new(),
            &BTreeMap::new(),
            "1.2.3",
            "test.md",
        )
        .expect("expand");
        assert_eq!(out, "write `{{ $var }}` here");
    }
}
