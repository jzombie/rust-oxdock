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
//! - Every terminal `RETURN` checks top-down against the declared
//!   return type via structural subtyping (`is_compatible`): unions
//!   widen only by membership on the declared side and narrow by
//!   containment on the actual side, containers recurse, and
//!   `VOID` unifies only with itself. There is no bottom-up
//!   unification on the script-function path. `EXIT` ends a path
//!   without contributing a type.
//! - A body with a `RETURN` on some live path but reachable
//!   fallthrough fails with `MissingReturn` unless the declared
//!   type is `VOID`. A body with no live `RETURN` is `VOID` only
//!   when declared `VOID`; `VOID` never defaults to `STRING`.
//! - Bare `RETURN` is the void exit: legal only in `VOID` bodies,
//!   a `MissingReturn`-family error everywhere else.
//! - `VOID` is position-locked: legal only as a bare function
//!   return or as the direct payload of `HANDLE`. Every other
//!   declaration position rejects it naming the site.
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
//!   literal text (`Arg::String`, `ArgPart::Text`) scans for embedded
//!   `{{ env:KEY }}` / `{{ script_env:KEY }}` reads through the env
//!   declaration gate, and task-handle (`AWAIT`, `CANCEL`,
//!   `AWAIT`-capture), pipe (`WITH_IO` bindings), and list
//!   (`LIST_APPEND`) names read through site-aware liveness with
//!   their runtime type rules (`PIPE`/`LIST` exactly).
//! - Environments are declared, except where the contract admits:
//!   `ENV` and `INHERIT_ENV` declare names into the current scope
//!   (scoped like variables, scrubbed by `ISOLATED`), and every other
//!   `env:` read — `env:KEY` expressions, `{{ env:KEY }}` templates,
//!   `LoadEnv` math operands, and `[env:KEY]` guards — must be
//!   declared in the same file first. The `ENV_CONTRACTS.md` baseline
//!   (host-mirrored seed rows plus engine-owned names) needs no
//!   declaration, exactly as the contract documents. Each file stands
//!   alone: module functions seed from their own file's top-level
//!   declarations, never the importer's.
//!
//! Performance: single traversal, no fixpoint, no I/O, no registry
//! writes. Branch forks clone a map of live vars; the merge walk
//! would cost O(vars) regardless, so overlays would save nothing
//! measurable. `MAX_STATIC_DEPTH` caps nesting with a static error.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use oxdock_fs::GuardedPath;
use oxdock_parser::{
    CompareOp, Expr, GuardExpr, IoBinding, MathOp, PipeTarget, Step, StepKind, TypeTag, Value,
    render_structural,
};
use oxdock_process::ProcessManager;

use super::{
    ExecState, FuncMeta, FuncParam,
    import::{ClosureFile, ClosureSite},
    required_arity,
};

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
#[derive(Clone, Debug)]
enum StaticTy {
    Ty(TypeTag),
    Void,
}

/// One collected script function: resolved params, body, and owning
/// file module (#115, `None` for entry/REPL functions). The owner
/// decides whose shared store the body resolves against.
#[derive(Clone)]
struct FuncInfo {
    params: Vec<(String, TypeTag)>,
    declared: TypeTag,
    is_async: bool,
    body: Vec<Step>,
    module: Option<String>,
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
    /// Module-shared bindings visible to module-owned code (#115):
    /// the owning file's top-level `LET`s with a touch flag shared
    /// across forks (reads on any path mark the body state-dependent,
    /// which is what the harvest gate probes). Reads consult these
    /// after lexical frames miss; entry code never carries one.
    module: Option<ModuleScope>,
    /// Declared environment names, one set per lexical frame: `ENV`
    /// and `INHERIT_ENV` declare into the top frame, reads resolve
    /// outward. The stack mirrors `frames` push for push, so braced
    /// blocks, branch arms, and loop bodies scope declarations
    /// exactly like the runtime `push_scope` discards them. Contract
    /// names need no declaration (see the checker's admitted set);
    /// every other `env:` read must be preceded by an explicit
    /// declaration in the same file.
    env_frames: Vec<HashSet<String>>,
}

/// Module-shared bindings plus a fork-shared touch flag. `Rc` (not
/// `Arc`): the checker is single-threaded; forks clone the `Rc` so a
/// touch on any conditional path is still observed after the fork is
/// discarded.
#[derive(Clone, Debug)]
struct ModuleScope {
    /// Owning module key, so nested checks re-seed from the checker's
    /// module table instead of inheriting possibly-stale bindings.
    key: String,
    lets: HashMap<String, TypeTag>,
    touched: std::rc::Rc<std::cell::Cell<bool>>,
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
    /// Position of the current step for error attribution: source
    /// line/column plus the walk index. Set per step by the body
    /// walk; nested walks overwrite it for their steps. Rendered only
    /// at the error site via [`step_pos`], exactly like the runtime
    /// `step_error` context.
    line: usize,
    col: usize,
    idx: usize,
}

/// Position context for one step, mirroring the runtime `step_error`
/// rendering one for one: `step N` plus `line L` / `col C` when the
/// step carries coordinates (parsed scripts do; hand-built steps keep
/// the historical bare rendering).
fn step_pos(line: usize, col: usize, idx: usize) -> String {
    let mut loc = format!("step {}", idx + 1);
    if line > 0 {
        loc.push_str(&format!(", line {line}"));
        if col > 0 {
            loc.push_str(&format!(", col {col}"));
        }
    }
    loc
}

/// The pass state: collected functions, host metas, inference
/// memoization, recursion guard, and the linearity counter.
struct Checker<'a, P: ProcessManager> {
    exec: &'a ExecState<P>,
    funcs: HashMap<String, FuncInfo>,
    /// Module-owned top-level `LET`s by module key (#115): seeded into
    /// the environment when checking module-owned function bodies, so
    /// static resolution sees the same store the runtime drains into.
    module_lets: HashMap<String, Vec<(String, TypeTag)>>,
    /// The entry's top-level `ENV` / `INHERIT_ENV` union, plus one per
    /// module key. Function bodies seed from the union covering their
    /// file: a module function sees its own file's prelude
    /// declarations, never the importer's. Each file textually
    /// declares what it reads; module calls additionally execute in
    /// an isolated context (seed plus init contributions), so a
    /// module that reads without declaring fails here, before the
    /// first step.
    module_env_unions: HashMap<String, HashSet<String>>,
    entry_env_union: HashSet<String>,
    /// Contract-admitted env names: the `ENV_CONTRACTS.md` baseline
    /// plus engine-owned rows ([`seeded_env_names`](oxdock_process::seeded_env_names)).
    /// Uses of these need no declaration, exactly as the contract
    /// documents. Everything else must be declared in the same file.
    /// Harness/CLI-injected names are never admitted here: they route
    /// explicitly through `INHERIT_ENV`.
    seeded_env: HashSet<String>,
    hosts: HashMap<String, FuncMeta>,
    inferred: HashMap<String, StaticTy>,
    inferring: Vec<String>,
    visits: u64,
    /// Nesting depth inside `ISOLATED` bodies: the isolated allowlist node
    /// is legal only below an `ISOLATED` head, at any depth including
    /// through nested blocks and nested `ISOLATED` itself.
    isolated_depth: usize,
}

/// Run the static pass over an imported file's body (#115). The
/// imported top level is function-like: `RETURN` is the value channel
/// back to the importer (allowed here, unlike entry scripts), and
/// falling off the end yields `""` instead of failing. Nested `FUNC`
/// bodies are fully checked like entry functions. `module` names the
/// file's module record: top-level `LET`s seed the module environment
/// that module-owned functions resolve against, mirroring the runtime
/// store drain on first initialization.
pub fn validate_import_body_types<P: ProcessManager>(
    steps: &[Step],
    exec: &ExecState<P>,
    module: &str,
) -> Result<()> {
    let mut checker = Checker::new(exec);
    checker.module_lets.insert(
        module.to_string(),
        module_lets(steps, exec)?.into_iter().collect(),
    );
    checker
        .module_env_unions
        .insert(module.to_string(), file_env_union(steps));
    checker.collect_owned(steps, Some(module))?;
    checker.check_import_body("import", steps, Some(module))?;
    for key in checker.func_keys() {
        checker.func_return(&key, 0)?;
    }
    Ok(())
}

/// Probe whether harvesting `names` from a module body would observe
/// module state (#115): returns the first state-touching name, if any.
/// The file body never executes on harvest, so a stateful harvest has
/// no initialized store to read; it must fail and direct the author to
/// a full import. Pure helpers probe clean.
pub(crate) fn harvest_touches_module<P: ProcessManager>(
    exec: &ExecState<P>,
    module: &str,
    steps: &[Step],
    names: &[String],
) -> Result<Option<String>> {
    let mut checker = Checker::new(exec);
    checker.module_lets.insert(
        module.to_string(),
        module_lets(steps, exec)?.into_iter().collect(),
    );
    checker
        .module_env_unions
        .insert(module.to_string(), file_env_union(steps));
    for name in names {
        let Some((params, return_type, is_async, body)) =
            steps.iter().find_map(|step| match &step.kind {
                StepKind::FuncDef {
                    name: def_name,
                    params,
                    return_type,
                    is_async,
                    body,
                    ..
                } if def_name == name => {
                    Some((params.clone(), return_type.clone(), *is_async, body.clone()))
                }
                _ => None,
            })
        else {
            continue;
        };
        let probe_key = format!("__probe__::{name}");
        checker.define_collected(
            &probe_key,
            vec![probe_key.clone()],
            &params,
            &return_type,
            is_async,
            &body,
            Some(module),
        )?;
        if checker.func_touches_module(&probe_key)? {
            return Ok(Some(name.clone()));
        }
    }
    Ok(None)
}

