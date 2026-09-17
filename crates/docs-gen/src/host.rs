//! Host functions exposing docs-gen pipeline stages to the DSL.
//!
//! Every helper here is a value-returning `#[oxdock_func]`: pure string,
//! map, and rendering transforms plus narrow filesystem readers. The
//! script in `lib.rs` owns all sequencing and all writes through native
//! `WRITE`/`APPEND`/`EXPAND`, so the pipeline reads as DSL with Rust
//! only where the DSL has no primitives (registry rendering, TOML
//! metadata, strict JSON encoding, placeholder-safe stems).

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use oxdock_core::{HostRegistration, OxDockFn, StepCtx, Value};
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_func_macro::oxdock_func;
use oxdock_process::ProcessManager;

fn docs_resolver(cx: &StepCtx<impl ProcessManager>) -> Result<(GuardedPath, PathResolver)> {
    // docs-gen always runs the script with cwd at the repo root, so stage
    // helpers resolve against it exactly like `run()` used to.
    let root = cx.cwd().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path())?;
    Ok((root, resolver))
}

/// List workspace member paths from the root Cargo.toml.
#[oxdock_func(returns = "LIST")]
fn workspace_members<P: ProcessManager>(cx: &mut StepCtx<P>) -> Result<Value> {
    let (root, resolver) = docs_resolver(cx)?;
    let members = crate::rust::cargo::workspace_members(&root, &resolver)?;
    Ok(Value::list(
        members.into_iter().map(Value::string).collect(),
    ))
}

/// Resolve the render version: script-visible `CRATE_VERSION` wins,
/// otherwise fall back to the workspace manifest.
#[oxdock_func(returns = "STRING")]
fn workspace_version<P: ProcessManager>(cx: &mut StepCtx<P>) -> Result<Value> {
    let (root, resolver) = docs_resolver(cx)?;
    let version =
        crate::rust::crate_version_with_env(&root, &resolver, cx.get_env("CRATE_VERSION"))?;
    Ok(Value::string(version))
}

/// Package name and description from a member manifest. Returns empty
/// strings when there is no manifest or no package name, so the script
/// can skip with `IF $pkg.name != ""` instead of catching an error.
#[oxdock_func(returns = "MAP")]
fn cargo_package<P: ProcessManager>(cx: &mut StepCtx<P>, member: String) -> Result<Value> {
    let (root, resolver) = docs_resolver(cx)?;
    let (name, description) =
        crate::rust::cargo::cargo_package(&root, &resolver, &member)?.unwrap_or_default();
    let mut entries = BTreeMap::new();
    entries.insert("name".to_string(), Value::string(name));
    entries.insert("description".to_string(), Value::string(description));
    Ok(Value::map(entries))
}

/// Placeholder key for a fragment path: file name up to the first dot,
/// so `intro.md.tmpl` and `intro.md` both resolve as `intro`. Keys stay
/// `[A-Za-z0-9_-]` so every placeholder stays readable; anything else
/// fails with a rename hint instead of producing a placeholder nobody
/// can type with confidence.
#[oxdock_func(pure, returns = "STRING")]
fn file_stem(path: String) -> Result<Value> {
    let file = path.rsplit('/').next().unwrap_or(&path);
    let stem = file.split('.').next().unwrap_or("");
    if stem.is_empty()
        || !stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!(
            "fragment '{path}' has no placeholder-safe stem; rename it to [A-Za-z0-9_-] segments"
        );
    }
    Ok(Value::string(stem.to_string()))
}

/// Report whether a map holds a key, so scripts can branch on optional
/// fields without tripping the strict missing-key error.
#[oxdock_func(pure, returns = "BOOL")]
fn has_key(map: Value, key: String) -> Result<Value> {
    let entries = map.as_map().ok_or_else(|| {
        anyhow::anyhow!(
            "HAS_KEY expects a MAP as its first argument, got {}",
            map.type_name()
        )
    })?;
    Ok(Value::bool(entries.contains_key(&key)))
}

