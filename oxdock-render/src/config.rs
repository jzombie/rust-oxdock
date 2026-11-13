//! Pipeline config surface: filename default plus plugin registry.
//!
//! Rust owns only the filename edge (flag, env, neutral default),
//! strict config validation, the `--init` template, and reading the
//! plugin path list out of JSON. Lifecycle defaults and all
//! sequencing stay in the DSL (`render.oxfile`).

use anyhow::{Context, Result, bail};
use oxdock_core::Value;
use oxdock_fs::{GuardedPath, PathResolver, WorkspaceFs};

/// Known top-level `render.json` keys. Anything else fails closed so
/// typos (`scope` for `scopes`) surface instead of silently changing
/// the pipeline. Mirrors every `$cfg.*` read in the lifecycle
/// scripts, plus `hooks` below.
const TOP_LEVEL_KEYS: &[&str] = &[
    "generated",
    "global_values",
    "hooks",
    "lifecycle",
    "merge_policy",
    "scopes",
    "staging_dir",
    "vars",
    "version_key",
];

/// Template `render.json` written by `--init`: the minimal valid
/// pipeline (hooks first, workspace root scope). Authored as an
/// OxDock template (`init.json.tmpl`) and expanded through the
/// pipeline's own fragment engine before writing, so the suggested
/// starting point is always engine-rendered output. Comments are
/// allowed (`//` and `/* */`) in every JSON file the pipeline
/// reads; they never reach validation. Validates clean.
pub const INIT_TEMPLATE: &str = include_str!("../templates/init.json.tmpl");

/// Expand the init template through the pipeline's own fragment
/// engine with an empty scope, restoring the trailing newline the
/// engine strips (mirroring the `RENDER` stage). Any `{{ }}`
/// typo in the template fails here instead of shipping a broken
/// starting point.
pub(crate) fn render_init_template() -> Result<String> {
    let empty = Value::map(std::collections::BTreeMap::new());
    let text = crate::engine::render_fragment(INIT_TEMPLATE, &empty, &empty, None)?
        .as_str()
        .expect("fragment expansion renders a string")
        .to_string();
    if INIT_TEMPLATE.ends_with('\n') && !text.ends_with('\n') {
        return Ok(format!("{text}\n"));
    }
    Ok(text)
}

/// Hook phases: scripts that run before the entry assembles the
/// child (`pre`) and after every target verifies (`post`). Named
/// here so call sites and validation never repeat the strings.
pub(crate) const HOOK_PRE: &str = "pre";
pub(crate) const HOOK_POST: &str = "post";
pub(crate) const HOOK_PHASES: &[&str] = &[HOOK_PRE, HOOK_POST];

/// Hook scripts from a validated config, keyed by phase. Either
/// side failing halts the pipeline.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Hooks {
    pub(crate) pre: Vec<String>,
    pub(crate) post: Vec<String>,
}

fn expect_object<'v>(
    value: &'v serde_json::Value,
    what: &str,
    config_name: &str,
) -> Result<&'v serde_json::Map<String, serde_json::Value>> {
    value
        .as_object()
        .with_context(|| format!("{config_name}: '{what}' must be a JSON object"))
}

fn expect_string(value: &serde_json::Value, what: &str, config_name: &str) -> Result<String> {
    value
        .as_str()
        .with_context(|| format!("{config_name}: '{what}' must be a string"))
        .map(str::to_string)
}

fn expect_string_list(
    value: &serde_json::Value,
    what: &str,
    config_name: &str,
) -> Result<Vec<String>> {
    let arr = value
        .as_array()
        .with_context(|| format!("{config_name}: '{what}' must be a list of strings"))?;
    arr.iter()
        .map(|entry| expect_string(entry, what, config_name))
        .collect()
}