/// Top-level `LET` bindings of a module body (#115): the declarations
/// that drain into the shared store on first initialization. Names
/// resolve through the same tags execution uses; dead-guarded steps
/// are skipped exactly like collection skips them.
fn module_lets<P: ProcessManager>(
    steps: &[Step],
    exec: &ExecState<P>,
) -> Result<Vec<(String, TypeTag)>> {
    let mut out = Vec::new();
    for step in steps {
        if matches!(classify_guard(step.guard.as_ref()), GuardClass::Dead) {
            continue;
        }
        // Every top-level binding form drains into the shared store at
        // runtime (`import_file` takes the whole init frame), so every
        // form seeds here too: a module using capture/async/readline
        // initializers must resolve, exactly like plain `LET`.
        match &step.kind {
            StepKind::Assign { var, decl_type, .. }
            | StepKind::AssignCapture { var, decl_type, .. }
            | StepKind::AssignAsync { var, decl_type, .. } => {
                let tag = exec
                    .resolve_tag(decl_type)
                    .map_err(|err| anyhow::anyhow!("module variable `${var}`: {err:#}"))?;
                out.push((clean_var(var), tag));
            }
            StepKind::ReadLine { var } | StepKind::Prompt { var, .. } => {
                out.push((clean_var(var), TypeTag::String));
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Run `f` with `aliases` merged into the global type directory,
/// restoring the previous bindings afterward (stack discipline, so
/// nested scopes combine). The closure pass checks every file with
/// exactly its own aliases plus the shared harvests visible, never
/// another file's defs.
fn with_scope_aliases<P: ProcessManager, R>(
    exec: &mut ExecState<P>,
    aliases: &[(String, String)],
    f: impl FnOnce(&mut ExecState<P>) -> Result<R>,
) -> Result<R> {
    let saved: Vec<(String, Option<String>)> = aliases
        .iter()
        .map(|(name, _)| (name.clone(), exec.type_aliases.get(name).cloned()))
        .collect();
    for (name, target) in aliases {
        exec.type_aliases.insert(name.clone(), target.clone());
    }
    let outcome = f(&mut *exec);
    for (name, prev) in saved {
        match prev {
            Some(target) => {
                exec.type_aliases.insert(name, target);
            }
            None => {
                exec.type_aliases.remove(&name);
            }
        }
    }
    outcome
}

/// Merge one `IMPORT [TYPE ...]` harvest into the global directory:
/// resolve the path against the importing file's directory through
/// the closure (no filesystem; expansion already loaded everything),
/// EXPORT-gate each name, collision-checked loud like the runtime.
/// Harvests are explicit global requests, so they merge globally;
/// file-local defs never do. Host-module harvests validate globally
/// and record nothing.
fn merge_harvest<P: ProcessManager>(
    exec: &mut ExecState<P>,
    files: &[ClosureFile],
    dir: &GuardedPath,
    path: &str,
    alias: Option<&str>,
    type_names: &[String],
) -> Result<()> {
    let resolved = dir
        .join(path)
        .map_err(|err| anyhow::anyhow!("IMPORT `{path}` cannot be resolved: {err}"))?;
    let key = resolved.display();
    let Some(file) = files.iter().find(|f| f.key == key) else {
        if exec.module_table.modules.contains_key(path) {
            for name in type_names {
                exec.resolve_tag(name).map_err(|err| {
                    anyhow::anyhow!("IMPORT `{path}` has no such TYPE `{name}`: {err:#}")
                })?;
            }
            return Ok(());
        }
        anyhow::bail!("IMPORT `{path}` cannot be read: not in the import closure");
    };
    let aliases = super::typing::collect_type_aliases(&file.steps, exec)?;
    for name in type_names {
        if !file.type_exports.iter().any(|e| e == name) {
            anyhow::bail!("IMPORT `{path}` does not export TYPE `{name}`");
        }
        let Some(target) = aliases.get(name) else {
            anyhow::bail!("IMPORT `{path}` has no such TYPE `{name}`");
        };
        super::import::insert_imported_type(exec, name, target, alias, path)?;
    }
    Ok(())
}

/// Merge every `IMPORT [TYPE ...]` harvest in `steps` (including
/// nested bodies: validation is conservative, like expansion).
fn merge_harvests_in<P: ProcessManager>(
    exec: &mut ExecState<P>,
    files: &[ClosureFile],
    dir: &GuardedPath,
    steps: &[Step],
) -> Result<()> {
    for step in steps {
        if let StepKind::ImportFuncs {
            path,
            alias,
            type_names,
            ..
        } = &step.kind
            && !type_names.is_empty()
        {
            merge_harvest(exec, files, dir, path, alias.as_deref(), type_names)?;
        }
        for body in super::import::child_bodies(&step.kind) {
            merge_harvests_in(exec, files, dir, body)?;
        }
    }
    Ok(())
}
/// Run the static pass over the entry script plus the entire import
/// closure (#115). Every transitively imported file is collected and
/// checked before the first step executes: cross-file calls resolve
/// against the collected signatures (arity and argument types enforced
/// at the call site), and nested files cannot smuggle type errors past
/// the entry gate. Cycles, depth overflow, and parse failures surface
/// during expansion, also before any step runs.
///
/// Scoping is per-file: every check runs with exactly its own file's
/// aliases plus the shared harvests visible, never another file's
/// defs, so same-named types in different files coexist. Harvests are
/// explicit global requests (collision-checked loud); a bare `IMPORT`
/// of a file exposes its exports only as `ALIAS::NAME`.
pub fn validate_script_closure<P: ProcessManager>(
    entry: &[Step],
    files: &[super::import::ClosureFile],
    exec: &mut ExecState<P>,
) -> Result<()> {
    for file in files {
        let mut local = std::collections::HashSet::new();
        for step in &file.steps {
            if let StepKind::TypeAlias { name, .. } = &step.kind {
                local.insert(name.clone());
            }
        }
        for name in &file.type_exports {
            if !local.contains(name) {
                anyhow::bail!(
                    "IMPORT `{}` EXPORT TYPE `{name}` does not match any top-level TYPE in that file",
                    file.path
                );
            }
        }
    }
    // Global harvests first: every `IMPORT [TYPE ...]` across entry and
    // files resolves through the closure (host modules validate
    // globally), EXPORT-gated, collision-checked loud. Explicit
    // harvests are global requests; file-local defs stay per-file
    // below, so same-named types in different files coexist.
    let saved_global = exec.type_aliases.clone();
    let outcome: Result<()> = (|| {
        let entry_base = super::import::import_base(exec);
        merge_harvests_in(exec, files, &entry_base, entry)?;
        for file in files {
            merge_harvests_in(exec, files, &file.dir, &file.steps)?;
        }
        // Qualified exports: a bare `IMPORT ["f" AS C]` exposes the
        // file's EXPORTed types only as `C::NAME`, mirroring func
        // views, so nothing bare leaks into the importer. Same-name
        // types in different files coexist; collisions fail loud.
        for file in files {
            let full = file.sites.iter().any(|site| {
                site.alias.is_some() && site.func_names.is_none() && site.type_names.is_none()
            });
            if !full {
                continue;
            }
            let defs = super::typing::collect_type_aliases(&file.steps, exec)?;
            for site in &file.sites {
                if site.alias.is_none() || site.func_names.is_some() || site.type_names.is_some() {
                    continue;
                }
                for name in &file.type_exports {
                    if let Some(target) = defs.get(name) {
                        super::import::insert_imported_type(
                            exec,
                            name,
                            target,
                            site.alias.as_deref(),
                            &file.path,
                        )?;
                    }
                }
            }
        }
        // Collect funcs per unit so params resolve in the defining
        // file's scope. Harvested funcs land under their import alias
        // (bare harvests stay bare); bodies check below.
        let mut all_funcs = HashMap::new();
        let mut all_lets: HashMap<String, Vec<(String, TypeTag)>> = HashMap::new();
        let entry_defs: Vec<(String, String)> = super::typing::collect_type_aliases(entry, exec)?
            .into_iter()
            .collect();
        with_scope_aliases(exec, &entry_defs, |exec| {
            let mut checker = Checker::new(exec);
            checker.collect(entry)?;
            all_funcs.extend(checker.funcs);
            Ok(())
        })?;
        for file in files {
            let defs: Vec<(String, String)> =
                super::typing::collect_type_aliases(&file.steps, exec)?
                    .into_iter()
                    .collect();
            with_scope_aliases(exec, &defs, |exec| {
                let mut checker = Checker::new(exec);
                for site in &file.sites {
                    checker.collect_site(site, file)?;
                }
                all_funcs.extend(checker.funcs);
                all_lets.insert(file.key.clone(), module_lets(&file.steps, exec)?);
                Ok(())
            })?;
        }
        // Infer every collected function under its owner's scope
        // FIRST, into one shared memo, so call sites reuse
        // owner-checked signatures instead of inferring in whatever
        // scope happens to be current. Sorted for determinism.
        let mut inferred: HashMap<String, StaticTy> = HashMap::new();
        let mut keys: Vec<String> = all_funcs.keys().cloned().collect();

        keys.sort();
        for key in keys {
            let owner = all_funcs.get(&key).and_then(|info| info.module.clone());
            let defs: Vec<(String, String)> = match owner {
                None => entry_defs.clone(),
                Some(module) => files
                    .iter()
                    .find(|f| f.key == module)
                    .map(|file| {
                        super::typing::collect_type_aliases(&file.steps, exec)
                            .map(|aliases| aliases.into_iter().collect())
                    })
                    .transpose()?
                    .unwrap_or_default(),
            };
            with_scope_aliases(exec, &defs, |exec| {
                let mut checker =
                    seeded_checker(exec, &all_funcs, &all_lets, &inferred, entry, files);
                let ty = checker.func_return(&key, 0)?;
                inferred.insert(key.clone(), ty);
                Ok(())
            })?;
        }
        // Check every body with exactly its own scope visible: entry
        // defs for the entry, file defs for each file. Calls resolve
        // against the pre-inferred signatures; bodies never see
        // another file's defs.
        /// Fresh checker seeded with the collected functions, module lets,
        /// and environment unions. Built per scope so every check runs with
        /// exactly its own file's aliases plus the shared harvests visible.
        fn seeded_checker<'a, P: ProcessManager>(
            exec: &'a mut ExecState<P>,
            funcs: &HashMap<String, FuncInfo>,
            lets: &HashMap<String, Vec<(String, TypeTag)>>,
            inferred: &HashMap<String, StaticTy>,
            entry: &[Step],
            files: &[ClosureFile],
        ) -> Checker<'a, P> {
            let mut checker = Checker::new(exec);
            checker.funcs = funcs.clone();
            checker.module_lets = lets.clone();
            checker.inferred = inferred.clone();
            checker.build_env_unions(entry, files);
            checker
        }
        with_scope_aliases(exec, &entry_defs, |exec| {
            seeded_checker(exec, &all_funcs, &all_lets, &inferred, entry, files)
                .check_top_level(entry)
        })?;
        for file in files {
            let defs: Vec<(String, String)> =
                super::typing::collect_type_aliases(&file.steps, exec)?
                    .into_iter()
                    .collect();
            with_scope_aliases(exec, &defs, |exec| {
                seeded_checker(exec, &all_funcs, &all_lets, &inferred, entry, files)
                    .check_import_body(&file.key, &file.steps, Some(&file.key))
            })?;
        }
        Ok(())
    })();
    exec.type_aliases = saved_global;
    if outcome.is_ok() {
        // Proven once, trusted thereafter: every file in the closure
        // passed with full nested-import knowledge, and the registry
        // cache pins the same parsed bodies for execution. Runtime
        // re-validation would only false-positive on nested calls, so
        // imports skip the single-file re-proof for these keys.
        // Unseen files (direct API use, no static pass) still
        // validate at import time.
        for file in files {
            exec.validated_modules.insert(file.key.clone());
        }
    }
    outcome
}

/// Same pass returning the visit count, for the linearity test: the
/// count must stay proportional to input size on adversarial input.
/// Test-only: production calls `validate_script_closure`.
#[cfg(test)]
pub fn validate_script_types_counted<P: ProcessManager>(
    steps: &[Step],
    exec: &ExecState<P>,
) -> Result<u64> {
    let mut checker = Checker::new(exec);
    checker.entry_env_union = file_env_union(steps);
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
/// Shared with the union resolve path for member deduplication.
pub(super) fn tags_equal(left: &TypeTag, right: &TypeTag) -> bool {
    // Different constructors never equate: one comparison up front,
    // so below each variant lists once and a new variant fails to
    // compile here until its rule is explicit. The `let-else`
    // rejections are unreachable through the discriminant check;
    // they stay loud (never panic) rather than assumed.
    if std::mem::discriminant(left) != std::mem::discriminant(right) {
        return false;
    }
    match left {
        TypeTag::String
        | TypeTag::Int
        | TypeTag::Float
        | TypeTag::Bool
        | TypeTag::List
        | TypeTag::Map
        | TypeTag::Pipe
        | TypeTag::Handle
        | TypeTag::Void
        | TypeTag::Duration
        | TypeTag::Path
        | TypeTag::Semaphore
        | TypeTag::Permit
        | TypeTag::Any => true,
        TypeTag::Custom(a) => {
            let TypeTag::Custom(b) = right else {
                return false;
            };
            std::ptr::eq(*a, *b)
        }
        TypeTag::ListOf(a) => {
            let TypeTag::ListOf(b) = right else {
                return false;
            };
            tags_equal(a, b)
        }
        TypeTag::HandleOf(a) => {
            let TypeTag::HandleOf(b) = right else {
                return false;
            };
            tags_equal(a, b)
        }
        TypeTag::MapOf(a) => {
            let TypeTag::MapOf(b) = right else {
                return false;
            };
            tags_equal(a, b)
        }
        TypeTag::Record(a) => {
            let TypeTag::Record(b) = right else {
                return false;
            };
            std::sync::Arc::ptr_eq(a, b)
        }
        // Member sets compare order-free: construction deduplicates,
        // so equal length plus full containment is set equality.
        TypeTag::Union(_, a) => {
            let TypeTag::Union(_, b) = right else {
                return false;
            };
            a.len() == b.len()
                && a.iter()
                    .all(|member| b.iter().any(|other| tags_equal(member, other)))
        }
    }
}

/// Scalar pairs the runtime coercion admits by value. Single table
/// shared by `check_arg`: static and runtime agree by construction
/// instead of one promising what the other rejects. Deliberately
/// excludes `STRING` into `INT`/`FLOAT` and `FLOAT` into `INT`:
/// those need the explicit converter at the call site.
fn coercible_pair(actual: &TypeTag, expected: &TypeTag) -> bool {
    matches!(
        (actual, expected),
        (TypeTag::String, TypeTag::Bool)
            | (TypeTag::String, TypeTag::Duration)
            | (TypeTag::String, TypeTag::Path)
            | (TypeTag::Int, TypeTag::String)
            | (TypeTag::Int, TypeTag::Float)
            | (TypeTag::Float, TypeTag::String)
            | (TypeTag::Bool, TypeTag::String)
            | (TypeTag::Duration, TypeTag::String)
            | (TypeTag::Path, TypeTag::String)
            | (TypeTag::ListOf(_), TypeTag::List)
            | (TypeTag::List, TypeTag::ListOf(_))
            | (TypeTag::Record(_), TypeTag::Map)
            | (TypeTag::MapOf(_), TypeTag::Map)
            | (TypeTag::Map, TypeTag::MapOf(_))
    )
}

/// Single mismatch constructor for exact shape checks: names both
/// sides structurally. The three call sites below reported
/// byte-identical text before; they share this now so a wording fix
/// lands once.
fn exact_mismatch(ctx: &str, expected: &TypeTag, actual: &TypeTag) -> anyhow::Error {
    anyhow::anyhow!(
        "{ctx}: TypeMismatch: expected {}, got {}",
        render_structural(expected),
        render_structural(actual)
    )
}

/// Map-kind tags: bare words, fixed shapes, and value shapes alike.
/// Literals whose every element is maplike join to the unknown map
/// shape instead of collapsing to a bare list.
fn is_maplike(tag: &TypeTag) -> bool {
    matches!(tag, TypeTag::Map | TypeTag::Record(_) | TypeTag::MapOf(_))
}

/// List-kind tags, mirroring [`is_maplike`] for nested lists.
fn is_listlike(tag: &TypeTag) -> bool {
    matches!(tag, TypeTag::List | TypeTag::ListOf(_))
}

/// Top-down declared-return compatibility: recursive structural subtyping.
/// ANY as declared admits anything. Union actual requires every member
/// compatible with declared. Union declared admits when actual matches
/// some member. Generic containers recurse structurally. Otherwise exact
/// equality via tags_equal. VOID never admits a value (call-site rejects).
fn is_compatible(actual: &TypeTag, declared: &TypeTag) -> bool {
    if matches!(actual, TypeTag::Void) || matches!(declared, TypeTag::Void) {
        return matches!(actual, TypeTag::Void) && matches!(declared, TypeTag::Void);
    }
    if matches!(declared, TypeTag::Any) {
        return true;
    }
    if let TypeTag::Union(_, members) = actual {
        return members.iter().all(|m| is_compatible(m, declared));
    }
    if let TypeTag::Union(_, members) = declared {
        return members.iter().any(|m| is_compatible(actual, m));
    }
    match (actual, declared) {
        (TypeTag::ListOf(a), TypeTag::ListOf(e)) => is_compatible(a, e),
        (TypeTag::MapOf(a), TypeTag::MapOf(e)) => is_compatible(a, e),
        (TypeTag::Record(a_fields), TypeTag::MapOf(e)) => {
            a_fields.iter().all(|field| is_compatible(&field.ty, e))
        }
        (TypeTag::HandleOf(a), TypeTag::HandleOf(e)) => is_compatible(a, e),
        (TypeTag::Record(a_fields), TypeTag::Record(e_fields)) => {
            e_fields
                .iter()
                .all(|ef| match a_fields.iter().find(|af| af.name == ef.name) {
                    Some(af) => is_compatible(&af.ty, &ef.ty),
                    None => ef.optional,
                })
                && a_fields
                    .iter()
                    .all(|af| e_fields.iter().any(|ef| ef.name == af.name))
        }
        (TypeTag::Record(_), TypeTag::Map) => true,
        (TypeTag::MapOf(_), TypeTag::Map) => true,
        (TypeTag::ListOf(_), TypeTag::List) => true,
        (TypeTag::HandleOf(a), TypeTag::Handle) => is_compatible(a, &TypeTag::Any),
        (TypeTag::Handle, TypeTag::HandleOf(e)) => is_compatible(&TypeTag::Any, e),
        _ => tags_equal(actual, declared),
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
    // Unions widen on mixed joins: member-wise unification would mint
    // precise-but-unreadable sets, while `ANY` already means "defer
    // to runtime" everywhere else joins give up precision. Identical
    // unions unify exactly through `tags_equal` above.
    if matches!(left, TypeTag::Union(..)) || matches!(right, TypeTag::Union(..)) {
        return Ok(TypeTag::Any);
    }
    // Same-kind containment: a shaped value IS its bare word, so mixed
    // branches widen to the general tag instead of failing. Narrowing
    // never happens here: unknown shapes never unify INTO fixed ones.
    if matches!(
        (&left, &right),
        (TypeTag::ListOf(_), TypeTag::List) | (TypeTag::List, TypeTag::ListOf(_))
    ) {
        return Ok(TypeTag::List);
    }
    if matches!(
        (&left, &right),
        (TypeTag::Record(_), TypeTag::Map)
            | (TypeTag::Map, TypeTag::Record(_))
            | (TypeTag::Record(_), TypeTag::MapOf(_))
            | (TypeTag::MapOf(_), TypeTag::Record(_))
            | (TypeTag::MapOf(_), TypeTag::Map)
            | (TypeTag::Map, TypeTag::MapOf(_))
    ) {
        return Ok(TypeTag::Map);
    }
    bail!(
        "{ctx}: cannot unify {} and {}",
        render_structural(&left),
        render_structural(&right)
    );
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
    bare_return: bool,
    types: Vec<(TypeTag, usize, usize)>,
}

/// Union of env names declared by direct top-level steps (never
/// descending into bodies): `FUNC` bodies are function-local,
/// branch/loop/block/`ISOLATED` interiors are scope-local at
/// runtime, so only linear top-level `ENV` / `INHERIT_ENV` names
/// seed the file's functions. Order-free by design: a function
/// may run after any top-level step, so the union over-approximates
/// visibility instead of tracking call sites.
fn file_env_union(steps: &[Step]) -> HashSet<String> {
    let mut union = HashSet::new();
    for step in steps {
        match &step.kind {
            StepKind::Env { key, .. } => {
                union.insert(key.clone());
            }
            StepKind::InheritEnv { keys } | StepKind::IsolatedInheritEnv { keys } => {
                union.extend(keys.iter().cloned());
            }
            _ => {}
        }
    }
    union
}

impl Env {
    fn one_frame(bindings: Vec<(String, TypeTag)>) -> Self {
        let mut top = HashMap::new();
        for (name, tag) in bindings {
            top.insert(name, vec![Binding { tag, guard: None }]);
        }
        Self {
            frames: vec![top],
            module: None,
            env_frames: vec![HashSet::new()],
        }
    }

    /// Fork for a scoped body: clone the stack and push a fresh frame,
    /// mirroring `push_scope`. Discarding the fork is exact — arm and
    /// loop declarations never leak at runtime.
    fn fork(&self) -> Self {
        let mut fork = self.clone();
        fork.frames.push(HashMap::new());
        fork.env_frames.push(HashSet::new());
        fork
    }

    /// Declare an environment name in the top frame. Re-declaration is
    /// always allowed: `ENV` overwrites and `INHERIT_ENV` refreshes at
    /// runtime, so there is no shadowing error to mirror.
    fn declare_env(&mut self, name: String) {
        if let Some(frame) = self.env_frames.last_mut() {
            frame.insert(name);
        }
    }

    /// Whether `name` was declared in any live frame.
    fn is_env_declared(&self, name: &str) -> bool {
        self.env_frames.iter().any(|frame| frame.contains(name))
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
                        None => entry.tag.clone(),
                        Some(acc) => unify(
                            acc,
                            entry.tag.clone(),
                            &format!("${key} across platform guards"),
                        )?,
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
        // Module-shared bindings sit beneath every lexical frame (#115):
        // a hit marks the body as state-dependent for the harvest gate.
        if let Some(scope) = &self.module
            && let Some(tag) = scope.lets.get(key)
        {
            scope.touched.set(true);
            return Ok((tag.clone(), None));
        }
        bail!("undefined variable ${key}");
    }

    fn is_declared(&self, key: &str) -> bool {
        self.frames
            .iter()
            .rev()
            .any(|frame| frame.contains_key(key))
            || self
                .module
                .as_ref()
                .is_some_and(|scope| scope.lets.contains_key(key))
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
            module_lets: HashMap::new(),
            module_env_unions: HashMap::new(),
            entry_env_union: HashSet::new(),
            seeded_env: oxdock_process::seeded_env_names()
                .into_iter()
                .map(|name| name.to_string())
                .collect(),
            hosts,
            inferred: HashMap::new(),
            inferring: Vec::new(),
            visits: 0,
            isolated_depth: 0,
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
    fn collect_owned(&mut self, steps: &[Step], module: Option<&str>) -> Result<()> {
        for step in steps {
            if matches!(classify_guard(step.guard.as_ref()), GuardClass::Dead) {
                continue;
            }
            self.collect_step(step, module)?;
        }
        Ok(())
    }

    /// Entry shorthand: ownerless collection for entry scripts.
    fn collect(&mut self, steps: &[Step]) -> Result<()> {
        self.collect_owned(steps, None)
    }

    /// Seed env unions for function-body seeding: the entry's union
    /// plus one per closure file. Module unions gate module functions
    /// on their own file's prelude declarations (textual, per-file);
    /// at runtime those calls execute isolated (seed plus init
    /// contributions), so declarations are load-bearing, not advisory.
    fn build_env_unions(&mut self, entry: &[Step], files: &[super::import::ClosureFile]) {
        self.entry_env_union = file_env_union(entry);
        for file in files {
            self.module_env_unions
                .insert(file.key.clone(), file_env_union(&file.steps));
        }
    }

    /// Whether `name` may be read here: declared in a live frame, in
    /// the seeding file union (function bodies start from their
    /// file's declarations), or admitted by the env contract (baseline
    /// seed rows and `CARGO_FEATURE_*`/`CARGO_CFG_*` patterns need no
    /// declaration). Nothing else admits: no host fallbacks, no
    /// cross-file leakage.
    fn env_declared(&self, env: &Env, name: &str) -> bool {
        env.is_env_declared(name)
            || self.seeded_env.contains(name)
            || oxdock_process::is_sanctioned_seed(name)
    }

    /// Reject an undeclared environment read at its use site, naming
    /// the declarations that would allow it. `prefix` is the source
    /// spelling (`env:` / `script_env:`) so the message quotes what
    /// the author wrote. Position renders like the runtime
    /// `step_error` context (`step N, line L, col C`).
    fn check_env_name(&self, env: &Env, name: &str, prefix: &str, scope: &Scope) -> Result<()> {
        if !self.env_declared(env, name) {
            bail!(
                "{} in {}: uses undeclared environment variable `{prefix}{name}`: declare it first with ENV or INHERIT_ENV",
                step_pos(scope.line, scope.col, scope.idx),
                scope.label,
            );
        }
        Ok(())
    }

    /// Scan raw string text for `{{ env:KEY }}` / `{{ script_env:KEY }}`
    /// placeholders and reject undeclared names. Runs on the shared
    /// [`TemplateSegments`](oxdock_parser::template::TemplateSegments)
    /// splitter, so what counts as a placeholder here is exactly what
    /// expands at runtime. Bare keys and `$var` placeholders are not
    /// env reads and are ignored here.
    fn check_template_env(&self, text: &str, env: &Env, scope: &Scope) -> Result<()> {
        use oxdock_parser::template::{TemplateSegment, TemplateSegments};
        for segment in TemplateSegments::new(text) {
            let TemplateSegment::Placeholder { key } = segment else {
                continue;
            };
            let found = key
                .strip_prefix("env:")
                .map(|name| ("env:", name))
                .or_else(|| {
                    key.strip_prefix("script_env:")
                        .map(|name| ("script_env:", name))
                });
            if let Some((prefix, name)) = found {
                let name = name.trim();
                if !name.is_empty() {
                    self.check_env_name(env, name, prefix, scope)?;
                }
            }
        }
        Ok(())
    }

    /// Collect every `[env:KEY]` guard reference in a guard tree.
    fn guard_env_names(guard: &GuardExpr, out: &mut Vec<String>) {
        match guard {
            GuardExpr::Predicate(pred) => {
                if let oxdock_parser::Guard::Attr { ns, key, .. } = pred
                    && matches!(ns, oxdock_parser::Ns::Env)
                    && let Some(name) = key
                {
                    out.push(name.clone());
                }
            }
            GuardExpr::All(children) | GuardExpr::Or(children) => {
                for child in children {
                    Self::guard_env_names(child, out);
                }
            }
            GuardExpr::Not(inner) => Self::guard_env_names(inner, out),
        }
    }

    /// Define one collected function under explicit keys: resolve the
    /// param tags now (the same error execution reports), store the
    /// body for return inference, and collect nested definitions.
    #[allow(clippy::too_many_arguments)]
    fn define_collected(
        &mut self,
        name: &str,
        keys: Vec<String>,
        params: &[(String, String)],
        return_type: &str,
        is_async: bool,
        body: &[Step],
        module: Option<&str>,
    ) -> Result<()> {
        let mut resolved = Vec::with_capacity(params.len());
        for (pname, ptype) in params {
            let tag = self
                .exec
                .resolve_tag(ptype)
                .map_err(|err| anyhow::anyhow!("function `{name}`: {err:#}"))?;
            self.exec
                .check_declaration_tag(&format!("function `{name}` param `${pname}`"), &tag)?;
            resolved.push((clean_var(pname), tag));
        }
        let declared = self
            .exec
            .resolve_tag(return_type)
            .map_err(|err| anyhow::anyhow!("function `{name}`: {err:#}"))?;
        self.exec
            .check_return_tag(&format!("function `{name}` returns"), &declared)
            .map_err(|err| anyhow::anyhow!("function `{name}`: {err:#}"))?;
        for key in keys {
            self.funcs.insert(
                key,
                FuncInfo {
                    params: resolved.clone(),
                    declared: declared.clone(),
                    is_async,
                    body: body.to_vec(),
                    module: module.map(|m| m.to_string()),
                },
            );
        }
        let nested = body.to_vec();
        self.collect_owned(&nested, module)?;
        Ok(())
    }

    fn collect_step(&mut self, step: &Step, module: Option<&str>) -> Result<()> {
        match &step.kind {
            StepKind::FuncDef {
                name,
                params,
                return_type,
                is_async,
                body,
                ..
            } => {
                self.define_collected(
                    name,
                    vec![name.clone(), format!("SCRIPT::{name}")],
                    params,
                    return_type,
                    *is_async,
                    body,
                    module,
                )?;
            }
            StepKind::If {
                then_body,
                else_ifs,
                else_body,
                ..
            } => {
                self.collect_owned(then_body, module)?;
                for (_, arm) in else_ifs {
                    self.collect_owned(arm, module)?;
                }
                if let Some(arm) = else_body {
                    self.collect_owned(arm, module)?;
                }
            }
            StepKind::For { body, .. } | StepKind::While { body, .. } => {
                self.collect_owned(body, module)?;
            }
            StepKind::AsyncBlock { body } => self.collect_owned(body, module)?,
            _ => {}
        }
        Ok(())
    }

    /// Collect one import site (#115): every wanted *exported* top-level
    /// `FUNC` registers under the site's namespace (bare `SCRIPT::` names
    /// for alias-free harvests), owner-tagged with the module so static
    /// resolution sees the same shared store as the runtime. Unexported
    /// means private: full imports skip unexported functions, and
    /// harvesting one fails upfront. Harvesting a state-touching
    /// function fails too: the body never runs, so there is no
    /// initialized state for it to observe. Nested control-flow bodies
    /// collect under the same owner. Shadowing follows the same
    /// last-wins rule as [`collect_owned`](Self::collect_owned); the
    /// runtime registry remains the enforcer for true duplicates.
    /// Type harvests gate against `EXPORT TYPE` the same way; types
    /// persist bare globally for later `resolve_tag` calls, which read
    /// the global alias map.
    fn collect_site(&mut self, site: &ClosureSite, file: &ClosureFile) -> Result<()> {
        let module = file.key.as_str();
        let steps = file.steps.as_slice();
        let alias = site.alias.as_deref();
        let func_names = site.func_names.as_deref();
        let type_names = site.type_names.as_deref();
        let func_exports = &file.func_exports;
        let type_exports = &file.type_exports;
        let file_path = file.path.as_str();
        if !self.module_lets.contains_key(module) {
            let lets = module_lets(steps, self.exec)?;
            self.module_lets.insert(module.to_string(), lets);
        }
        if let Some(names) = func_names {
            let defined: Vec<&str> = steps
                .iter()
                .filter_map(|step| match &step.kind {
                    StepKind::FuncDef { name, .. } => Some(name.as_str()),
                    _ => None,
                })
                .collect();
            for name in names {
                if !defined.iter().any(|d| d == name) {
                    bail!("IMPORT `{file_path}` has no such function `{name}`");
                }
                if !func_exports.iter().any(|e| e == name) {
                    bail!("IMPORT `{file_path}` does not export `{name}`");
                }
            }
        }
        if let Some(type_names) = type_names {
            let defined_types: Vec<&str> = steps
                .iter()
                .filter_map(|step| match &step.kind {
                    StepKind::TypeAlias { name, .. } => Some(name.as_str()),
                    _ => None,
                })
                .collect();
            for name in type_names {
                if !defined_types.iter().any(|d| d == name) {
                    bail!("IMPORT `{file_path}` has no such TYPE `{name}`");
                }
                if !type_exports.iter().any(|e| e == name) {
                    bail!("IMPORT `{file_path}` does not export TYPE `{name}`");
                }
            }
        }
        let func_exported: std::collections::HashSet<&str> =
            func_exports.iter().map(|e| e.as_str()).collect();
        for step in steps {
            if matches!(classify_guard(step.guard.as_ref()), GuardClass::Dead) {
                continue;
            }
            if let StepKind::FuncDef {
                name,
                params,
                return_type,
                is_async,
                body,
                ..
            } = &step.kind
            {
                if !func_exported.contains(name.as_str()) {
                    continue;
                }
                if let Some(names) = func_names
                    && !names.iter().any(|n| n == name)
                {
                    continue;
                }
                match alias {
                    Some(alias) => {
                        self.define_collected(
                            name,
                            vec![format!("{alias}::{name}")],
                            params,
                            return_type,
                            *is_async,
                            body,
                            Some(module),
                        )?;
                    }
                    None => {
                        self.define_collected(
                            name,
                            vec![name.clone(), format!("SCRIPT::{name}")],
                            params,
                            return_type,
                            *is_async,
                            body,
                            Some(module),
                        )?;
                    }
                }
            } else {
                // Control-flow bodies can nest definitions; collect
                // those under the same module owner.
                self.collect_step(step, Some(module))?;
            }
        }
        // Harvests never execute the file body: any wanted function
        // that observes module state has nothing to read, so fail the
        // site with the fix attached.
        if let Some(names) = func_names {
            for name in names {
                let key = match alias {
                    Some(alias) => format!("{alias}::{name}"),
                    None => name.clone(),
                };
                if self.func_touches_module(&key)? {
                    bail!(
                        "IMPORT `{file_path}` cannot harvest stateful function `{name}`: import the file"
                    );
                }
            }
        }
        Ok(())
    }

    /// Check an imported file's top-level steps (#115): function-like,
    /// so `RETURN` is the value channel back and falling off the end
    /// yields `""` instead of failing. The label names the file for
    /// error attribution. The walk env carries the module's shared
    /// bindings beneath the top frame, so nested definitions resolve
    /// (and owner-tag) exactly like collected ones.
    fn check_import_body(
        &mut self,
        label: &str,
        steps: &[Step],
        module: Option<&str>,
    ) -> Result<()> {
        let mut env = Env::one_frame(Vec::new());
        if let Some(module) = module {
            let mut lets = HashMap::new();
            for (name, tag) in self.module_lets.get(module).cloned().unwrap_or_default() {
                lets.insert(name, tag);
            }
            env.module = Some(ModuleScope {
                key: module.to_string(),
                lets,
                touched: std::rc::Rc::new(std::cell::Cell::new(false)),
            });
        }
        let scope = Scope {
            label,
            ret: RetCtx::Func("IMPORT".to_string()),
            loops: 0,
            site: None,
            line: 0,
            col: 0,
            idx: 0,
        };
        let out = self.walk_body(steps, &mut env, &scope, 0)?;
        let _ = out;
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
            line: 0,
            col: 0,
            idx: 0,
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
            return Ok(known.clone());
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
        self.inferred.insert(key.to_string(), ty.clone());
        Ok(ty)
    }

    /// Check one function body: terminal unification, MissingReturn,
    /// VOID classification. Module-owned bodies resolve against the
    /// owning file's top-level `LET`s beneath the params, mirroring
    /// the runtime shared store.
    fn check_function_body(
        &mut self,
        key: &str,
        info: &FuncInfo,
        depth: usize,
    ) -> Result<StaticTy> {
        self.check_function_body_probed(key, info, depth, None, true)
    }

    /// Same, with an optional touch probe: when present, any read of a
    /// module-shared binding sets it, so harvest callers can reject
    /// state-dependent functions. The probe `Rc` is shared across env
    /// forks, so touches on discarded conditional paths still count.
    /// Touch probes (harvest statefulness gates) walk only for
    /// reads: return-type enforcement belongs to the dedicated
    /// passes (the static pre-pass with full nested-import
    /// knowledge; the runtime single-file validation for direct API
    /// use), so a probe with a nested-unknown callee must not
    /// false-positive here. Pass `enforce_types: false` from the
    /// touch probes, `true` everywhere else.
    fn check_function_body_probed(
        &mut self,
        key: &str,
        info: &FuncInfo,
        depth: usize,
        probe: Option<std::rc::Rc<std::cell::Cell<bool>>>,
        enforce_types: bool,
    ) -> Result<StaticTy> {
        if depth > MAX_STATIC_DEPTH {
            bail!("static analysis depth exceeded in function `{key}`");
        }
        let mut env = Env::one_frame(info.params.clone());
        if let Some(module) = info.module.as_deref() {
            let lets = self.module_lets.get(module).cloned().unwrap_or_default();
            let touched = probe.unwrap_or_default();
            let mut module_lets = HashMap::new();
            for (name, tag) in lets {
                module_lets.insert(name, tag);
            }
            env.module = Some(ModuleScope {
                key: module.to_string(),
                lets: module_lets,
                touched,
            });
            // The file's prelude union seeds the body: a module
            // function sees its own file's top-level declarations,
            // never the importer's. Execution matches: module calls
            // run isolated (seed plus init contributions), so the gate
            // here approves exactly what the runtime will provide.
            if let Some(union) = self.module_env_unions.get(module) {
                for name in union {
                    env.declare_env(name.clone());
                }
            }
        } else if !self.entry_env_union.is_empty() {
            // Entry top-level declarations persist globally for later
            // calls, so entry functions seed from the entry union.
            for name in self.entry_env_union.clone() {
                env.declare_env(name);
            }
        }
        let scope = Scope {
            label: key,
            ret: RetCtx::Func(key.to_string()),
            loops: 0,
            site: None,
            line: 0,
            col: 0,
            idx: 0,
        };
        let out = self.walk_body(&info.body, &mut env, &scope, depth + 1)?;
        if !enforce_types {
            // Touch probe: reads are recorded in the module scope
            // above; return-type enforcement lives in the dedicated
            // passes, which have the nested knowledge this walk lacks.
            return Ok(StaticTy::Ty(TypeTag::Any));
        }
        let declared = &info.declared;
        let is_void = matches!(declared, TypeTag::Void);
        if info.is_async && !matches!(declared, TypeTag::Handle | TypeTag::HandleOf(_)) {
            bail!(
                "function `{key}` declares {}, but ASYNC FUNC must declare HANDLE or HANDLE<T>",
                render_structural(declared)
            );
        }
        if out.has_return && out.falls_through && !is_void {
            bail!(
                "function `{key}` declares {} but may fall through with no RETURN on some path",
                render_structural(declared)
            );
        }
        if !out.has_return {
            if is_void {
                return Ok(StaticTy::Void);
            }
            bail!(
                "function `{key}` declares {} but has no RETURN on any path",
                render_structural(declared)
            );
        }
        if out.bare_return && !is_void {
            bail!(
                "function `{key}` declares {} but has a bare RETURN on some path; bare RETURN is only a void exit",
                render_structural(declared)
            );
        }
        if is_void {
            if let Some((ty, line, _col)) = out.types.first() {
                bail!(
                    "function `{key}` declares VOID but RETURN at line {line} yields {}",
                    render_structural(ty)
                );
            }
            return Ok(StaticTy::Void);
        }
        if out.types.is_empty() {
            bail!(
                "function `{key}` declares {} but has no value RETURN on any path",
                render_structural(declared)
            );
        }
        for (ty, line, _col) in &out.types {
            if !is_compatible(ty, declared) {
                bail!(
                    "function `{key}` declares {} but RETURN at line {line} yields {}",
                    render_structural(declared),
                    render_structural(ty)
                );
            }
        }
        Ok(StaticTy::Ty(declared.clone()))
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
        let base_env_frames = env.env_frames.len();
        let mut out = WalkOut {
            falls_through: true,
            has_return: false,
            bare_return: false,
            types: Vec::new(),
        };
        for (idx, step) in steps.iter().enumerate() {
            self.visits += 1;
            // The read site carries the conjunction of every enclosing
            // guard, not just the step's own: a loop variable declared
            // under a block guard reads cleanly anywhere beneath it.
            // A bare step under no guard keeps a bare site, so
            // top-level coverage is unchanged.
            let site = match (&scope.site, &step.guard) {
                (None, None) => None,
                (Some(rule), None) | (None, Some(rule)) => Some(rule.clone()),
                (Some(outer), Some(own)) => Some(GuardExpr::all(vec![outer.clone(), own.clone()])),
            };
            let scoped = Scope {
                label: scope.label,
                ret: scope.ret.clone(),
                loops: scope.loops,
                site,
                line: step.line,
                col: step.col,
                idx,
            };
            let scope = &scoped;
            // Positional scopes mirror execution exactly: pushed
            // before the guard check, restored after the step, even
            // for dead steps (net effect zero there).
            for _ in 0..step.scope_enter {
                env.frames.push(HashMap::new());
                env.env_frames.push(HashSet::new());
            }
            // Guard `[env:KEY]` references read the environment: they
            // need the same declaration as any other use. Checked
            // before the step runs, mirroring evaluation order.
            if let Some(guard) = step.guard.as_ref()
                && !matches!(classify_guard(Some(guard)), GuardClass::Dead)
            {
                let mut names = Vec::new();
                Self::guard_env_names(guard, &mut names);
                for name in names {
                    self.check_env_name(env, &name, "env:", scope)?;
                }
            }
            match classify_guard(step.guard.as_ref()) {
                GuardClass::Dead => {}
                GuardClass::Conditional => {
                    let inner = self.walk_step(step, env, scope, idx, depth)?;
                    out.has_return |= inner.has_return;
                    out.bare_return |= inner.bare_return;
                    out.types.extend(inner.types);
                }
                GuardClass::Unconditional => {
                    let inner = self.walk_step(step, env, scope, idx, depth)?;
                    out.has_return |= inner.has_return;
                    out.bare_return |= inner.bare_return;
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
                if env.env_frames.len() > base_env_frames {
                    env.env_frames.pop();
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
            bare_return: false,
            types: Vec::new(),
        };
        match &step.kind {
            StepKind::Return { expr, is_bare } => {
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
                // Bare RETURN is the void exit, never a value. It marks the
                // path as returning without contributing a value type.
                if *is_bare {
                    let mut out = live;
                    out.has_return = true;
                    out.falls_through = false;
                    out.bare_return = true;
                    return Ok(out);
                }
                let ty = self.infer_expr(expr, env, scope, idx, depth + 1)?;
                let mut out = live;
                out.has_return = true;
                out.falls_through = false;
                if let StaticTy::Ty(tag) = ty {
                    out.types.push((tag, scope.line, scope.col));
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
                    bare_return: false,
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
                    out.bare_return |= arm_out.bare_return;
                    out.types.extend(arm_out.types);
                    arms_terminal &= !arm_out.falls_through;
                }
                match else_body {
                    Some(arm) => {
                        let mut fork = env.fork();
                        let arm_out = self.walk_body(arm, &mut fork, scope, depth + 1)?;
                        out.has_return |= arm_out.has_return;
                        out.bare_return |= arm_out.bare_return;
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
                out.has_return = body_out.has_return;
                out.bare_return = body_out.bare_return;
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
                out.bare_return = body_out.bare_return;
                out.types = body_out.types;
                Ok(out)
            }
            StepKind::FuncDef {
                name,
                params,
                return_type,
                is_async,
                body,
                ..
            } => {
                let mut resolved = Vec::with_capacity(params.len());
                for (pname, ptype) in params {
                    let tag = self
                        .exec
                        .resolve_tag(ptype)
                        .map_err(|err| anyhow::anyhow!("function `{name}`: {err:#}"))?;
                    self.exec.check_declaration_tag(
                        &format!("function `{name}` param `${pname}`"),
                        &tag,
                    )?;
                    resolved.push((clean_var(pname), tag));
                }
                let declared = self
                    .exec
                    .resolve_tag(return_type)
                    .map_err(|err| anyhow::anyhow!("function `{name}`: {err:#}"))?;
                self.exec
                    .check_return_tag(&format!("function `{name}` returns"), &declared)
                    .map_err(|err| anyhow::anyhow!("function `{name}`: {err:#}"))?;
                let info = FuncInfo {
                    params: resolved,
                    declared,
                    is_async: *is_async,
                    body: body.clone(),
                    module: env.module.as_ref().map(|scope| scope.key.clone()),
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
                self.exec
                    .check_declaration_tag(&format!("step {}", idx + 1), &tag)?;
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
                self.walk_wrapped(step, cmd, env, scope, idx, depth + 1)?;
                let tag = self
                    .exec
                    .resolve_tag(decl_type)
                    .map_err(|err| anyhow::anyhow!("step {}: {err:#}", idx + 1))?;
                self.exec
                    .check_declaration_tag(&format!("step {}", idx + 1), &tag)?;
                // Yield check mirroring runtime assign_capture (see
                // capture_yield): without it a TIMEOUT-wrapped (or any
                // non-call) command sails through with an incompatible
                // declaration and fails only at runtime coercion.
                if let Some(yield_tag) = self.capture_yield(cmd, depth)?
                    && self
                        .check_arg(
                            &tag,
                            StaticTy::Ty(yield_tag.clone()),
                            &format!("step {}", idx + 1),
                        )
                        .is_err()
                {
                    bail!(
                        "step {}: LET ${} captures {} but declares {}",
                        idx + 1,
                        clean_var(var),
                        render_structural(&yield_tag),
                        render_structural(&tag)
                    );
                }
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
                let handle_tag = Self::check_name_read(env, task_var, scope, idx)?;
                let tag = self
                    .exec
                    .resolve_tag(out_type)
                    .map_err(|err| anyhow::anyhow!("step {}: {err:#}", idx + 1))?;
                self.exec
                    .check_declaration_tag(&format!("step {}", idx + 1), &tag)?;
                let payload = match handle_tag {
                    TypeTag::Handle => TypeTag::Any,
                    TypeTag::HandleOf(inner) => (*inner).clone(),
                    other => bail!(
                        "step {}: AWAIT ${} is {}, not HANDLE",
                        idx + 1,
                        clean_var(task_var),
                        render_structural(&other)
                    ),
                };
                if matches!(payload, TypeTag::Void) {
                    bail!(
                        "step {}: AWAIT ${} is HANDLE<VOID>: no value to bind to `${}`; use bare AWAIT for join",
                        idx + 1,
                        clean_var(task_var),
                        clean_var(out_var)
                    );
                }
                if !is_compatible(&payload, &tag) && !is_compatible(&tag, &payload) {
                    // Top-down: capture type must accept the task payload.
                    // Allow coercible pairs via check_arg shape.
                    if self
                        .check_arg(
                            &tag,
                            StaticTy::Ty(payload.clone()),
                            &format!("step {}", idx + 1),
                        )
                        .is_err()
                    {
                        bail!(
                            "step {}: AWAIT ${} yields {} but `${}` declares {}",
                            idx + 1,
                            clean_var(task_var),
                            render_structural(&payload),
                            clean_var(out_var),
                            render_structural(&tag)
                        );
                    }
                }
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
                    line: scope.line,
                    col: scope.col,
                    idx: scope.idx,
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
                // function. Every ASYNC position is HANDLE-headed.
                let mut fork = env.fork();
                let task = Scope {
                    label: scope.label,
                    ret: RetCtx::AsyncValue,
                    loops: 0,
                    site: scope.site.clone(),
                    line: scope.line,
                    col: scope.col,
                    idx: scope.idx,
                };
                let inner = self.walk_body(body, &mut fork, &task, depth + 1)?;
                let tag = self
                    .exec
                    .resolve_tag(decl_type)
                    .map_err(|err| anyhow::anyhow!("step {}: {err:#}", idx + 1))?;
                // HANDLE-headed first (the async rule owns that
                // message), then the shared position rule for nested
                // VOID (HANDLE<LIST<VOID>> and friends).
                let payload = match &tag {
                    TypeTag::Handle => TypeTag::Any,
                    TypeTag::HandleOf(inner_tag) => (**inner_tag).clone(),
                    _ => bail!(
                        "step {}: ASYNC `${}` declares {}, not HANDLE; every ASYNC position is HANDLE-headed",
                        idx + 1,
                        clean_var(var),
                        render_structural(&tag)
                    ),
                };
                self.exec
                    .check_declaration_tag(&format!("step {}", idx + 1), &tag)
                    .map_err(|err| anyhow::anyhow!("step {}: {err:#}", idx + 1))?;
                if inner.bare_return && !matches!(payload, TypeTag::Void) {
                    bail!(
                        "step {}: ASYNC `${}` declares {} but task has a bare RETURN; bare RETURN is only a void exit",
                        idx + 1,
                        clean_var(var),
                        render_structural(&tag)
                    );
                }
                if matches!(payload, TypeTag::Void) {
                    if let Some((ty, line, _col)) = inner.types.first() {
                        bail!(
                            "step {}: HANDLE<VOID> task RETURN at line {line} yields {}",
                            idx + 1,
                            render_structural(ty)
                        );
                    }
                } else if inner.types.is_empty() {
                    if !matches!(payload, TypeTag::Any) {
                        bail!(
                            "step {}: ASYNC `${}` declares {} but task has no value RETURN",
                            idx + 1,
                            clean_var(var),
                            render_structural(&tag)
                        );
                    }
                } else {
                    for (ty, line, _col) in &inner.types {
                        if !is_compatible(ty, &payload) {
                            bail!(
                                "step {}: ASYNC `${}` declares {} but task RETURN at line {line} yields {}",
                                idx + 1,
                                clean_var(var),
                                render_structural(&tag),
                                render_structural(ty)
                            );
                        }
                    }
                }
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
                out.bare_return = inner.bare_return;
                out.types = inner.types;
                out.falls_through = inner.falls_through;
                if !inner.falls_through {
                    return Ok(out);
                }
                Ok(out)
            }
            StepKind::Isolated { body } => {
                // Same in-place scoping as TIMEOUT: `RETURN` inside
                // still returns from the enclosing function; only the
                // child env is scrubbed, never the type flow. Depth
                // tracks nesting so the allowlist node stays legal at
                // any depth below this head, including nested blocks
                // and nested `ISOLATED`.
                self.isolated_depth += 1;
                // Scrub outer declarations for the walk: the runtime
                // resets the live map, so outer names are unreadable
                // inside unless the allowlist (or an in-block `ENV`)
                // re-adds them. Saved and restored around the body so
                // declarations inside never leak outward, exactly like
                // the runtime scope pop. Types keep flowing in place.
                let saved = std::mem::replace(&mut env.env_frames, vec![HashSet::new()]);
                let inner = self.walk_body(body, env, scope, depth + 1);
                env.env_frames = saved;
                let inner = inner?;
                self.isolated_depth -= 1;
                let mut out = live;
                out.has_return = inner.has_return;
                out.bare_return = inner.bare_return;
                out.types = inner.types;
                out.falls_through = inner.falls_through;
                if !inner.falls_through {
                    return Ok(out);
                }
                Ok(out)
            }
            StepKind::InheritEnv { keys } => {
                // Declaration, not a read: each key becomes readable
                // from here on in this scope. Absent host names read as
                // empty at runtime, but the name is declared either way,
                // so later uses pass here and resolve empty when missing.
                for key in keys {
                    env.declare_env(key.clone());
                }
                Ok(live)
            }
            StepKind::IsolatedInheritEnv { keys } => {
                // Only the parser produces this node, strictly inside
                // `ISOLATED` bodies; anywhere else it is a bug, never a
                // user spelling (the grammar has no other production).
                // Re-adds scrubbed snapshot names for the rest of the
                // block, mirroring `isolated_inherit_env` one for one.
                if self.isolated_depth == 0 {
                    return Err(anyhow::anyhow!(
                        "step {}: isolated allowlist outside ISOLATED",
                        idx + 1
                    ));
                }
                for key in keys {
                    env.declare_env(key.clone());
                }
                Ok(live)
            }
            StepKind::RemoteBlock {
                target,
                vars,
                env: header_env,
                body,
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
                // Header `env:` names read the outer environment at
                // ship time, so each must be declared outside; inside
                // the seal they arrive declared.
                for name in header_env {
                    self.check_env_name(env, name, "env:", scope).map_err(|_| {
                        anyhow::anyhow!(
                            "{} in {}: REMOTE '{target}' ships undeclared environment variable `env:{name}`: declare it first with ENV or INHERIT_ENV",
                            step_pos(scope.line, scope.col, scope.idx),
                            scope.label,
                        )
                    })?;
                    sealed.declare_env(name.clone());
                }
                // Body declarations shadow injected names: the seal
                // pushes a scope exactly like a braced block.
                sealed.frames.push(HashMap::new());
                sealed.env_frames.push(HashSet::new());
                let remote = Scope {
                    label: scope.label,
                    ret: RetCtx::Top,
                    loops: 0,
                    site: None,
                    line: scope.line,
                    col: scope.col,
                    idx: scope.idx,
                };
                self.walk_body(body, &mut sealed, &remote, depth + 1)?;
                Ok(live)
            }
            StepKind::WithIo { bindings, cmd } => {
                // Bindings resolve before the wrapped command runs.
                Self::check_pipe_bindings(env, bindings, scope, idx)?;
                self.walk_wrapped(step, cmd, env, scope, idx, depth + 1)
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
            StepKind::Prompt { var, .. } => {
                // Same declare-or-mutate contract as `read_line`:
                // the answer binds as `STRING`, mutating when present.
                let key = clean_var(var);
                if !env.is_declared(&key) {
                    env.insert_top(key, TypeTag::String, step.guard.as_ref());
                }
                Ok(live)
            }
            StepKind::Env { key, .. } => {
                // `ENV KEY=value`: the value may read live bindings
                // (checked first, so self-reads fail), then the key
                // becomes readable from here on in this scope.
                self.check_step_args(&step.kind, env, scope, idx, depth)?;
                env.declare_env(key.clone());
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
                    StepKind::ListAppend { list, item } => {
                        let tag = Self::check_name_read(env, list, scope, idx)?;
                        let element = match tag {
                            TypeTag::List => None,
                            TypeTag::ListOf(element) => Some(element.as_ref().clone()),
                            // Every non-list variant is listed: a new
                            // tag fails here until its push rule is
                            // explicit, so appends can never silently
                            // enter an unexamined shape.
                            TypeTag::String
                            | TypeTag::Int
                            | TypeTag::Float
                            | TypeTag::Bool
                            | TypeTag::Map
                            | TypeTag::Pipe
                            | TypeTag::Handle
                            | TypeTag::HandleOf(_)
                            | TypeTag::Void
                            | TypeTag::Duration
                            | TypeTag::Path
                            | TypeTag::Semaphore
                            | TypeTag::Permit
                            | TypeTag::Any
                            | TypeTag::Custom(_)
                            | TypeTag::MapOf(_)
                            | TypeTag::Record(_)
                            | TypeTag::Union(..) => bail!(
                                "step {}: LIST_APPEND ${}: variable is {}, not LIST",
                                idx + 1,
                                clean_var(list),
                                render_structural(&tag)
                            ),
                        };
                        // Shaped lists check the item exactly (no coercion
                        // applies on push, so unlike call arguments there
                        // is no gate: unknown items defer to the runtime
                        // walk, which validates every push).
                        if let Some(expected) = element {
                            let actual = match item {
                                oxdock_parser::Arg::Expr(expr) => {
                                    self.infer_expr(expr, env, scope, idx, depth + 1)?
                                }
                                _ => StaticTy::Ty(TypeTag::String),
                            };
                            let StaticTy::Ty(actual) = actual else {
                                bail!(
                                    "step {}: LIST_APPEND ${}: VOID has no value to push",
                                    idx + 1,
                                    clean_var(list)
                                );
                            };
                            Self::check_exact(&expected, &actual, &format!("step {}", idx + 1))?;
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
    /// fragments (`Arg::Parts`) read live bindings; literal text
    /// (`Arg::String`, `ArgPart::Text`) scans for embedded
    /// `{{ env:KEY }}` reads through the declaration gate. Built on the
    /// exhaustive `walk_exprs` / `walk_raw_strings` visitors, so a new
    /// expression- or string-carrying variant fails compilation in the
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
        // Embedded `{{ env:KEY }}` placeholders in literal text expand
        // at runtime through the same `expand_string` as whole-arg
        // expressions, so they read through the same declaration gate.
        // `Arg::String` and `ArgPart::Text` carry nothing else.
        let mut failed: Option<anyhow::Error> = None;
        kind.walk_raw_strings(&mut |text| {
            if failed.is_none()
                && let Err(err) = self.check_template_env(text, env, scope)
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
                bare_return: false,
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
    /// caller, so this inherits the live context directly. Coordinates
    /// come from the outer step, so errors inside the box name the
    /// source line that produced them instead of going coordinate-less.
    fn walk_wrapped(
        &mut self,
        outer: &Step,
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
            line: outer.line,
            col: outer.col,
        };
        self.walk_step(&step, env, scope, idx, depth)
    }

    /// Yield type of a `LET $x: T = <command>` capture, mirroring
    /// runtime `assign_capture`: a `Call` (under `WithIo`/`TIMEOUT`
    /// layers, single-command bodies only) binds the callee's
    /// `RETURN` type; an `ImportFile` binds a pipeline value the
    /// checker cannot spell, so it defers (`None`); anything else
    /// binds captured stdout `TEXT`. `TIMEOUT` peels exactly where
    /// the runtime peels, so a deadline never changes the checked
    /// type — only whether the binding happens before it fires.
    fn capture_yield(&mut self, cmd: &StepKind, depth: usize) -> Result<Option<TypeTag>> {
        let mut current = cmd;
        loop {
            match current {
                StepKind::WithIo { cmd, .. } => current = cmd,
                StepKind::Timeout { body, .. } if body.len() == 1 => {
                    current = &body[0].kind;
                }
                StepKind::Call { name, .. } => {
                    let callee = self.resolve_call(name, depth)?;
                    return Ok(match callee.ret {
                        StaticTy::Ty(tag) => Some(tag),
                        // A VOID callee still binds something at
                        // runtime (coerced on declaration); defer.
                        StaticTy::Void => None,
                    });
                }
                StepKind::ImportFile { .. } => return Ok(None),
                _ => return Ok(Some(TypeTag::String)),
            }
        }
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
                render_structural(expected),
                render_structural(expected)
            );
        }
        if tags_equal(expected, &actual) {
            return Ok(());
        }
        // Union admission is subset-based and order-free, combining
        // with the pairs below (a `STRING` actual still reaches the
        // `INT` pair through the `STRING` member, never around it).
        // Actual-union goes first: every member must be admitted
        // (any member may flow), which reduces unions to members
        // before the expected-union rule sees them. Expected-union
        // then admits where some member admits (the value holds one
        // runtime shape). Recursion bottoms out immediately:
        // construction flattens, so members never nest unions.
        if let TypeTag::Union(_, members) = actual {
            for member in members.iter() {
                self.check_arg(expected, StaticTy::Ty((*member).clone()), ctx)?;
            }
            return Ok(());
        }
        if let TypeTag::Union(_, members) = expected {
            for member in members.iter() {
                if self
                    .check_arg(member, StaticTy::Ty(actual.clone()), ctx)
                    .is_ok()
                {
                    return Ok(());
                }
            }
            return Err(exact_mismatch(ctx, expected, &actual));
        }
        // Mirror coerce_value/coerce_scalar: allow pairs the runtime
        // decides by value; reject pairs that always bail. `STRING`
        // never coerces into `INT` or `FLOAT`, and `FLOAT` never
        // narrows into `INT`: those need the explicit `INT()` or
        // `FLOAT()` converter at the call site. Shaped values satisfy
        // bare words (a record IS a map); bare words never satisfy
        // fixed shapes, except the unknown list shape: an empty or
        // heterogeneous literal infers bare `LIST`, which enters
        // `LIST<...>` vacuously (mirroring the bare `MAP` into
        // `MAP<...>` arm). Literals infer their own shapes, so
        // dynamic values still gate through `LET` instead of slipping
        // into shaped positions. Same-constructor shapes compare
        // exactly below; the runtime walk re-validates every element,
        // so a mixed literal still fails loudly at execution.
        if coercible_pair(&actual, expected) {
            return Ok(());
        }
        // Handles delegate to the canonical relation so the gates
        // cannot drift: shaped satisfies bare by payload (VOID never
        // flows); bare satisfies only bare or ANY-payload. See
        // is_compatible.
        if matches!(expected, TypeTag::Handle | TypeTag::HandleOf(_))
            && matches!(actual, TypeTag::Handle | TypeTag::HandleOf(_))
        {
            return match is_compatible(&actual, expected) {
                true => Ok(()),
                false => Err(exact_mismatch(ctx, expected, &actual)),
            };
        }
        if matches!(
            (&actual, expected),
            (TypeTag::ListOf(_), TypeTag::ListOf(_))
                | (TypeTag::MapOf(_), TypeTag::MapOf(_))
                | (TypeTag::Record(_), TypeTag::Record(_))
                | (TypeTag::Record(_), TypeTag::MapOf(_))
        ) {
            return Self::check_exact(expected, &actual, ctx);
        }
        Err(exact_mismatch(ctx, expected, &actual))
    }

    /// Exact shape compatibility for same-constructor tags: element by
    /// element, field by field, with `ANY` deferring to the runtime
    /// walk. No scalar coercion applies inside collections (the walk
    /// checks exactly), so mismatches fail here naming the position.
    /// Record comparison is order-free: same names with compatible
    /// types satisfy regardless of declaration order.
    fn check_exact(expected: &TypeTag, actual: &TypeTag, ctx: &str) -> Result<()> {
        if tags_equal(expected, actual) {
            return Ok(());
        }
        if matches!(expected, TypeTag::Any) || matches!(actual, TypeTag::Any) {
            return Ok(());
        }
        match (actual, expected) {
            (TypeTag::ListOf(a), TypeTag::ListOf(e)) | (TypeTag::MapOf(a), TypeTag::MapOf(e)) => {
                Self::check_exact(e, a, ctx)
            }
            (TypeTag::HandleOf(a), TypeTag::HandleOf(e)) => Self::check_exact(e, a, ctx),
            (TypeTag::HandleOf(a), TypeTag::Handle) => {
                if matches!(a.as_ref(), TypeTag::Void) {
                    return Err(exact_mismatch(ctx, expected, actual));
                }
                Ok(())
            }
            // Bare satisfies only bare or ANY-payload, mirroring
            // is_compatible so element checks cannot drift either.
            (TypeTag::Handle, TypeTag::HandleOf(e)) => {
                if matches!(e.as_ref(), TypeTag::Any) {
                    Ok(())
                } else {
                    Err(exact_mismatch(ctx, expected, actual))
                }
            }
            (TypeTag::Record(a_fields), TypeTag::Record(e_fields)) => {
                for field in e_fields.iter() {
                    match a_fields.iter().find(|found| found.name == field.name) {
                        Some(found) => Self::check_exact(&field.ty, &found.ty, ctx)?,
                        None if field.optional => {}
                        None => bail!("{ctx}: missing field '{}'", field.name),
                    }
                }
                for field in a_fields.iter() {
                    if !e_fields.iter().any(|known| known.name == field.name) {
                        bail!("{ctx}: unknown field '{}'", field.name);
                    }
                }
                Ok(())
            }
            (TypeTag::Record(a_fields), TypeTag::MapOf(inner)) => {
                for field in a_fields.iter() {
                    Self::check_exact(inner, &field.ty, ctx)?;
                }
                Ok(())
            }
            (TypeTag::ListOf(_), TypeTag::List)
            | (TypeTag::Record(_), TypeTag::Map)
            | (TypeTag::MapOf(_), TypeTag::Map) => Ok(()),
            // Union sets match order-free both ways: construction
            // deduplicates, so containment both ways is set equality.
            (TypeTag::Union(_, a_members), TypeTag::Union(_, e_members)) => {
                if a_members.len() == e_members.len()
                    && a_members.iter().all(|actual| {
                        e_members
                            .iter()
                            .any(|member| Self::check_exact(member, actual, ctx).is_ok())
                    })
                {
                    Ok(())
                } else {
                    Err(exact_mismatch(ctx, expected, actual))
                }
            }
            // A concrete actual against a union expectation (a union
            // actual pairs with the arm above): some member must match
            // exactly, since no coercion applies inside collections.
            // Total over actual shapes by construction: every word is
            // tested against the members, so future variants behave
            // identically without listing.
            (_, TypeTag::Union(_, e_members)) => {
                if e_members
                    .iter()
                    .any(|member| Self::check_exact(member, actual, ctx).is_ok())
                {
                    Ok(())
                } else {
                    Err(exact_mismatch(ctx, expected, actual))
                }
            }
            _ => Err(exact_mismatch(ctx, expected, actual)),
        }
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
                    .map(|param| {
                        (
                            param.name.clone(),
                            param.param_type.clone().unwrap_or(TypeTag::Any),
                        )
                    })
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

    /// Whether checking this function observes module-shared state
    /// (#115): runs the body check with an armed probe and reports the
    /// flag. Used by the harvest gate, statically and at load time.
    fn func_touches_module(&mut self, key: &str) -> Result<bool> {
        let info = self.funcs.get(key).cloned();
        let Some(info) = info else {
            return Ok(false);
        };
        if info.module.is_none() {
            return Ok(false);
        }
        let probe = std::rc::Rc::new(std::cell::Cell::new(false));
        let _ = self.check_function_body_probed(key, &info, 0, Some(probe.clone()), false)?;
        Ok(probe.get())
    }

    /// Memoized script return with recursion guard, keyed by the
    /// registry key the call resolved through.
    fn func_return_owned(&mut self, info: &FuncInfo, key: &str, depth: usize) -> Result<StaticTy> {
        if let Some(known) = self.inferred.get(key) {
            return Ok(known.clone());
        }
        if self.inferring.iter().any(|name| name == key) {
            return Ok(StaticTy::Ty(TypeTag::Any));
        }
        self.inferring.push(key.to_string());
        let result = self.check_function_body(key, info, depth);
        self.inferring.pop();
        let ty = result?;
        self.inferred.insert(key.to_string(), ty.clone());
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
        // Arity range comes from the full host metadata (which carries
        // the optional flags); script FUNCs and bare shape lists are
        // always exact. Missing lists (INSPECT-like) skip the check:
        // the dedicated AST path owns that call shape.
        let (required, total) = match callee.host_params.as_deref() {
            Some(host) => (required_arity(host), host.len()),
            None => match callee.params.as_deref() {
                Some(shape) => (shape.len(), shape.len()),
                None => (0, usize::MAX),
            },
        };
        if args.len() < required || args.len() > total {
            if required == total {
                bail!(
                    "step {}: {}() expects {} argument(s), got {}",
                    idx + 1,
                    base_name(name),
                    total,
                    args.len()
                );
            }
            bail!(
                "step {}: {}() expects {} to {} argument(s), got {}",
                idx + 1,
                base_name(name),
                required,
                total,
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
        let (required, total) = match host_params.as_deref() {
            Some(host) => (required_arity(host), host.len()),
            None => match params.as_deref() {
                Some(shape) => (shape.len(), shape.len()),
                // INSPECT-like entries skip arity: the dedicated AST
                // path owns that call shape.
                None => (0, usize::MAX),
            },
        };
        if arg_tys.len() < required || arg_tys.len() > total {
            if required == total {
                bail!(
                    "{}{display}() expects {} argument(s), got {}",
                    loc(at),
                    total,
                    arg_tys.len()
                );
            }
            bail!(
                "{}{display}() expects {} to {} argument(s), got {}",
                loc(at),
                required,
                total,
                arg_tys.len()
            );
        }
        for (position, actual) in arg_tys.iter().enumerate() {
            let Some(expected) = params.as_deref().and_then(|list| list.get(position)) else {
                continue;
            };
            let (pname, ptype) = expected;
            let pat = format!("{}{display}() argument `${pname}`", loc(at));
            self.check_arg(ptype, (*actual).clone(), &pat)?;
            if let Some(host_list) = host_params.as_deref()
                && let Some(param) = host_list.get(position)
                && let Some(allowed) = param.allowed
                && let Some(Some(text)) = literals.get(position)
                && !allowed.contains(text)
            {
                bail!("{pat} must be one of: {}, got {text:?}", allowed.join(", "));
            }
        }
        Ok(ret.clone())
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
    /// Infer a list literal's shape: homogeneous non-empty lists lock
    /// their element tag through the interned spelling (shared with
    /// declarations, so identical shapes unify). Uniformly maplike or
    /// listlike elements join to the unknown shape (`LIST<MAP<ANY>>`,
    /// `LIST<LIST<ANY>>`); anything else stays bare `LIST` for the
    /// runtime walk to decide. Never fails: shapes that cannot be
    /// spelled fall back instead of erroring.
    fn infer_list_literal(
        &mut self,
        items: &[Expr],
        env: &mut Env,
        scope: &Scope,
        idx: usize,
        depth: usize,
    ) -> Result<StaticTy> {
        let mut tags = Vec::with_capacity(items.len());
        for item in items {
            match self.infer_expr(item, env, scope, idx, depth + 1)? {
                StaticTy::Ty(tag) => tags.push(tag),
                StaticTy::Void => return Ok(StaticTy::Ty(TypeTag::List)),
            }
        }
        let Some(first) = tags.first() else {
            return Ok(StaticTy::Ty(TypeTag::List));
        };
        if matches!(first, TypeTag::Any) || !tags.iter().all(|tag| tags_equal(first, tag)) {
            let join = if tags.iter().all(is_maplike) {
                Some("LIST<MAP<ANY>>")
            } else if tags.iter().all(is_listlike) {
                Some("LIST<LIST<ANY>>")
            } else {
                None
            };
            let Some(spelling) = join else {
                return Ok(StaticTy::Ty(TypeTag::List));
            };
            return Ok(self
                .exec
                .resolve_tag(spelling)
                .map(StaticTy::Ty)
                .unwrap_or(StaticTy::Ty(TypeTag::List)));
        }
        let spelling = format!("LIST<{}>", render_structural(first));
        Ok(self
            .exec
            .resolve_tag(&spelling)
            .map(StaticTy::Ty)
            .unwrap_or(StaticTy::Ty(TypeTag::List)))
    }

    /// Infer a map literal's shape through its canonical spelling, so
    /// conforming literals share the interned entry that declarations
    /// resolve to. Source order is significant (spellings canonicalize
    /// without reordering): matching declaration order unifies, anything
    /// else falls back to bare shape handling downstream. Never fails.
    fn infer_map_literal(
        &mut self,
        entries: &[(String, Expr)],
        env: &mut Env,
        scope: &Scope,
        idx: usize,
        depth: usize,
    ) -> Result<StaticTy> {
        let mut parts = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            match self.infer_expr(value, env, scope, idx, depth + 1)? {
                StaticTy::Ty(tag) => parts.push(format!("{key}: {}", render_structural(&tag))),
                StaticTy::Void => return Ok(StaticTy::Ty(TypeTag::Map)),
            }
        }
        if parts.is_empty() {
            return Ok(StaticTy::Ty(TypeTag::Map));
        }
        let spelling = format!("MAP<{}>", parts.join(", "));
        Ok(self
            .exec
            .resolve_tag(&spelling)
            .map(StaticTy::Ty)
            .unwrap_or(StaticTy::Ty(TypeTag::Map)))
    }

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
            Expr::Literal(value) => {
                // String literals expand `{{ env:KEY }}` at runtime
                // through `expand_string`, so embedded reads gate here
                // exactly like whole-arg expressions.
                if let Some(text) = value.as_str() {
                    self.check_template_env(text, env, scope)?;
                }
                Ok(StaticTy::Ty(literal_tag(value)))
            }
            Expr::Var(name) => {
                let key = clean_var(name);
                if !env.is_declared(&key) {
                    bail!("{}undefined variable ${key}", loc(Some(&at())));
                }
                Ok(StaticTy::Ty(env.read_guarded(&key, scope.site.as_ref())?))
            }
            Expr::Env(name) => {
                self.check_env_name(env, name, "env:", scope)?;
                Ok(StaticTy::Ty(TypeTag::String))
            }
            Expr::KeyPath { base, keys } => {
                let key = clean_var(base);
                if !env.is_declared(&key) {
                    bail!("{}undefined variable ${key}", loc(Some(&at())));
                }
                let mut current = env.read_guarded(&key, scope.site.as_ref())?;
                for part in keys {
                    match current {
                        TypeTag::Record(fields) => {
                            let Some(field) = fields
                                .iter()
                                .find(|field| field.name.as_str() == part.as_str())
                            else {
                                bail!(
                                    "{}: unknown field '{part}'; expected fields: {}",
                                    at(),
                                    fields
                                        .iter()
                                        .map(|field| field.name.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                );
                            };
                            current = field.ty.clone();
                        }
                        TypeTag::ListOf(element) => {
                            // Shaped lists step into the element type on
                            // numeric segments (bounds stay runtime);
                            // anything else mirrors the evaluator's
                            // invalid-index failure statically.
                            if part.parse::<usize>().is_err() {
                                bail!("{}: Invalid array index '{part}'", at());
                            }
                            current = element.as_ref().clone();
                        }
                        TypeTag::MapOf(values) => {
                            // Unknown maps step into their value shape;
                            // keys resolve by value at runtime.
                            current = values.as_ref().clone();
                        }
                        // MAP keys and LIST indices resolve by value at
                        // runtime (missing keys, bounds); the shape
                        // stays unknown here.
                        TypeTag::Map | TypeTag::List | TypeTag::Any => {
                            return Ok(StaticTy::Ty(TypeTag::Any));
                        }
                        // Unions defer when some member traverses (the
                        // runtime value holds one member; static cannot
                        // know which) and fail only when no member can:
                        // an all-scalar set definitely fails, exactly
                        // like a scalar below.
                        TypeTag::Union(_, members) => {
                            let traversable = |member: &TypeTag| {
                                matches!(
                                    member,
                                    TypeTag::Record(_)
                                        | TypeTag::ListOf(_)
                                        | TypeTag::MapOf(_)
                                        | TypeTag::Map
                                        | TypeTag::List
                                        | TypeTag::Any
                                )
                            };
                            if members.iter().all(|member| !traversable(member)) {
                                bail!(
                                    "{}: Cannot traverse into scalar value at key '{part}'",
                                    at()
                                );
                            }
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
            Expr::List(items) => self.infer_list_literal(items, env, scope, idx, depth),
            Expr::Map(entries) => self.infer_map_literal(entries, env, scope, idx, depth),
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
                        line: scope.line,
                        col: scope.col,
                        idx: scope.idx,
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
                if out.types.is_empty() {
                    return Ok(StaticTy::Void);
                }
                let mut unified: Option<TypeTag> = None;
                for (ty, _, _) in out.types {
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
                MathOp::PushConst(value) => {
                    if let Some(text) = value.as_str() {
                        self.check_template_env(text, env, scope)?;
                    }
                    stack.push(StaticTy::Ty(literal_tag(value)))
                }
                MathOp::LoadVar(name) => {
                    let key = clean_var(name);
                    if !env.is_declared(&key) {
                        bail!("undefined variable ${key}");
                    }
                    stack.push(StaticTy::Ty(env.read_guarded(&key, scope.site.as_ref())?));
                }
                MathOp::LoadEnv(name) => {
                    self.check_env_name(env, name, "env:", scope)?;
                    stack.push(StaticTy::Ty(TypeTag::String))
                }
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
                                            .map(|field| field.name.as_str())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    );
                                };
                                current = field.ty.clone();
                            }
                            TypeTag::ListOf(element) => {
                                if part.parse::<usize>().is_err() {
                                    bail!("Invalid array index '{part}'");
                                }
                                current = element.as_ref().clone();
                            }
                            TypeTag::MapOf(values) => {
                                current = values.as_ref().clone();
                            }
                            TypeTag::Map | TypeTag::List | TypeTag::Any => {
                                current = TypeTag::Any;
                                break;
                            }
                            // Unions mirror the AST path: defer when
                            // some member traverses, fail only when no
                            // member can.
                            TypeTag::Union(_, members) => {
                                let traversable = |member: &TypeTag| {
                                    matches!(
                                        member,
                                        TypeTag::Record(_)
                                            | TypeTag::ListOf(_)
                                            | TypeTag::MapOf(_)
                                            | TypeTag::Map
                                            | TypeTag::List
                                            | TypeTag::Any
                                    )
                                };
                                if members.iter().all(|member| !traversable(member)) {
                                    bail!("Cannot traverse into scalar value at key '{part}'");
                                }
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
    use oxdock_process::MockProcessManager;

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

    /// Single source of truth for test coverage of every TypeTag
    /// constructor. The grammar is deliberately ident-only: `_` is
    /// not an ident, so a wildcard arm is SYNTACTICALLY
    /// inexpressible here -- the bypass an LLM would otherwise reach
    /// for (the compiler even suggests it for E0004) cannot be
    /// written. Representatives are DERIVED from the names, not
    /// authored, so a bogus representative is equally inexpressible.
    /// Each line expands to BOTH an inventory entry and an anchor
    /// match arm, atomically:
    /// - `units`: bare constructors; pattern and representative are
    ///   the constructor itself.
    /// - `shaped`: single-payload constructors with one representative
    ///   payload each; pattern ignores the payload, representative
    ///   wraps it.
    /// - `special`: multi-field constructors with hand-fixed shapes
    ///   (Custom carries a descriptor, Record a field table, Union two
    ///   slots); patterns and representatives are hardcoded in the
    ///   macro body. A new multi-field constructor forces a visible
    ///   macro-body edit.
    ///
    /// Adding any constructor without extending this list breaks
    /// compilation at the invocation below (non-exhaustive anchor),
    /// so coverage can never silently lag the enum. Deleting or
    /// hollowing the anchor is review-visible sabotage, not drift:
    /// it shows up as test deletion in the diff.
    macro_rules! tag_coverage {
        (
            units: [ $($unit:ident),* $(,)? ],
            shaped: [ $($shaped:ident ( $($payload:ident),* )),* $(,)? ],
            special: [ $($special:ident),* $(,)? ]
        ) => {
            fn variant_inventory() -> Vec<TypeTag> {
                vec![
                    $( TypeTag::$unit, )*
                    $( $( TypeTag::$shaped(std::sync::Arc::new(TypeTag::$payload)), )* )*
                    $( special_representative(stringify!($special)), )*
                ]
            }
            // One arm per constructor: payloads nest under their
            // variant, so no arm is shadowed and no constructor is
            // listed twice. Adding a constructor without extending
            // this list breaks compilation here.
            fn anchor_exhaustive(tag: &TypeTag) {
                match tag {
                    $( TypeTag::$unit => {} ),*,
                    $( TypeTag::$shaped(_) => {} ),*,
                    TypeTag::Custom(_) => {},
                    TypeTag::Record(_) => {},
                    TypeTag::Union(..) => {},
                }
            }
        };
    }
    fn special_representative(which: &str) -> TypeTag {
        match which {
            "Custom" => TypeTag::Custom(oxdock_parser::startup_descriptors()[0].1),
            "Record" => TypeTag::Record(
                vec![oxdock_parser::Field {
                    name: "name".to_string(),
                    ty: TypeTag::String,
                    docs: "",
                    optional: false,
                }]
                .into(),
            ),
            "Union" => TypeTag::union(vec![TypeTag::Int, TypeTag::String]),
            _ => unreachable!("tag_coverage special must name Custom, Record, or Union"),
        }
    }
    tag_coverage! {
        units: [String, Int, Float, Bool, List, Map, Pipe, Handle, Void, Duration, Path, Semaphore, Permit, Any],
        shaped: [HandleOf(String, Int, Void, Any), ListOf(Int), MapOf(String)],
        special: [Custom, Record, Union]
    }

    /// Tag universe for the subtyping laws: the full constructor
    /// inventory (macro-generated above) plus representative
    /// compounds (VOID payloads, nested containers, wider unions).
    fn law_universe() -> Vec<TypeTag> {
        use std::sync::Arc;
        let mut universe = variant_inventory();
        universe.extend([
            TypeTag::HandleOf(Arc::new(TypeTag::Void)),
            TypeTag::HandleOf(Arc::new(TypeTag::Any)),
            TypeTag::HandleOf(Arc::new(TypeTag::Int)),
            TypeTag::HandleOf(Arc::new(TypeTag::ListOf(Arc::new(TypeTag::Int)))),
            TypeTag::HandleOf(Arc::new(TypeTag::Union(
                "INT|STRING".into(),
                vec![TypeTag::Int, TypeTag::String].into(),
            ))),
            TypeTag::ListOf(Arc::new(TypeTag::Union(
                "INT|STRING".into(),
                vec![TypeTag::Int, TypeTag::String].into(),
            ))),
            TypeTag::ListOf(Arc::new(TypeTag::ListOf(Arc::new(TypeTag::Int)))),
            TypeTag::MapOf(Arc::new(TypeTag::Union(
                "INT|STRING".into(),
                vec![TypeTag::Int, TypeTag::String].into(),
            ))),
            TypeTag::Record(
                vec![
                    oxdock_parser::Field {
                        name: "req".to_string(),
                        ty: TypeTag::ListOf(Arc::new(TypeTag::String)),
                        docs: "",
                        optional: false,
                    },
                    oxdock_parser::Field {
                        name: "opt".to_string(),
                        ty: TypeTag::Int,
                        docs: "",
                        optional: true,
                    },
                ]
                .into(),
            ),
            TypeTag::Union(
                "INT|STRING|BOOL".into(),
                vec![TypeTag::Int, TypeTag::String, TypeTag::Bool].into(),
            ),
            TypeTag::Union(
                "LIST<INT>|MAP<STRING>".into(),
                vec![
                    TypeTag::ListOf(Arc::new(TypeTag::Int)),
                    TypeTag::MapOf(Arc::new(TypeTag::String)),
                ]
                .into(),
            ),
        ]);
        universe
    }

    // NOTE: no duplicates test here on purpose. The macro above makes
    // omission impossible (anchor fails compilation) and duplication
    // harmless (laws are universal quantifications; running one twice
    // changes nothing). Payload variety inside `shaped` (VOID/ANY
    // payloads) is intentional and pinned by the dedicated handle
    // laws below, which hardcode their tags rather than relying on
    // inventory membership -- so eroding the list cannot silently
    // weaken them. What remains is review-visible diff, not drift.
    #[test]
    fn anchor_covers_inventory() {
        // Ties the two macro outputs together at runtime: every
        // inventoried tag passes through the exhaustive anchor.
        // (Compile-time exhaustiveness is what forces future arms;
        // this asserts the pairing stays live.)
        for tag in variant_inventory() {
            anchor_exhaustive(&tag);
        }
    }

    #[test]
    fn subtyping_is_reflexive() {
        for tag in law_universe() {
            assert!(
                is_compatible(&tag, &tag),
                "must admit itself: {}",
                render_structural(&tag)
            );
        }
    }

    #[test]
    fn void_is_bottom() {
        // VOID unifies only with VOID: it never flows into a value
        // slot, not even ANY.
        for tag in law_universe() {
            let is_void = matches!(tag, TypeTag::Void);
            assert_eq!(
                is_compatible(&TypeTag::Void, &tag),
                is_void,
                "VOID actual vs {}",
                render_structural(&tag)
            );
            assert_eq!(
                is_compatible(&tag, &TypeTag::Void),
                is_void,
                "{} actual vs VOID",
                render_structural(&tag)
            );
        }
    }

    #[test]
    fn any_declared_is_top_for_values() {
        for tag in law_universe() {
            if matches!(tag, TypeTag::Void) {
                continue;
            }
            assert!(
                is_compatible(&tag, &TypeTag::Any),
                "{} must flow into ANY",
                render_structural(&tag)
            );
        }
    }

    #[test]
    fn any_actual_admits_nothing_concrete() {
        // An untyped value never satisfies a concrete contract (the
        // LET-gate rule); only ANY itself admits it.
        for tag in law_universe() {
            assert_eq!(
                is_compatible(&TypeTag::Any, &tag),
                matches!(tag, TypeTag::Any),
                "ANY actual vs {}",
                render_structural(&tag)
            );
        }
    }

    #[test]
    fn handle_bare_is_any_payload() {
        // The isomorphism both ways, VOID exclusion included: bare
        // HANDLE behaves exactly as HANDLE<ANY>.
        use std::sync::Arc;
        for payload in [TypeTag::String, TypeTag::Int, TypeTag::Void, TypeTag::Any] {
            let shaped = TypeTag::HandleOf(Arc::new(payload));
            assert_eq!(
                is_compatible(&shaped, &TypeTag::Handle),
                is_compatible(&shaped, &TypeTag::HandleOf(Arc::new(TypeTag::Any))),
                "shaped->bare must equal shaped->HANDLE<ANY>: {}",
                render_structural(&shaped)
            );
            assert!(
                !is_compatible(
                    &TypeTag::HandleOf(Arc::new(TypeTag::Void)),
                    &TypeTag::Handle
                ),
                "HANDLE<VOID> must never erase to HANDLE"
            );
        }
        assert!(is_compatible(
            &TypeTag::Handle,
            &TypeTag::HandleOf(Arc::new(TypeTag::Any))
        ));
        assert!(!is_compatible(
            &TypeTag::Handle,
            &TypeTag::HandleOf(Arc::new(TypeTag::String))
        ));
    }

    #[test]
    fn subtyping_is_transitive() {
        // Fuzz the preorder law over the whole universe: if a admits
        // into b and b into c, a must admit into c. A blind `=> true`
        // arm or a misplaced ANY check breaks this combinatorically,
        // so inconsistent admission rules cannot hide behind a few
        // hand-picked cases. VOID is excluded only as an endpoint the
        // bottom law already pins (VOID admits nothing, nothing
        // admits VOID except VOID).
        let universe = law_universe();
        for a in &universe {
            for b in &universe {
                if !is_compatible(a, b) {
                    continue;
                }
                for c in &universe {
                    if !is_compatible(b, c) {
                        continue;
                    }
                    assert!(
                        is_compatible(a, c),
                        "transitivity violated: {} <: {} <: {} but {} !<: {}",
                        render_structural(a),
                        render_structural(b),
                        render_structural(c),
                        render_structural(a),
                        render_structural(c)
                    );
                }
            }
        }
    }

    #[test]
    fn union_subtyping_is_covariant() {
        let narrow = TypeTag::Union(
            "INT|STRING".into(),
            vec![TypeTag::Int, TypeTag::String].into(),
        );
        let wide = TypeTag::Union(
            "INT|STRING|BOOL".into(),
            vec![TypeTag::Int, TypeTag::String, TypeTag::Bool].into(),
        );
        assert!(is_compatible(&TypeTag::Int, &narrow));
        assert!(is_compatible(&TypeTag::String, &narrow));
        assert!(!is_compatible(&TypeTag::Bool, &narrow));
        assert!(is_compatible(&narrow, &wide));
        assert!(!is_compatible(&wide, &narrow));
        assert!(!is_compatible(&narrow, &TypeTag::Int));
    }

    #[test]
    fn check_exact_handles_mirror_compatible() {
        // The element gate agrees with the canonical relation:
        // shaped satisfies bare unless VOID; bare satisfies only
        // bare or ANY-payload. One documented divergence: ANY
        // payloads defer to the runtime walk (expected ANY or actual
        // ANY passes), exactly like LIST<ANY> vs LIST<STRING> — the
        // strict top-down checker (is_compatible) has no such
        // deferral by design.
        use std::sync::Arc;
        type C = Checker<'static, MockProcessManager>;
        let string_handle = || TypeTag::HandleOf(Arc::new(TypeTag::String));
        let void_handle = || TypeTag::HandleOf(Arc::new(TypeTag::Void));
        let any_handle = || TypeTag::HandleOf(Arc::new(TypeTag::Any));
        assert!(C::check_exact(&TypeTag::Handle, &string_handle(), "t").is_ok());
        assert!(C::check_exact(&TypeTag::Handle, &void_handle(), "t").is_err());
        assert!(C::check_exact(&string_handle(), &string_handle(), "t").is_ok());
        assert!(C::check_exact(&string_handle(), &any_handle(), "t").is_ok());
        assert!(C::check_exact(&any_handle(), &TypeTag::Handle, "t").is_ok());
        assert!(C::check_exact(&string_handle(), &TypeTag::Handle, "t").is_err());
        assert!(C::check_exact(&void_handle(), &TypeTag::Handle, "t").is_err());
    }
}