/// Insert one key into a map, failing on duplicates so two files sharing
/// a stem (or two targets sharing a name) fail the run instead of
/// silently shadowing each other.
#[oxdock_func(pure, returns = "MAP")]
fn map_set(map: Value, key: String, value: Value) -> Result<Value> {
    let entries = map.as_map().ok_or_else(|| {
        anyhow::anyhow!(
            "MAP_SET expects a MAP as its first argument, got {}",
            map.type_name()
        )
    })?;
    if entries.contains_key(&key) {
        bail!("MAP_SET duplicate key '{key}'; placeholders must resolve to exactly one entry");
    }
    let mut next = entries.clone();
    next.insert(key, value);
    Ok(Value::map(next))
}

/// Encode a script value as JSON with one trailing newline, so the DSL
/// can write manifests and values files byte-identical to the previous
/// Rust serializers. Only template-safe shapes survive; anything else
/// fails here instead of rendering as a silent empty.
#[oxdock_func(pure, returns = "STRING")]
fn to_json(value: Value) -> Result<Value> {
    let json = value_to_json(&value)?;
    let mut out = serde_json::to_string(&json).context("encode JSON")?;
    out.push('\n');
    Ok(Value::string(out))
}

/// Expand one fragment with the stock streaming expander and the exact
/// scope a per-file pipeline `EXPAND` sees (global values, target
/// values, `CRATE_VERSION` and nothing else), then drop one trailing
/// newline: the placeholder line in the master template supplies the
/// line structure back, which keeps master assembly byte-identical to
/// plain concatenation.
#[oxdock_func(pure, returns = "STRING")]
fn expand_fragment(raw: String, global: Value, ctx: Value, version: String) -> Result<Value> {
    let global_map = global.as_map().ok_or_else(|| {
        anyhow::anyhow!(
            "EXPAND_FRAGMENT expects a MAP as its global argument, got {}",
            global.type_name()
        )
    })?;
    let ctx_map = ctx.as_map().ok_or_else(|| {
        anyhow::anyhow!(
            "EXPAND_FRAGMENT expects a MAP as its ctx argument, got {}",
            ctx.type_name()
        )
    })?;
    let mut env = HashMap::new();
    env.insert("CRATE_VERSION".to_string(), version);
    let mut vars = HashMap::new();
    vars.insert("docs_global".to_string(), Value::map(global_map.clone()));
    vars.insert("docs_ctx".to_string(), Value::map(ctx_map.clone()));
    let mut expander = oxdock_process::StreamingExpand::new(&[], &env).with_vars(&vars);
    let mut out = Vec::with_capacity(raw.len());
    expander
        .process_bytes(raw.as_bytes(), &mut out)
        .context("expand fragment")?;
    expander.flush(&mut out).context("expand fragment")?;
    let mut text = String::from_utf8(out).context("expand fragment produced non-UTF-8")?;
    if text.ends_with('\n') {
        text.pop();
    }
    Ok(Value::string(text))
}

/// Render one member values file: manifest name and description with the
/// committed `name` override winning, stable key order, serde escaping,
/// and one trailing newline. `existing` may be an empty map when no
/// values file exists yet; a missing `name` there keeps the manifest
/// name instead of failing.
#[oxdock_func(pure, returns = "STRING")]
fn package_values_json(pkg: Value, existing: Value) -> Result<Value> {
    let pkg_map = pkg.as_map().ok_or_else(|| {
        anyhow::anyhow!(
            "PACKAGE_VALUES_JSON expects a MAP as its pkg argument, got {}",
            pkg.type_name()
        )
    })?;
    let name = pkg_map
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("PACKAGE_VALUES_JSON pkg misses name"))?
        .to_string();
    let description = pkg_map
        .get("description")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("PACKAGE_VALUES_JSON pkg misses description"))?
        .to_string();
    let existing_map = existing.as_map().ok_or_else(|| {
        anyhow::anyhow!(
            "PACKAGE_VALUES_JSON expects a MAP as its existing argument, got {}",
            existing.type_name()
        )
    })?;
    let mut final_name = name;
    if let Some(override_name) = existing_map.get("name").and_then(|v| v.as_str()) {
        final_name = override_name.to_string();
    }
    let merged = [("name", final_name), ("description", description)];
    let mut out = String::from("{");
    for (idx, (key, value)) in merged.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        out.push_str(&serde_json::to_string(key).context("encode values")?);
        out.push_str(": ");
        out.push_str(&serde_json::to_string(value).context("encode values")?);
    }
    out.push_str("}\n");
    Ok(Value::string(out))
}

