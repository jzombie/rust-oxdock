//! Run-start static type pass for scripts: top-level steps and every
//! script `FUNC` body.
//!
//! Host signatures are enforced at the boundary by macro-generated
//! extractors; this pass is the script-side counterpart. It runs once
//! per run from `finish_run`, before the first step executes, and
//! fails fast on what execution would otherwise discover late:
//! missing returns, disjoint branch types, undeclared variables, and
//! values flowing into typed parameters without a coercion gate.
//!
//! Top-level steps are checked with an empty environment: undeclared
//! variables, bad arities, and mistyped arguments fail at run start
//! whether they sit at the top level or inside a function. A top-level
//! `RETURN` fails here with the same boundary message execution would
//! produce.
//!
//! The rules, in full:
//!
//! - Every terminal `RETURN` path of a `FUNC` body must unify to one
//!   type (`T` + `T` = `T`; anything + `ANY` = `ANY`; disjoint
//!   concrete types fail naming both). `EXIT` ends a path without
//!   contributing a type.
//! - A body with a `RETURN` on some live path but reachable
//!   fallthrough fails with `MissingReturn`. A body with no live
//!   `RETURN` is `VOID`; `VOID` never defaults to `STRING`.
//! - Guards classify statically: absent or `[bool:true]` is
//!   unconditional; `[bool:false]` is statically unreachable and
//!   excluded from classification, unification, completeness, and
//!   checking entirely; every other guard is conditional. Loops never
//!   satisfy completeness (zero-trip); a `RETURN` solely inside a loop
//!   needs an unconditional fallback after it.
//! - Environments are flow-sensitive and sequential: `LET` declares,
//!   `SET` keeps the declared type (runtime coerces), branches fork
//!   and discard (arm declarations are scope-local at runtime, so no
//!   merge is needed — discarding is exact, not conservative).
//! - `ANY` widens unidirectionally only. An `ANY`-typed argument into
//!   a concrete parameter fails naming the `LET $x: T` gate; an
//!   `ANY`-returning call assigned to `LET $x: T` is allowed because
//!   `declare_var` coercion is the gate.
//! - Every statement argument participates: `Arg::Expr` operands and
//!   `{{ }}` fragments inside `Arg::Parts` read through inference,
//!   and task-handle (`AWAIT`, `CANCEL`, `AWAIT`-capture), pipe
//!   (`WITH_IO` bindings), and list (`LIST_APPEND`) names read
//!   through site-aware liveness with their runtime type rules
//!   (`PIPE`/`LIST` exactly). Plain `Arg::String` text stays
//!   unchecked by design: `{{ }}` there expands missing names to
//!   empty at runtime and never errors, so rejecting it would be a
//!   false positive.
//!
//! Performance: single traversal, no fixpoint, no I/O, no registry
//! writes. Branch forks clone a map of live vars; the merge walk
//! would cost O(vars) regardless, so overlays would save nothing
//! measurable. `MAX_STATIC_DEPTH` caps nesting with a static error.

use std::collections::HashMap;

use anyhow::{Result, bail};
use oxdock_parser::{
    CompareOp, Expr, GuardExpr, IoBinding, MathOp, PipeTarget, Step, StepKind, TypeTag, Value,
};
use oxdock_process::ProcessManager;

use super::{ExecState, FuncMeta, FuncParam};

/// Maximum expression nesting and nested-body descent. Parser-bounded
/// scripts never approach it; hand-built ASTs cannot wedge the pass.
/// Exceeding it is a static error, never silent truncation.
pub const MAX_STATIC_DEPTH: usize = 64;

/// Static guard classification. Only unconditional steps terminate
/// paths; only dead steps are skipped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GuardClass {
    Unconditional,
    Conditional,
    Dead,
}

/// Inferred static type: a tag, `ANY` (as `TypeTag::Any`), or `VOID`
/// for bodies with no `RETURN`. `VOID` is pass-level only, never a
/// value tag.
#[derive(Clone, Copy, Debug)]
enum StaticTy {
    Ty(TypeTag),
    Void,
}

/// One collected script function: resolved params plus body.
#[derive(Clone)]
struct FuncInfo {
    params: Vec<(String, TypeTag)>,
    body: Vec<Step>,
}

/// Lexical environment: a stack of frames mirroring the runtime
/// `var_scopes`. Frames push on `scope_enter` markers, branch and
/// loop bodies, and function entries; they pop on `scope_exit` or are
/// discarded with a fork. `LET` declares in the top frame only (a
/// duplicate there mirrors the runtime redeclaration error);
/// shadowing an outer frame is legal exactly where the runtime
/// pushes a scope. `SET` and reads walk outward; types never change
/// on mutation (runtime coerces).
/// One live declaration: its tag plus the guard it was declared
/// under. Same-scope duplicates with disjoint guards coexist (only
/// one path ever binds); reads unify across entries since the path
/// is unknowable statically.
#[derive(Clone, Debug)]
struct Binding {
    tag: TypeTag,
    guard: Option<GuardExpr>,
}

#[derive(Clone, Default, Debug)]
struct Env {
    frames: Vec<HashMap<String, Vec<Binding>>>,
}

/// Where a `RETURN`/`BREAK`/`CONTINUE` may legally land, mirroring
/// the runtime boundary errors one for one.
#[derive(Clone, Debug)]
enum RetCtx {
    Top,
    Func(String),
    Block,
    AsyncValue,
    AsyncStmt,
}

/// Walk context: message label, return boundary, loop depth,
/// plus the current step's guard for site-aware reads.
/// `BREAK`/`CONTINUE` are legal only with `loops > 0`; blocks,
/// functions, and tasks reset it (control never crosses those
/// boundaries at runtime).
#[derive(Clone)]
struct Scope<'x> {
    label: &'x str,
    ret: RetCtx,
    loops: usize,
    site: Option<GuardExpr>,
}

/// The pass state: collected functions, host metas, inference
/// memoization, recursion guard, and the linearity counter.
struct Checker<'a, P: ProcessManager> {
    exec: &'a ExecState<P>,
    funcs: HashMap<String, FuncInfo>,
    hosts: HashMap<String, FuncMeta>,
    inferred: HashMap<String, StaticTy>,
    inferring: Vec<String>,
    visits: u64,
}

/// Run the static pass over top-level steps plus every collected
/// script function. Fails before the first step executes.
pub fn validate_script_types<P: ProcessManager>(steps: &[Step], exec: &ExecState<P>) -> Result<()> {
    let mut checker = Checker::new(exec);
    checker.collect(steps)?;
    checker.check_top_level(steps)?;
    for key in checker.func_keys() {
        checker.func_return(&key, 0)?;
    }
    Ok(())
}

/// Same pass returning the visit count, for the linearity test: the
/// count must stay proportional to input size on adversarial input.
/// Test-only: production calls `validate_script_types`.
#[cfg(test)]
pub fn validate_script_types_counted<P: ProcessManager>(
    steps: &[Step],
    exec: &ExecState<P>,
) -> Result<u64> {
    let mut checker = Checker::new(exec);
    checker.collect(steps)?;
    checker.check_top_level(steps)?;
    for key in checker.func_keys() {
        checker.func_return(&key, 0)?;
    }
    Ok(checker.visits)
}

