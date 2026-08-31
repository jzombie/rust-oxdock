use anyhow::{Result, bail};
use oxdock_parser::{Arg, CompareOp, Expr, LogicalOp, Value};
use oxdock_process::ProcessManager;

use super::state::ExecState;
use super::steps::StepCtx;

/// Resolve an [`Arg`] using an [`ExecState`] directly (no [`StepCtx`] needed).
///
/// * `Arg::Literal` — returned as-is, no expansion.
/// * `Arg::Template` — `$variable` references are resolved first, then
///   `{{ env:KEY }}` template expressions are expanded via [`StreamingExpand`].
pub(crate) fn resolve_arg_state<P: ProcessManager>(
    arg: &Arg,
    state: &ExecState<P>,
) -> Result<String> {
    match arg {
        Arg::Literal(s) => Ok(s.clone()),
        Arg::Template(t) => {
            let resolved = resolve_dollar_vars(&t.0, state);
            let ctx = state.command_ctx()?;
            let expander = oxdock_process::StreamingExpand::new(&[], ctx.envs());
            expander
                .expand_string(&resolved)
                .map_err(|e| anyhow::anyhow!("failed to expand template {}: {}", t.0, e))
        }
    }
}

/// Resolve an [`Arg`] by delegating to [`resolve_arg_state`] with `cx.state`.
pub(crate) fn resolve_arg<P: ProcessManager>(arg: &Arg, cx: &StepCtx<'_, P>) -> Result<String> {
    resolve_arg_state(arg, cx.state)
}

/// Resolve an optional [`Arg`]. Returns `Ok(None)` when `None` is passed.
pub(crate) fn resolve_arg_opt<P: ProcessManager>(
    arg: &Option<Arg>,
    cx: &StepCtx<'_, P>,
) -> Result<Option<String>> {
    match arg {
        Some(a) => resolve_arg(a, cx).map(Some),
        None => Ok(None),
    }
}

/// Resolve a list of `(name, Arg)` override pairs.
pub(crate) fn resolve_overrides<P: ProcessManager>(
    overrides: &[(String, Arg)],
    cx: &StepCtx<'_, P>,
) -> Result<Vec<(String, String)>> {
    overrides
        .iter()
        .map(|(k, v)| resolve_arg(v, cx).map(|val| (k.clone(), val)))
        .collect()
}

/// Evaluate an [`Expr`] to a [`Value`].
pub(crate) fn evaluate_expr<P: ProcessManager>(
    expr: &Expr,
    cx: &mut StepCtx<'_, P>,
) -> Result<Value> {
    match expr {
        Expr::Literal(Value::String(s)) => {
            // Resolve embedded $variable references in string literals
            Ok(Value::String(resolve_dollar_vars(s, cx.state)))
        }
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Var(name) => cx
            .state
            .get_var(name)
            .or_else(|| cx.state.envs.get(name).map(|v| Value::String(v.clone())))
            .ok_or_else(|| anyhow::anyhow!("undefined variable ${name}")),
        Expr::KeyPath { base, keys } => {
            let mut current = cx
                .state
                .get_var(base)
                .or_else(|| cx.state.envs.get(base).map(|v| Value::String(v.clone())))
                .ok_or_else(|| anyhow::anyhow!("undefined variable ${base}"))?;
            for key in keys {
                match current {
                    Value::Map(map) => {
                        current = map
                            .get(key)
                            .cloned()
                            .ok_or_else(|| anyhow::anyhow!("Key '{}' not found in map", key))?;
                    }
                    Value::List(list) => {
                        let idx: usize = key
                            .parse()
                            .map_err(|_| anyhow::anyhow!("Invalid array index '{}'", key))?;
                        current = list
                            .get(idx)
                            .cloned()
                            .ok_or_else(|| anyhow::anyhow!("Index {} out of bounds", idx))?;
                    }
                    Value::String(_) | Value::Bool(_) => {
                        bail!("Cannot traverse into scalar value at key '{}'", key);
                    }
                }
            }
            Ok(current)
        }
        Expr::List(items) => {
            let mut result = Vec::new();
            for item in items {
                result.push(evaluate_expr(item, cx)?);
            }
            Ok(Value::List(result))
        }
        Expr::Call { name, args } => match name.as_str() {
            "GLOB" => evaluate_glob(args, cx),
            "LOAD_TOML" => evaluate_load_toml(args, cx),
            "LOAD_JSON" => evaluate_load_json(args, cx),
            _ => bail!("unknown function {name}"),
        },
        Expr::Compare { op, left, right } => {
            let lv = evaluate_expr(left, cx)?;
            let rv = evaluate_expr(right, cx)?;
            let ls = format_value_for_string(&lv);
            let rs = format_value_for_string(&rv);
            let result = match op {
                CompareOp::Eq => ls == rs,
                CompareOp::Ne => ls != rs,
            };
            Ok(Value::Bool(result))
        }
        Expr::Logical { op, left, right } => {
            let left_val = evaluate_expr(left, cx)?;
            let left_truthy = is_truthy(&left_val)?;
            match op {
                LogicalOp::Or => {
                    if left_truthy {
                        Ok(Value::Bool(true))
                    } else {
                        Ok(Value::Bool(is_truthy(&evaluate_expr(right, cx)?)?))
                    }
                }
                LogicalOp::And => {
                    if !left_truthy {
                        Ok(Value::Bool(false))
                    } else {
                        Ok(Value::Bool(is_truthy(&evaluate_expr(right, cx)?)?))
                    }
                }
            }
        }
    }
}