/// Generated command index table from parser metadata, so docs can never
/// list a removed command.
#[oxdock_func(pure, returns = "STRING")]
fn command_index() -> Result<Value> {
    Ok(Value::string(crate::oxdock::command_ref::render_index()))
}

/// Generated command body from parser metadata plus value types.
#[oxdock_func(pure, returns = "STRING")]
fn command_body() -> Result<Value> {
    Ok(Value::string(crate::oxdock::command_ref::render_body()?))
}

/// Generated function reference from the `#[oxdock_func]` registry.
#[oxdock_func(pure, returns = "STRING")]
fn function_reference() -> Result<Value> {
    Ok(Value::string(
        crate::oxdock::command_ref::render_function_reference(),
    ))
}

/// Every docs-gen host registration for the render engine.
pub fn registrations<P: ProcessManager>() -> Vec<HostRegistration<P>> {
    vec![
        WorkspaceMembers::registration(),
        WorkspaceVersion::registration(),
        CargoPackage::registration(),
        FileStem::registration(),
        HasKey::registration(),
        MapSet::registration(),
        ToJson::registration(),
        ExpandFragment::registration(),
        PackageValuesJson::registration(),
        CommandIndex::registration(),
        CommandBody::registration(),
        FunctionReference::registration(),
    ]
}

