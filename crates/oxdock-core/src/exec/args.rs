use anyhow::{Result, bail};
use oxdock_fs::EntryKind;
use oxdock_parser::{Arg, ArgPart, ArithOp, CompareOp, Expr, LogicalOp, MathOp, Value};
use oxdock_process::ProcessManager;

use super::state::ExecState;
use super::steps::StepCtx;

/// Coerce a runtime [`Value`] word into a declared type name. Single
/// coercion point. Type references are plain names resolved against the
/// run's name directory here (not at parse time, where host descriptors
/// are not visible). Pipe targets validate against the live PipeRegistry
/// via ExecState.
pub(crate) fn coerce_value<P: ProcessManager>(
    value: Value,
    expected: &str,
    state: &ExecState<P>,
) -> Result<Value> {
    if !state.is_known_type(expected) {
        return Err(anyhow::anyhow!(
            "unknown type `{expected}`: no descriptor registered (expected one of {})",
            state.type_names().join(", "),
        ));
    }
    // Same-type passthrough for every type: the word carries its own
    // vtable, so descriptor-name equality is type equality. Pipe handles
    // are already owned values: passing one through never instantiates
    // backend state (materialization happens only at binding sites), and
    // `LET $q: PIPE = $p` shares the backend by cloning the handle.
    if value.type_name() == expected {
        return Ok(value);
    }
    // Values of other registered types never cross-coerce; the mismatch
    // below reports both names through the descriptor.
    match (value.as_str(), expected) {
        (Some(s), "INT") => {
            s.trim().parse::<i64>().map(Value::int).map_err(|_| {
                anyhow::anyhow!("TypeMismatch: expected {expected}, got STRING ({s:?})")
            })
        }
        (Some(s), "FLOAT") => {
            s.trim().parse::<f64>().map(Value::float).map_err(|_| {
                anyhow::anyhow!("TypeMismatch: expected {expected}, got STRING ({s:?})")
            })
        }
        (Some(s), "BOOL") => match s.trim() {
            "true" => Ok(Value::bool(true)),
            "false" => Ok(Value::bool(false)),
            _ => Err(anyhow::anyhow!(
                "TypeMismatch: expected {expected}, got STRING ({s:?})"
            )),
        },
        (Some(s), "PIPE") => {
            // Strict: plain strings never coerce to pipes, so a handle is
            // always created explicitly via `LET $p: PIPE`. Anything else
            // is a TypeMismatch.
            Err(anyhow::anyhow!(
                "TypeMismatch: expected {expected}, got STRING ({s:?}); declare LET $x: PIPE and pass $x"
            ))
        }
        (Some(s), "DURATION") => oxdock_parser::command::parse_duration(s.trim())
            .map(Value::duration)
            .map_err(|_| anyhow::anyhow!("TypeMismatch: expected {expected}, got STRING ({s:?})")),
        (Some(s), "PATH") => {
            // Narrow exception: materializing the PATH payload. Guard checks
            // still run through oxdock-fs at use time.
            #[allow(clippy::disallowed_types)]
            let path = std::path::PathBuf::from(s.trim());
            Ok(Value::path(path))
        }
        (Some(s), "LIST") => Err(anyhow::anyhow!(
            "TypeMismatch: expected {expected}, got STRING ({s:?})"
        )),
        (Some(s), "MAP") => Err(anyhow::anyhow!(
            "TypeMismatch: expected {expected}, got STRING ({s:?})"
        )),
        (Some(s), "HANDLE") => Err(anyhow::anyhow!(
            "TypeMismatch: expected {expected}, got STRING ({s:?})"
        )),
        _ => coerce_scalar(&value, expected),
    }
}