/// Pointer-identity-aware tag equality. `TypeTag` carries no
/// `PartialEq` (descriptors are unsized vtables), and structural
/// comparison would equate distinct registrations; descriptors are
/// `'static` singletons, so pointer equality is the sound rule.
fn tags_equal(left: &TypeTag, right: &TypeTag) -> bool {
    match (left, right) {
        (TypeTag::String, TypeTag::String)
        | (TypeTag::Int, TypeTag::Int)
        | (TypeTag::Float, TypeTag::Float)
        | (TypeTag::Bool, TypeTag::Bool)
        | (TypeTag::List, TypeTag::List)
        | (TypeTag::Map, TypeTag::Map)
        | (TypeTag::Pipe, TypeTag::Pipe)
        | (TypeTag::Handle, TypeTag::Handle)
        | (TypeTag::Duration, TypeTag::Duration)
        | (TypeTag::Path, TypeTag::Path)
        | (TypeTag::Semaphore, TypeTag::Semaphore)
        | (TypeTag::Permit, TypeTag::Permit)
        | (TypeTag::Any, TypeTag::Any) => true,
        (TypeTag::Custom(a), TypeTag::Custom(b)) => std::ptr::eq(*a, *b),
        (TypeTag::ListOf(a), TypeTag::ListOf(b)) => tags_equal(a, b),
        (TypeTag::Record(a), TypeTag::Record(b)) => std::ptr::eq(*a, *b),
        _ => false,
    }
}

/// Unify two terminal types: equal stays, any `ANY` widens to `ANY`
/// (unidirectional — `ANY` never narrows to concrete), disjoint
/// concrete types fail naming both.
fn unify(left: TypeTag, right: TypeTag, ctx: &str) -> Result<TypeTag> {
    if tags_equal(&left, &right) {
        return Ok(left);
    }
    if matches!(left, TypeTag::Any) || matches!(right, TypeTag::Any) {
        return Ok(TypeTag::Any);
    }
    bail!("{ctx}: cannot unify {} and {}", left.name(), right.name());
}

fn classify_guard(guard: Option<&GuardExpr>) -> GuardClass {
    match guard {
        None => GuardClass::Unconditional,
        Some(expr) => {
            if !oxdock_parser::guard_satisfiable(expr) {
                // Unsatisfiable (e.g. `[os:macos]` + `[os:windows]` on one
                // step): never executes, excluded everywhere.
                GuardClass::Dead
            } else if oxdock_parser::guard_valid(expr) {
                GuardClass::Unconditional
            } else {
                GuardClass::Conditional
            }
        }
    }
}

/// Strip the `$` sigil the way execution does (`assign`/`set_var_value`
/// trim it before registry calls), so lookups match declarations
/// regardless of spelling.
fn clean_var(name: &str) -> String {
    name.trim_start_matches('$').to_string()
}

/// Display form of a callee name: the runtime boundary messages use
/// the bare name (`INT()`), never the qualified registry key.
fn base_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Step prefix, or empty for RPN contexts whose runtime errors carry
/// no step number.
fn loc(at: Option<&str>) -> String {
    at.map(|s| format!("{s}: ")).unwrap_or_default()
}

/// Resolved callee for one call: parameter shapes (absent when the
/// entry declares none), return type, and host closed-set metadata.
struct Callee {
    params: Option<Vec<(String, TypeTag)>>,
    ret: StaticTy,
    host_params: Option<Vec<FuncParam>>,
}

/// Outcome of walking one body: whether fallthrough is reachable,
/// whether a live `RETURN` exists on some path, and the collected
/// terminal types.
struct WalkOut {
    falls_through: bool,
    has_return: bool,
    types: Vec<TypeTag>,
}

impl Env {
    fn one_frame(bindings: Vec<(String, TypeTag)>) -> Self {
        let mut top = HashMap::new();
        for (name, tag) in bindings {
            top.insert(name, vec![Binding { tag, guard: None }]);
        }
        Self { frames: vec![top] }
    }

    /// Fork for a scoped body: clone the stack and push a fresh frame,
    /// mirroring `push_scope`. Discarding the fork is exact — arm and
    /// loop declarations never leak at runtime.
    fn fork(&self) -> Self {
        let mut fork = self.clone();
        fork.frames.push(HashMap::new());
        fork
    }

    /// Read a binding under a read-site guard: first frame outward
    /// holding the name wins (shadowing); only entries overlapping
    /// the site are live on that path, and they unify. No overlap
    /// means the name is never bound where the read executes — the
    /// same undeclared error runtime would produce there.
    fn read_guarded(&self, key: &str, site: Option<&GuardExpr>) -> Result<TypeTag> {
        Ok(self.read_entry_guarded(key, site)?.0)
    }

    /// Same as `read_guarded`, plus the precise liveness guard for
    /// re-seeding (REMOTE headers): the union of surviving entry
    /// guards conjoined with the site. A value crossing scopes is
    /// live exactly where both hold.
    fn read_entry_guarded(
        &self,
        key: &str,
        site: Option<&GuardExpr>,
    ) -> Result<(TypeTag, Option<GuardExpr>)> {
        for frame in self.frames.iter().rev() {
            if let Some(entries) = frame.get(key) {
                let live: Vec<&Binding> = entries
                    .iter()
                    .filter(|entry| oxdock_parser::guards_overlap(entry.guard.as_ref(), site))
                    .collect();
                if live.is_empty() {
                    bail!("undefined variable ${key}");
                }
                // Coverage: an unguarded live entry fires everywhere,
                // otherwise the site must imply the disjunction of
                // live binding guards. That is, site AND NOT(each
                // live guard) must be unsatisfiable — else some
                // reachable path reads unbound.
                let any_bare = live.iter().any(|entry| entry.guard.is_none());
                if !any_bare {
                    let mut atoms: Vec<GuardExpr> = Vec::new();
                    if let Some(site_expr) = site {
                        atoms.push(site_expr.clone());
                    }
                    for entry in &live {
                        if let Some(guard) = entry.guard.as_ref() {
                            atoms.push(GuardExpr::Not(Box::new(guard.clone())));
                        }
                    }
                    if oxdock_parser::guard_satisfiable(&GuardExpr::All(atoms)) {
                        bail!(
                            "variable '${key}' is not guaranteed to be defined on all reachable paths for this step"
                        );
                    }
                }
                let mut unified: Option<TypeTag> = None;
                let mut union: Option<GuardExpr> = None;
                for entry in live {
                    unified = Some(match unified {
                        None => entry.tag,
                        Some(acc) => {
                            unify(acc, entry.tag, &format!("${key} across platform guards"))?
                        }
                    });
                    union = match (union, entry.guard.clone()) {
                        (None, None) => None,
                        (Some(g), None) | (None, Some(g)) => Some(g),
                        (Some(g1), Some(g2)) => Some(GuardExpr::Or(vec![g1, g2])),
                    };
                }
                let guard = match (site.cloned(), union) {
                    (None, union) => union,
                    (Some(site), None) => Some(site),
                    (Some(site), Some(union)) => Some(GuardExpr::All(vec![site, union])),
                };
                return Ok((unified.expect("live never empty"), guard));
            }
        }
        bail!("undefined variable ${key}");
    }

    fn is_declared(&self, key: &str) -> bool {
        self.frames
            .iter()
            .rev()
            .any(|frame| frame.contains_key(key))
    }

    /// Declare in the top frame. A duplicate whose guard overlaps any
    /// live entry mirrors the runtime redeclaration error; a disjoint
    /// one (e.g. `[os:windows]` after `[os:macos]`) shadows across exclusive
    /// paths and is allowed.
    fn declare(
        &mut self,
        key: String,
        tag: TypeTag,
        guard: Option<&GuardExpr>,
        idx: usize,
    ) -> Result<()> {
        if let Some(frame) = self.frames.last()
            && let Some(entries) = frame.get(&key)
        {
            for entry in entries {
                if oxdock_parser::guards_overlap(entry.guard.as_ref(), guard) {
                    bail!(
                        "step {}: redeclaration error: ${key} already declared in this scope; \
                         use ${key} = ... to mutate",
                        idx + 1
                    );
                }
            }
        }
        if let Some(frame) = self.frames.last_mut() {
            frame.entry(key).or_default().push(Binding {
                tag,
                guard: guard.cloned(),
            });
        }
        Ok(())
    }

