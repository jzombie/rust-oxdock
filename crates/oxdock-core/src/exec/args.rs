use anyhow::{Result, bail};
use oxdock_parser::{Arg, ArgPart, ArithOp, CompareOp, Expr, LogicalOp, MathOp, TypeKind, Value};
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
            // The `pipe:NAME` operator is the explicit handle constructor:
            // a fresh name registers on first use (existing entries keep
            // their type), so pipes can be declared before any `WITH_IO`
            // mentions them.
            if !state.io.pipe_exists(&n) {
                state.io.ensure_pipe_for(&n, false)?;
            }
            Ok(Value::Pipe(n))
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
            // Strict: plain strings never coerce to pipes, so a handle is
            // always created explicitly via the `pipe:NAME` operator
            // (`LET $p: PIPE = pipe:log`). Anything else is a TypeMismatch.
            Err(anyhow::anyhow!(
                "TypeMismatch: expected {expected_label}, got STRING ({s:?}); use pipe:NAME to name a pipe"
            ))
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
        Expr::KeyPath { base, keys } => resolve_key_path_value(cx, base, keys),
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
            "INSPECT" => evaluate_inspect(args, cx),
            "INT" => evaluate_int(args, cx),
            "FLOAT" => evaluate_float(args, cx),
            _ => bail!("unknown function {name}"),
        },
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

/// Evaluate an `INSPECT($var)` call to a MAP snapshot: declared type and
/// value plus live details (pipe backend stats, task phase). Like
/// `LOAD_JSON`/`LOAD_TOML`, this evaluates to a value without running
/// script steps. The argument must be a `$variable`, not an arbitrary
/// expression, so the snapshot can name what it describes.
fn evaluate_inspect<P: ProcessManager>(args: &[Expr], cx: &mut StepCtx<'_, P>) -> Result<Value> {
    let [arg] = args else {
        bail!("INSPECT requires exactly one argument: INSPECT($var)");
    };
    let Expr::Var(var) = arg else {
        bail!("INSPECT requires a $variable argument, found {arg:?}");
    };
    super::handlers::inspect_var_map(cx, var).map(Value::Map)
}

/// Pop one operand off the RPN stack with a structured error instead of a
/// panic on malformed/truncated `CompiledMath` vectors.
fn pop_stack(stack: &mut Vec<Value>) -> Result<Value> {
    stack
        .pop()
        .ok_or_else(|| anyhow::anyhow!("compiled math stack underflow"))
}

fn as_f64_value(val: &Value) -> Option<f64> {
    match val {
        Value::Int(n) => Some(*n as f64),
        Value::Float(f) if f.is_finite() => Some(*f),
        _ => None,
    }
}

/// Shared integer/float arithmetic: `Int x Int -> Int` (checked, integer
/// division, div-by-zero and overflow bail); any `Float` operand promotes
/// (`Int as f64`) to `Float`; float div-by-zero bails; non-finite results
/// bail rather than storing; anything else is a Type Error.
fn apply_arith(op: ArithOp, left: Value, right: Value) -> Result<Value> {
    match (&left, &right) {
        (Value::Int(a), Value::Int(b)) => {
            let v = match op {
                ArithOp::Add => a.checked_add(*b),
                ArithOp::Sub => a.checked_sub(*b),
                ArithOp::Mul => a.checked_mul(*b),
                ArithOp::Div => a.checked_div(*b),
            };
            v.map(Value::Int).ok_or_else(|| {
                anyhow::anyhow!("arithmetic error: integer overflow or division by zero")
            })
        }
        _ => {
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
                Ok(Value::Float(v))
            } else {
                bail!("arithmetic error: non-finite float result")
            }
        }
    }
}