/// Scalar cross-coercions between numeric, boolean, duration, path, and
/// pipe-name words. Anything else is a `TypeMismatch`.
fn coerce_scalar(value: &Value, expected: &str) -> Result<Value> {
    if let Some(n) = value.as_i64() {
        return match expected {
            "STRING" => Ok(Value::string(n.to_string())),
            "FLOAT" => Ok(Value::float(n as f64)),
            _ => Err(mismatch(expected, value)),
        };
    }
    if let Some(f) = value.as_f64() {
        return match expected {
            "STRING" => Ok(Value::string(f.to_string())),
            "INT" if f.fract() == 0.0 && f.is_finite() => Ok(Value::int(f as i64)),
            _ => Err(mismatch(expected, value)),
        };
    }
    if let Some(b) = value.as_bool() {
        return match expected {
            "STRING" => Ok(Value::string(b.to_string())),
            _ => Err(mismatch(expected, value)),
        };
    }
    if let Some(d) = value.as_duration() {
        return match expected {
            "STRING" => Ok(Value::string(oxdock_parser::command::format_duration(&d))),
            _ => Err(mismatch(expected, value)),
        };
    }
    if let Some(p) = value.as_path() {
        return match expected {
            "STRING" => Ok(Value::string(p.to_string_lossy().to_string())),
            _ => Err(mismatch(expected, value)),
        };
    }
    if value.as_pipe_handle().is_some() {
        return match expected {
            // Opaque rendering: stringifying a handle was already
            // meaningless with names; `<pipe>` keeps the totality.
            "STRING" => Ok(Value::string(format!("{value}"))),
            _ => Err(mismatch(expected, value)),
        };
    }
    Err(mismatch(expected, value))
}

fn mismatch(expected: &str, value: &Value) -> anyhow::Error {
    anyhow::anyhow!("TypeMismatch: expected {expected}, got value ({value:?})")
}