    fn insert_top(&mut self, key: String, tag: TypeTag, guard: Option<&GuardExpr>) {
        if let Some(frame) = self.frames.last_mut() {
            frame.insert(
                key,
                vec![Binding {
                    tag,
                    guard: guard.cloned(),
                }],
            );
        }
    }
}

impl<'a, P: ProcessManager> Checker<'a, P> {
    fn new(exec: &'a ExecState<P>) -> Self {
        let mut hosts = HashMap::new();
        for meta in exec.functions.all_metas() {
            hosts.insert(meta.name.clone(), meta);
        }
        Self {
            exec,
            funcs: HashMap::new(),
            hosts,
            inferred: HashMap::new(),
            inferring: Vec::new(),
            visits: 0,
        }
    }

    fn func_keys(&self) -> Vec<String> {
        self.funcs.keys().cloned().collect()
    }

    /// Phase A: collect every `FuncDef` in the tree, resolving param
    /// type words now (the same `resolve_tag` execution uses, so an
    /// unknown word fails here with the identical message). Dead
    /// steps are skipped: unregistered code is never checked.
    /// Each function is keyed by bare and `SCRIPT::`-qualified name
    /// so calls resolve regardless of parser qualification.
    fn collect(&mut self, steps: &[Step]) -> Result<()> {
        for step in steps {
            if matches!(classify_guard(step.guard.as_ref()), GuardClass::Dead) {
                continue;
            }
            self.collect_step(step)?;
        }
        Ok(())
    }

    fn collect_step(&mut self, step: &Step) -> Result<()> {
        match &step.kind {
            StepKind::FuncDef { name, params, body } => {
                let mut resolved = Vec::with_capacity(params.len());
                for (pname, ptype) in params {
                    let tag = self
                        .exec
                        .resolve_tag(ptype)
                        .map_err(|err| anyhow::anyhow!("function `{name}`: {err:#}"))?;
                    resolved.push((clean_var(pname), tag));
                }
                let info = FuncInfo {
                    params: resolved,
                    body: body.clone(),
                };
                self.funcs.insert(name.clone(), info);
                self.funcs.insert(
                    format!("SCRIPT::{name}"),
                    FuncInfo {
                        params: self.funcs[name].params.clone(),
                        body: self.funcs[name].body.clone(),
                    },
                );
                // Nested definitions register if execution reaches
                // them; collect their bodies too.
                let nested = self.funcs[name].body.clone();
                self.collect(&nested)?;
            }
            StepKind::If {
                then_body,
                else_ifs,
                else_body,
                ..
            } => {
                self.collect(then_body)?;
                for (_, arm) in else_ifs {
                    self.collect(arm)?;
                }
                if let Some(arm) = else_body {
                    self.collect(arm)?;
                }
            }
            StepKind::For { body, .. } | StepKind::While { body, .. } => {
                self.collect(body)?;
            }
            StepKind::AsyncBlock { body } => self.collect(body)?,
            _ => {}
        }
        Ok(())
    }

    /// Top-level steps run with an empty environment: undeclared
    /// variables, bad arities, and mistyped arguments fail here, the
    /// same as inside any function.
    fn check_top_level(&mut self, steps: &[Step]) -> Result<()> {
        let mut env = Env::one_frame(Vec::new());
        let scope = Scope {
            label: "top level",
            ret: RetCtx::Top,
            loops: 0,
            site: None,
        };
        let out = self.walk_body(steps, &mut env, &scope, 0)?;
        let _ = out;
        Ok(())
    }

    /// Phase B: inferred return type of one script function,
    /// memoized. Recursive and forward references widen to `ANY`
    /// while inference is in progress.
    fn func_return(&mut self, key: &str, depth: usize) -> Result<StaticTy> {
        if let Some(known) = self.inferred.get(key) {
            return Ok(*known);
        }
        if self.inferring.iter().any(|name| name == key) {
            return Ok(StaticTy::Ty(TypeTag::Any));
        }
        let info = self.funcs.get(key).cloned();
        let Some(info) = info else {
            return Ok(StaticTy::Ty(TypeTag::Any));
        };
        self.inferring.push(key.to_string());
        let result = self.check_function_body(key, &info, depth);
        self.inferring.pop();
        let ty = result?;
        self.inferred.insert(key.to_string(), ty);
        Ok(ty)
    }

    /// Check one function body: terminal unification, MissingReturn,
    /// VOID classification.
    fn check_function_body(
        &mut self,
        key: &str,
        info: &FuncInfo,
        depth: usize,
    ) -> Result<StaticTy> {
        if depth > MAX_STATIC_DEPTH {
            bail!("static analysis depth exceeded in function `{key}`");
        }
        let mut env = Env::one_frame(info.params.clone());
        let scope = Scope {
            label: key,
            ret: RetCtx::Func(key.to_string()),
            loops: 0,
            site: None,
        };
        let out = self.walk_body(&info.body, &mut env, &scope, depth + 1)?;
        if out.has_return && out.falls_through {
            bail!(
                "function `{key}` may fall through with no RETURN on some path; \
                 add an unconditional RETURN or EXIT"
            );
        }
        if !out.has_return {
            return Ok(StaticTy::Void);
        }
        let mut unified: Option<TypeTag> = None;
        for ty in out.types {
            unified = Some(match unified {
                None => ty,
                Some(acc) => unify(acc, ty, &format!("function `{key}`"))?,
            });
        }
        Ok(StaticTy::Ty(unified.expect("has_return implies a type")))
    }

    /// Walk one body sequentially. A conditionally-guarded step keeps
    /// its returns and types but never terminates the walk: the path
    /// passes through it. Branches and loops fork the environment and
    /// discard it (arm and loop declarations are scope-local at
    /// runtime); `SET` cannot change types (runtime coerces), so no
    /// merge is needed — discarding is exact.
    #[allow(clippy::too_many_lines)]
    fn walk_body(
        &mut self,
        steps: &[Step],
        env: &mut Env,
        scope: &Scope,
        depth: usize,
    ) -> Result<WalkOut> {
        if depth > MAX_STATIC_DEPTH {
            bail!("static analysis depth exceeded in `{}`", scope.label);
        }
        let base_frames = env.frames.len();
        let mut out = WalkOut {
            falls_through: true,
            has_return: false,
            types: Vec::new(),
        };
        for (idx, step) in steps.iter().enumerate() {
            self.visits += 1;
            let scoped = Scope {
                label: scope.label,
                ret: scope.ret.clone(),
                loops: scope.loops,
                site: step.guard.clone(),
            };
            let scope = &scoped;
            // Positional scopes mirror execution exactly: pushed
            // before the guard check, restored after the step, even
            // for dead steps (net effect zero there).
            for _ in 0..step.scope_enter {
                env.frames.push(HashMap::new());
            }
            match classify_guard(step.guard.as_ref()) {
                GuardClass::Dead => {}
                GuardClass::Conditional => {
                    let inner = self.walk_step(step, env, scope, idx, depth)?;
                    out.has_return |= inner.has_return;
                    out.types.extend(inner.types);
                }
                GuardClass::Unconditional => {
                    let inner = self.walk_step(step, env, scope, idx, depth)?;
                    out.has_return |= inner.has_return;
                    out.types.extend(inner.types);
                    if !inner.falls_through {
                        out.falls_through = false;
                        return Ok(out);
                    }
                }
            }
            for _ in 0..step.scope_exit {
                if env.frames.len() > base_frames {
                    env.frames.pop();
                }
            }
            // Early terminal returns above skip the pops and discard
            // the env, matching observable runtime behavior (restore
            // then propagate out of the body).
        }
        Ok(out)
    }

