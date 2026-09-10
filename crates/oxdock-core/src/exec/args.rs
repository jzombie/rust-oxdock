use anyhow::{Result, bail};
use oxdock_parser::{Arg, ArgPart, CompareOp, Expr, LogicalOp, TypeKind, Value};
use oxdock_process::ProcessManager;

use super::state::ExecState;
use super::steps::StepCtx;

/// Coerce a runtime Value into a declared TypeKind. Single coercion point.
/// Pipe targets validate against the live PipeRegistry via ExecState.
pub(crate) fn coerce_value<P: ProcessManager>(
    value: Value,
    expected: TypeKind,
    state: &ExecState<P>,
) -> Result<Value> {
    let expected_label = expected.label();
    match (value, expected) {
        (v @ Value::String(_), TypeKind::String) => Ok(v),
        (v @ Value::Int(_), TypeKind::Int) => Ok(v),
        (v @ Value::Float(_), TypeKind::Float) => Ok(v),
        (v @ Value::Bool(_), TypeKind::Bool) => Ok(v),
        (Value::Pipe(n), TypeKind::Pipe) => {
            if state.io.pipe_exists(&n) {
                Ok(Value::Pipe(n))
            } else {
                Err(anyhow::anyhow!(
                    "TypeMismatch: expected {}, got unregistered pipe ({n:?})",
                    expected_label,
                ))
            }
        }
        (v @ Value::List(_), TypeKind::List) => Ok(v),
        (v @ Value::Map(_), TypeKind::Map) => Ok(v),
        (v @ Value::TaskHandle(_), TypeKind::Handle) => Ok(v),
        (v @ Value::Duration(_), TypeKind::Duration) => Ok(v),
        (v @ Value::Path(_), TypeKind::Path) => Ok(v),
        (Value::String(s), TypeKind::Int) => {
            s.trim().parse::<i64>().map(Value::Int).map_err(|_| {
                anyhow::anyhow!("TypeMismatch: expected {expected_label}, got STRING ({s:?})")
            })
        }
        (Value::String(s), TypeKind::Float) => {
            s.trim().parse::<f64>().map(Value::Float).map_err(|_| {
                anyhow::anyhow!("TypeMismatch: expected {expected_label}, got STRING ({s:?})")
            })
        }
        (Value::String(s), TypeKind::Bool) => match s.trim() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(anyhow::anyhow!(
                "TypeMismatch: expected {expected_label}, got STRING ({s:?})"
            )),
        },
        (Value::String(s), TypeKind::Pipe) => {
            let name = s.trim().to_string();
            if state.io.pipe_exists(&name) {
                Ok(Value::Pipe(name))
            } else {
                Err(anyhow::anyhow!(
                    "TypeMismatch: expected {}, got unregistered pipe ({name:?})",
                    expected_label,
                ))
            }
        }
        (Value::String(s), TypeKind::Duration) => oxdock_parser::command::parse_duration(s.trim())
            .map(Value::Duration)
            .map_err(|_| {
                anyhow::anyhow!("TypeMismatch: expected {expected_label}, got STRING ({s:?})")
            }),
        (Value::String(s), TypeKind::Path) => {
            // Narrow exception: materializing the PATH payload. Guard checks
            // still run through oxdock-fs at use time.
            #[allow(clippy::disallowed_types)]
            let path = std::path::PathBuf::from(s.trim());
            Ok(Value::Path(path))
        }
        (Value::String(s), TypeKind::List) => Err(anyhow::anyhow!(
            "TypeMismatch: expected {expected_label}, got STRING ({s:?})"
        )),
        (Value::String(s), TypeKind::Map) => Err(anyhow::anyhow!(
            "TypeMismatch: expected {expected_label}, got STRING ({s:?})"
        )),
        (Value::String(s), TypeKind::Handle) => Err(anyhow::anyhow!(
            "TypeMismatch: expected {expected_label}, got STRING ({s:?})"
        )),
        (Value::Int(n), TypeKind::String) => Ok(Value::String(n.to_string())),
        (Value::Float(f), TypeKind::String) => Ok(Value::String(f.to_string())),
        (Value::Bool(b), TypeKind::String) => Ok(Value::String(b.to_string())),
        (Value::Int(n), TypeKind::Float) => Ok(Value::Float(n as f64)),
        (Value::Float(f), TypeKind::Int) => {
            if f.fract() == 0.0 && f.is_finite() {
                Ok(Value::Int(f as i64))
            } else {
                Err(anyhow::anyhow!(
                    "TypeMismatch: expected {expected_label}, got FLOAT ({f:?})"
                ))
            }
        }
        (Value::Duration(d), TypeKind::String) => {
            Ok(Value::String(oxdock_parser::command::format_duration(&d)))
        }
        (Value::Path(p), TypeKind::String) => Ok(Value::String(p.to_string_lossy().to_string())),
        (Value::Pipe(n), TypeKind::String) => Ok(Value::String(n)),
        (v, _) => Err(anyhow::anyhow!(
            "TypeMismatch: expected {expected_label}, got value ({v:?})"
        )),
    }
}