/// Resolve an [`Arg`] using an [`ExecState`] directly (no [`StepCtx`] needed).
/// Handles `Arg::String` and all-`Text` `Arg::Parts` — `Arg::Expr` requires a
/// `StepCtx` and must go through `resolve_arg`. Filesystem-free by design
/// (issue #131): assertion-needle pre-registration runs before step dispatch
/// while the snapshot is still pending, so this must never touch the snapshot
/// choke point (`command_ctx` / `resolve_write` would materialize it).
pub(crate) fn resolve_arg_state<P: ProcessManager>(
    arg: &Arg,
    state: &ExecState<P>,
) -> Result<String> {
    match arg {
        Arg::String(s, _) => Ok(expand_string(s, &state.envs, state)?),
        Arg::Expr(e) => bail!("Arg::Expr cannot be resolved without StepCtx: {:?}", e),
        Arg::Parts(parts) => {
            let mut out = String::new();
            for part in parts {
                match part {
                    ArgPart::Text(s, _) => {
                        out.push_str(&expand_string(s, &state.envs, state)?);
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

/// Evaluate an assertion operand to a typed [`Value`]: expressions
/// evaluate (variables, key-paths, calls, pre-typed literals); strings,
/// templates, and parts render to `String`. This is what makes
/// `ASSERT_EQ` strict: no coercion happens here.
pub(crate) fn evaluate_assert_operand<P: ProcessManager>(
    arg: &Arg,
    cx: &mut StepCtx<'_, P>,
) -> Result<Value> {
    match arg {
        Arg::Expr(expr) => evaluate_expr(expr, cx),
        _ => Ok(Value::string(resolve_arg(arg, cx)?)),
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
) -> Result<i64> {
    let resolved = resolve_arg(arg, cx)?;
    resolved
        .parse::<i64>()
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
        Expr::Literal(v) => {
            if let Some(s) = v.as_str() {
                // Expand {{ $var }} and {{ env:KEY }} in string literals
                return Ok(Value::string(expand_string(s, &cx.state.envs, cx.state)?));
            }
            Ok(v.clone())
        }
        Expr::Var(name) => cx
            .state
            .get_var(name)
            .ok_or_else(|| anyhow::anyhow!("undefined variable ${name}")),
        Expr::Env(key) => match cx.state.envs.get(key) {
            Some(v) => Ok(Value::string(v.clone())),
            None => anyhow::bail!("undefined environment variable `env:{key}`"),
        },
        Expr::KeyPath { base, keys } => resolve_key_path_value(cx, base, keys),
        Expr::List(items) => {
            let mut result = Vec::new();
            for item in items {
                result.push(evaluate_expr(item, cx)?);
            }
            Ok(Value::list(result))
        }
        Expr::Map(entries) => {
            let mut result = std::collections::BTreeMap::new();
            for (key, val_expr) in entries {
                let val = evaluate_expr(val_expr, cx)?;
                result.insert(key.clone(), val);
            }
            Ok(Value::map(result))
        }
        Expr::Call { name, args } => {
            // DSL-defined functions run through the call path so arity,
            // depth budget, and scoping apply uniformly.
            if cx.state.functions.contains_script(name.as_str()) {
                return super::handlers::call_func_value(cx, 0, name, args);
            }
            // Native entries gate before effects too: existence, the depth
            // budget, and the registry arity are all known without
            // evaluating anything. Messages match the wrapper format (no
            // step context exists at this layer); DSL-routed calls above
            // keep their step prefix.
            let Some(meta) = cx.state.native_meta(name.as_str()) else {
                bail!("unknown function {name}");
            };
            if cx.state.call_depth >= super::state::MAX_CALL_DEPTH {
                bail!(
                    "recursion depth limit exceeded in FUNC {}",
                    super::base_name(name)
                );
            }
            if let Some(params) = &meta.params
                && params.len() != args.len()
            {
                bail!(
                    "{}() expects {} argument(s), got {}",
                    super::base_name(name),
                    params.len(),
                    args.len()
                );
            }
            let mut vals = Vec::with_capacity(args.len());
            for arg in args {
                vals.push(evaluate_expr(arg, cx)?);
            }
            // Pure scalar fns first (no context needed), then stateful/IO fns.
            // Each clone ends the registry borrow before invoking user code.
            if let Some(func) = cx.state.clone_native_pure(name.as_str()) {
                func(vals)
            } else if let Some(func) = cx.state.clone_native_ctx(name.as_str()) {
                func(cx, vals)
            } else {
                bail!("unknown function {name}")
            }
        }
        // Variable inspection carries the binding name unevaluated (see
        // `Expr::Inspect`): no function-name matching happens here.
        Expr::Inspect(var) => evaluate_inspect_var(var, cx),
        // Bare `LET $p: PIPE`: mint a fresh unbound handle. Backends
        // materialize lazily on first binding, so declaration never
        // pre-commits a backend type with zero usage context.
        Expr::FreshPipe => Ok(Value::pipe_fresh()),
        Expr::Arithmetic { op, left, right } => {
            let left_val = evaluate_expr(left, cx)?;
            let right_val = evaluate_expr(right, cx)?;
            apply_arith(*op, left_val, right_val)
        }
        Expr::CompiledMath(ops) => evaluate_compiled_math(ops, cx),
        Expr::UnsignedIntBoundary(n) => {
            bail!("internal error: unstaged integer boundary {n}")
        }
        Expr::Compare { op, left, right } => {
            let lv = evaluate_expr(left, cx)?;
            let rv = evaluate_expr(right, cx)?;
            apply_compare(*op, &lv, &rv)
        }
        Expr::Logical { op, left, right } => {
            let left_val = evaluate_expr(left, cx)?;
            let left_truthy = is_truthy(&left_val)?;
            match op {
                LogicalOp::Or => {
                    if left_truthy {
                        Ok(Value::bool(true))
                    } else {
                        Ok(Value::bool(is_truthy(&evaluate_expr(right, cx)?)?))
                    }
                }
                LogicalOp::And => {
                    if !left_truthy {
                        Ok(Value::bool(false))
                    } else {
                        Ok(Value::bool(is_truthy(&evaluate_expr(right, cx)?)?))
                    }
                }
            }
        }
        Expr::Not(inner) => Ok(Value::bool(!is_truthy(&evaluate_expr(inner, cx)?)?)),
    }
}

/// Check if a value is truthy. Only `BOOL` words are accepted; all other types produce a TypeError.
pub(crate) fn is_truthy(val: &Value) -> Result<bool> {
    match val.as_bool() {
        Some(b) => Ok(b),
        None => bail!("Type Error: condition must be a Bool, found {:?}", val),
    }
}

/// Evaluate an `INSPECT($var)` node to a MAP snapshot: declared type and
/// value plus live details (pipe backend stats, task phase). Like
/// `LOAD_JSON`/`LOAD_TOML`, this evaluates to a value without running
/// script steps. The parser guarantees the argument is a `$variable`, so
/// the snapshot can name what it describes.
fn evaluate_inspect_var<P: ProcessManager>(var: &str, cx: &mut StepCtx<'_, P>) -> Result<Value> {
    super::handlers::inspect_var_map(cx, var).map(Value::map)
}

/// Value-semantics core of `PATH_TYPE()`: operates on an already-evaluated
/// path value so the registry can dispatch without re-evaluating.
pub(crate) fn path_type_from_value<P: ProcessManager>(
    args: &[Value],
    cx: &mut StepCtx<'_, P>,
) -> Result<Value> {
    let path_val = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("PATH_TYPE requires a path argument"))?;
    let path_str = match path_val.as_str() {
        Some(s) => s.to_string(),
        None => bail!("PATH_TYPE path argument must evaluate to a string"),
    };
    let target = cx
        .state
        .fs
        .resolve_write(&cx.state.cwd, &path_str)
        .map_err(|e| anyhow::anyhow!("failed to resolve path '{}': {}", path_str, e))?;
    let kind = match cx.state.fs.entry_kind_no_follow(&target) {
        Ok(EntryKind::Symlink) => "symlink",
        Ok(EntryKind::File) => "file",
        Ok(EntryKind::Dir) => "dir",
        Err(_) => "absent",
    };
    Ok(Value::string(kind.to_string()))
}

/// Pop one operand off the RPN stack with a structured error instead of a
/// panic on malformed/truncated `CompiledMath` vectors.
fn pop_stack(stack: &mut Vec<Value>) -> Result<Value> {
    stack
        .pop()
        .ok_or_else(|| anyhow::anyhow!("compiled math stack underflow"))
}

fn as_f64_value(val: &Value) -> Option<f64> {
    if let Some(n) = val.as_i64() {
        return Some(n as f64);
    }
    match val.as_f64() {
        Some(f) if f.is_finite() => Some(f),
        _ => None,
    }
}

/// Shared integer/float arithmetic: `Int x Int -> Int` (checked, integer
/// division, div-by-zero and overflow bail); any `Float` operand promotes
/// (`Int as f64`) to `Float`; float div-by-zero bails; non-finite results
/// bail rather than storing; anything else is a Type Error.
fn apply_arith(op: ArithOp, left: Value, right: Value) -> Result<Value> {
    if let (Some(a), Some(b)) = (left.as_i64(), right.as_i64()) {
        let v = match op {
            ArithOp::Add => a.checked_add(b),
            ArithOp::Sub => a.checked_sub(b),
            ArithOp::Mul => a.checked_mul(b),
            ArithOp::Div => a.checked_div(b),
        };
        return v.map(Value::int).ok_or_else(|| {
            anyhow::anyhow!("arithmetic error: integer overflow or division by zero")
        });
    }
    {
        let (Some(a), Some(b)) = (as_f64_value(&left), as_f64_value(&right)) else {
            bail!("Type Error: arithmetic requires Int or Float, found {left:?} and {right:?}");
        };
        if matches!(op, ArithOp::Div) && b == 0.0 {
            bail!("arithmetic error: float division by zero");
        }
        let v = match op {
            ArithOp::Add => a + b,
            ArithOp::Sub => a - b,
            ArithOp::Mul => a * b,
            ArithOp::Div => a / b,
        };
        if v.is_finite() {
            Ok(Value::float(v))
        } else {
            bail!("arithmetic error: non-finite float result")
        }
    }
}

fn apply_neg(val: Value) -> Result<Value> {
    if let Some(n) = val.as_i64() {
        return n
            .checked_neg()
            .map(Value::int)
            .ok_or_else(|| anyhow::anyhow!("arithmetic error: integer overflow"));
    }
    if let Some(f) = val.as_f64() {
        if f.is_finite() {
            return Ok(Value::float(-f));
        }
        bail!("arithmetic error: non-finite float result");
    }
    bail!("Type Error: unary '-' requires Int or Float, found {val:?}")
}

/// Shared comparison: both `Int`/`Float` compare numerically (`1 == 1.0` is
/// true, exact equality, no epsilon). Exactness follows binary fractions:
/// decimals whose reduced denominator is a power of 2 (0.5, 0.25, 0.75)
/// compare cleanly, while denominators containing factor 5 (0.1, 0.2, 0.3)
/// are repeating fractions in binary, so `0.1 + 0.2 == 0.3` is false.
/// The LET reference documents this with examples; otherwise `==`/`!=`
/// keep the stringified behavior and ordering on non-numerics bails.
fn apply_compare(op: CompareOp, left: &Value, right: &Value) -> Result<Value> {
    if let (Some(a), Some(b)) = (as_f64_value(left), as_f64_value(right)) {
        // Both numeric (finite floats; ints always qualify).
        if left.as_i64().is_some() && right.as_i64().is_some() {
            let (a, b) = (left.as_i64().unwrap_or(0), right.as_i64().unwrap_or(0));
            let result = match op {
                CompareOp::Eq => a == b,
                CompareOp::Ne => a != b,
                CompareOp::Lt => a < b,
                CompareOp::Le => a <= b,
                CompareOp::Gt => a > b,
                CompareOp::Ge => a >= b,
            };
            return Ok(Value::bool(result));
        }
        let result = match op {
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
            CompareOp::Lt => a < b,
            CompareOp::Le => a <= b,
            CompareOp::Gt => a > b,
            CompareOp::Ge => a >= b,
        };
        return Ok(Value::bool(result));
    }
    match op {
        CompareOp::Eq | CompareOp::Ne => {
            let ls = format_value_for_string(left);
            let rs = format_value_for_string(right);
            let result = match op {
                CompareOp::Eq => ls == rs,
                CompareOp::Ne => ls != rs,
                _ => bail!("internal error: equality shape"),
            };
            Ok(Value::bool(result))
        }
        CompareOp::Lt | CompareOp::Le | CompareOp::Gt | CompareOp::Ge => {
            bail!(
                "Type Error: ordering comparison requires Int or Float, found {left:?} and {right:?}"
            );
        }
    }
}

fn trim_ascii(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_ascii_whitespace())
}

/// `INT(x)`: trims ASCII whitespace and parses `i64`. Passes `Int` through;
/// `Float` only when integral and finite; anything else bails.
pub(crate) fn int_from_value(val: Value) -> Result<Value> {
    if val.as_i64().is_some() {
        return Ok(val);
    }
    if let Some(f) = val.as_f64() {
        if f.is_finite() && f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
            return Ok(Value::int(f as i64));
        }
        bail!("INT() requires an integer value, found FLOAT ({f:?})");
    }
    if let Some(s) = val.as_str() {
        let trimmed = trim_ascii(s);
        return trimmed
            .parse::<i64>()
            .map(Value::int)
            .map_err(|_| anyhow::anyhow!("INT() requires an integer string, found {s:?}"));
    }
    bail!("INT() requires an Int, Float, or String, found {val:?}")
}

/// `FLOAT(x)`: parses `f64` (accepts int strings), bails on non-finite or
/// non-numeric. Passes `Float`/`Int as f64` through.
pub(crate) fn float_from_value(val: Value) -> Result<Value> {
    if let Some(f) = val.as_f64() {
        if f.is_finite() {
            return Ok(Value::float(f));
        }
        bail!("FLOAT() requires a finite value, found FLOAT ({f:?})");
    }
    if let Some(n) = val.as_i64() {
        return Ok(Value::float(n as f64));
    }
    if let Some(s) = val.as_str() {
        let trimmed = trim_ascii(s);
        let parsed: f64 = trimmed
            .parse()
            .map_err(|_| anyhow::anyhow!("FLOAT() requires a numeric string, found {s:?}"))?;
        if parsed.is_finite() {
            return Ok(Value::float(parsed));
        }
        bail!("FLOAT() requires a finite value, found {s:?}");
    }
    bail!("FLOAT() requires an Int, Float, or String, found {val:?}")
}

/// Resolve `$base.key...` against the scope chain (shared by the AST
/// `KeyPath` arm and the RPN `LoadKeyPath` op).
fn resolve_key_path_value<P: ProcessManager>(
    cx: &StepCtx<'_, P>,
    base: &str,
    keys: &[String],
) -> Result<Value> {
    let mut current = cx
        .state
        .get_var(base)
        .ok_or_else(|| anyhow::anyhow!("undefined variable ${base}"))?;
    for key in keys {
        if let Some(map) = current.as_map() {
            current = map
                .get(key)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Key '{}' not found in map", key))?;
        } else if let Some(list) = current.as_list() {
            let idx: usize = key
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid array index '{}'", key))?;
            current = list
                .get(idx)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Index {} out of bounds", idx))?;
        } else {
            bail!("Cannot traverse into scalar value at key '{}'", key);
        }
    }
    Ok(current)
}

/// Execute flat `CompiledMath` RPN with a local stack loop. All pops are
/// checked (`pop_stack`); `Call` args are popped then reversed to restore
/// left-to-right order before dispatch.
fn evaluate_compiled_math<P: ProcessManager>(
    ops: &[MathOp],
    cx: &mut StepCtx<'_, P>,
) -> Result<Value> {
    let mut stack: Vec<Value> = Vec::with_capacity(8);
    for op in ops {
        match op {
            MathOp::PushConst(v) => {
                if let Some(s) = v.as_str() {
                    stack.push(Value::string(expand_string(s, &cx.state.envs, cx.state)?));
                } else {
                    stack.push(v.clone());
                }
            }
            MathOp::LoadVar(name) => {
                let val = cx
                    .state
                    .get_var(name)
                    .ok_or_else(|| anyhow::anyhow!("undefined variable ${name}"))?;
                stack.push(val);
            }
            MathOp::LoadEnv(key) => match cx.state.envs.get(key) {
                Some(v) => stack.push(Value::string(v.clone())),
                None => anyhow::bail!("undefined environment variable `env:{key}`"),
            },
            MathOp::LoadKeyPath { base, keys } => {
                stack.push(resolve_key_path_value(cx, base, keys)?);
            }
            MathOp::Call { name, arity } => {
                let mut args = Vec::with_capacity(*arity);
                for _ in 0..*arity {
                    args.push(pop_stack(&mut stack)?);
                }
                args.reverse();
                // RPN math path: pure scalar fns plus stateful fns opted
                // into `rpn` run through the registry; anything else
                // (PATH_TYPE, FUNCTIONS, DESCRIBE, TYPES, TYPE_DESCRIBE)
                // is AST-only by design.
                let val = if let Some(func) = cx.state.clone_native_pure(name.as_str()) {
                    func(args)?
                } else if cx
                    .state
                    .native_meta(name.as_str())
                    .is_some_and(|meta| meta.rpn)
                    && let Some(func) = cx.state.clone_native_ctx(name.as_str())
                {
                    func(cx, args)?
                } else {
                    bail!("unknown function {name}")
                };
                stack.push(val);
            }
            MathOp::Inspect(name) => {
                stack.push(Value::map(super::handlers::inspect_var_map(cx, name)?));
            }
            MathOp::Neg => {
                let val = pop_stack(&mut stack)?;
                stack.push(apply_neg(val)?);
            }
            MathOp::Add => {
                let right = pop_stack(&mut stack)?;
                let left = pop_stack(&mut stack)?;
                stack.push(apply_arith(ArithOp::Add, left, right)?);
            }
            MathOp::Sub => {
                let right = pop_stack(&mut stack)?;
                let left = pop_stack(&mut stack)?;
                stack.push(apply_arith(ArithOp::Sub, left, right)?);
            }
            MathOp::Mul => {
                let right = pop_stack(&mut stack)?;
                let left = pop_stack(&mut stack)?;
                stack.push(apply_arith(ArithOp::Mul, left, right)?);
            }
            MathOp::Div => {
                let right = pop_stack(&mut stack)?;
                let left = pop_stack(&mut stack)?;
                stack.push(apply_arith(ArithOp::Div, left, right)?);
            }
            MathOp::Lt => {
                let right = pop_stack(&mut stack)?;
                let left = pop_stack(&mut stack)?;
                stack.push(apply_compare(CompareOp::Lt, &left, &right)?);
            }
            MathOp::Le => {
                let right = pop_stack(&mut stack)?;
                let left = pop_stack(&mut stack)?;
                stack.push(apply_compare(CompareOp::Le, &left, &right)?);
            }
            MathOp::Gt => {
                let right = pop_stack(&mut stack)?;
                let left = pop_stack(&mut stack)?;
                stack.push(apply_compare(CompareOp::Gt, &left, &right)?);
            }
            MathOp::Ge => {
                let right = pop_stack(&mut stack)?;
                let left = pop_stack(&mut stack)?;
                stack.push(apply_compare(CompareOp::Ge, &left, &right)?);
            }
            MathOp::Eq => {
                let right = pop_stack(&mut stack)?;
                let left = pop_stack(&mut stack)?;
                stack.push(apply_compare(CompareOp::Eq, &left, &right)?);
            }
            MathOp::Ne => {
                let right = pop_stack(&mut stack)?;
                let left = pop_stack(&mut stack)?;
                stack.push(apply_compare(CompareOp::Ne, &left, &right)?);
            }
        }
    }
    if stack.len() != 1 {
        bail!("compiled math left {} values on the stack", stack.len());
    }
    pop_stack(&mut stack)
}

/// Value-semantics core of `GLOB()`: operates on an already-evaluated
/// pattern value so both the AST and RPN paths share one implementation.
pub(crate) fn glob_from_value<P: ProcessManager>(
    args: &[Value],
    cx: &mut StepCtx<'_, P>,
) -> Result<Value> {
    let pattern_val = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("GLOB requires a pattern argument"))?;
    let Some(raw_pattern) = pattern_val.as_str() else {
        bail!("GLOB pattern argument must evaluate to a string");
    };
    let raw_pattern = raw_pattern.to_string();

    // Up-front sandbox gate (mirrors `GuardedPath::glob_paths`): patterns are
    // sandbox-root-relative, so any `..` component escapes. Return empty
    // without traversing — the same empty-on-no-match GLOB semantics.
    if raw_pattern
        .replace('\\', "/")
        .split('/')
        .any(|seg| seg == "..")
    {
        return Ok(Value::list(Vec::new()));
    }

    // GLOB lists the current root: ride the snapshot choke point so a
    // pending snapshot materializes here (a snapshot read), while local
    // roots resolve with zero I/O. The listing below then runs on concrete
    // paths in both cases.
    let _ = cx.state.fs.resolve_read(&cx.state.cwd, ".")?;
    let root = cx.state.fs.root().clone();
    let root_path = root.as_path().to_path_buf();
    let mut entries: Vec<Value> = root
        .glob_paths(&raw_pattern)?
        .into_iter()
        .filter_map(|p| {
            p.strip_prefix(&root_path)
                .ok()
                .map(|rel| Value::string(rel.to_string_lossy().replace('\\', "/")))
        })
        .collect();

    entries.sort_by(|a, b| format!("{}", a).cmp(&format!("{}", b)));
    Ok(Value::list(entries))
}

/// Value-semantics core of `LOAD_TOML()`.
pub(crate) fn load_toml_from_value<P: ProcessManager>(
    args: &[Value],
    cx: &mut StepCtx<'_, P>,
) -> Result<Value> {
    let path_val = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("LOAD_TOML requires a path argument"))?;
    let Some(path_str) = path_val.as_str() else {
        bail!("LOAD_TOML path argument must evaluate to a string");
    };
    let path_str = path_str.to_string();
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

/// Value-semantics core of `LOAD_JSON()`.
pub(crate) fn load_json_from_value<P: ProcessManager>(
    args: &[Value],
    cx: &mut StepCtx<'_, P>,
) -> Result<Value> {
    let path_val = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("LOAD_JSON requires a path argument"))?;
    let Some(path_str) = path_val.as_str() else {
        bail!("LOAD_JSON path argument must evaluate to a string");
    };
    let path_str = path_str.to_string();
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
        serde_json::Value::String(s) => Value::string(s),
        serde_json::Value::Bool(b) => Value::bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::int(i)
            } else if let Some(f) = n.as_f64() {
                Value::float(f)
            } else {
                Value::string(n.to_string())
            }
        }
        serde_json::Value::Array(arr) => Value::list(arr.into_iter().map(json_to_value).collect()),
        serde_json::Value::Object(map) => Value::map(
            map.into_iter()
                .map(|(k, v)| (k, json_to_value(v)))
                .collect(),
        ),
        serde_json::Value::Null => Value::string(String::new()),
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
                                    Some(v) => {
                                        if let Some(map) = v.as_map() {
                                            map.get(part_trim).cloned()
                                        } else if let Some(list) = v.as_list() {
                                            part_trim
                                                .parse::<usize>()
                                                .ok()
                                                .and_then(|idx| list.get(idx).cloned())
                                        } else {
                                            None
                                        }
                                    }
                                    None => None,
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
///
/// Float rendering is defined as `f.to_string()`: Rust's `Display` for `f64`
/// already produces the shortest round-trip decimal (Ryū since 1.36), so
/// `ECHO` / `{{ }}` output stays clean (`3.14`, not `3.140000`) with no
/// extra dependency. This rendering also feeds the `==`/`!=` string
/// fallback for non-numeric operands.
pub(crate) fn format_value_for_string(val: &Value) -> String {
    if let Some(s) = val.as_str() {
        return s.to_string();
    }
    if let Some(i) = val.as_i64() {
        return i.to_string();
    }
    if let Some(f) = val.as_f64() {
        return f.to_string();
    }
    if let Some(b) = val.as_bool() {
        return b.to_string();
    }
    // Pipes render through `Display` (`<pipe>`) via the fallthrough below;
    // handles are opaque and have no string form to spell.
    if let Some(d) = val.as_duration() {
        return oxdock_parser::command::format_duration(&d);
    }
    if let Some(p) = val.as_path() {
        return p.to_string_lossy().to_string();
    }
    if let Some(items) = val.as_list() {
        return items
            .iter()
            .map(format_value_for_string)
            .collect::<Vec<_>>()
            .join(" ");
    }
    if let Some(map) = val.as_map() {
        return map
            .iter()
            .map(|(k, v)| format!("\"{}\": {}", k, format_value_for_string(v)))
            .collect::<Vec<_>>()
            .join(", ");
    }
    if let Some(id) = val.as_handle() {
        return format!("task#{}", id);
    }
    format!("{}", val)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pop_stack_underflow_returns_error() {
        let mut stack: Vec<Value> = Vec::new();
        assert!(
            pop_stack(&mut stack).is_err(),
            "empty pop must error, not panic"
        );
        stack.push(Value::int(1));
        assert_eq!(pop_stack(&mut stack).unwrap(), Value::int(1));
        assert!(pop_stack(&mut stack).is_err());
    }

    #[test]
    fn apply_arith_int_and_errors() {
        assert_eq!(
            apply_arith(ArithOp::Add, Value::int(2), Value::int(3)).unwrap(),
            Value::int(5)
        );
        assert!(apply_arith(ArithOp::Div, Value::int(1), Value::int(0)).is_err());
        assert!(
            apply_arith(ArithOp::Add, Value::int(i64::MAX), Value::int(1)).is_err(),
            "int overflow must bail"
        );
        assert!(
            apply_arith(ArithOp::Add, Value::string("a".to_string()), Value::int(1)).is_err(),
            "string arithmetic must be a Type Error"
        );
        assert!(apply_neg(Value::int(i64::MIN)).is_err());
    }

    #[test]
    fn apply_arith_float_promotion() {
        assert_eq!(
            apply_arith(ArithOp::Add, Value::int(1), Value::float(2.5)).unwrap(),
            Value::float(3.5)
        );
        assert!(apply_arith(ArithOp::Div, Value::float(1.0), Value::float(0.0)).is_err());
    }

    #[test]
    fn apply_compare_numeric_and_fallback() {
        assert_eq!(
            apply_compare(CompareOp::Eq, &Value::int(1), &Value::float(1.0)).unwrap(),
            Value::bool(true)
        );
        assert_eq!(
            apply_compare(CompareOp::Lt, &Value::int(3), &Value::float(4.5)).unwrap(),
            Value::bool(true)
        );
        assert_eq!(
            apply_compare(
                CompareOp::Eq,
                &Value::string("a".to_string()),
                &Value::string("a".to_string())
            )
            .unwrap(),
            Value::bool(true)
        );
        assert!(
            apply_compare(
                CompareOp::Lt,
                &Value::string("a".to_string()),
                &Value::string("b".to_string())
            )
            .is_err(),
            "ordering on strings must bail"
        );
    }

    #[test]
    fn int_float_conversions() {
        assert_eq!(
            int_from_value(Value::string("  123\n".to_string())).unwrap(),
            Value::int(123)
        );
        assert_eq!(int_from_value(Value::float(3.0)).unwrap(), Value::int(3));
        assert!(int_from_value(Value::float(3.5)).is_err());
        assert!(int_from_value(Value::string("abc".to_string())).is_err());
        assert_eq!(
            float_from_value(Value::string("2.5".to_string())).unwrap(),
            Value::float(2.5)
        );
        assert_eq!(float_from_value(Value::int(3)).unwrap(), Value::float(3.0));
        for bad in ["nan", "inf", "-inf", "abc"] {
            assert!(
                float_from_value(Value::string(bad.to_string())).is_err(),
                "{bad:?} must fail"
            );
        }
    }
}