    /// Walk a single live step. The returned `falls_through` is
    /// relative to the step alone; the caller applies guard semantics.
    fn walk_step(
        &mut self,
        step: &Step,
        env: &mut Env,
        scope: &Scope,
        idx: usize,
        depth: usize,
    ) -> Result<WalkOut> {
        let live = WalkOut {
            falls_through: true,
            has_return: false,
            types: Vec::new(),
        };
        match &step.kind {
            StepKind::Return { expr } => {
                match &scope.ret {
                    RetCtx::Func(_) | RetCtx::Block | RetCtx::AsyncValue => {}
                    RetCtx::Top => {
                        bail!(
                            "step {}: RETURN outside function, ASYNC task, or LET block",
                            idx + 1
                        );
                    }
                    RetCtx::AsyncStmt => {
                        bail!("step {}: RETURN cannot cross ASYNC boundary", idx + 1);
                    }
                }
                let ty = self.infer_expr(expr, env, scope, idx, depth + 1)?;
                let mut out = live;
                out.has_return = true;
                out.falls_through = false;
                if let StaticTy::Ty(tag) = ty {
                    out.types.push(tag);
                }
                Ok(out)
            }
            StepKind::Break => self.walk_break("BREAK", scope, idx),
            StepKind::Continue => self.walk_break("CONTINUE", scope, idx),
            StepKind::Exit(_) => {
                self.check_step_args(&step.kind, env, scope, idx, depth)?;
                Ok(WalkOut {
                    falls_through: false,
                    has_return: false,
                    types: Vec::new(),
                })
            }
            StepKind::If {
                cond,
                then_body,
                else_ifs,
                else_body,
            } => {
                self.infer_expr(cond, env, scope, idx, depth + 1)?;
                let mut arms_terminal = true;
                let mut out = live;
                let mut arms = vec![then_body];
                for (else_cond, arm) in else_ifs {
                    self.infer_expr(else_cond, env, scope, idx, depth + 1)?;
                    arms.push(arm);
                }
                for arm in arms {
                    let mut fork = env.fork();
                    let arm_out = self.walk_body(arm, &mut fork, scope, depth + 1)?;
                    out.has_return |= arm_out.has_return;
                    out.types.extend(arm_out.types);
                    arms_terminal &= !arm_out.falls_through;
                }
                match else_body {
                    Some(arm) => {
                        let mut fork = env.fork();
                        let arm_out = self.walk_body(arm, &mut fork, scope, depth + 1)?;
                        out.has_return |= arm_out.has_return;
                        out.types.extend(arm_out.types);
                        arms_terminal &= !arm_out.falls_through;
                    }
                    None => arms_terminal = false,
                }
                out.falls_through = !arms_terminal;
                Ok(out)
            }
            StepKind::For {
                key_var,
                key_type,
                var,
                var_type,
                in_expr,
                body,
            } => {
                self.infer_expr(in_expr, env, scope, idx, depth + 1)?;
                let val_tag = self
                    .exec
                    .resolve_tag(var_type)
                    .map_err(|err| anyhow::anyhow!("step {}: {err:#}", idx + 1))?;
                let mut fork = env.fork();
                fork.insert_top(clean_var(var), val_tag, step.guard.as_ref());
                if let Some(key) = key_var {
                    let key_tag = match key_type {
                        Some(name) => self
                            .exec
                            .resolve_tag(name)
                            .map_err(|err| anyhow::anyhow!("step {}: {err:#}", idx + 1))?,
                        None => TypeTag::Int,
                    };
                    fork.insert_top(clean_var(key), key_tag, step.guard.as_ref());
                }
                let mut inner_scope = scope.clone();
                inner_scope.loops += 1;
                let body_out = self.walk_body(body, &mut fork, &inner_scope, depth + 1)?;
                let mut out = live;
                // Loops may trip zero times: fallthrough always
                // survives, but body returns are live paths.
                out.has_return = body_out.has_return;
                out.types = body_out.types;
                Ok(out)
            }
            StepKind::While { cond, body } => {
                self.infer_expr(cond, env, scope, idx, depth + 1)?;
                let mut fork = env.fork();
                let mut inner_scope = scope.clone();
                inner_scope.loops += 1;
                let body_out = self.walk_body(body, &mut fork, &inner_scope, depth + 1)?;
                let mut out = live;
                out.has_return = body_out.has_return;
                out.types = body_out.types;
                Ok(out)
            }
            StepKind::FuncDef { name, params, body } => {
                let mut resolved = Vec::with_capacity(params.len());
                for (pname, ptype) in params {
                    let tag = self
                        .exec
                        .resolve_tag(ptype)
                        .map_err(|err| anyhow::anyhow!("function `{name}`: {err:#}"))?;
                    resolved.push((clean_var(pname), tag));
                }
                let info = FuncInfo {
                    params: resolved,
                    body: body.clone(),
                };
                self.check_function_body(name, &info, depth + 1)?;
                Ok(live)
            }
            StepKind::Call { name, args } => {
                self.check_call(name, args, env, scope, idx, depth + 1)?;
                Ok(live)
            }
            StepKind::Assign {
                var,
                decl_type,
                expr,
            } => {
                let rhs = self.infer_expr(expr, env, scope, idx, depth + 1)?;
                if matches!(rhs, StaticTy::Void) {
                    bail!(
                        "step {}: VOID block has no value to bind to `${var}`",
                        idx + 1
                    );
                }
                let tag = self
                    .exec
                    .resolve_tag(decl_type)
                    .map_err(|err| anyhow::anyhow!("step {}: {err:#}", idx + 1))?;
                let key = clean_var(var);
                env.declare(key, tag, step.guard.as_ref(), idx)?;
                Ok(live)
            }
            StepKind::AssignCapture {
                var,
                decl_type,
                cmd,
            } => {
                // The captured command runs first at runtime, so its
                // expressions check before the declaration lands.
                self.walk_wrapped(cmd, env, scope, idx, depth + 1)?;
                let tag = self
                    .exec
                    .resolve_tag(decl_type)
                    .map_err(|err| anyhow::anyhow!("step {}: {err:#}", idx + 1))?;
                let key = clean_var(var);
                env.declare(key, tag, step.guard.as_ref(), idx)?;
                Ok(live)
            }
            StepKind::Set { var, expr } => {
                let key = clean_var(var);
                if !env.is_declared(&key) {
                    bail!(
                        "step {}: undeclared variable ${key}: declare it first with LET ${key}: TYPE = ...",
                        idx + 1
                    );
                }
                // Site-aware liveness: the binding must hold on this
                // step's path, not merely somewhere in scope. A bare
                // existence check would accept a mutation on a path
                // where the variable never binds.
                let _ = env.read_guarded(&key, scope.site.as_ref())?;
                self.infer_expr(expr, env, scope, idx, depth + 1)?;
                Ok(live)
            }
            StepKind::AwaitCapture {
                out_var,
                out_type,
                task_var,
            } => {
                // The handle resolves before the output declares.
                Self::check_name_read(env, task_var, scope, idx)?;
                let tag = self
                    .exec
                    .resolve_tag(out_type)
                    .map_err(|err| anyhow::anyhow!("step {}: {err:#}", idx + 1))?;
                let key = clean_var(out_var);
                env.declare(key, tag, step.guard.as_ref(), idx)?;
                Ok(live)
            }
            StepKind::AsyncBlock { body } => {
                // Bare tasks never publish a value: control never
                // crosses the thread boundary at runtime.
                let mut fork = env.fork();
                let task = Scope {
                    label: scope.label,
                    ret: RetCtx::AsyncStmt,
                    loops: 0,
                    site: scope.site.clone(),
                };
                self.walk_body(body, &mut fork, &task, depth + 1)?;
                Ok(live)
            }
            StepKind::AssignAsync {
                var,
                decl_type,
                body,
            } => {
                // `LET $o = ASYNC` bodies publish through `AWAIT`;
                // returns belong to the task, never the enclosing
                // function.
                let mut fork = env.fork();
                let task = Scope {
                    label: scope.label,
                    ret: RetCtx::AsyncValue,
                    loops: 0,
                    site: scope.site.clone(),
                };
                self.walk_body(body, &mut fork, &task, depth + 1)?;
                let tag = self
                    .exec
                    .resolve_tag(decl_type)
                    .map_err(|err| anyhow::anyhow!("step {}: {err:#}", idx + 1))?;
                let key = clean_var(var);
                env.insert_top(key, tag, step.guard.as_ref());
                Ok(live)
            }
            StepKind::Timeout { body, .. } => {
                // A dynamic duration evaluates before the body runs.
                self.check_step_args(&step.kind, env, scope, idx, depth)?;
                let inner = self.walk_body(body, env, scope, depth + 1)?;
                let mut out = live;
                // TIMEOUT bounds execution in place: a `RETURN`
                // inside still returns from the enclosing function.
                out.has_return = inner.has_return;
                out.types = inner.types;
                out.falls_through = inner.falls_through;
                if !inner.falls_through {
                    return Ok(out);
                }
                Ok(out)
            }
            StepKind::RemoteBlock {
                target, vars, body, ..
            } => {
                // Sealed scope: the guest starts empty and only
                // header-listed names cross, seeded from the outer
                // environment. A body `LET` over an injected name
                // shadows within the seal, exactly like a braced
                // block. Returns never propagate: the shipped scope
                // runs as ordinary top-level steps.
                let mut sealed = Env::one_frame(Vec::new());
                for name in vars {
                    let key = clean_var(name);
                    let (tag, guard) =
                        env.read_entry_guarded(&key, step.guard.as_ref())
                            .map_err(|_| {
                                anyhow::anyhow!("REMOTE '{target}' uses unknown variable ${key}")
                            })?;
                    sealed.insert_top(key, tag, guard.as_ref());
                }
                // Body declarations shadow injected names: the seal
                // pushes a scope exactly like a braced block.
                sealed.frames.push(HashMap::new());
                let remote = Scope {
                    label: scope.label,
                    ret: RetCtx::Top,
                    loops: 0,
                    site: None,
                };
                self.walk_body(body, &mut sealed, &remote, depth + 1)?;
                Ok(live)
            }
            StepKind::WithIo { bindings, cmd } => {
                // Bindings resolve before the wrapped command runs.
                Self::check_pipe_bindings(env, bindings, scope, idx)?;
                self.walk_wrapped(cmd, env, scope, idx, depth + 1)
            }
            StepKind::ReadLine { var } => {
                // Declares `STRING` when absent, mutates (type kept)
                // when present — mirroring `read_line` exactly.
                let key = clean_var(var);
                if !env.is_declared(&key) {
                    env.insert_top(key, TypeTag::String, step.guard.as_ref());
                }
                Ok(live)
            }
            _ => {
                // Every remaining variant is a simple command: all its
                // argument expressions (including `{{ }}` template
                // fragments) read through inference, and handle/pipe/list
                // names read through site-aware liveness. Control-flow,
                // declaration, and wrapped forms return through their own
                // arms above, so nothing here double-checks.
                self.check_step_args(&step.kind, env, scope, idx, depth)?;
                match &step.kind {
                    StepKind::Await { var } | StepKind::Cancel { var } => {
                        Self::check_name_read(env, var, scope, idx)?;
                    }
                    StepKind::WithIoBlock { bindings } => {
                        Self::check_pipe_bindings(env, bindings, scope, idx)?;
                    }
                    StepKind::ListAppend { list, .. } => {
                        let tag = Self::check_name_read(env, list, scope, idx)?;
                        if !tags_equal(&tag, &TypeTag::List) {
                            bail!(
                                "step {}: LIST_APPEND ${}: variable is {}, not LIST",
                                idx + 1,
                                clean_var(list),
                                tag.name()
                            );
                        }
                    }
                    _ => {}
                }
                Ok(live)
            }
        }
    }