/// Check if a value is truthy. Only `Value::Bool` is accepted; all other types produce a TypeError.
pub(crate) fn is_truthy(val: &Value) -> Result<bool> {
    match val {
        Value::Bool(b) => Ok(*b),
        other => bail!("Type Error: condition must be a Bool, found {:?}", other),
    }
}

/// Evaluate a `GLOB()` function call.
fn evaluate_glob<P: ProcessManager>(args: &[Expr], cx: &mut StepCtx<'_, P>) -> Result<Value> {
    if args.is_empty() {
        bail!("GLOB requires a pattern argument");
    }

    let pattern_val = evaluate_expr(&args[0], cx)?;
    let raw_pattern = match pattern_val {
        Value::String(s) => s,
        _ => bail!("GLOB pattern argument must evaluate to a string"),
    };

    let root = cx.state.fs.root().clone();
    let root_path = root.as_path().to_path_buf();
    let mut entries: Vec<Value> = root
        .glob_paths(&raw_pattern)?
        .into_iter()
        .filter_map(|p| {
            p.strip_prefix(&root_path)
                .ok()
                .map(|rel| Value::String(rel.to_string_lossy().replace('\\', "/")))
        })
        .collect();

    entries.sort_by(|a, b| format!("{}", a).cmp(&format!("{}", b)));
    Ok(Value::List(entries))
}

/// Evaluate a `LOAD_TOML()` function call.
fn evaluate_load_toml<P: ProcessManager>(args: &[Expr], cx: &mut StepCtx<'_, P>) -> Result<Value> {
    if args.is_empty() {
        bail!("LOAD_TOML requires a path argument");
    }
    let path_val = evaluate_expr(&args[0], cx)?;
    let path_str = match path_val {
        Value::String(s) => s,
        _ => bail!("LOAD_TOML path argument must evaluate to a string"),
    };
    let target = cx
        .state
        .fs
        .resolve_read(&cx.state.cwd, &path_str)
        .map_err(|e| anyhow::anyhow!("failed to resolve TOML path '{}': {}", path_str, e))?;
    let content = cx
        .state
        .fs
        .read_file(&target)
        .map_err(|e| anyhow::anyhow!("failed to read TOML file '{}': {}", path_str, e))?;
    let content_str = std::str::from_utf8(&content)
        .map_err(|e| anyhow::anyhow!("invalid UTF-8 in TOML file '{}': {}", path_str, e))?;
    load_toml_value(content_str)
}

/// Evaluate a `LOAD_JSON()` function call.
fn evaluate_load_json<P: ProcessManager>(args: &[Expr], cx: &mut StepCtx<'_, P>) -> Result<Value> {
    if args.is_empty() {
        bail!("LOAD_JSON requires a path argument");
    }
    let path_val = evaluate_expr(&args[0], cx)?;
    let path_str = match path_val {
        Value::String(s) => s,
        _ => bail!("LOAD_JSON path argument must evaluate to a string"),
    };
    let target = cx
        .state
        .fs
        .resolve_read(&cx.state.cwd, &path_str)
        .map_err(|e| anyhow::anyhow!("failed to resolve JSON path '{}': {}", path_str, e))?;
    let content = cx
        .state
        .fs
        .read_file(&target)
        .map_err(|e| anyhow::anyhow!("failed to read JSON file '{}': {}", path_str, e))?;
    let content_str = std::str::from_utf8(&content)
        .map_err(|e| anyhow::anyhow!("invalid UTF-8 in JSON file '{}': {}", path_str, e))?;
    load_json_value(content_str)
}

