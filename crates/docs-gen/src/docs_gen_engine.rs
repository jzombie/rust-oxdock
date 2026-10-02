//! Render engine builtins for document pipelines.
//!
//! The file-set assembler's own primitives: fragment keys and fragment
//! expansion. Map assembly (`MAP_SET`, `HAS_KEY`) and manifest encoding
//! (`TO_JSON`) live in STD; domain content comes from plugins (`OXDOCK`,
//! `RUST`, `MARKDOWN`, ...). A pipeline for a project outside OxDock
//! registers these plus its own domain plugins and nothing else.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use oxdock_core::{HostModule, OxDockFn, StepCtx, TypeTag, Value};
use oxdock_func_macro::oxdock_func;
use oxdock_process::ProcessManager;

/// Placeholder key for a fragment path: file name up to the first dot,
/// so `intro.md.tmpl` and `intro.md` both resolve as `intro`. Keys stay
/// `[A-Za-z0-9_-]` so every placeholder stays readable; anything else
/// fails with a rename hint instead of producing a placeholder nobody
/// can type with confidence.
#[oxdock_func(pure, returns = TypeTag::String)]
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

/// Expand one template with the stock streaming expander over a
/// caller-built scope: `vars` supplies every `$name` placeholder and
/// `env` supplies every `{{ env:KEY }}` one, so no variable name is
/// hardcoded here and each pipeline maps its own conventions. Then drop
/// one trailing newline: the placeholder line in the master template
/// supplies the line structure back, which keeps master assembly
/// byte-identical to plain concatenation. The scope assembly plus the
/// newline strip are this wrapper's whole job over core `EXPAND`.
/// Placeholder calls dispatch through the live registry snapshot, the
/// same table generic `EXPAND` uses: no module or function name is
/// hardcoded here, so `{{ MARKDOWN::MAP_TO_MD_TABLE($var) }}` and any
/// future pure helper resolve identically in every expansion scope.
#[oxdock_func(returns = TypeTag::String)]
fn expand_template<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    raw: String,
    vars: Value,
    env: Value,
) -> Result<Value> {
    render_fragment(&raw, &vars, &env, Some(cx.pure_functions()))
}