    /// Check every argument expression of one simple command through
    /// `infer_expr`: `Arg::Expr` operands and `{{ }}` template
    /// fragments (`Arg::Parts`) read live bindings; plain strings
    /// carry nothing. Built on the exhaustive `walk_exprs` visitor,
    /// so a new expression-carrying variant fails compilation in the
    /// parser instead of slipping past this pass.
    fn check_step_args(
        &mut self,
        kind: &StepKind,
        env: &mut Env,
        scope: &Scope,
        idx: usize,
        depth: usize,
    ) -> Result<()> {
        let mut failed: Option<anyhow::Error> = None;
        kind.walk_exprs(&mut |expr| {
            if failed.is_none()
                && let Err(err) = self.infer_expr(expr, env, scope, idx, depth + 1)
            {
                failed = Some(err);
            }
        });
        if let Some(err) = failed {
            return Err(err);
        }
        Ok(())
    }

    /// Static counterpart of a runtime name lookup (`get_var` and
    /// friends): the binding must be declared and live on this
    /// step's path, mirroring the `undefined variable` failure one
    /// for one.
    fn check_name_read(env: &Env, name: &str, scope: &Scope, idx: usize) -> Result<TypeTag> {
        let key = clean_var(name);
        if !env.is_declared(&key) {
            bail!("step {}: undefined variable ${key}", idx + 1);
        }
        env.read_guarded(&key, scope.site.as_ref())
    }

    /// Static counterpart of `resolve_pipe_handle`: a `$var` pipe
    /// endpoint must be a live `PIPE` binding, mirroring the runtime
    /// `TypeMismatch` one for one.
    fn check_pipe_bindings(
        env: &Env,
        bindings: &[IoBinding],
        scope: &Scope,
        idx: usize,
    ) -> Result<()> {
        for binding in bindings {
            if let Some(PipeTarget::Var(name)) = &binding.pipe {
                let tag = Self::check_name_read(env, name, scope, idx)?;
                if !tags_equal(&tag, &TypeTag::Pipe) {
                    bail!(
                        "step {}: TypeMismatch: expected PIPE, got {}",
                        idx + 1,
                        tag.name()
                    );
                }
            }
        }
        Ok(())
    }

    /// `BREAK`/`CONTINUE`: legal only inside a loop body. Anywhere
    /// else the boundary error mirrors execution one for one.
    fn walk_break(&mut self, word: &str, scope: &Scope, idx: usize) -> Result<WalkOut> {
        if scope.loops > 0 {
            return Ok(WalkOut {
                falls_through: true,
                has_return: false,
                types: Vec::new(),
            });
        }
        match &scope.ret {
            RetCtx::Top => bail!("step {}: {word} outside loop", idx + 1),
            RetCtx::Func(name) => {
                bail!(
                    "step {}: {word} cannot cross function boundary (in {name}())",
                    idx + 1
                );
            }
            RetCtx::Block => {
                bail!("step {}: {word} cannot cross block boundary", idx + 1);
            }
            RetCtx::AsyncValue | RetCtx::AsyncStmt => {
                bail!("step {}: {word} cannot cross ASYNC boundary", idx + 1);
            }
        }
    }