fn apply_neg(val: Value) -> Result<Value> {
    match val {
        Value::Int(n) => n
            .checked_neg()
            .map(Value::Int)
            .ok_or_else(|| anyhow::anyhow!("arithmetic error: integer overflow")),
        Value::Float(f) => {
            if f.is_finite() {
                Ok(Value::Float(-f))
            } else {
                bail!("arithmetic error: non-finite float result");
            }
        }
        other => bail!("Type Error: unary '-' requires Int or Float, found {other:?}"),
    }
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
        let both_int = matches!(left, Value::Int(_)) && matches!(right, Value::Int(_));
        if both_int {
            let (Value::Int(a), Value::Int(b)) = (left, right) else {
                bail!("internal error: int comparison shape");
            };
            let result = match op {
                CompareOp::Eq => a == b,
                CompareOp::Ne => a != b,
                CompareOp::Lt => a < b,
                CompareOp::Le => a <= b,
                CompareOp::Gt => a > b,
                CompareOp::Ge => a >= b,
            };
            return Ok(Value::Bool(result));
        }
        let result = match op {
            CompareOp::Eq => a == b,
            CompareOp::Ne => a != b,
            CompareOp::Lt => a < b,
            CompareOp::Le => a <= b,
            CompareOp::Gt => a > b,
            CompareOp::Ge => a >= b,
        };
        return Ok(Value::Bool(result));
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
            Ok(Value::Bool(result))
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
fn int_from_value(val: Value) -> Result<Value> {
    match val {
        Value::Int(_) => Ok(val),
        Value::Float(f) => {
            if f.is_finite() && f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
                Ok(Value::Int(f as i64))
            } else {
                bail!("INT() requires an integer value, found FLOAT ({f:?})");
            }
        }
        Value::String(s) => {
            let trimmed = trim_ascii(&s);
            trimmed
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| anyhow::anyhow!("INT() requires an integer string, found {s:?}"))
        }
        other => bail!("INT() requires an Int, Float, or String, found {other:?}"),
    }
}

/// `FLOAT(x)`: parses `f64` (accepts int strings), bails on non-finite or
/// non-numeric. Passes `Float`/`Int as f64` through.
fn float_from_value(val: Value) -> Result<Value> {
    match val {
        Value::Float(f) => {
            if f.is_finite() {
                Ok(Value::Float(f))
            } else {
                bail!("FLOAT() requires a finite value, found FLOAT ({f:?})");
            }
        }
        Value::Int(n) => Ok(Value::Float(n as f64)),
        Value::String(s) => {
            let trimmed = trim_ascii(&s);
            let parsed: f64 = trimmed
                .parse()
                .map_err(|_| anyhow::anyhow!("FLOAT() requires a numeric string, found {s:?}"))?;
            if parsed.is_finite() {
                Ok(Value::Float(parsed))
            } else {
                bail!("FLOAT() requires a finite value, found {s:?}");
            }
        }
        other => bail!("FLOAT() requires an Int, Float, or String, found {other:?}"),
    }
}

fn evaluate_int<P: ProcessManager>(args: &[Expr], cx: &mut StepCtx<'_, P>) -> Result<Value> {
    let [arg] = args else {
        bail!("INT requires exactly one argument: INT($var)");
    };
    let val = evaluate_expr(arg, cx)?;
    int_from_value(val)
}