/// Resolve an [`Arg`] using an [`ExecState`] directly (no [`StepCtx`] needed).
/// Handles `Arg::String` and all-`Text` `Arg::Parts` — `Arg::Expr` requires a
/// `StepCtx` and must go through `resolve_arg`.
pub(crate) fn resolve_arg_state<P: ProcessManager>(
    arg: &Arg,
    state: &ExecState<P>,
) -> Result<String> {
    match arg {
        Arg::String(s, _) => {
            let ctx = state.command_ctx()?;
            Ok(expand_string(s, ctx.envs(), state)?)
        }
        Arg::Expr(e) => bail!("Arg::Expr cannot be resolved without StepCtx: {:?}", e),
        Arg::Parts(parts) => {
            let ctx = state.command_ctx()?;
            let mut out = String::new();
            for part in parts {
                match part {
                    ArgPart::Text(s, _) => {
                        out.push_str(&expand_string(s, ctx.envs(), state)?);
                    }
                    ArgPart::Expr(e) => {
                        bail!("Arg::Expr cannot be resolved without StepCtx: {:?}", e)
                    }
                }
            }
            Ok(out)
        }
    }
}

/// Resolve an [`Arg`] — handles all variants.
pub(crate) fn resolve_arg<P: ProcessManager>(arg: &Arg, cx: &mut StepCtx<'_, P>) -> Result<String> {
    match arg {
        Arg::String(s, _) => Ok(expand_string(s, &cx.state.envs, cx.state)?),
        Arg::Expr(e) => {
            let val = evaluate_expr(e, cx)?;
            Ok(format_value_for_string(&val))
        }
        Arg::Parts(parts) => {
            let mut out = String::new();
            for part in parts {
                match part {
                    ArgPart::Text(s, _) => {
                        out.push_str(&expand_string(s, &cx.state.envs, cx.state)?);
                    }
                    ArgPart::Expr(e) => {
                        let val = evaluate_expr(e, cx)?;
                        out.push_str(&format_value_for_string(&val));
                    }
                }
            }
            Ok(out)
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

/// Resolve an [`Arg`] to an integer, enforcing the declared `int` type
/// on the interpolated value. Dynamics (`$var`, templates) validate
/// here — lower time only sees their unevaluated form.
pub(crate) fn resolve_arg_as_int<P: ProcessManager>(
    arg: &Arg,
    cx: &mut StepCtx<'_, P>,
) -> Result<i32> {
    let resolved = resolve_arg(arg, cx)?;
    resolved
        .parse::<i32>()
        .map_err(|_| anyhow::anyhow!("expected int, got {resolved:?}"))
}

/// Resolve an [`Arg`] to a duration, enforcing the declared `duration`
/// type on the interpolated value. Same lower/runtime split as ints.
pub(crate) fn resolve_arg_as_duration<P: ProcessManager>(
    arg: &Arg,
    cx: &mut StepCtx<'_, P>,
) -> Result<std::time::Duration> {
    let resolved = resolve_arg(arg, cx)?;
    oxdock_parser::command::parse_duration(&resolved)
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
            .ok_or_else(|| anyhow::anyhow!("undefined variable ${name}")),
        Expr::Env(key) => match cx.state.envs.get(key) {
            Some(v) => Ok(Value::String(v.clone())),
            None => anyhow::bail!("undefined environment variable `env:{key}`"),
        },
        Expr::KeyPath { base, keys } => {
            let mut current = cx
                .state
                .get_var(base)
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
                    Value::String(_)
                    | Value::Bool(_)
                    | Value::Int(_)
                    | Value::Float(_)
                    | Value::Pipe(_)
                    | Value::Duration(_)
                    | Value::Path(_)
                    | Value::TaskHandle(_) => {
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
        Expr::Map(entries) => {
            let mut result = std::collections::BTreeMap::new();
            for (key, val_expr) in entries {
                let val = evaluate_expr(val_expr, cx)?;
                result.insert(key.clone(), val);
            }
            Ok(Value::Map(result))
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
        Expr::Not(inner) => Ok(Value::Bool(!is_truthy(&evaluate_expr(inner, cx)?)?)),
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

    // Up-front sandbox gate (mirrors `GuardedPath::glob_paths`): patterns are
    // sandbox-root-relative, so any `..` component escapes. Return empty
    // without traversing — the same empty-on-no-match GLOB semantics.
    if raw_pattern
        .replace('\\', "/")
        .split('/')
        .any(|seg| seg == "..")
    {
        return Ok(Value::List(Vec::new()));
    }

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
                Value::Float(f)
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
                        // Bare $var never reads the environment; use {{ env:KEY }}.
                        let mut parts = var_expr.split('.');
                        if let Some(base_var) = parts.next() {
                            let base_trim = base_var.trim();
                            let mut current = state.get_var(base_trim);
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
                        // {{ env:KEY }} — look up in env (script + process)
                        // {{ script_env:KEY }} — explicit script env
                        // {{ bare_key }} — DSL variable only, NOT env
                        if let Some(env_key) = key
                            .strip_prefix("env:")
                            .or_else(|| key.strip_prefix("script_env:"))
                        {
                            if let Some(val) = env.get(env_key) {
                                output.push_str(val);
                            }
                            // Missing → emit empty
                        } else if let Some(val) = state.get_var(key) {
                            output.push_str(&format_value_for_string(&val));
                        }
                        // Bare key not in DSL vars → emit empty
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
/// Two escape hatches pass text through to the shell untouched:
/// `\$` emits a literal `$` (backslash consumed, no expansion), and `$`
/// inside a `{{ ... }}` span is never expanded (such spans are literal by
/// construction — real templates were already interpolated upstream, and
/// `\{{` escapes arrive here with their braces intact).
/// Note: `\\$var` (literal backslash plus interpolation) is indistinguishable
/// from `\$var` at this stage (`expand_string` already collapsed `\\`), so it
/// also yields a literal `$var`; prefer `{{ $var }}`-adjacent forms when a
/// literal backslash must precede an interpolated value.
pub(crate) fn expand_dsl_vars<P: ProcessManager>(input: &str, state: &ExecState<P>) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'$') {
            output.push('$');
            chars.next();
            continue;
        }
        if c == '{' && chars.peek() == Some(&'{') {
            output.push('{');
            output.push(chars.next().unwrap_or('{'));
            while let Some(ch) = chars.next() {
                output.push(ch);
                if ch == '}' && chars.peek() == Some(&'}') {
                    output.push(chars.next().unwrap_or('}'));
                    break;
                }
            }
            continue;
        }
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
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Pipe(n) => format!("pipe:{n}"),
        Value::Duration(d) => oxdock_parser::command::format_duration(d),
        Value::Path(p) => p.to_string_lossy().to_string(),
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
        Value::TaskHandle(id) => format!("task#{}", id),
    }
}