    /// Walk a single boxed `StepKind` (the `WithIo` payload) as if it
    /// were a step: the enclosing guard was already classified by the
    /// caller, so this inherits the live context directly.
    fn walk_wrapped(
        &mut self,
        cmd: &StepKind,
        env: &mut Env,
        scope: &Scope,
        idx: usize,
        depth: usize,
    ) -> Result<WalkOut> {
        let step = Step {
            guard: None,
            kind: cmd.clone(),
            scope_enter: 0,
            scope_exit: 0,
        };
        self.walk_step(&step, env, scope, idx, depth)
    }

    /// Static counterpart of `coerce_value` for argument positions.
    /// `Ok` where runtime coercion provably succeeds or decides by
    /// value; `Err` where it provably fails — plus the `ANY` gate:
    /// an untyped value into a concrete parameter names the
    /// `LET $x: T` binding that would coerce it.
    fn check_arg(&mut self, expected: &TypeTag, actual: StaticTy, ctx: &str) -> Result<()> {
        let StaticTy::Ty(actual) = actual else {
            bail!("{ctx}: VOID has no value to pass");
        };
        if matches!(expected, TypeTag::Any) {
            return Ok(());
        }
        if matches!(actual, TypeTag::Any) {
            bail!(
                "{ctx}: untyped ANY value flows into {} parameter; \
                 bind through LET $x: {} first",
                expected.name(),
                expected.name()
            );
        }
        if tags_equal(expected, &actual) {
            return Ok(());
        }
        // Mirror coerce_value/coerce_scalar: allow pairs the runtime
        // decides by value; reject pairs that always bail.
        let ok = match (&actual, expected) {
            (TypeTag::String, TypeTag::Int)
            | (TypeTag::String, TypeTag::Float)
            | (TypeTag::String, TypeTag::Bool)
            | (TypeTag::String, TypeTag::Duration)
            | (TypeTag::String, TypeTag::Path) => true,
            (TypeTag::Int, TypeTag::String) | (TypeTag::Int, TypeTag::Float) => true,
            (TypeTag::Float, TypeTag::String) | (TypeTag::Float, TypeTag::Int) => true,
            (TypeTag::Bool, TypeTag::String)
            | (TypeTag::Duration, TypeTag::String)
            | (TypeTag::Path, TypeTag::String) => true,
            // Container words satisfy shaped expectations; the
            // runtime shape walk is the gate. Shaped values satisfy
            // bare words; they are those words.
            (TypeTag::List, TypeTag::ListOf(_))
            | (TypeTag::ListOf(_), TypeTag::List)
            | (TypeTag::Map, TypeTag::Record(_))
            | (TypeTag::Record(_), TypeTag::Map) => true,
            _ => false,
        };
        if ok {
            return Ok(());
        }
        bail!(
            "{ctx}: TypeMismatch: expected {}, got {}",
            expected.name(),
            actual.name()
        );
    }

    /// Resolve a callee: script functions (bare or `SCRIPT::`-qualified)
    /// win over host metas, matching runtime shadowing. Unknown names
    /// yield `ANY`: the parse-time unknown-function gate owns that
    /// error, not this pass.
    fn resolve_call(&mut self, name: &str, depth: usize) -> Result<Callee> {
        if let Some(info) = self.funcs.get(name) {
            let info = info.clone();
            let ret = self.func_return_owned(&info, name, depth)?;
            let params = info.params;
            return Ok(Callee {
                params: Some(params),
                ret,
                host_params: None,
            });
        }
        if let Some(meta) = self.hosts.get(name) {
            let meta = meta.clone();
            let ret = meta
                .returns
                .map(StaticTy::Ty)
                .unwrap_or(StaticTy::Ty(TypeTag::Any));
            // A missing param list (virtual entries like INSPECT)
            // means no static arity or argument checks: the dedicated
            // AST path owns that call shape.
            let params = meta.params.as_deref().map(|list| {
                list.iter()
                    .map(|param| (param.name.clone(), param.param_type.unwrap_or(TypeTag::Any)))
                    .collect()
            });
            // NOTE: host `allowed` sets are checked from the meta
            // below; the tag triple here carries only shape.
            return Ok(Callee {
                params,
                ret,
                host_params: meta.params.clone(),
            });
        }
        Ok(Callee {
            params: None,
            ret: StaticTy::Ty(TypeTag::Any),
            host_params: None,
        })
    }

    /// Memoized script return with recursion guard, keyed by the
    /// registry key the call resolved through.
    fn func_return_owned(&mut self, info: &FuncInfo, key: &str, depth: usize) -> Result<StaticTy> {
        if let Some(known) = self.inferred.get(key) {
            return Ok(*known);
        }
        if self.inferring.iter().any(|name| name == key) {
            return Ok(StaticTy::Ty(TypeTag::Any));
        }
        self.inferring.push(key.to_string());
        let result = self.check_function_body(key, info, depth);
        self.inferring.pop();
        let ty = result?;
        self.inferred.insert(key.to_string(), ty);
        Ok(ty)
    }

    /// Check one call: arity, then each argument against its parameter
    /// (host `allowed` sets enforced on string literals from the same
    /// tokens that render the signature). Returns the callee return
    /// type with `VOID` mapped to `STRING`, mirroring
    /// `Flow::Done => Ok("")`.
    fn check_call(
        &mut self,
        name: &str,
        args: &[Expr],
        env: &mut Env,
        scope: &Scope,
        idx: usize,
        depth: usize,
    ) -> Result<StaticTy> {
        // Order mirrors execution: the arity gate fires before any
        // argument evaluates, so undeclared names in extra arguments
        // never surface. (`native_arity_failure_...` pins this.)
        let callee = self.resolve_call(name, depth)?;
        if let Some(expected) = callee.params.as_deref()
            && args.len() != expected.len()
        {
            bail!(
                "step {}: {}() expects {} argument(s), got {}",
                idx + 1,
                base_name(name),
                expected.len(),
                args.len()
            );
        }
        let mut tys = Vec::with_capacity(args.len());
        let mut literals: Vec<Option<&str>> = Vec::with_capacity(args.len());
        for arg in args.iter() {
            let actual = self.infer_expr(arg, env, scope, idx, depth + 1)?;
            let literal = match arg {
                Expr::Literal(value) => value.as_str(),
                _ => None,
            };
            tys.push(actual);
            literals.push(literal);
        }
        self.apply_checked(
            name,
            &callee,
            &tys,
            &literals,
            Some(&format!("step {}", idx + 1)),
        )
    }

    /// Apply already-inferred argument types to a resolved callee:
    /// per-parameter shape plus closed-set membership for string
    /// literals. Shared by AST calls (literals available) and RPN
    /// calls (shapes only).
    fn apply_checked(
        &mut self,
        name: &str,
        callee: &Callee,
        arg_tys: &[StaticTy],
        literals: &[Option<&str>],
        at: Option<&str>,
    ) -> Result<StaticTy> {
        let display = base_name(name);
        let Callee {
            params,
            ret,
            host_params,
        } = callee;
        if let Some(expected) = params.as_deref()
            && arg_tys.len() != expected.len()
        {
            bail!(
                "{}{display}() expects {} argument(s), got {}",
                loc(at),
                expected.len(),
                arg_tys.len()
            );
        }
        for (position, actual) in arg_tys.iter().enumerate() {
            let Some(expected) = params.as_deref().and_then(|list| list.get(position)) else {
                continue;
            };
            let (pname, ptype) = expected;
            let pat = format!("{}{display}() argument `${pname}`", loc(at));
            self.check_arg(ptype, *actual, &pat)?;
            if let Some(host_list) = host_params.as_deref()
                && let Some(param) = host_list.get(position)
                && let Some(allowed) = param.allowed
                && let Some(Some(text)) = literals.get(position)
                && !allowed.contains(text)
            {
                bail!("{pat} must be one of: {}, got {text:?}", allowed.join(", "));
            }
        }
        match *ret {
            StaticTy::Void => Ok(StaticTy::Ty(TypeTag::String)),
            other => Ok(other),
        }
    }
}