/// Strict `render.json` validation: unknown keys fail naming the
/// known set, mistyped values name the expected shape. Presence is
/// required only for keys the entry script reads unconditionally
/// (`scopes`, `global_values`); everything else is optional but
/// type-checked when present, so custom lifecycles that skip
/// `vars` or `generated` keep working.
pub(crate) fn validate_config(value: &serde_json::Value, config_name: &str) -> Result<()> {
    let cfg = expect_object(value, "config", config_name)?;
    for key in cfg.keys() {
        if !TOP_LEVEL_KEYS.contains(&key.as_str()) {
            bail!(
                "{config_name}: unknown key '{key}'; known keys: {}",
                TOP_LEVEL_KEYS.join(", ")
            );
        }
    }
    let required = ["scopes", "global_values"];
    for key in required {
        if !cfg.contains_key(key) {
            bail!("{config_name}: missing required key '{key}'");
        }
    }
    expect_string_list(
        cfg.get("scopes").expect("scopes presence checked"),
        "scopes",
        config_name,
    )?;
    let gv = cfg
        .get("global_values")
        .expect("global_values presence checked");
    let gv_ok = gv.as_str().is_some()
        || gv
            .as_array()
            .is_some_and(|arr| arr.iter().all(|entry| entry.as_str().is_some()));
    if !gv_ok {
        bail!("{config_name}: 'global_values' must be a string or a list of strings");
    }
    if let Some(vars) = cfg.get("vars") {
        let vars = expect_object(vars, "vars", config_name)?;
        for key in vars.keys() {
            if !["global", "ctx", "workspace_pkg"].contains(&key.as_str()) {
                bail!(
                    "{config_name}: unknown vars key '{key}'; known keys: global, ctx, workspace_pkg"
                );
            }
        }
        for key in ["global", "ctx", "workspace_pkg"] {
            if let Some(entry) = vars.get(key) {
                expect_string(entry, &format!("vars.{key}"), config_name)?;
            }
        }
    }
    if let Some(lifecycle) = cfg.get("lifecycle") {
        let lifecycle = expect_object(lifecycle, "lifecycle", config_name)?;
        for key in lifecycle.keys() {
            if !["files", "types"].contains(&key.as_str()) {
                bail!("{config_name}: unknown lifecycle key '{key}'; known keys: files, types");
            }
        }
        if let Some(files) = lifecycle.get("files") {
            expect_string_list(files, "lifecycle.files", config_name)?;
        }
        if let Some(types) = lifecycle.get("types") {
            expect_string(types, "lifecycle.types", config_name)?;
        }
    }
    if let Some(generated) = cfg.get("generated") {
        let entries = generated
            .as_array()
            .with_context(|| format!("{config_name}: 'generated' must be a list"))?;
        for (idx, entry) in entries.iter().enumerate() {
            let entry = expect_object(entry, &format!("generated[{idx}]"), config_name)?;
            for key in entry.keys() {
                if !["key", "out"].contains(&key.as_str()) {
                    bail!(
                        "{config_name}: unknown generated[{idx}] key '{key}'; known keys: key, out"
                    );
                }
            }
            for key in ["key", "out"] {
                let field = entry.get(key).with_context(|| {
                    format!("{config_name}: 'generated[{idx}]' needs string '{key}'")
                })?;
                expect_string(field, &format!("generated[{idx}].{key}"), config_name)?;
            }
        }
    }
    if let Some(hooks) = cfg.get("hooks") {
        let hooks = expect_object(hooks, "hooks", config_name)?;
        for key in hooks.keys() {
            if !HOOK_PHASES.contains(&key.as_str()) {
                bail!(
                    "{config_name}: unknown hooks key '{key}'; known keys: {}",
                    HOOK_PHASES.join(", ")
                );
            }
        }
        for phase in HOOK_PHASES {
            if let Some(list) = hooks.get(*phase) {
                expect_string_list(list, &format!("hooks.{phase}"), config_name)?;
            }
        }
    }
    for key in ["staging_dir", "merge_policy", "version_key"] {
        if let Some(entry) = cfg.get(key) {
            expect_string(entry, key, config_name)?;
        }
    }
    Ok(())
}