/// Parse TOML content into a DSL `Value`.
pub fn load_toml_value(content: &str) -> Result<Value> {
    let json_val: serde_json::Value =
        toml::from_str(content).map_err(|e| anyhow::anyhow!("TOML parse error: {}", e))?;
    Ok(json_to_value(json_val))
}

/// Parse JSON content into a DSL `Value`.
pub fn load_json_value(content: &str) -> Result<Value> {
    let json_val: serde_json::Value =
        serde_json::from_str(content).map_err(|e| anyhow::anyhow!("JSON parse error: {}", e))?;
    Ok(json_to_value(json_val))
}

/// Convert a `serde_json::Value` to a DSL `Value`.
fn json_to_value(v: serde_json::Value) -> Value {
    match v {
        serde_json::Value::String(s) => Value::String(s),
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Number(n) => Value::String(n.to_string()),
        serde_json::Value::Array(arr) => Value::List(arr.into_iter().map(json_to_value).collect()),
        serde_json::Value::Object(map) => Value::Map(
            map.into_iter()
                .map(|(k, v)| (k, json_to_value(v)))
                .collect(),
        ),
        serde_json::Value::Null => Value::String(String::new()),
    }
}

/// Resolve `$variable` references in `input`, replacing each with its value
/// from the current scope chain. Supports key-path traversal (`$pkg.name`)
/// and falls back to environment variables when vars are not found.
/// Unknown variables are left as-is.
pub(crate) fn resolve_dollar_vars<P: ProcessManager>(input: &str, state: &ExecState<P>) -> String {
    let mut result = String::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            // Collect base identifier
            let mut name = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_alphanumeric() || ch == '_' {
                    name.push(ch);
                    chars.next();
                } else {
                    break;
                }
            }
            if name.is_empty() {
                result.push('$');
                continue;
            }
            // Attempt to look up base variable
            let base_val = match state.get_var(&name) {
                Some(v) => v,
                None => {
                    if let Some(env_val) = state.envs.get(&name) {
                        Value::String(env_val.clone())
                    } else {
                        result.push('$');
                        result.push_str(&name);
                        continue;
                    }
                }
            };
            // Try to consume dot-separated key-path segments
            let mut current = base_val;
            let mut consumed_segments: Vec<String> = Vec::new();
            let mut lookup_failed = false;
            loop {
                if chars.peek() != Some(&'.') {
                    break;
                }
                // Peek ahead: collect the segment without consuming
                let mut temp_chars = chars.clone();
                temp_chars.next(); // consume '.'
                let mut segment = String::new();
                while let Some(&ch) = temp_chars.peek() {
                    if ch.is_alphanumeric() || ch == '_' {
                        segment.push(ch);
                        temp_chars.next();
                    } else {
                        break;
                    }
                }
                if segment.is_empty() {
                    break;
                }
                // Try to traverse into current value
                match &current {
                    Value::Map(map) => {
                        if let Some(next) = map.get(&segment) {
                            current = next.clone();
                            chars = temp_chars; // commit consumption
                            consumed_segments.push(segment);
                        } else {
                            lookup_failed = true;
                            break;
                        }
                    }
                    Value::List(list) => {
                        if let Ok(idx) = segment.parse::<usize>() {
                            if let Some(next) = list.get(idx) {
                                current = next.clone();
                                chars = temp_chars;
                                consumed_segments.push(segment);
                            } else {
                                lookup_failed = true;
                                break;
                            }
                        } else {
                            lookup_failed = true;
                            break;
                        }
                    }
                    Value::String(_) | Value::Bool(_) => {
                        break; // Scalar reached; remaining dots belong to static text
                    }
                }
            }
            if lookup_failed {
                // Emit the full original expression: $base.seg1.seg2
                result.push('$');
                result.push_str(&name);
                for seg in &consumed_segments {
                    result.push('.');
                    result.push_str(seg);
                }
            } else {
                result.push_str(&format_value_for_string(&current));
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Format a `Value` as a string for inline interpolation.
pub(crate) fn format_value_for_string(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::List(items) => items
            .iter()
            .map(format_value_for_string)
            .collect::<Vec<_>>()
            .join(" "),
        Value::Map(map) => map
            .iter()
            .map(|(k, v)| format!("\"{}\": {}", k, format_value_for_string(v)))
            .collect::<Vec<_>>()
            .join(", "),
    }
}