/// Map a literal word to its tag via the script vocabulary; custom
/// words (opaque handles) widen to `ANY`.
fn literal_tag(value: &Value) -> TypeTag {
    TypeTag::builtin(value.type_name()).unwrap_or(TypeTag::Any)
}

/// Binary arithmetic rule, mirroring `apply_arith`: `INT op INT` is
/// `INT`, any `FLOAT` among numerics promotes, `ANY` defers, and
/// concrete non-numerics are a static Type Error (the evaluator
/// bails identically).
fn numeric_pair(left: StaticTy, right: StaticTy, at: Option<&str>) -> Result<StaticTy> {
    let (StaticTy::Ty(left), StaticTy::Ty(right)) = (left, right) else {
        bail!("{}VOID has no value in arithmetic", loc(at));
    };
    if matches!(left, TypeTag::Any) || matches!(right, TypeTag::Any) {
        return Ok(StaticTy::Ty(TypeTag::Any));
    }
    if matches!(
        (&left, &right),
        (TypeTag::Int, TypeTag::Int)
            | (TypeTag::Int, TypeTag::Float)
            | (TypeTag::Float, TypeTag::Int)
            | (TypeTag::Float, TypeTag::Float)
    ) {
        return Ok(StaticTy::Ty(
            if matches!(left, TypeTag::Int) && matches!(right, TypeTag::Int) {
                TypeTag::Int
            } else {
                TypeTag::Float
            },
        ));
    }
    bail!("{}Type Error: arithmetic requires Int or Float", loc(at));
}

/// Unary negation rule, mirroring `apply_neg`.
fn neg_rule(top: StaticTy, at: Option<&str>) -> Result<StaticTy> {
    match top {
        StaticTy::Ty(TypeTag::Int) => Ok(StaticTy::Ty(TypeTag::Int)),
        StaticTy::Ty(TypeTag::Float) => Ok(StaticTy::Ty(TypeTag::Float)),
        StaticTy::Ty(TypeTag::Any) => Ok(StaticTy::Ty(TypeTag::Any)),
        StaticTy::Void => bail!("{}VOID has no value to negate", loc(at)),
        StaticTy::Ty(other) => {
            bail!(
                "{}Type Error: unary '-' requires Int or Float, got {}",
                loc(at),
                other.name()
            )
        }
    }
}

/// Ordering comparison rule, mirroring `apply_compare`.
fn ordering_pair(left: StaticTy, right: StaticTy, at: Option<&str>) -> Result<()> {
    let (StaticTy::Ty(left), StaticTy::Ty(right)) = (left, right) else {
        bail!("{}VOID has no value to compare", loc(at));
    };
    if matches!(left, TypeTag::Any) || matches!(right, TypeTag::Any) {
        return Ok(());
    }
    let numeric = |tag: &TypeTag| matches!(tag, TypeTag::Int | TypeTag::Float);
    if numeric(&left) && numeric(&right) {
        return Ok(());
    }
    bail!(
        "{}Type Error: ordering comparison requires Int or Float",
        loc(at)
    );
}

impl<'a, P: ProcessManager> Checker<'a, P> {
    /// Infer an expression's static type, running nested checks
    /// (undeclared variables, nested calls) along the way.
    fn infer_expr(
        &mut self,
        expr: &Expr,
        env: &mut Env,
        scope: &Scope,
        idx: usize,
        depth: usize,
    ) -> Result<StaticTy> {
        if depth > MAX_STATIC_DEPTH {
            bail!("static analysis depth exceeded in `{}`", scope.label);
        }
        self.visits += 1;
        let at = || format!("step {}", idx + 1);
        match expr {
            Expr::Literal(value) => Ok(StaticTy::Ty(literal_tag(value))),
            Expr::Var(name) => {
                let key = clean_var(name);
                if !env.is_declared(&key) {
                    bail!("{}undefined variable ${key}", loc(Some(&at())));
                }
                Ok(StaticTy::Ty(env.read_guarded(&key, scope.site.as_ref())?))
            }
            Expr::Env(_) => Ok(StaticTy::Ty(TypeTag::String)),
            Expr::KeyPath { base, keys } => {
                let key = clean_var(base);
                if !env.is_declared(&key) {
                    bail!("{}undefined variable ${key}", loc(Some(&at())));
                }
                let mut current = env.read_guarded(&key, scope.site.as_ref())?;
                for part in keys {
                    match current {
                        TypeTag::Record(fields) => {
                            let Some(field) = fields.iter().find(|field| field.name == part) else {
                                bail!(
                                    "{}: unknown field '{part}'; expected fields: {}",
                                    at(),
                                    fields
                                        .iter()
                                        .map(|field| field.name)
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                );
                            };
                            current = field.ty;
                        }
                        // MAP keys and LIST indices resolve by value at
                        // runtime (missing keys, bounds); the shape
                        // stays unknown here.
                        TypeTag::Map | TypeTag::List | TypeTag::Any => {
                            return Ok(StaticTy::Ty(TypeTag::Any));
                        }
                        _ => {
                            bail!(
                                "{}: Cannot traverse into scalar value at key '{part}'",
                                at()
                            );
                        }
                    }
                }
                Ok(StaticTy::Ty(current))
            }
            Expr::List(_) => Ok(StaticTy::Ty(TypeTag::List)),
            Expr::Map(_) => Ok(StaticTy::Ty(TypeTag::Map)),
            Expr::Block(steps) => {
                // Blocks run in a fresh scope but read enclosing
                // bindings: fork, check, discard. Same terminal rules
                // as function bodies, with block-worded errors.
                let mut fork = env.fork();
                let out = self.walk_body(
                    steps,
                    &mut fork,
                    &Scope {
                        label: scope.label,
                        ret: RetCtx::Block,
                        loops: 0,
                        site: scope.site.clone(),
                    },
                    depth + 1,
                )?;
                if out.has_return && out.falls_through {
                    bail!(
                        "{}: block may fall through with no RETURN on some path",
                        at()
                    );
                }
                if !out.has_return {
                    return Ok(StaticTy::Void);
                }
                let mut unified: Option<TypeTag> = None;
                for ty in out.types {
                    unified = Some(match unified {
                        None => ty,
                        Some(acc) => unify(acc, ty, &at())?,
                    });
                }
                Ok(StaticTy::Ty(unified.expect("has_return implies a type")))
            }
            Expr::Call { name, args } => self.check_call(name, args, env, scope, idx, depth),
            Expr::Inspect(name) => {
                let key = clean_var(name);
                if !env.is_declared(&key) {
                    bail!("{}undefined variable ${key}", loc(Some(&at())));
                }
                env.read_guarded(&key, scope.site.as_ref())?;
                Ok(StaticTy::Ty(TypeTag::Any))
            }
            Expr::Compare { op, left, right } => {
                let left_ty = self.infer_expr(left, env, scope, idx, depth + 1)?;
                let right_ty = self.infer_expr(right, env, scope, idx, depth + 1)?;
                match op {
                    // Equality stringifies anything: always BOOL.
                    CompareOp::Eq | CompareOp::Ne => {}
                    // Ordering needs numerics (mirror apply_compare).
                    CompareOp::Lt | CompareOp::Le | CompareOp::Gt | CompareOp::Ge => {
                        ordering_pair(left_ty, right_ty, Some(&at()))?;
                    }
                }
                Ok(StaticTy::Ty(TypeTag::Bool))
            }
            Expr::Arithmetic { left, right, .. } => {
                let left_ty = self.infer_expr(left, env, scope, idx, depth + 1)?;
                let right_ty = self.infer_expr(right, env, scope, idx, depth + 1)?;
                // All four operators share one typing rule in
                // `apply_arith`; only operand shapes matter.
                numeric_pair(left_ty, right_ty, Some(&at()))
            }
            Expr::CompiledMath(ops) => self.infer_rpn(ops, env, scope, depth),
            Expr::FreshPipe => Ok(StaticTy::Ty(TypeTag::Pipe)),
            Expr::UnsignedIntBoundary(_) => Ok(StaticTy::Ty(TypeTag::Int)),
            Expr::Not(inner) => {
                self.infer_expr(inner, env, scope, idx, depth + 1)?;
                Ok(StaticTy::Ty(TypeTag::Bool))
            }
            Expr::Logical { left, .. } => {
                // Short-circuit: the right side may never execute at
                // runtime, so it is never checked here. Whatever runs
                // is still gated at execution.
                self.infer_expr(left, env, scope, idx, depth + 1)?;
                Ok(StaticTy::Ty(TypeTag::Bool))
            }
        }
    }

