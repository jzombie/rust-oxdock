use anyhow::{Result, bail};
use oxdock_parser::{Arg, CompareOp, Expr, LogicalOp, Value};
use oxdock_process::ProcessManager;

use super::state::ExecState;
use super::steps::StepCtx;

/// Resolve an [`Arg`] using an [`ExecState`] directly (no [`StepCtx`] needed).
/// Only handles `Arg::String` — `Arg::Expr` requires a `StepCtx` and must go through `resolve_arg`.
pub(crate) fn resolve_arg_state<P: ProcessManager>(
    arg: &Arg,
    state: &ExecState<P>,
) -> Result<String> {
    match arg {
        Arg::String(s) => {
            let ctx = state.command_ctx()?;
            Ok(expand_string(s, ctx.envs(), state)?)
        }
        Arg::Expr(e) => bail!("Arg::Expr cannot be resolved without StepCtx: {:?}", e),
    }
}

/// Resolve an [`Arg`] — handles all variants.
pub(crate) fn resolve_arg<P: ProcessManager>(arg: &Arg, cx: &mut StepCtx<'_, P>) -> Result<String> {
    match arg {
        Arg::String(s) => Ok(expand_string(s, &cx.state.envs, cx.state)?),
        Arg::Expr(e) => {
            let val = evaluate_expr(e, cx)?;
            Ok(format_value_for_string(&val))
        }
    }
}

/// Resolve an optional [`Arg`]. Returns `Ok(None)` when `None` is passed.
pub(crate) fn resolve_arg_opt<P: ProcessManager>(
    arg: &Option<Arg>,
    cx: &mut StepCtx<'_, P>,
) -> Result<Option<String>> {
    match arg {
        Some(a) => resolve_arg(a, cx).map(Some),
        None => Ok(None),
    }
}

/// Resolve a list of `(name, Arg)` override pairs.
pub(crate) fn resolve_overrides<P: ProcessManager>(
    overrides: &[(String, Arg)],
    cx: &mut StepCtx<'_, P>,
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
            // Expand {{ $var }} and {{ env:KEY }} in string literals
            Ok(Value::String(expand_string(s, &cx.state.envs, cx.state)?))
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
                    Value::String(_) | Value::Bool(_) | Value::Int(_) => {
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
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(f) = n.as_f64() {
                Value::String(f.to_string())
            } else {
                Value::String(n.to_string())
            }
        }
        serde_json::Value::Array(arr) => Value::List(arr.into_iter().map(json_to_value).collect()),
        serde_json::Value::Object(map) => Value::Map(
            map.into_iter()
                .map(|(k, v)| (k, json_to_value(v)))
                .collect(),
        ),
        serde_json::Value::Null => Value::String(String::new()),
    }
}

/// Single-pass string expansion: handles escapes and `{{ }}` template tags.
///
/// `{{ $var }}` — interpolates script variable (supports key-paths: `{{ $d.name.0 }}`).
/// `{{ env:KEY }}` — interpolates environment variable.
/// Bare `$` is literal text — `{{ }}` is the ONLY interpolation trigger.
///
/// Escape rules:
/// - `\\` → literal `\`
/// - `\{{` → literal `{{` (skip template expansion)
/// - `\n`, `\t`, `\r`, `\"` → control characters
/// - Unrecognized `\X` → literal `\X`
pub(crate) fn expand_string<P: ProcessManager>(
    input: &str,
    env: &std::collections::HashMap<String, String>,
    state: &ExecState<P>,
) -> Result<String> {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.peek() {
                Some(&'\\') => {
                    output.push('\\');
                    chars.next();
                }
                Some(&'{') => {
                    // Check for \{{ (literal {{)
                    let mut lookahead = chars.clone();
                    lookahead.next(); // skip first {
                    if lookahead.next() == Some('{') {
                        output.push_str("{{");
                        chars.next(); // skip first {
                        chars.next(); // skip second {
                    } else {
                        output.push('\\');
                    }
                }
                Some(&'n') => {
                    output.push('\n');
                    chars.next();
                }
                Some(&'t') => {
                    output.push('\t');
                    chars.next();
                }
                Some(&'r') => {
                    output.push('\r');
                    chars.next();
                }
                Some(&'"') => {
                    output.push('"');
                    chars.next();
                }
                _ => {
                    output.push('\\');
                }
            },
            '{' if chars.peek() == Some(&'{') => {
                chars.next(); // consume second {
                let mut template_key = String::new();
                let mut found_close = false;
                while let Some(ch) = chars.next() {
                    if ch == '}' && chars.peek() == Some(&'}') {
                        chars.next(); // consume second }
                        found_close = true;
                        break;
                    }
                    template_key.push(ch);
                }
                if found_close {
                    let key = template_key.trim();
                    if let Some(var_expr) = key.strip_prefix('$') {
                        // {{ $var }} or {{ $var.path.0 }} — look up in scope chain.
                        // Parse key-path from the extracted string, NOT from chars.
                        // Trim whitespace from segments to tolerate spaces around dots.
                        let mut parts = var_expr.split('.');
                        if let Some(base_var) = parts.next() {
                            let base_trim = base_var.trim();
                            let mut current = state.get_var(base_trim).or_else(|| {
                                env.get(base_trim)
                                    .map(|v| Value::String(v.clone()))
                            });
                            for part in parts {
                                let part_trim = part.trim();
                                current = match current {
                                    Some(Value::Map(map)) => map.get(part_trim).cloned(),
                                    Some(Value::List(list)) => part_trim
                                        .parse::<usize>()
                                        .ok()
                                        .and_then(|idx| list.get(idx).cloned()),
                                    _ => None,
                                };
                                if current.is_none() {
                                    break;
                                }
                            }
                            if let Some(v) = current {
                                output.push_str(&format_value_for_string(&v));
                            }
                            // Missing → emit empty
                        }
                    } else {
                        // {{ env:KEY }} or {{ bare_key }} — look up in env, then DSL vars
                        let env_key = key
                            .strip_prefix("env:")
                            .or_else(|| key.strip_prefix("script_env:"))
                            .unwrap_or(key);
                        if let Some(val) = env.get(env_key) {
                            output.push_str(val);
                        } else if let Some(val) = state.get_var(env_key) {
                            output.push_str(&format_value_for_string(&val));
                        }
                        // Missing → emit empty
                    }
                } else {
                    // Unclosed template — preserve verbatim
                    output.push_str("{{");
                    output.push_str(&template_key);
                }
            }
            _ => {
                output.push(c);
            }
        }
    }
    Ok(output)
}

/// Expand bare `$var` references in a string using DSL scope.
/// Used by RUN commands to expand DSL variables before passing to shell.
/// Undefined variables are left as-is (shell will handle them).
pub(crate) fn expand_dsl_vars<P: ProcessManager>(
    input: &str,
    state: &ExecState<P>,
) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            let mut var_name = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_ascii_alphanumeric() || ch == '_' {
                    var_name.push(ch);
                    chars.next();
                } else {
                    break;
                }
            }
            if var_name.is_empty() {
                output.push('$');
                continue;
            }
            if let Some(val) = state.get_var(&var_name) {
                output.push_str(&format_value_for_string(&val));
            } else {
                output.push('$');
                output.push_str(&var_name);
            }
        } else {
            output.push(c);
        }
    }
    output
}

/// Format a `Value` as a string for inline interpolation.
pub(crate) fn format_value_for_string(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Int(i) => i.to_string(),
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