fn evaluate_float<P: ProcessManager>(args: &[Expr], cx: &mut StepCtx<'_, P>) -> Result<Value> {
    let [arg] = args else {
        bail!("FLOAT requires exactly one argument: FLOAT($var)");
    };
    let val = evaluate_expr(arg, cx)?;
    float_from_value(val)
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
            MathOp::PushConst(v) => match v {
                Value::String(s) => {
                    stack.push(Value::String(expand_string(s, &cx.state.envs, cx.state)?));
                }
                other => stack.push(other.clone()),
            },
            MathOp::LoadVar(name) => {
                let val = cx
                    .state
                    .get_var(name)
                    .ok_or_else(|| anyhow::anyhow!("undefined variable ${name}"))?;
                stack.push(val);
            }
            MathOp::LoadEnv(key) => match cx.state.envs.get(key) {
                Some(v) => stack.push(Value::String(v.clone())),
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
                let val = match name.as_str() {
                    "INT" => {
                        let [arg] = args.as_slice() else {
                            bail!("INT requires exactly one argument: INT($var)");
                        };
                        int_from_value(arg.clone())?
                    }
                    "FLOAT" => {
                        let [arg] = args.as_slice() else {
                            bail!("FLOAT requires exactly one argument: FLOAT($var)");
                        };
                        float_from_value(arg.clone())?
                    }
                    "GLOB" => glob_from_value(args.as_slice(), cx)?,
                    "LOAD_TOML" => load_toml_from_value(args.as_slice(), cx)?,
                    "LOAD_JSON" => load_json_from_value(args.as_slice(), cx)?,
                    _ => bail!("unknown function {name}"),
                };
                stack.push(val);
            }
            MathOp::Inspect(name) => {
                stack.push(Value::Map(super::handlers::inspect_var_map(cx, name)?));
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

/// Evaluate a `GLOB()` function call.
fn evaluate_glob<P: ProcessManager>(args: &[Expr], cx: &mut StepCtx<'_, P>) -> Result<Value> {
    if args.is_empty() {
        bail!("GLOB requires a pattern argument");
    }

    let pattern_val = evaluate_expr(&args[0], cx)?;
    glob_from_value(std::slice::from_ref(&pattern_val), cx)
}

/// Value-semantics core of `GLOB()`: operates on an already-evaluated
/// pattern value so both the AST and RPN paths share one implementation.
fn glob_from_value<P: ProcessManager>(args: &[Value], cx: &mut StepCtx<'_, P>) -> Result<Value> {
    let pattern_val = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("GLOB requires a pattern argument"))?;
    let raw_pattern = match pattern_val {
        Value::String(s) => s.clone(),
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
    load_toml_from_value(std::slice::from_ref(&path_val), cx)
}

/// Value-semantics core of `LOAD_TOML()`.
fn load_toml_from_value<P: ProcessManager>(
    args: &[Value],
    cx: &mut StepCtx<'_, P>,
) -> Result<Value> {
    let path_val = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("LOAD_TOML requires a path argument"))?;
    let path_str = match path_val {
        Value::String(s) => s.clone(),
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
    load_json_from_value(std::slice::from_ref(&path_val), cx)
}

/// Value-semantics core of `LOAD_JSON()`.
fn load_json_from_value<P: ProcessManager>(
    args: &[Value],
    cx: &mut StepCtx<'_, P>,
) -> Result<Value> {
    let path_val = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("LOAD_JSON requires a path argument"))?;
    let path_str = match path_val {
        Value::String(s) => s.clone(),
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
///
/// Float rendering is defined as `f.to_string()`: Rust's `Display` for `f64`
/// already produces the shortest round-trip decimal (Ryū since 1.36), so
/// `ECHO` / `{{ }}` output stays clean (`3.14`, not `3.140000`) with no
/// extra dependency. This rendering also feeds the `==`/`!=` string
/// fallback for non-numeric operands.
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
        stack.push(Value::Int(1));
        assert_eq!(pop_stack(&mut stack).unwrap(), Value::Int(1));
        assert!(pop_stack(&mut stack).is_err());
    }

    #[test]
    fn apply_arith_int_and_errors() {
        assert_eq!(
            apply_arith(ArithOp::Add, Value::Int(2), Value::Int(3)).unwrap(),
            Value::Int(5)
        );
        assert!(apply_arith(ArithOp::Div, Value::Int(1), Value::Int(0)).is_err());
        assert!(
            apply_arith(ArithOp::Add, Value::Int(i64::MAX), Value::Int(1)).is_err(),
            "int overflow must bail"
        );
        assert!(
            apply_arith(ArithOp::Add, Value::String("a".to_string()), Value::Int(1)).is_err(),
            "string arithmetic must be a Type Error"
        );
        assert!(apply_neg(Value::Int(i64::MIN)).is_err());
    }

    #[test]
    fn apply_arith_float_promotion() {
        assert_eq!(
            apply_arith(ArithOp::Add, Value::Int(1), Value::Float(2.5)).unwrap(),
            Value::Float(3.5)
        );
        assert!(apply_arith(ArithOp::Div, Value::Float(1.0), Value::Float(0.0)).is_err());
    }

    #[test]
    fn apply_compare_numeric_and_fallback() {
        assert_eq!(
            apply_compare(CompareOp::Eq, &Value::Int(1), &Value::Float(1.0)).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            apply_compare(CompareOp::Lt, &Value::Int(3), &Value::Float(4.5)).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            apply_compare(
                CompareOp::Eq,
                &Value::String("a".to_string()),
                &Value::String("a".to_string())
            )
            .unwrap(),
            Value::Bool(true)
        );
        assert!(
            apply_compare(
                CompareOp::Lt,
                &Value::String("a".to_string()),
                &Value::String("b".to_string())
            )
            .is_err(),
            "ordering on strings must bail"
        );
    }

    #[test]
    fn int_float_conversions() {
        assert_eq!(
            int_from_value(Value::String("  123\n".to_string())).unwrap(),
            Value::Int(123)
        );
        assert_eq!(int_from_value(Value::Float(3.0)).unwrap(), Value::Int(3));
        assert!(int_from_value(Value::Float(3.5)).is_err());
        assert!(int_from_value(Value::String("abc".to_string())).is_err());
        assert_eq!(
            float_from_value(Value::String("2.5".to_string())).unwrap(),
            Value::Float(2.5)
        );
        assert_eq!(float_from_value(Value::Int(3)).unwrap(), Value::Float(3.0));
        for bad in ["nan", "inf", "-inf", "abc"] {
            assert!(
                float_from_value(Value::String(bad.to_string())).is_err(),
                "{bad:?} must fail"
            );
        }
    }
}