    /// Ordering (`<`, `<=`, `>`, `>=`) operand rule: numerics pass,
    /// `ANY` defers to runtime, concrete non-numerics are a static
    /// Type Error mirroring `apply_compare`.
    /// RPN stack simulation over inferred operand tags. Operators
    /// apply the same rules as their AST twins; stack underflow or a
    /// trailing stack depth other than one is an internal error (the
    /// lowering never emits those shapes).
    fn infer_rpn(
        &mut self,
        ops: &[MathOp],
        env: &mut Env,
        scope: &Scope,
        depth: usize,
    ) -> Result<StaticTy> {
        if depth > MAX_STATIC_DEPTH {
            bail!("static analysis depth exceeded in `{}`", scope.label);
        }
        let mut stack: Vec<StaticTy> = Vec::new();
        for op in ops {
            match op {
                MathOp::PushConst(value) => stack.push(StaticTy::Ty(literal_tag(value))),
                MathOp::LoadVar(name) => {
                    let key = clean_var(name);
                    if !env.is_declared(&key) {
                        bail!("undefined variable ${key}");
                    }
                    stack.push(StaticTy::Ty(env.read_guarded(&key, scope.site.as_ref())?));
                }
                MathOp::LoadEnv(_) => stack.push(StaticTy::Ty(TypeTag::String)),
                MathOp::LoadKeyPath { base, keys } => {
                    let key = clean_var(base);
                    if !env.is_declared(&key) {
                        bail!("undefined variable ${key}");
                    }
                    let mut current = env.read_guarded(&key, scope.site.as_ref())?;
                    for part in keys {
                        match current {
                            TypeTag::Record(fields) => {
                                let Some(field) = fields.iter().find(|field| field.name == *part)
                                else {
                                    bail!(
                                        "unknown field '{part}'; expected fields: {}",
                                        fields
                                            .iter()
                                            .map(|field| field.name)
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    );
                                };
                                current = field.ty;
                            }
                            TypeTag::Map | TypeTag::List | TypeTag::Any => {
                                current = TypeTag::Any;
                                break;
                            }
                            _ => {
                                bail!("Cannot traverse into scalar value at key '{part}'");
                            }
                        }
                    }
                    stack.push(StaticTy::Ty(current));
                }
                MathOp::Call { name, arity } => {
                    let before = stack.len();
                    if before < *arity {
                        bail!("internal error: RPN stack underflow");
                    }
                    let mut taken: Vec<StaticTy> = stack.split_off(before - arity);
                    taken.reverse();
                    // RPN operands carry no literal forms, so closed
                    // `#[values]` sets cannot be checked here; shape and
                    // arity still are. (Math-position calls are
                    // numeric builtins in practice.)
                    let blanks = vec![None; taken.len()];
                    let callee = self.resolve_call(name, depth)?;
                    let ret = self.apply_checked(name, &callee, &taken, &blanks, None)?;
                    stack.push(ret);
                }
                MathOp::Inspect(name) => {
                    let key = clean_var(name);
                    if !env.is_declared(&key) {
                        bail!("undefined variable ${key}");
                    }
                    env.read_guarded(&key, scope.site.as_ref())?;
                    stack.push(StaticTy::Ty(TypeTag::Any));
                }
                MathOp::Neg => {
                    let Some(top) = stack.pop() else {
                        bail!("internal error: RPN stack underflow");
                    };
                    stack.push(neg_rule(top, None)?);
                }
                MathOp::Add | MathOp::Sub | MathOp::Mul | MathOp::Div => {
                    let (Some(right), Some(left)) = (stack.pop(), stack.pop()) else {
                        bail!("internal error: RPN stack underflow");
                    };
                    stack.push(numeric_pair(left, right, None)?);
                }
                MathOp::Lt | MathOp::Le | MathOp::Gt | MathOp::Ge => {
                    let (Some(right), Some(left)) = (stack.pop(), stack.pop()) else {
                        bail!("internal error: RPN stack underflow");
                    };
                    ordering_pair(left, right, None)?;
                    stack.push(StaticTy::Ty(TypeTag::Bool));
                }
                MathOp::Eq | MathOp::Ne => {
                    if stack.pop().is_none() || stack.pop().is_none() {
                        bail!("internal error: RPN stack underflow");
                    }
                    stack.push(StaticTy::Ty(TypeTag::Bool));
                }
            }
        }
        if stack.len() != 1 {
            bail!("internal error: RPN stack left {} values", stack.len());
        }
        Ok(stack.pop().expect("length checked"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxdock_parser::Guard;

    fn static_bool(value: &str) -> GuardExpr {
        GuardExpr::Predicate(Guard::Attr {
            ns: oxdock_parser::Ns::Bool,
            key: None,
            val: Some(value.to_string()),
        })
    }

    #[test]
    fn guard_trichotomy_classifies_statically() {
        assert_eq!(classify_guard(None), GuardClass::Unconditional);
        assert_eq!(
            classify_guard(Some(&static_bool("true"))),
            GuardClass::Unconditional
        );
        assert_eq!(
            classify_guard(Some(&static_bool("false"))),
            GuardClass::Dead
        );
        assert_eq!(
            classify_guard(Some(&GuardExpr::All(vec![static_bool("true")]))),
            GuardClass::Unconditional
        );
        assert_eq!(
            classify_guard(Some(&GuardExpr::All(vec![static_bool("false")]))),
            GuardClass::Dead
        );
        assert_eq!(
            classify_guard(Some(&GuardExpr::Not(Box::new(static_bool("false"))))),
            GuardClass::Unconditional
        );
    }

    #[test]
    fn unify_widens_any_unidirectionally() {
        assert!(tags_equal(
            &unify(TypeTag::Int, TypeTag::Int, "t").expect("same"),
            &TypeTag::Int
        ));
        assert!(tags_equal(
            &unify(TypeTag::Int, TypeTag::Any, "t").expect("widen"),
            &TypeTag::Any
        ));
        assert!(tags_equal(
            &unify(TypeTag::Any, TypeTag::Map, "t").expect("widen"),
            &TypeTag::Any
        ));
        assert!(unify(TypeTag::Int, TypeTag::String, "t").is_err());
    }
}