/// Fragment expansion over an explicit scope and call table. Pure over
/// its inputs so tests drive it without engine state: pass `None` for
/// plain key path scope, or `Some` shared table to enable
/// `{{ MODULE::FUNC(args) }}` calls. Lookup is nested by module and
/// function with zero allocation; the known-functions listing builds
/// only on failure.
pub(crate) fn render_fragment(
    raw: &str,
    vars: &Value,
    env: &Value,
    calls: Option<std::sync::Arc<oxdock_core::PureTable>>,
) -> Result<Value> {
    let vars_map = vars.as_map().ok_or_else(|| {
        anyhow::anyhow!(
            "EXPAND_TEMPLATE expects a MAP as its vars argument, got {}",
            vars.type_name()
        )
    })?;
    let env_map = env.as_map().ok_or_else(|| {
        anyhow::anyhow!(
            "EXPAND_TEMPLATE expects a MAP as its env argument, got {}",
            env.type_name()
        )
    })?;
    let mut inherit = HashMap::new();
    for (key, value) in env_map.iter() {
        let text = value.as_str().ok_or_else(|| {
            anyhow::anyhow!(
                "EXPAND_TEMPLATE env key '{key}' must be a STRING, got {}",
                value.type_name()
            )
        })?;
        inherit.insert(key.clone(), text.to_string());
    }
    let mut scope = HashMap::new();
    for (key, value) in vars_map.iter() {
        scope.insert(key.clone(), value.clone());
    }
    let mut expander = oxdock_process::StreamingExpand::new(&[], &inherit).with_vars(&scope);
    if let Some(table) = calls {
        let resolver: oxdock_process::PlaceholderCall = Arc::new(move |module, func, args| {
            let callee = table
                .get(module)
                .and_then(|entries| entries.get(func))
                .ok_or_else(|| {
                    let mut known: Vec<String> = table
                        .iter()
                        .flat_map(|(m, entries)| entries.keys().map(move |f| format!("{m}::{f}")))
                        .collect();
                    known.sort();
                    anyhow::anyhow!(
                        "unknown placeholder function '{module}::{func}'; known functions: {}",
                        known.join(", ")
                    )
                })?;
            callee(args)
        });
        expander = expander.with_call_resolver(resolver);
    }
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

// TODO: Move to STD as `merge_maps`
/// Merge loaded values files in order under one duplicate policy.
///
/// `fail_on_duplicate` (the default) fails naming the repeated key,
/// so two files claiming one placeholder fail the run instead of
/// shadowing each other. `overwrite` lets later files win, for
/// environment overlays. Anything else fails listing the two known
/// policies. Non MAP elements fail naming their position.
#[oxdock_func(pure, returns = TypeTag::Map)]
fn merge_values(maps: Value, policy: String) -> Result<Value> {
    let items = maps.as_list().ok_or_else(|| {
        anyhow::anyhow!(
            "MERGE_VALUES expects a LIST of MAPs, got {}",
            maps.type_name()
        )
    })?;
    if policy != "fail_on_duplicate" && policy != "overwrite" {
        bail!("unknown merge policy '{policy}'; known policies: fail_on_duplicate, overwrite");
    }
    let mut merged = std::collections::BTreeMap::new();
    for (index, item) in items.iter().enumerate() {
        let entries = item.as_map().ok_or_else(|| {
            anyhow::anyhow!(
                "MERGE_VALUES element {index} is {}, not MAP",
                item.type_name()
            )
        })?;
        for (key, value) in entries {
            if merged.contains_key(key) && policy == "fail_on_duplicate" {
                bail!(
                    "duplicate values key '{key}'; later files must not repeat it under fail_on_duplicate"
                );
            }
            merged.insert(key.clone(), value.clone());
        }
    }
    Ok(Value::map(merged))
}

/// The render engine builtins, registered unconditionally by
/// `crate::run` alongside whichever domain plugins a pipeline needs.
pub fn module<P: ProcessManager>() -> HostModule<P> {
    HostModule {
        name: "DOCS_GEN_ENGINE".to_string(),
        funcs: vec![
            FileStem::registration(),
            ExpandTemplate::registration(),
            MergeValues::registration(),
        ],
        types: vec![],
        record_schemas: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn merge_values_layers_in_order() {
        let base = map(&[("a", Value::string("1".to_string()))]);
        let overlay = map(&[("b", Value::string("2".to_string())), ("c", Value::int(3))]);
        let merged_value =
            merge_values(Value::list(vec![base, overlay]), "overwrite".to_string()).expect("merge");
        let merged = merged_value.as_map().expect("map");
        assert_eq!(
            merged.get("a").expect("a").as_str(),
            Some("1"),
            "base keys survive",
        );
        assert_eq!(
            merged.get("c").expect("c").as_i64(),
            Some(3),
            "overlay keys land",
        );
    }

    #[test]
    fn merge_values_fail_on_duplicate_names_the_key() {
        let first = map(&[("name", Value::string("base".to_string()))]);
        let second = map(&[("name", Value::string("overlay".to_string()))]);
        let err = merge_values(
            Value::list(vec![first, second]),
            "fail_on_duplicate".to_string(),
        )
        .expect_err("duplicate must fail");
        let text = format!("{err:#}");
        assert!(text.contains("name"), "error names the key: {text}");
        let overlay_value = merge_values(
            Value::list(vec![
                map(&[("name", Value::string("base".to_string()))]),
                map(&[("name", Value::string("overlay".to_string()))]),
            ]),
            "overwrite".to_string(),
        )
        .expect("overwrite allows it");
        let won = overlay_value.as_map().expect("map");
        assert_eq!(
            won.get("name").expect("name").as_str(),
            Some("overlay"),
            "later files win under overwrite",
        );
    }

    #[test]
    fn merge_values_rejects_bad_policy_and_shapes() {
        let err =
            merge_values(Value::list(vec![]), "merge".to_string()).expect_err("policy must fail");
        let text = format!("{err:#}");
        assert!(text.contains("merge"), "error names the policy: {text}");
        assert!(
            text.contains("fail_on_duplicate"),
            "error lists known policies: {text}"
        );
        assert!(
            merge_values(Value::string("nope".to_string()), "overwrite".to_string()).is_err(),
            "non LIST fails",
        );
        assert!(
            merge_values(Value::list(vec![Value::int(1)]), "overwrite".to_string(),).is_err(),
            "non MAP element fails",
        );
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
    fn fragment_expansion_mirrors_pipeline_scope() {
        let vars = map(&[
            (
                "docs_global",
                map(&[("workspace", Value::string("OxDock".to_string()))]),
            ),
            (
                "docs_ctx",
                map(&[("name", Value::string("oxdock".to_string()))]),
            ),
        ]);
        let env = map(&[("CRATE_VERSION", Value::string("1.2.3".to_string()))]);
        let out = render_fragment(
            "# {{ $docs_ctx.name }} {{ $docs_global.workspace }} {{ env:CRATE_VERSION }}\n",
            &vars,
            &env,
            None,
        )
        .expect("expand");
        assert_eq!(out.as_str().expect("string"), "# oxdock OxDock 1.2.3");
    }

    #[test]
    fn fragment_expansion_uses_caller_scope_keys() {
        // No variable name is hardcoded in the engine: a pipeline maps
        // whatever conventions it wants through the vars and env maps.
        let vars = map(&[("who", Value::string("ada".to_string()))]);
        let env = map(&[("RELEASE", Value::string("7".to_string()))]);
        let out =
            render_fragment("# {{ $who }} {{ env:RELEASE }}\n", &vars, &env, None).expect("expand");
        assert_eq!(out.as_str().expect("string"), "# ada 7");
    }

    #[test]
    fn fragment_expansion_rejects_non_map_scope() {
        let env = Value::map(BTreeMap::new());
        let err = render_fragment("hi\n", &Value::string("nope".to_string()), &env, None)
            .expect_err("vars must be a map");
        assert!(format!("{err:#}").contains("vars"));
        let vars = Value::map(BTreeMap::new());
        let err =
            render_fragment("hi\n", &vars, &Value::int(1), None).expect_err("env must be a map");
        assert!(format!("{err:#}").contains("env"));
    }

    #[test]
    fn fragment_expansion_rejects_non_string_env() {
        let vars = Value::map(BTreeMap::new());
        let env = map(&[("KEY", Value::int(1))]);
        let err = render_fragment("hi\n", &vars, &env, None).expect_err("env must be strings");
        assert!(format!("{err:#}").contains("KEY"));
    }

    #[test]
    fn fragment_expansion_keeps_doc_examples_literal() {
        let empty = Value::map(BTreeMap::new());
        let out =
            render_fragment("write `\\{{ $var }}` here\n", &empty, &empty, None).expect("expand");
        assert_eq!(out.as_str().expect("string"), "write `{{ $var }}` here");
    }

    /// Explicit call table for fragment tests: the qualified name below
    /// is test input data, not dispatch logic. Production passes the
    /// live registry snapshot instead of naming anything.
    fn markdown_call_table() -> std::sync::Arc<oxdock_core::PureTable> {
        let mut markdown = HashMap::new();
        markdown.insert(
            "MAP_TO_MD_TABLE".to_string(),
            Arc::new(|args: Vec<Value>| {
                if args.len() != 1 {
                    bail!("MAP_TO_MD_TABLE expects 1 argument, got {}", args.len());
                }
                let table = oxdock_markdown_plugin::markdown::map_to_table(&args[0])?;
                Ok(Value::string(table))
            }) as oxdock_core::PureFn,
        );
        let mut table = HashMap::new();
        table.insert("MARKDOWN".to_string(), markdown);
        std::sync::Arc::new(table)
    }

    #[test]
    fn fragment_placeholder_call_renders_markdown_table() {
        let vars = map(&[(
            "ctx",
            map(&[(
                "skills",
                Value::list(vec![
                    map(&[
                        ("language", Value::string("rust".to_string())),
                        ("level", Value::string("advanced".to_string())),
                    ]),
                    map(&[
                        ("language", Value::string("toml".to_string())),
                        ("note", Value::string("learning".to_string())),
                    ]),
                ]),
            )]),
        )]);
        let env = Value::map(BTreeMap::new());
        let table = markdown_call_table();
        let out = render_fragment(
            indoc::indoc! {r#"
                # Skills
                {{ MARKDOWN::MAP_TO_MD_TABLE($ctx.skills) }}
            "#},
            &vars,
            &env,
            Some(table),
        )
        .expect("expand");
        assert_eq!(
            out.as_str().expect("string"),
            indoc::indoc! {r#"
                # Skills
                | language | level    | note     |
                | -------- | -------- | -------- |
                | rust     | advanced |          |
                | toml     |          | learning |
            "#}
        );
    }

    #[test]
    fn fragment_placeholder_call_rejects_unknown_functions() {
        let empty = Value::map(BTreeMap::new());
        let table = markdown_call_table();
        let err = render_fragment("{{ NOPE::MISSING(\"hi\") }}", &empty, &empty, Some(table))
            .expect_err("unknown must fail");
        assert!(format!("{err:#}").contains("NOPE::MISSING"));
    }
}