/// Script values to JSON. Maps stay sorted (the word holds a BTreeMap);
/// only template-safe shapes survive.
fn value_to_json(value: &Value) -> Result<serde_json::Value> {
    if let Some(map) = value.as_map() {
        return map
            .iter()
            .map(|(key, item)| value_to_json(item).map(|v| (key.clone(), v)))
            .collect::<Result<serde_json::Map<_, _>>>()
            .map(serde_json::Value::Object);
    }
    if let Some(list) = value.as_list() {
        return list
            .iter()
            .map(value_to_json)
            .collect::<Result<Vec<_>>>()
            .map(serde_json::Value::Array);
    }
    if let Some(s) = value.as_str() {
        return Ok(serde_json::Value::String(s.to_string()));
    }
    if let Some(i) = value.as_i64() {
        return Ok(serde_json::Value::Number(serde_json::Number::from(i)));
    }
    if let Some(f) = value.as_f64() {
        let number = serde_json::Number::from_f64(f)
            .ok_or_else(|| anyhow::anyhow!("TO_JSON cannot encode non-finite float {f}"))?;
        return Ok(serde_json::Value::Number(number));
    }
    if let Some(b) = value.as_bool() {
        return Ok(serde_json::Value::Bool(b));
    }
    bail!(
        "TO_JSON cannot encode {} values; use STRING, INT, FLOAT, BOOL, LIST, or MAP",
        value.type_name()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn stem_strips_compound_extensions() {
        let stem = |rel: &str| {
            file_stem(rel.to_string())
                .expect("stem")
                .as_str()
                .expect("string")
                .to_string()
        };
        assert_eq!(stem("a/b/intro.md.tmpl"), "intro");
        assert_eq!(stem("x/y.md"), "y");
        assert_eq!(stem("header.md.tmpl"), "header");
        assert_eq!(stem("a/quick-start.md"), "quick-start");
    }

    #[test]
    fn stem_rejects_placeholder_hostile_names() {
        assert!(file_stem("a/usage-(build.rs).md".to_string()).is_err());
        assert!(file_stem("a/.md".to_string()).is_err());
    }

    #[test]
    fn map_set_inserts_and_rejects_duplicates() {
        let empty = Value::map(BTreeMap::new());
        let one = map_set(empty, "a".to_string(), Value::string("x".to_string())).expect("insert");
        assert_eq!(
            one.as_map().expect("map").get("a").expect("key").as_str(),
            Some("x")
        );
        assert!(map_set(one, "a".to_string(), Value::string("y".to_string())).is_err());
        assert!(
            map_set(
                Value::string("nope".to_string()),
                "a".to_string(),
                Value::int(1)
            )
            .is_err()
        );
    }

    #[test]
    fn has_key_reports_presence() {
        let mary = map(&[("name", Value::string("mary".to_string()))]);
        assert!(
            has_key(mary.clone(), "name".to_string())
                .expect("has")
                .as_bool()
                == Some(true)
        );
        assert!(has_key(mary, "globs".to_string()).expect("has").as_bool() == Some(false));
    }

    #[test]
    fn to_json_sorts_keys_and_appends_newline() {
        let value = map(&[
            ("b", Value::int(2)),
            (
                "a",
                Value::list(vec![Value::string("x".to_string()), Value::bool(true)]),
            ),
        ]);
        let encoded = to_json(value)
            .expect("encode")
            .as_str()
            .expect("string")
            .to_string();
        assert_eq!(encoded, "{\"a\":[\"x\",true],\"b\":2}\n");
    }

    #[test]
    fn to_json_rejects_unsupported_words() {
        assert!(to_json(Value::handle(1)).is_err());
    }

    #[test]
    fn fragment_expansion_mirrors_pipeline_scope() {
        let global = map(&[("workspace", Value::string("OxDock".to_string()))]);
        let ctx = map(&[("name", Value::string("oxdock".to_string()))]);
        let out = expand_fragment(
            "# {{ $docs_ctx.name }} {{ $docs_global.workspace }} {{ env:CRATE_VERSION }}\n"
                .to_string(),
            global,
            ctx,
            "1.2.3".to_string(),
        )
        .expect("expand");
        assert_eq!(out.as_str().expect("string"), "# oxdock OxDock 1.2.3");
    }

    #[test]
    fn fragment_expansion_keeps_doc_examples_literal() {
        let empty = Value::map(BTreeMap::new());
        let out = expand_fragment(
            "write `\\{{ $var }}` here\n".to_string(),
            empty.clone(),
            empty,
            "1.2.3".to_string(),
        )
        .expect("expand");
        assert_eq!(out.as_str().expect("string"), "write `{{ $var }}` here");
    }

    #[test]
    fn package_values_merge_keeps_committed_name() {
        let pkg = map(&[
            ("name", Value::string("demo-pkg".to_string())),
            (
                "description",
                Value::string("second description".to_string()),
            ),
        ]);
        let existing = map(&[
            ("name", Value::string("Display".to_string())),
            ("description", Value::string("stale copy".to_string())),
        ]);
        let out = package_values_json(pkg, existing)
            .expect("encode")
            .as_str()
            .expect("string")
            .to_string();
        assert!(
            out.contains("\"description\": \"second description\""),
            "manifest description must flow through, got: {out}"
        );
        assert!(
            out.contains("\"name\": \"Display\""),
            "committed name override must win, got: {out}"
        );
    }

    #[test]
    fn package_values_without_existing_uses_manifest() {
        let pkg = map(&[
            ("name", Value::string("demo-pkg".to_string())),
            ("description", Value::string("fresh".to_string())),
        ]);
        let out = package_values_json(pkg, Value::map(BTreeMap::new()))
            .expect("encode")
            .as_str()
            .expect("string")
            .to_string();
        assert_eq!(
            out,
            "{\"name\": \"demo-pkg\", \"description\": \"fresh\"}\n"
        );
    }

    #[test]
    fn package_values_escapes_quotes() {
        let pkg = map(&[
            ("name", Value::string("demo".to_string())),
            ("description", Value::string("says \"hi\"".to_string())),
        ]);
        let out = package_values_json(pkg, Value::map(BTreeMap::new()))
            .expect("encode")
            .as_str()
            .expect("string")
            .to_string();
        assert_eq!(
            out,
            "{\"name\": \"demo\", \"description\": \"says \\\"hi\\\"\"}\n"
        );
    }

    #[test]
    fn renderers_stay_non_empty() {
        assert!(
            command_index()
                .expect("index")
                .as_str()
                .expect("string")
                .contains("## Command Reference")
        );
        assert!(
            function_reference()
                .expect("functions")
                .as_str()
                .expect("string")
                .contains("## Functions")
        );
    }
}