/// Hook script paths from an already-validated config. Lenient by
/// construction: validation guarantees the shape, so anything
/// unexpected here reads as empty.
pub(crate) fn hooks_of(value: &serde_json::Value) -> Hooks {
    let mut hooks = Hooks::default();
    let get_list = |phase: &str| {
        value
            .get("hooks")
            .and_then(|hooks| hooks.get(phase))
            .and_then(|list| list.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|entry| entry.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    hooks.pre = get_list(HOOK_PRE);
    hooks.post = get_list(HOOK_POST);
    hooks
}

/// Write the template config into a directory without touching an
/// existing `render.json`: returns the written path, or fails naming
/// the file to delete first. Used by `--init`.
pub fn init_config_file(root: &GuardedPath) -> Result<GuardedPath> {
    let resolver =
        PathResolver::new_guarded(root.clone(), root.clone()).context("resolver for --init")?;
    let path = root
        .join("render.json")
        .context("resolve render.json for --init")?;
    if resolver.entry_kind(&path).is_ok() {
        bail!(
            "{} already exists; refusing to overwrite (delete it to re-scaffold)",
            path.display()
        );
    }
    resolver
        .write_file(&path, render_init_template()?.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// The engine's default lifecycle providers: the source scanner
/// plus the generic stage owner. Used when the config names no
/// plugins of its own.
pub(crate) const SCANNER_PLUGIN: &str = "oxdock-render/src/lifecycle/scanner.oxfile";
pub(crate) const GENERIC_PLUGIN: &str = "oxdock-render/src/lifecycle/generic.oxfile";

/// Default registry order: discovery plus expansion first,
/// manifest promotion second.
pub(crate) fn default_plugins() -> Vec<String> {
    vec![SCANNER_PLUGIN.to_string(), GENERIC_PLUGIN.to_string()]
}

/// Lifecycle plugin paths from the config, or the generic default
/// when the config names none. Entries are plain paths: every
/// plugin file exposes the same lifecycle methods, so there is
/// nothing else to declare.
pub(crate) fn configured_plugins(
    fs: &dyn WorkspaceFs,
    root: &GuardedPath,
    config_name: &str,
) -> Result<Vec<String>> {
    let path = root.join(config_name)?;
    let raw = fs
        .read_to_string(&path)
        .with_context(|| format!("read {config_name} for plugin list"))?;
    let cfg: serde_json::Value = serde_json::from_str(&oxdock_core::strip_json_comments(&raw))
        .with_context(|| format!("parse {config_name} for plugin list"))?;
    let files = match cfg.get("lifecycle").and_then(|lc| lc.get("files")) {
        None => return Ok(default_plugins()),
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

/// Config filename: explicit `RENDER_CONFIG` wins, else the one
/// neutral name. Anything else fails closed naming what was looked
/// for; the DSL never carries a filename default.
pub(crate) fn resolve_config_name(fs: &dyn WorkspaceFs, root: &GuardedPath) -> Result<String> {
    if let Ok(name) = std::env::var(oxdock_fs::env::render::CONFIG) {
        return Ok(name);
    }
    let neutral = root.join("render.json")?;
    if fs.metadata(&neutral).is_ok() {
        return Ok("render.json".to_string());
    }
    anyhow::bail!(
        "no render.json under {}; pass --config or set {}",
        root.as_path().to_string_lossy(),
        oxdock_fs::env::render::CONFIG,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> serde_json::Value {
        serde_json::from_str(&oxdock_core::strip_json_comments(text)).expect("fixture parses")
    }

    #[test]
    fn init_template_validates_clean() {
        // Self-parse verification: the suggested template is expanded
        // through the pipeline's own fragment engine first, and the
        // rendered bytes (what `--init` actually writes) must parse
        // and validate. A template the pipeline cannot read back is
        // a broken suggestion, so this fails instead of shipping it.
        let rendered = render_init_template().expect("init renders");
        assert_eq!(
            rendered, INIT_TEMPLATE,
            "template carries no live placeholders; a deliberate {{ }} must wire scope explicitly"
        );
        let value = parse(&rendered);
        validate_config(&value, "render.json").expect("template is valid");
        assert_eq!(hooks_of(&value), Hooks::default());
    }

    #[test]
    fn commented_config_parses_like_plain_json() {
        // `//` and `/* */` notes never reach validation; `//` inside
        // a string stays data.
        let value = parse(
            r#"{
                // leading note
                "global_values": "http://x/y", /* trailing note */
                "scopes": ["."] // after a value
            }"#,
        );
        validate_config(&value, "render.json").expect("comments parse");
        assert_eq!(
            value.get("global_values").and_then(|v| v.as_str()),
            Some("http://x/y")
        );
    }

    #[test]
    fn minimal_config_validates() {
        let value = parse(r#"{"global_values": "v.json", "scopes": ["."]}"#);
        validate_config(&value, "render.json").expect("minimal is valid");
        assert_eq!(hooks_of(&value), Hooks::default());
    }

    #[test]
    fn full_config_validates_with_hooks() {
        let value = parse(
            r#"{
                "global_values": ["a.json", "b.json"],
                "scopes": [".", "extra"],
                "generated": [{"key": "k", "out": "o.tmpl"}],
                "vars": {"global": "g", "ctx": "c", "workspace_pkg": "w"},
                "lifecycle": {"files": ["a.oxfile"], "types": "t.oxfile"},
                "staging_dir": "target/x",
                "merge_policy": "overwrite",
                "version_key": "V",
                "hooks": {"pre": ["a.oxfile"], "post": ["b.oxfile", "c.oxfile"]}
            }"#,
        );
        validate_config(&value, "render.json").expect("full is valid");
        assert_eq!(
            hooks_of(&value),
            Hooks {
                pre: vec!["a.oxfile".to_string()],
                post: vec!["b.oxfile".to_string(), "c.oxfile".to_string()],
            }
        );
    }

    #[test]
    fn unknown_top_level_key_fails_naming_known_keys() {
        let value = parse(r#"{"global_values": "v.json", "scopes": ["."], "scope": ["."]}"#);
        let err = validate_config(&value, "render.json").expect_err("typo must fail");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("unknown key 'scope'"), "{rendered}");
        assert!(rendered.contains("scopes"), "{rendered}");
    }

    #[test]
    fn unknown_nested_keys_fail() {
        let value =
            parse(r#"{"global_values": "v.json", "scopes": ["."], "hooks": {"prelude": []}}"#);
        let err = validate_config(&value, "r.json").expect_err("bad hooks key must fail");
        assert!(format!("{err:#}").contains("unknown hooks key 'prelude'"));

        let value = parse(
            r#"{"global_values": "v.json", "scopes": ["."], "vars": {"global": "g", "ctx": "c", "extra": 1}}"#,
        );
        let err = validate_config(&value, "r.json").expect_err("bad vars key must fail");
        assert!(format!("{err:#}").contains("unknown vars key 'extra'"));

        let value = parse(
            r#"{"global_values": "v.json", "scopes": ["."], "generated": [{"key": "k", "output": "o"}]}"#,
        );
        let err = validate_config(&value, "r.json").expect_err("bad entry key must fail");
        assert!(format!("{err:#}").contains("unknown generated[0] key 'output'"));
    }

    #[test]
    fn mistyped_values_fail_naming_shapes() {
        let value = parse(r#"{"global_values": "v.json", "scopes": "all"}"#);
        let err = validate_config(&value, "r.json").expect_err("scopes must be a list");
        assert!(format!("{err:#}").contains("'scopes' must be a list of strings"));

        let value = parse(r#"{"global_values": 42, "scopes": []}"#);
        let err = validate_config(&value, "r.json").expect_err("global_values must be str|list");
        assert!(
            format!("{err:#}").contains("'global_values' must be a string or a list of strings")
        );

        let value =
            parse(r#"{"global_values": "v.json", "scopes": [], "hooks": {"pre": "a.oxfile"}}"#);
        let err = validate_config(&value, "r.json").expect_err("hooks.pre must be a list");
        assert!(format!("{err:#}").contains("'hooks.pre' must be a list of strings"));
    }

    #[test]
    fn missing_required_keys_fail() {
        let value = parse(r#"{"global_values": "v.json"}"#);
        let err = validate_config(&value, "r.json").expect_err("scopes is required");
        assert!(format!("{err:#}").contains("missing required key 'scopes'"));
    }

    #[test]
    fn non_object_config_fails() {
        let value = parse(r#"["scopes"]"#);
        let err = validate_config(&value, "r.json").expect_err("array is not an object");
        assert!(format!("{err:#}").contains("must be a JSON object"));
    }
}
