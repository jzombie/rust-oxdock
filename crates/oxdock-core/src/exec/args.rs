use anyhow::{Result, bail};
use oxdock_parser::{Arg, Expr, Value};
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
            .ok_or_else(|| anyhow::anyhow!("undefined variable ${name}")),
        Expr::List(items) => {
            let mut result = Vec::new();
            for item in items {
                match evaluate_expr(item, cx)? {
                    Value::String(s) => result.push(s),
                    other => bail!("list items must evaluate to strings, got: {:?}", other),
                }
            }
            Ok(Value::List(result))
        }
        Expr::Call { name, args } => match name.as_str() {
            "GLOB" => evaluate_glob(args, cx),
            _ => bail!("unknown function {name}"),
        },
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
    let mut entries: Vec<String> = root
        .glob_paths(&raw_pattern)?
        .into_iter()
        .filter_map(|p| {
            p.strip_prefix(&root_path)
                .ok()
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        })
        .collect();

    entries.sort();
    Ok(Value::List(entries))
}

/// Resolve `$variable` references in `input`, replacing each with its value
/// from the current scope chain. Unknown variables are left as-is.
pub(crate) fn resolve_dollar_vars<P: ProcessManager>(
    input: &str,
    state: &ExecState<P>,
) -> String {
    let mut result = String::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            let mut name = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_alphanumeric() || ch == '_' {
                    name.push(ch);
                    chars.next();
                } else {
                    break;
                }
            }
            // Lookup uses clean name (without '$' prefix)
            if let Some(value) = state.get_var(&name) {
                match value {
                    Value::String(s) => result.push_str(&s),
                    Value::List(items) => result.push_str(&items.join(" ")),
                    Value::Bool(b) => result.push_str(&b.to_string()),
                }
            } else {
                result.push('$');
                result.push_str(&name);
            }
        } else {
            result.push(c);
        }
    }
    result
}
