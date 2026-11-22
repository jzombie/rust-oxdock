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

/// Home-scope function index: (defining file, base) to all guarded
/// overloads. Single alias so the complex type appears once.
type HomeFuncs = HashMap<(String, String), Vec<(Option<GuardExpr>, FuncInfo)>>;

use anyhow::{Result, bail};
use oxdock_fs::GuardedPath;
use oxdock_parser::constants::{
    KEYWORD_BREAK, KEYWORD_CONST, KEYWORD_CONTINUE, KEYWORD_IMPORT, KEYWORD_LET, MODULE_SEPARATOR,
};
use oxdock_parser::template::{
    PlaceholderKey, TemplateSegment, TemplateSegments, classify_placeholder_key,
};
use oxdock_parser::{
    CompareOp, ExportKind, Expr, GuardExpr, IoBinding, MathOp, PipeTarget, Step, StepKind, TypeTag,
    Value, base_name, qualify, render_structural, split_qualified,
};
use oxdock_process::ProcessManager;

use super::{
    ExecState, FuncMeta, FuncParam,
    import::{ClosureFile, ClosureSite},
    required_arity, resolve_script_params,
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
#[derive(Clone, Debug)]
struct FuncInfo {
    params: Vec<(String, TypeTag, bool)>,
    declared: TypeTag,
    is_async: bool,
    body: Vec<Step>,
    module: Option<String>,
    /// Base name for per-function import overlays (see
    /// `body_alias_scopes`): the call-site spellings vary, the
    /// (owner, name, definition site) triple does not.
    name: String,
    /// Definition site: distinguishes same-named block-local
    /// functions in different blocks of one unit.
    line: usize,
    col: usize,
}

/// One top-level binding with its guard and constness. Guards ride along
/// so platform-specific `LET`s/`CONST`s enforce coverage instead of
/// reading as unconditional. `is_const` distinguishes the binding
/// family: `SET`, `LIST_APPEND`, and redeclaration diagnostics consult
/// it, and only `is_const` bindings satisfy `EXPORT CONST`.
#[derive(Clone, Debug)]
struct GuardedLet {
    name: String,
    tag: TypeTag,
    guard: Option<GuardExpr>,
    is_const: bool,
}
/// Module-owned bindings by module key.
type ModuleLets = HashMap<String, Vec<GuardedLet>>;

/// Fold top-level bindings into a static scope map: one entry list
/// per name so disjoint guards coexist exactly like the runtime
/// same-frame walk. Single owner behind module-store seeding and
/// entry `CONST` seeding; callers filter first (modules seed every
/// family, entry seeds `CONST` only).
fn seed_map(bindings: &[GuardedLet]) -> HashMap<String, Vec<Binding>> {
    let mut map: HashMap<String, Vec<Binding>> = HashMap::new();
    for entry in bindings {
        map.entry(entry.name.clone()).or_default().push(Binding {
            tag: entry.tag.clone(),
            guard: entry.guard.clone(),
            is_const: entry.is_const,
        });
    }
    map
}

/// Lexical environment: a stack of frames mirroring the runtime
/// `var_scopes`. Frames push on `scope_enter` markers, branch and
/// loop bodies, and function entries; they pop on `scope_exit` or are
/// discarded with a fork. `LET` declares in the top frame only (a
/// duplicate there mirrors the runtime redeclaration error);
/// shadowing an outer frame is legal exactly where the runtime
/// pushes a scope. `SET` and reads walk outward; types never change
/// on mutation (runtime coerces).
/// One live declaration: its tag, the guard it was declared under,
/// and whether it is a `CONST`. Same-scope duplicates with disjoint
/// guards coexist (only one path ever binds); reads unify across
/// entries since the path is unknowable statically.
#[derive(Clone, Debug)]
struct Binding {
    tag: TypeTag,
    guard: Option<GuardExpr>,
    is_const: bool,
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
    /// Body-local nested `FUNC` definitions, one map per lexical
    /// frame (#322): mirrors the runtime registry save/restore
    /// frames, so a nested def shadows the home top-level and
    /// globals for the rest of its block in both layers. Pushed and
    /// popped with every other frame below, never separately.
    func_frames: Vec<HashMap<String, FuncInfo>>,
}

/// Module-shared bindings plus a fork-shared touch flag. `Rc` (not
/// `Arc`): the checker is single-threaded; forks clone the `Rc` so a
/// touch on any conditional path is still observed after the fork is
/// discarded. Bindings keep per-declaration guards, so platform-guarded
/// top-level `LET`s enforce the same coverage as lexical frames.
#[derive(Clone, Debug)]
struct ModuleScope {
    /// Owning module key, so nested checks re-seed from the checker's
    /// module table instead of inheriting possibly-stale bindings.
    key: String,
    lets: HashMap<String, Vec<Binding>>,
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
/// `BREAK` is legal with `loops > 0` or `switches > 0` (a `SWITCH`
/// consumes it, so an enclosing loop never sees it); `CONTINUE`
/// needs `loops > 0` (a `SWITCH` never captures it). Blocks,
/// functions, and tasks reset both (control never crosses those
/// boundaries at runtime).
#[derive(Clone)]
struct Scope<'x> {
    label: &'x str,
    ret: RetCtx,
    loops: usize,
    switches: usize,
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
    /// Each entry carries its declaration guard, so platform-guarded
    /// bindings enforce path coverage instead of reading unconditionally.
    module_lets: ModuleLets,
    /// Entry top-level `LET`s: the full union, fallback for uncalled
    /// functions and single-file probes. Reachable entry bodies seed
    /// from `entry_let_scopes` instead: only bindings declared before
    /// the function's earliest reachable call site, mirroring the
    /// runtime outer scope the call actually observes. Guards preserved
    /// per binding so disjoint platform declarations accumulate instead
    /// of overwriting.
    entry_lets: Vec<GuardedLet>,
    /// Per-entry-function top-level `LET` view: bindings declared
    /// before the function's earliest reachable call site, keyed by
    /// (name, line, col) like `body_scopes`. Missing entries (uncalled
    /// dead code, single-file probes) fall back to the full
    /// `entry_lets` union.
    entry_let_scopes: HashMap<(String, usize, usize), Vec<GuardedLet>>,
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
    /// Template-capable host metadata only (`Pure` plus `Both` template
    /// arms). Runtime `{{ MODULE::FUNC }}` resolves through
    /// `pure_function_table`, never `Ctx` or `Script`, so the template
    /// gate reads this instead of `hosts`.
    pure_hosts: HashMap<String, FuncMeta>,
    inferred: HashMap<String, StaticTy>,
    inferring: Vec<String>,
    visits: u64,
    /// Fail closed on qualified calls no table can resolve: set for
    /// the closure pass (which sees every unit's imports) so sibling
    /// aliases and typos bail instead of reading `ANY`. Bare
    /// `Checker::new` uses (single-file validation, collect-phase
    /// touch probes) stay lenient: without the closure they cannot
    /// tell an unimported alias from a not-yet-collected one.
    strict_qualified: bool,
    /// Home-scope function index (#319): every top-level `FUNC` per
    /// closure file, exported or private, keyed by (defining file,
    /// base name) to all guarded overloads. Disjoint guarded defs
    /// coexist like runtime `top_funcs`; resolution filters by the
    /// active site guard instead of last-wins. Entry top-level
    /// `FUNC`s index under (`"entry"`, base) the same way, so entry
    /// bodies resolve their own file frame (#322).
    home_funcs: HomeFuncs,
    /// First textual top-level `FUNC` per file, keyed like
    /// `home_funcs` (#322): harvests bind the first matching
    /// definition (mirroring `import_funcs`' first-match lookup),
    /// while the file frame serves the last live one. Separate
    /// index so neither lookup can drift into the other's rule.
    harvest_first: HashMap<(String, String), FuncInfo>,
    /// Bare-harvest visibility (#322): (importing unit, base) to
    /// defining file. An explicit harvest names its file, served in
    /// the importing unit like the runtime harvest map. Entry imports
    /// key under `"entry"`.
    harvested: HashMap<(String, String), String>,
    /// Outbound alias targets for the checked unit: `ALIAS` to
    /// (defining file, import guard) pairs. `resolve_call` requires a
    /// unique resolvable target, so same-alias different-file imports
    /// cannot cross-resolve.
    alias_targets: super::native::AliasTargets,
    /// `EXPORT CONST` names per module key, populated alongside
    /// `module_lets` for the closure pass. Qualified `$ALIAS::NAME`
    /// reads gate against these: only listed names resolve, and only
    /// to `CONST` bindings.
    module_const_exports: HashMap<String, Vec<String>>,
    /// Outbound alias targets per closure file, keyed by module key:
    /// the unit scope each file's own qualified reads resolve under.
    /// [`resolve_const_ref`](Checker::resolve_const_ref) chases
    /// qualified `EXPORT CONST` forwards through these, so a hop
    /// always resolves in its exporting file's namespace, never the
    /// reader's.
    file_targets: HashMap<String, super::native::AliasTargets>,
    /// `EXPORT FUNC` names per module key, built from the closure
    /// files alongside `module_const_exports`. `resolve_call` reads
    /// these for the #319 missing-import hint: only listed
    /// (exported, never private) names can be suggested.
    module_func_exports: HashMap<String, Vec<String>>,
    /// Per-function import overlays: file-top imports plus the
    /// function's own subtree imports, keyed by (owner module key,
    /// `"entry"` for entry functions, function name). Swapped in while
    /// checking that body so function-local imports resolve without
    /// leaking into sibling functions or the top level.
    body_scopes: BodyScopes,
    /// Nesting depth inside `ISOLATED` bodies: the isolated allowlist node
    /// is legal only below an `ISOLATED` head, at any depth including
    /// through nested blocks and nested `ISOLATED` itself.
    isolated_depth: usize,
}

/// Read-only [`ModuleGraphView`](super::native::ModuleGraphView) over
/// borrowed closure tables. One struct serves the static twin and the
/// eager closure check, so both read the same tables through the same
/// code.
struct CheckerConstView<'a> {
    exports: &'a HashMap<String, Vec<String>>,
    targets: &'a HashMap<String, super::native::AliasTargets>,
    lets: &'a ModuleLets,
}

impl super::native::ModuleGraphView for CheckerConstView<'_> {
    fn const_exports(&self, file: &str) -> Option<&[String]> {
        self.exports.get(file).map(Vec::as_slice)
    }

    fn alias_targets(&self, file: &str) -> super::native::AliasTargets {
        self.targets.get(file).cloned().unwrap_or_default()
    }

    fn binding(&self, file: &str, name: &str) -> super::native::BindingPresence {
        use super::native::BindingPresence;
        match self
            .lets
            .get(file)
            .and_then(|lets| lets.iter().find(|entry| entry.name == name))
        {
            Some(entry) if entry.is_const => BindingPresence::Const,
            Some(_) => BindingPresence::Let,
            None => BindingPresence::Absent,
        }
    }
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
            0,
            0,
        )?;
        if checker.func_touches_module(&probe_key)? {
            return Ok(Some(name.clone()));
        }
    }
    Ok(None)
}

/// Top-level `LET` bindings of one file body: the declarations that
/// execution makes visible to that file's functions (drained into the
/// shared store for modules, held in the outer scope for the entry).
/// Names resolve through the same tags execution uses; dead-guarded
/// steps are skipped exactly like collection skips them. Each binding
/// keeps its declaration guard so guarded top-level `LET`s enforce
/// path coverage instead of reading as unconditional.
fn module_lets<P: ProcessManager>(steps: &[Step], exec: &ExecState<P>) -> Result<Vec<GuardedLet>> {
    Ok(module_lets_indexed(steps, exec)?
        .into_iter()
        .map(|(binding, _)| binding)
        .collect())
}

/// Top-level `LET` declarations with their step indices: backs
/// earliest-call seeding so entry functions only observe bindings
/// declared before their first reachable call. Single owner for the
/// binding-form match; [`module_lets`] strips the indices.
fn module_lets_indexed<P: ProcessManager>(
    steps: &[Step],
    exec: &ExecState<P>,
) -> Result<Vec<(GuardedLet, usize)>> {
    let mut out = Vec::new();
    for (idx, step) in steps.iter().enumerate() {
        if matches!(classify_guard(step.guard.as_ref()), GuardClass::Dead) {
            continue;
        }
        // Every top-level binding counts, whatever its right-hand
        // side, so capture/async/readline initializers resolve
        // exactly like plain `LET`/`CONST`.
        match &step.kind {
            StepKind::Bind {
                var,
                decl_type,
                is_const,
                ..
            } => {
                let tag = exec
                    .resolve_tag(decl_type)
                    .map_err(|err| anyhow::anyhow!("variable `${var}`: {err:#}"))?;
                out.push((
                    GuardedLet {
                        name: clean_var(var),
                        tag,
                        guard: step.guard.clone(),
                        is_const: *is_const,
                    },
                    idx,
                ));
            }
            StepKind::ReadLine { var } | StepKind::Prompt { var, .. } => {
                out.push((
                    GuardedLet {
                        name: clean_var(var),
                        tag: TypeTag::String,
                        guard: step.guard.clone(),
                        is_const: false,
                    },
                    idx,
                ));
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
        exec.register_alias(name.clone(), target.clone());
    }
    let outcome = f(&mut *exec);
    for (name, prev) in saved {
        match prev {
            Some(target) => {
                exec.register_alias(name, target);
            }
            None => {
                exec.remove_alias(&name);
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
    is_entry: bool,
) -> Result<()> {
    // Entry-first resolution shared with expansion: `resolve_static_top_level`
    // probes the entry dir before `dir` for entry-unit imports, so an
    // outside-workspace entry finds its sibling lib instead of aborting
    // with "not in the import closure". Nested file units resolve purely
    // against their own directory.
    let entry_parent = exec.entry_file.as_ref().and_then(|entry| entry.parent());
    let Some(key) =
        super::import::resolve_static_top_level(entry_parent.as_ref(), dir, files, path, is_entry)
    else {
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
    let Some(file) = files.iter().find(|f| f.key == key) else {
        anyhow::bail!("IMPORT `{path}` cannot be read: not in the import closure");
    };
    let aliases = super::typing::collect_type_aliases(&file.steps, exec)?;
    for name in type_names {
        // Harvest validity lives in `super::import::gate_harvested_name`
        // (single owner, shared with the runtime): gate first, persist
        // after. The fetch below cannot miss (gated), but keeps loud
        // failure instead of restating the scan.
        super::import::gate_harvested_name(
            ExportKind::Type,
            &file.steps,
            &file.exports,
            name,
            path,
        )?;
        let Some(target) = aliases.get(name) else {
            anyhow::bail!("IMPORT `{path}` has no such TYPE `{name}`");
        };
        super::import::insert_imported_type(exec, name, target, alias, path)?;
    }
    Ok(())
}

/// Bare-harvest edges (#322): (importing unit, base) to defining
/// file, walked per importing file (entry keyed `"entry"`). Path
/// resolution reuses the shared static top-level resolver, so
/// defining keys match the runtime harvest map exactly. Host-module
/// paths are skipped: host funcs resolve through the host map.
/// Nested bodies included: validation is conservative, like
/// expansion; unexecuted conditional harvests fail loud at runtime.
fn collect_func_harvest_edges<P: ProcessManager>(
    exec: &ExecState<P>,
    files: &[super::import::ClosureFile],
    dir: &GuardedPath,
    steps: &[Step],
    unit: &str,
    is_entry: bool,
    out: &mut HashMap<(String, String), String>,
) -> Result<()> {
    for step in steps {
        if let StepKind::ImportFuncs {
            path,
            alias: None,
            func_names,
            ..
        } = &step.kind
            && !func_names.is_empty()
            && !exec.module_table.modules.contains_key(path)
        {
            let entry_parent = exec.entry_file.as_ref().and_then(|entry| entry.parent());
            let Some(key) = super::import::resolve_static_top_level(
                entry_parent.as_ref(),
                dir,
                files,
                path,
                is_entry,
            ) else {
                anyhow::bail!("IMPORT `{path}` cannot be read: not in the import closure");
            };
            for name in func_names {
                out.insert((unit.to_string(), name.clone()), key.clone());
            }
        }
        for body in super::import::child_bodies(&step.kind) {
            collect_func_harvest_edges(exec, files, dir, body, unit, is_entry, out)?;
        }
    }
    Ok(())
}

/// Merge every `IMPORT [TYPE ...]` harvest in `steps` (including
/// nested bodies: validation is conservative, like expansion).
/// `is_entry` selects entry-first closure-key resolution for the entry
/// unit; file units resolve purely against their own directory.
fn merge_harvests_in<P: ProcessManager>(
    exec: &mut ExecState<P>,
    files: &[ClosureFile],
    dir: &GuardedPath,
    steps: &[Step],
    is_entry: bool,
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
            merge_harvest(
                exec,
                files,
                dir,
                path,
                alias.as_deref(),
                type_names,
                is_entry,
            )?;
        }
        for body in super::import::child_bodies(&step.kind) {
            merge_harvests_in(exec, files, dir, body, is_entry)?;
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
        for name in &file.exports.types {
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
        merge_harvests_in(exec, files, &entry_base, entry, true)?;
        for file in files {
            merge_harvests_in(exec, files, &file.dir, &file.steps, false)?;
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
                for name in &file.exports.types {
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
        let mut all_lets: ModuleLets = HashMap::new();
        let mut all_const_exports: HashMap<String, Vec<String>> = HashMap::new();
        // Home-scope index (#319): every top-level `FUNC` per file,
        // exported or private, for sibling validation. Built
        // alongside `all_funcs` so both see identical definitions.
        let mut home_funcs: HomeFuncs = HashMap::new();
        // First textual def per file for harvest binding (#322):
        // mirrors `import_funcs` first-match. Extended alongside
        // `home_funcs` so both indexes see identical definitions.
        let mut harvest_first: HashMap<(String, String), FuncInfo> = HashMap::new();
        // Bare-harvest visibility (#322): base name to defining file
        // for every bare harvest in the closure. Global like the
        // runtime harvest map: an explicit harvest names its file.
        let mut harvested: HashMap<(String, String), String> = HashMap::new();
        // Per-unit file-import aliases: the entry's own outbound imports
        // plus each file's own outbound imports (nested included).
        // Zero-function modules contribute no `ALIAS::` func keys, so
        // the alias set (not a func-key prefix scan) proves the
        // namespace known. Scoped per unit so sibling modules cannot
        // cross-resolve each other's `AS` aliases.
        let entry_base = super::import::import_base(exec);
        let entry_parent = exec.entry_file.as_ref().and_then(|entry| entry.parent());
        let mut entry_targets: super::native::AliasTargets = HashMap::new();
        unit_imports(
            entry,
            &entry_base,
            files,
            entry_parent.as_ref(),
            &mut entry_targets,
        );
        let mut file_targets: HashMap<String, super::native::AliasTargets> = HashMap::new();
        for file in files {
            let mut targets = HashMap::new();
            unit_imports(&file.steps, &file.dir, files, None, &mut targets);
            file_targets.insert(file.key.clone(), targets);
        }
        // Per-function overlays: file-top imports plus each body
        // subtree, so function-local imports resolve inside their
        // function without leaking into sibling functions or the
        // top level (mirroring the runtime dynamic scope).
        let mut body_scopes: BodyScopes = HashMap::new();
        collect_body_scopes(
            entry,
            super::import::ENTRY_UNIT,
            &entry_base,
            files,
            entry_parent.as_ref(),
            &entry_targets,
            &mut body_scopes,
        );
        for file in files {
            collect_body_scopes(
                &file.steps,
                &file.key,
                &file.dir,
                files,
                None,
                &file_targets.get(&file.key).cloned().unwrap_or_default(),
                &mut body_scopes,
            );
        }
        let entry_defs: Vec<(String, String)> = super::typing::collect_type_aliases(entry, exec)?
            .into_iter()
            .collect();
        let mut entry_lets: Vec<GuardedLet> = Vec::new();
        with_scope_aliases(exec, &entry_defs, |exec| {
            let mut checker = Checker::new(exec);
            checker.collect(entry)?;
            // Entry file frame (#322): index the entry top-level
            // `FUNC`s under the entry unit key, mirroring
            // `collect_home` for files (dead-guarded defs skipped).
            for step in entry {
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
                    let info = checker.make_func_info(
                        name,
                        params,
                        return_type,
                        *is_async,
                        body,
                        None,
                        step.line,
                        step.col,
                    )?;
                    checker
                        .home_funcs
                        .entry((super::import::ENTRY_UNIT.to_string(), name.clone()))
                        .or_default()
                        .push((step.guard.clone(), info));
                }
            }
            extend_funcs_checked(&mut all_funcs, checker.funcs, "entry")?;
            home_funcs.extend(checker.home_funcs);
            entry_lets = module_lets(entry, exec)?;
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
                checker.collect_home(file)?;
                extend_funcs_checked(&mut all_funcs, checker.funcs, &file.path)?;
                home_funcs.extend(checker.home_funcs);
                harvest_first.extend(checker.harvest_first);
                all_lets.insert(file.key.clone(), module_lets(&file.steps, exec)?);
                all_const_exports.insert(file.key.clone(), file.exports.consts.clone());
                Ok(())
            })?;
        }
        // Eager forward check: every qualified `EXPORT CONST
        // ALIAS::NAME` must resolve through the shared walk now, even
        // when no expression reads it yet. An unread dangling
        // re-export is still a broken export list, so it fails here
        // instead of waiting for a first read that never comes.
        {
            let view = CheckerConstView {
                exports: &all_const_exports,
                targets: &file_targets,
                lets: &all_lets,
            };
            for file in files {
                for entry in &file.exports.consts {
                    if let Some((_, base)) = split_qualified(entry) {
                        super::native::chase_const_forward(&view, &file.key, base, entry)
                            .map_err(|err| anyhow::anyhow!("{}: {err}", file.key))?;
                    }
                }
            }
        }
        // Bare-harvest edges (#322): walk the entry and every file for
        // unaliased harvests, attributed to the file holding the step.
        // Entry imports key under the entry unit.
        collect_func_harvest_edges(
            exec,
            files,
            &entry_base,
            entry,
            super::import::ENTRY_UNIT,
            true,
            &mut harvested,
        )?;
        for file in files {
            collect_func_harvest_edges(
                exec,
                files,
                &file.dir,
                &file.steps,
                &file.key,
                false,
                &mut harvested,
            )?;
        }
        // Earliest-call LET views for entry functions (entry runs
        // top level sequentially, so calls only observe earlier
        // bindings; module bodies keep their whole-file union).
        let entry_let_scopes = entry_let_scopes(entry, &all_funcs, exec)?;
        // Infer every collected function under its owner's scope
        // FIRST, into one shared memo, so call sites reuse
        // owner-checked signatures instead of inferring in whatever
        // scope happens to be current. Sorted for determinism.
        let mut inferred: HashMap<String, StaticTy> = HashMap::new();
        let mut keys: Vec<String> = all_funcs.keys().cloned().collect();

        keys.sort();
        for key in keys {
            let owner = all_funcs.get(&key).and_then(|info| info.module.clone());
            let defs: Vec<(String, String)> = match &owner {
                None => entry_defs.clone(),
                Some(module) => files
                    .iter()
                    .find(|f| f.key == *module)
                    .map(|file| {
                        super::typing::collect_type_aliases(&file.steps, exec)
                            .map(|aliases| aliases.into_iter().collect())
                    })
                    .transpose()?
                    .unwrap_or_default(),
            };
            // Inference runs under the owner's own alias scope, so a
            // sibling module's alias never makes an unknown callee look
            // known while inferring.
            let owner_targets: super::native::AliasTargets = match &owner {
                None => entry_targets.clone(),
                Some(module) => file_targets.get(module).cloned().unwrap_or_default(),
            };
            with_scope_aliases(exec, &defs, |exec| {
                let mut checker = seeded_checker(
                    exec,
                    &all_funcs,
                    &home_funcs,
                    &harvest_first,
                    &harvested,
                    &all_lets,
                    &all_const_exports,
                    &entry_lets,
                    &inferred,
                    entry,
                    files,
                    &owner_targets,
                    &body_scopes,
                    &entry_let_scopes,
                    &file_targets,
                );
                let ty = checker.func_return(&key, 0)?;
                inferred.insert(key.clone(), ty);
                Ok(())
            })?;
        }
        // Infer private home entries under the same owner scope, so a
        // broken unexported helper fails here like any collected
        // function instead of surfacing only when a sibling calls it.
        // Entries already inferred under a collected key are skipped.
        // Sorted for determinism.
        let mut home_keys: Vec<(String, String)> = home_funcs.keys().cloned().collect();
        home_keys.sort();
        for (module, base) in home_keys {
            let canonical = super::native::file_func_key(&module, &base);
            if inferred.contains_key(&canonical) {
                continue;
            }
            let Some(overloads) = home_funcs.get(&(module.clone(), base.clone())) else {
                continue;
            };
            let Some((_, info)) = overloads.last().cloned() else {
                continue;
            };
            let defs: Vec<(String, String)> = files
                .iter()
                .find(|f| f.key == module)
                .map(|file| {
                    super::typing::collect_type_aliases(&file.steps, exec)
                        .map(|aliases| aliases.into_iter().collect())
                })
                .transpose()?
                .unwrap_or_default();
            let owner_targets: super::native::AliasTargets =
                file_targets.get(&module).cloned().unwrap_or_default();
            with_scope_aliases(exec, &defs, |exec| {
                let mut checker = seeded_checker(
                    exec,
                    &all_funcs,
                    &home_funcs,
                    &harvest_first,
                    &harvested,
                    &all_lets,
                    &all_const_exports,
                    &entry_lets,
                    &inferred,
                    entry,
                    files,
                    &owner_targets,
                    &body_scopes,
                    &entry_let_scopes,
                    &file_targets,
                );
                let ty = checker.func_return_owned(&info, &canonical, 0)?;
                inferred.insert(canonical.clone(), ty);
                Ok(())
            })?;
        }
        // Check every body with exactly its own scope visible: entry
        // defs for the entry, file defs for each file. Calls resolve
        // against the pre-inferred signatures; bodies never see
        // another file's defs.
        /// Fresh checker seeded with the collected functions, module lets,
        /// entry lets, and environment unions. Built per scope so every
        /// check runs with exactly its own file's aliases plus the shared
        /// harvests visible.
        #[allow(clippy::too_many_arguments)]
        fn seeded_checker<'a, P: ProcessManager>(
            exec: &'a mut ExecState<P>,
            funcs: &HashMap<String, FuncInfo>,
            home_funcs: &HomeFuncs,
            harvest_first: &HashMap<(String, String), FuncInfo>,
            harvested: &HashMap<(String, String), String>,
            lets: &ModuleLets,
            const_exports: &HashMap<String, Vec<String>>,
            entry_lets: &[GuardedLet],
            inferred: &HashMap<String, StaticTy>,
            entry: &[Step],
            files: &[ClosureFile],
            alias_targets: &super::native::AliasTargets,
            body_scopes: &BodyScopes,
            entry_let_scopes: &HashMap<(String, usize, usize), Vec<GuardedLet>>,
            file_targets: &HashMap<String, super::native::AliasTargets>,
        ) -> Checker<'a, P> {
            let mut checker = Checker::new(exec);
            checker.funcs = funcs.clone();
            checker.home_funcs = home_funcs.clone();
            checker.harvest_first = harvest_first.clone();
            checker.harvested = harvested.clone();
            checker.module_lets = lets.clone();
            checker.module_const_exports = const_exports.clone();
            checker.module_func_exports = files
                .iter()
                .map(|file| (file.key.clone(), file.exports.funcs.clone()))
                .collect();
            checker.entry_lets = entry_lets.to_vec();
            checker.entry_let_scopes = entry_let_scopes.clone();
            checker.inferred = inferred.clone();
            checker.strict_qualified = true;
            checker.alias_targets = alias_targets.clone();
            checker.file_targets = file_targets.clone();
            checker.body_scopes = body_scopes.clone();
            checker.build_env_unions(entry, files);
            checker
        }
        with_scope_aliases(exec, &entry_defs, |exec| {
            seeded_checker(
                exec,
                &all_funcs,
                &home_funcs,
                &harvest_first,
                &harvested,
                &all_lets,
                &all_const_exports,
                &entry_lets,
                &inferred,
                entry,
                files,
                &entry_targets,
                &body_scopes,
                &entry_let_scopes,
                &file_targets,
            )
            .check_top_level(entry)
        })?;
        for file in files {
            let defs: Vec<(String, String)> =
                super::typing::collect_type_aliases(&file.steps, exec)?
                    .into_iter()
                    .collect();
            let targets = file_targets.get(&file.key).cloned().unwrap_or_default();
            with_scope_aliases(exec, &defs, |exec| {
                seeded_checker(
                    exec,
                    &all_funcs,
                    &home_funcs,
                    &harvest_first,
                    &harvested,
                    &all_lets,
                    &all_const_exports,
                    &entry_lets,
                    &inferred,
                    entry,
                    files,
                    &targets,
                    &body_scopes,
                    &entry_let_scopes,
                    &file_targets,
                )
                .check_import_body(&file.key, &file.steps, Some(&file.key))
            })?;
        }
        Ok(())
    })();
    exec.restore_aliases(saved_global);
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
    checker.entry_lets = module_lets(steps, exec)?;
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

/// Single owner for `$` vs bare binding access in static checking:
/// every `Var`, `KeyPath` base, and `Inspect` read gates here, so AST
/// and RPN agree by construction. `is_const` true reads CONST only;
/// false reads LET only (a CONST under `$` names the bare form).
/// `prefix` carries the step attribution (`""` in RPN contexts whose
/// runtime errors carry no step number).
fn check_var_binding_access(
    env: &Env,
    key: &str,
    is_const: bool,
    site: Option<&GuardExpr>,
    prefix: &str,
) -> Result<TypeTag> {
    if is_const {
        if !env.is_declared(key) {
            bail!("{prefix}undefined CONST {key}");
        }
        if !env.is_const(key, site) {
            bail!("{prefix}`{key}` is a LET binding; use ${key}");
        }
    } else {
        if env.is_const(key, site) {
            bail!("{prefix}CONST reads bare: use {key}, not ${key}");
        }
        if !env.is_declared(key) {
            bail!("{prefix}undefined variable ${key}");
        }
    }
    env.read_guarded(key, site)
}

/// Single owner for static key-path traversal: every `KeyPath` and
/// `LoadKeyPath` (AST and RPN) walks here, so shape errors agree by
/// construction. `prefix` carries step attribution (`"at: "` in AST,
/// `""` in RPN); the empty-prefix RPN wording matches the historical
/// runtime errors. `Map`/`List`/`Any` and traversable unions defer to
/// `Any`, mirroring the evaluator's value-resolved misses.
fn traverse_static_keypath(mut current: TypeTag, keys: &[String], prefix: &str) -> Result<TypeTag> {
    for part in keys {
        match current {
            TypeTag::Record(fields) => {
                let Some(field) = fields.iter().find(|field| field.name == *part) else {
                    bail!(
                        "{prefix}unknown field '{part}'; expected fields: {}",
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
                    bail!("{prefix}Invalid array index '{part}'");
                }
                current = element.as_ref().clone();
            }
            TypeTag::MapOf(values) => {
                current = values.as_ref().clone();
            }
            TypeTag::Map | TypeTag::List | TypeTag::Any => {
                return Ok(TypeTag::Any);
            }
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
                    bail!("{prefix}Cannot traverse into scalar value at key '{part}'");
                }
                return Ok(TypeTag::Any);
            }
            _ => {
                bail!("{prefix}Cannot traverse into scalar value at key '{part}'");
            }
        }
    }
    Ok(current)
}

/// Step prefix, or empty for RPN contexts whose runtime errors carry
/// no step number.
fn loc(at: Option<&str>) -> String {
    at.map(|s| format!("{s}: ")).unwrap_or_default()
}

/// Resolved callee for one call: parameter shapes (absent when the
/// entry declares none), return type, and host closed-set metadata.
struct Callee {
    params: Option<Vec<(String, TypeTag, bool)>>,
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

/// Outbound imports of one step tree in a single walk: each visible
/// `AS` alias mapped to its defining file keys. `entry_parent`
/// enables entry-first key matching for the entry unit; file units
/// pass `None`. Single owner for the closure's per-unit import view;
/// dead steps register nowhere, `FUNC` bodies never leak into the
/// enclosing scope (they own per-function overlays), bare harvests
/// stay global. The target map's key set is the alias set: no
/// parallel `HashSet` is kept.
fn unit_imports(
    steps: &[Step],
    dir: &GuardedPath,
    files: &[ClosureFile],
    entry_parent: Option<&GuardedPath>,
    targets: &mut super::native::AliasTargets,
) {
    for step in steps {
        if matches!(classify_guard(step.guard.as_ref()), GuardClass::Dead) {
            continue;
        }
        // Function-local imports belong to the enclosing function's
        // overlay, never the unit: skipping `FUNC` bodies here keeps
        // sibling functions from cross-resolving each other's aliases.
        if matches!(step.kind, StepKind::FuncDef { .. }) {
            continue;
        }
        let (path, alias) = match &step.kind {
            StepKind::ImportFile { path, alias } => (path.clone(), Some(alias.clone())),
            StepKind::ImportFuncs {
                path,
                alias: Some(alias),
                ..
            } => (path.clone(), Some(alias.clone())),
            _ => (String::new(), None),
        };
        if let Some(alias) = alias {
            let unit = targets.entry(alias).or_default();
            let is_entry = entry_parent.is_some();
            // Host harvests have no closure file: record an explicit
            // host branch so disjoint file+host aliases never collapse
            // to `Canonical` (see `classify_alias_call`).
            let defining =
                super::import::resolve_static_top_level(entry_parent, dir, files, &path, is_entry);
            unit.push((defining, step.guard.clone()));
        }
        for body in super::import::child_bodies(&step.kind) {
            unit_imports(body, dir, files, entry_parent, targets);
        }
    }
}

/// Bare/`SCRIPT::` call names mentioned in one step's subtree:
/// statement calls, expression calls (including compiled-math calls
/// and `{{ }}` string templates), and template call heads. `FUNC`
/// bodies contribute only when `enter_funcdefs` (edge collection is
/// conservatively inclusive; the top-level scan excludes definitions,
/// whose bodies run at call time, not at their step). Single owner
/// for the entry call graph behind earliest-call seeding: a new call
/// position arrives here, not in a second walker.
fn step_call_names(kind: &StepKind, enter_funcdefs: bool, out: &mut Vec<String>) {
    if !enter_funcdefs && matches!(kind, StepKind::FuncDef { .. }) {
        return;
    }
    // Immediate expressions only, recursed manually below: nested
    // bodies arrive through the explicit `walk_child_kinds` recursion
    // and `Block` delegation, so the `FuncDef` exclusion holds at
    // every level (unlike `walk_step_exprs`, which descends into
    // nested definitions unconditionally and would leak uncalled
    // block-local callees into the top-level graph).
    kind.walk_exprs(&mut |expr| expr_call_names(expr, enter_funcdefs, out));
    // Immediate raw strings only (`walk_raw_strings` never descends),
    // so `If` / `For` / `While` / `WithIo` / `Isolated` bodies arrive
    // through the recursion below instead of being missed.
    kind.walk_raw_strings(&mut |text| template_call_names(text, out));
    kind.walk_child_kinds(&mut |sub| step_call_names(sub, enter_funcdefs, out));
}

/// Call names in one immediate expression. `Block` steps recurse
/// through [`step_call_names`] (not `Expr::walk`, which descends into
/// nested `FuncDef` bodies unconditionally), so excluded definitions
/// stay excluded inside inline blocks too. Mirrors `Expr::walk` arm
/// for arm; a new expression variant fails compilation here.
fn expr_call_names(expr: &Expr, enter_funcdefs: bool, out: &mut Vec<String>) {
    match expr {
        Expr::Call { name, args } => {
            push_entry_call_name(name, out);
            for arg in args {
                expr_call_names(arg, enter_funcdefs, out);
            }
        }
        Expr::CompiledMath(ops) => {
            for op in ops {
                match op {
                    MathOp::Call { name, .. } => push_entry_call_name(name, out),
                    MathOp::PushConst(value) => {
                        if let Some(text) = value.as_str() {
                            template_call_names(text, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        Expr::Literal(value) => {
            if let Some(text) = value.as_str() {
                template_call_names(text, out);
            }
        }
        Expr::Block(steps) => {
            for step in steps {
                step_call_names(&step.kind, enter_funcdefs, out);
            }
        }
        Expr::List(items) => {
            for item in items {
                expr_call_names(item, enter_funcdefs, out);
            }
        }
        Expr::Map(entries) => {
            for (_, value) in entries {
                expr_call_names(value, enter_funcdefs, out);
            }
        }
        Expr::Compare { left, right, .. }
        | Expr::Arithmetic { left, right, .. }
        | Expr::Logical { left, right, .. } => {
            expr_call_names(left, enter_funcdefs, out);
            expr_call_names(right, enter_funcdefs, out);
        }
        Expr::Not(inner) => expr_call_names(inner, enter_funcdefs, out),
        Expr::Var { .. }
        | Expr::ConstRef { .. }
        | Expr::Env(_)
        | Expr::KeyPath { .. }
        | Expr::Inspect { .. }
        | Expr::FreshPipe
        | Expr::UnsignedIntBoundary(_) => {}
    }
}

/// One call spelling snapped to entry-namespace names: bare heads and
/// `SCRIPT::` heads; file aliases and hosts belong to other units.
fn push_entry_call_name(name: &str, out: &mut Vec<String>) {
    if let Some((module, base)) = name.split_once("::") {
        if module == oxdock_parser::SCRIPT_MODULE_NAME {
            out.push(base.to_string());
        }
    } else {
        out.push(name.to_string());
    }
}

/// Bare/`SCRIPT::` template call heads in one text.
fn template_call_names(text: &str, out: &mut Vec<String>) {
    for segment in TemplateSegments::new(text) {
        if let TemplateSegment::Placeholder { key, .. } = segment
            && let PlaceholderKey::Call(call) = classify_placeholder_key(key)
        {
            match call.module {
                None => out.push(call.name.to_string()),
                Some(module) if module == oxdock_parser::SCRIPT_MODULE_NAME => {
                    out.push(call.name.to_string());
                }
                _ => {}
            }
        }
    }
}

/// Per-entry-function top-level `LET` view: bindings declared before
/// the function's earliest reachable call site. Entry scripts run top
/// level sequentially (unlike module files, whose whole body executes
/// before any call can land), so a call before a `LET` executes reads
/// uninitialized memory at runtime; the static gate must fail it
/// instead of seeding the full union. Uncalled functions keep the
/// full union: dead code observes nothing, so it stays lenient like
/// the single-file probes.
fn entry_let_scopes<P: ProcessManager>(
    entry: &[Step],
    all_funcs: &HashMap<String, FuncInfo>,
    exec: &ExecState<P>,
) -> Result<HashMap<(String, usize, usize), Vec<GuardedLet>>> {
    let indexed = module_lets_indexed(entry, exec)?;
    // Direct top-level mentions per bare name (definitions excluded:
    // their bodies run at call time, not at their step).
    let mut top_direct: HashMap<String, usize> = HashMap::new();
    for (idx, step) in entry.iter().enumerate() {
        if matches!(step.kind, StepKind::FuncDef { .. }) {
            continue;
        }
        let mut names = Vec::new();
        step_call_names(&step.kind, false, &mut names);
        for name in names {
            top_direct
                .entry(name)
                .and_modify(|at| *at = (*at).min(idx))
                .or_insert(idx);
        }
    }
    // Caller edges among entry functions (module-owned bodies belong
    // to other units and never read entry lets).
    let mut funcs: Vec<(String, usize, usize)> = Vec::new();
    let mut callees: HashMap<(String, usize, usize), Vec<String>> = HashMap::new();
    for info in all_funcs.values() {
        if info.module.is_some() {
            continue;
        }
        let key = (info.name.clone(), info.line, info.col);
        if callees.contains_key(&key) {
            continue;
        }
        let mut names = Vec::new();
        for step in &info.body {
            step_call_names(&step.kind, true, &mut names);
        }
        funcs.push(key.clone());
        callees.insert(key, names);
    }
    // Least fixpoint: a callee runs no later than its earliest caller.
    let mut earliest: HashMap<(String, usize, usize), usize> = HashMap::new();
    for key in &funcs {
        earliest.insert(
            key.clone(),
            top_direct.get(&key.0).copied().unwrap_or(usize::MAX),
        );
    }
    for _ in 0..funcs.len() {
        let mut changed = false;
        for caller in &funcs {
            let at = earliest[caller];
            if at == usize::MAX {
                continue;
            }
            for callee_name in callees[caller].clone() {
                for callee in funcs.iter().filter(|key| key.0 == callee_name) {
                    if earliest[callee] > at {
                        earliest.insert(callee.clone(), at);
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    // Seed each reachable function with the bindings declared before
    // its earliest call; the `LET $v = F()` step's own `$v` is
    // excluded (it binds after the call returns), matching execution.
    let mut out = HashMap::new();
    for key in &funcs {
        let Some(&at) = earliest.get(key) else {
            continue;
        };
        if at == usize::MAX {
            continue;
        }
        let scoped: Vec<GuardedLet> = indexed
            .iter()
            .filter(|(_, idx)| *idx < at)
            .map(|(binding, _)| binding.clone())
            .collect();
        out.insert(key.clone(), scoped);
    }
    Ok(out)
}
/// Per-function import overlay: the enclosing unit's top-level imports
/// plus the function's own direct imports. Nested function bodies get
/// their own overlays isolated to the unit top-level (never the
/// lexically enclosing function's locals), matching the runtime call
/// frames where each invocation pushes a fresh frame. Keyed by (owner
/// module key, function name, definition line, definition column) so
/// same-named block-local functions in distinct blocks never
/// overwrite each other's overlays. Single owner for function-local
/// alias visibility; `check_function_body_probed` swaps it in while
/// checking that body.
type BodyScopes = HashMap<(String, String, usize, usize), super::native::AliasTargets>;

/// Collect one overlay per `FUNC` definition under `steps`: each
/// overlay is the unit top-level plus that body's own direct imports.
/// `unit_top` is the unit's own table (never an enclosing function's
/// overlay), so nested definitions stay isolated exactly like the
/// runtime frames. Dead steps contribute nothing, like the unit walk.
#[allow(clippy::too_many_arguments)]
fn collect_body_scopes(
    steps: &[Step],
    module_key: &str,
    dir: &GuardedPath,
    files: &[ClosureFile],
    entry_parent: Option<&GuardedPath>,
    unit_top: &super::native::AliasTargets,
    out: &mut BodyScopes,
) {
    for step in steps {
        if matches!(classify_guard(step.guard.as_ref()), GuardClass::Dead) {
            continue;
        }
        if let StepKind::FuncDef { name, body, .. } = &step.kind {
            let mut scope = unit_top.clone();
            unit_imports(body, dir, files, entry_parent, &mut scope);
            out.insert(
                (module_key.to_string(), name.clone(), step.line, step.col),
                scope,
            );
            // Nested definitions isolate to the unit top-level too:
            // at runtime the nested body runs under its own fresh
            // frame, never the enclosing function's frame.
            collect_body_scopes(body, module_key, dir, files, entry_parent, unit_top, out);
        } else {
            for nested in super::import::child_bodies(&step.kind) {
                collect_body_scopes(nested, module_key, dir, files, entry_parent, unit_top, out);
            }
        }
    }
}

/// Merge one collected map into `all_funcs`, failing loud on same-name
/// different-owner collisions instead of last-wins overwrite. Only
/// global-namespace keys (bare, `SCRIPT::`) can collide now: aliased
/// file functions register under canonical defining-file keys, so
/// independent units reuse local aliases freely and the same file
/// re-imported under any alias merges identically. Same file
/// re-imported under the same alias re-registers identically and stays
/// silent; two global names claiming one key is a programmer error.
fn extend_funcs_checked(
    all_funcs: &mut HashMap<String, FuncInfo>,
    incoming: HashMap<String, FuncInfo>,
    display: &str,
) -> Result<()> {
    for (key, info) in incoming {
        if let Some(existing) = all_funcs.get(&key)
            && existing.module != info.module
        {
            bail!("duplicate function `{key}` from `{display}` collides across imports");
        }
        all_funcs.insert(key, info);
    }
    Ok(())
}

impl Env {
    fn one_frame(bindings: Vec<(String, TypeTag, bool)>) -> Self {
        let mut top = HashMap::new();
        for (name, tag, _) in bindings {
            top.insert(
                name,
                vec![Binding {
                    tag,
                    guard: None,
                    is_const: false,
                }],
            );
        }
        Self {
            frames: vec![top],
            module: None,
            env_frames: vec![HashSet::new()],
            func_frames: vec![HashMap::new()],
        }
    }

    /// Push one lexical frame across every stack (bindings, env
    /// names, nested functions): single owner mirroring runtime
    /// `push_scope`, so the three can never desync.
    fn push_frame(&mut self) {
        self.frames.push(HashMap::new());
        self.env_frames.push(HashSet::new());
        self.func_frames.push(HashMap::new());
    }

    /// Fork for a scoped body: clone the stack and push a fresh frame,
    /// mirroring `push_scope`. Discarding the fork is exact — arm and
    /// loop declarations never leak at runtime.
    fn fork(&self) -> Self {
        let mut fork = self.clone();
        fork.push_frame();
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
        // Guards apply exactly like lexical frames: only overlapping
        // entries are live, and guarded-only sets enforce coverage.
        if let Some(scope) = &self.module
            && let Some(entries) = scope.lets.get(key)
        {
            let live: Vec<&Binding> = entries
                .iter()
                .filter(|entry| oxdock_parser::guards_overlap(entry.guard.as_ref(), site))
                .collect();
            if !live.is_empty() {
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
                for entry in live {
                    unified = Some(match unified {
                        None => entry.tag.clone(),
                        Some(acc) => unify(
                            acc,
                            entry.tag.clone(),
                            &format!("${key} across platform guards"),
                        )?,
                    });
                }
                scope.touched.set(true);
                return Ok((unified.expect("live never empty"), None));
            }
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

    /// Whether `key` names a `CONST` binding visible here: the innermost
    /// frame holding the name decides (shadowing an outer `CONST`
    /// with an inner `LET` stays mutable); module-shared bindings
    /// consult their entries the same way. Only entries whose guard
    /// overlaps the active step's site count, mirroring
    /// [`declare_inner`](Self::declare_inner): a platform-conditional
    /// `LET` never reads as `CONST` through a disjoint `CONST` entry.
    /// Single owner for the static const gate behind `SET` and
    /// `LIST_APPEND` rejection.
    fn is_const(&self, key: &str, site: Option<&GuardExpr>) -> bool {
        for frame in self.frames.iter().rev() {
            if let Some(entries) = frame.get(key) {
                return entries.iter().any(|entry| {
                    entry.is_const && oxdock_parser::guards_overlap(entry.guard.as_ref(), site)
                });
            }
        }
        if let Some(scope) = &self.module
            && let Some(entries) = scope.lets.get(key)
        {
            return entries.iter().any(|entry| {
                entry.is_const && oxdock_parser::guards_overlap(entry.guard.as_ref(), site)
            });
        }
        false
    }

    /// Declare in the top frame. A duplicate whose guard overlaps any
    /// live entry mirrors the runtime redeclaration error; a disjoint
    /// one (e.g. `[os:windows]` after `[os:macos]`) shadows across exclusive
    /// paths and is allowed. `LET` and `CONST` share one namespace, so
    /// redeclaring across families fails here exactly like the runtime
    /// same-scope check; the message names the blocking family instead
    /// of suggesting a mutation that would fail.
    fn declare(
        &mut self,
        key: String,
        tag: TypeTag,
        guard: Option<&GuardExpr>,
        idx: usize,
    ) -> Result<()> {
        self.declare_inner(key, tag, guard, idx, false)
    }

    /// `CONST` twin of [`declare`](Self::declare): same overlap rule,
    /// marks the new entry immutable.
    fn declare_const(
        &mut self,
        key: String,
        tag: TypeTag,
        guard: Option<&GuardExpr>,
        idx: usize,
    ) -> Result<()> {
        self.declare_inner(key, tag, guard, idx, true)
    }

    fn declare_inner(
        &mut self,
        key: String,
        tag: TypeTag,
        guard: Option<&GuardExpr>,
        idx: usize,
        is_const: bool,
    ) -> Result<()> {
        if let Some(frame) = self.frames.last()
            && let Some(entries) = frame.get(&key)
        {
            for entry in entries {
                if oxdock_parser::guards_overlap(entry.guard.as_ref(), guard) {
                    if entry.is_const || is_const {
                        bail!(
                            "step {}: redeclaration error: {key} is a CONST binding and cannot be redeclared; CONST and LET share one namespace",
                            idx + 1
                        );
                    }
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
                is_const,
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
                    is_const: false,
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
        let mut pure_hosts = HashMap::new();
        for meta in exec.functions.pure_metas() {
            pure_hosts.insert(meta.name.clone(), meta);
        }
        Self {
            exec,
            funcs: HashMap::new(),
            module_lets: HashMap::new(),
            entry_lets: Vec::new(),
            module_env_unions: HashMap::new(),
            entry_env_union: HashSet::new(),
            seeded_env: oxdock_process::seeded_env_names()
                .into_iter()
                .map(|name| name.to_string())
                .collect(),
            hosts,
            pure_hosts,
            inferred: HashMap::new(),
            inferring: Vec::new(),
            visits: 0,
            strict_qualified: false,
            home_funcs: HashMap::new(),
            harvest_first: HashMap::new(),
            harvested: HashMap::new(),
            entry_let_scopes: HashMap::new(),
            alias_targets: HashMap::new(),
            module_const_exports: HashMap::new(),
            file_targets: HashMap::new(),
            module_func_exports: HashMap::new(),
            body_scopes: HashMap::new(),
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

    /// Scan raw string text for `{{ ... }}` reads: `env:` names through
    /// the declaration gate, `$var` and bare keys through expression
    /// inference. Runs on the shared [`TemplateSegments`]
    /// splitter, so what counts as a placeholder here is exactly what
    /// expands at runtime. Call shapes use the shared
    /// `template::parse_placeholder_call` grammar (also behind
    /// parse-time qualification and runtime dispatch), and call args
    /// use the shared `template::placeholder_vars` extraction, so var
    /// detection can never disagree with call detection on what
    /// counts as one call or one variable.
    fn check_template_env(
        &mut self,
        text: &str,
        env: &mut Env,
        scope: &Scope,
        depth: usize,
    ) -> Result<()> {
        use oxdock_parser::template::{
            PlaceholderKey, TemplateSegment, TemplateSegments, classify_placeholder_key,
            placeholder_vars,
        };
        for segment in TemplateSegments::new(text) {
            let TemplateSegment::Placeholder { key, .. } = segment else {
                continue;
            };
            match classify_placeholder_key(key) {
                // Empty keys never declare: runtime owns that error.
                PlaceholderKey::Empty => {}
                PlaceholderKey::Env(name) => {
                    let name = name.trim();
                    // Empty names bail like any undeclared read: `""`
                    // is never declared, so the shared gate reports it
                    // instead of silently skipping (runtime layers
                    // reject it too, never interpolating empty).
                    self.check_env_name(env, name, "env:", scope)?;
                }
                PlaceholderKey::ScriptEnv(name) => {
                    let name = name.trim();
                    self.check_env_name(env, name, "script_env:", scope)?;
                }
                // `$` and bare reads flow through the single var
                // extractor plus expression inference, so templates and
                // code agree on every error. Malformed `$` paths (empty
                // segments like `{{ $ }}`) fail here like they do in
                // template call arguments, instead of passing silently
                // when the extractor yields no variables. Call shapes
                // additionally resolve the callee: bare heads fail like
                // runtime (undefined script variable), qualified heads
                // resolve like expression calls (unknown `ALIAS::TYPO`
                // fails here, never at runtime).
                PlaceholderKey::DollarQualified { alias, name, keys } => {
                    return Err(anyhow::anyhow!(
                        oxdock_parser::template::dollar_qualified_error(alias, name, &keys)
                    ));
                }
                PlaceholderKey::Dollar(rest) => {
                    if oxdock_parser::template::parse_var_path(rest).is_none() {
                        bail!("invalid placeholder variable: empty variable reference");
                    }
                    for var in placeholder_vars(key) {
                        self.check_template_var(&var, env, scope, depth)?;
                    }
                }
                PlaceholderKey::ConstRef { .. }
                | PlaceholderKey::KeyPath { .. }
                | PlaceholderKey::Var(_)
                | PlaceholderKey::Bare(_) => {
                    for var in placeholder_vars(key) {
                        self.check_template_var(&var, env, scope, depth)?;
                    }
                }
                PlaceholderKey::Call(call) => {
                    for var in placeholder_vars(key) {
                        self.check_template_var(&var, env, scope, depth)?;
                    }
                    let Some(module) = call.module else {
                        bail!("undefined script variable: '{key}'");
                    };
                    self.check_template_call(module, call.name, call.args_raw, env, scope, depth)?;
                }
            }
        }
        Ok(())
    }

    /// One template variable read through expression inference, so
    /// templates and code agree on every error. `$a.b` traverses like
    /// `KeyPath`; bare keys read like `Const`; `ALIAS::NAME` reads like
    /// `ConstRef`. Key paths never ride qualified names: copy to a
    /// local binding first, mirroring the expression parser.
    fn check_template_var(
        &mut self,
        var: &oxdock_parser::template::TemplateVar,
        env: &mut Env,
        scope: &Scope,
        depth: usize,
    ) -> Result<()> {
        use oxdock_parser::template::TemplateVar;
        let expr = match var {
            TemplateVar::Path(parts) => {
                let (base, rest) = parts
                    .split_first()
                    .expect("placeholder_vars never emits empty paths");
                if rest.is_empty() {
                    Expr::Var {
                        name: base.clone(),
                        is_const: false,
                    }
                } else {
                    Expr::KeyPath {
                        base: base.clone(),
                        keys: rest.to_vec(),
                        is_const: false,
                    }
                }
            }
            TemplateVar::ConstRef { alias, name } => Expr::ConstRef {
                alias: alias.clone(),
                name: name.clone(),
            },
            TemplateVar::KeyPath { base, keys } => Expr::KeyPath {
                base: base.clone(),
                keys: keys.clone(),
                is_const: true,
            },
            TemplateVar::Var(name) => Expr::Var {
                name: name.clone(),
                is_const: true,
            },
            TemplateVar::Bare(name) => Expr::Var {
                name: name.clone(),
                is_const: true,
            },
        };
        let idx = scope.idx;
        let _ = self.infer_expr(&expr, env, scope, idx, depth + 1)?;
        Ok(())
    }

    /// Validate one `{{ MODULE::FUNC(args) }}` against the pure host
    /// table, the same registry runtime placeholder expansion reads.
    /// Script, file-aliased, and stateful (`Ctx`) callees never resolve
    /// in templates; each names the culprit plus the supported path
    /// (only genuinely unknown names keep the runtime `unknown
    /// placeholder function` diagnostic), and arity plus argument types
    /// enforce through `check_arg`.
    fn check_template_call(
        &mut self,
        module: &str,
        name: &str,
        args_raw: &str,
        env: &mut Env,
        scope: &Scope,
        depth: usize,
    ) -> Result<()> {
        let qualified = qualify(module, name);
        let Some(meta) = self.pure_hosts.get(&qualified).cloned() else {
            // Sibling-leakage gate through the shared alias-target
            // matcher, mirroring `resolve_call`. Anything reaching the
            // miss wording below is a script, stateful, or known-alias
            // callee that can never run in `{{ }}`.
            let mut is_script_visible = false;
            match super::native::classify_alias_call(&self.alias_targets, &qualified, &|key| {
                self.funcs.contains_key(key)
            }) {
                // Global spellings resolve as written; only `SCRIPT::`
                // names can hit a script entry this way. Visibility
                // walks the same frames as call resolution (#322):
                // nested defs, then the owning file (entry included),
                // then bare harvests.
                super::native::AliasResolution::Global => {
                    is_script_visible = self.funcs.contains_key(&qualified);
                    if module == oxdock_parser::SCRIPT_MODULE_NAME {
                        let unit =
                            super::import::unit_key(env.module.as_ref().map(|m| m.key.as_str()));
                        if self
                            .find_frame_hit(unit, name, &env.func_frames, scope.site.as_ref())
                            .is_some()
                        {
                            is_script_visible = true;
                        }
                    }
                }
                super::native::AliasResolution::Canonical(canonical) => {
                    is_script_visible = self.funcs.contains_key(&canonical);
                }
                super::native::AliasResolution::Ambiguous { files, .. } => {
                    bail!(
                        "{}",
                        super::native::ambiguous_alias_message(module, name, &files)
                    );
                }
                // Platform-conditional aliases: the active branch is
                // unknowable statically. The single miss helper decides
                // with `resolve_call` semantics (universal, host-aware),
                // so the gate renders word for word what the lazy
                // runtime path and the EXPAND snapshot report.
                super::native::AliasResolution::Any {
                    base,
                    files,
                    has_host,
                } => {
                    bail!(
                        "{}",
                        super::native::disjoint_alias_placeholder_miss(
                            module,
                            name,
                            &base,
                            &files,
                            has_host,
                            &|key| self.funcs.contains_key(key),
                        )
                    );
                }
                super::native::AliasResolution::Unknown => {}
            }
            if let Some(text) = super::native::describe_placeholder_miss(
                module,
                name,
                is_script_visible,
                self.hosts.contains_key(&qualified),
                self.alias_targets.contains_key(module),
            ) {
                bail!("{text}");
            }
            bail!("unknown placeholder function '{qualified}'");
        };
        let args_src = oxdock_parser::template::split_placeholder_args(args_raw);
        if let Some(host_params) = meta.params.as_deref() {
            let required = super::required_arity(host_params);
            let total = host_params.len();
            if args_src.len() < required || args_src.len() > total {
                if required == total {
                    bail!(
                        "{} in {}: {}() expects {} argument(s), got {}",
                        step_pos(scope.line, scope.col, scope.idx),
                        scope.label,
                        qualified,
                        total,
                        args_src.len()
                    );
                }
                bail!(
                    "{} in {}: {}() expects {} to {} argument(s), got {}",
                    step_pos(scope.line, scope.col, scope.idx),
                    scope.label,
                    qualified,
                    required,
                    total,
                    args_src.len()
                );
            }
            for (position, src) in args_src.iter().enumerate() {
                let trimmed = src.trim();
                let actual = self.infer_template_arg(trimmed, env, scope, depth, 0)?;
                if let Some(param) = host_params.get(position) {
                    if let Some(expected) = param.param_type.clone() {
                        let pat = format!(
                            "{} in {}: {}() argument `${}`",
                            step_pos(scope.line, scope.col, scope.idx),
                            scope.label,
                            qualified,
                            param.name
                        );
                        self.check_arg(&expected, actual, &pat)?;
                    }
                    if let Some(allowed) = param.allowed
                        && let Some(s) = self.template_arg_literal(trimmed)
                        && !allowed.contains(&s.as_str())
                    {
                        bail!(
                            "{} in {}: {}() argument `${}` must be one of: {}, got {s:?}",
                            step_pos(scope.line, scope.col, scope.idx),
                            scope.label,
                            qualified,
                            param.name,
                            allowed.join(", ")
                        );
                    }
                }
            }
            return Ok(());
        }
        for src in args_src.iter() {
            let _ = self.infer_template_arg(src.trim(), env, scope, depth, 0)?;
        }
        Ok(())
    }

    /// Infer one template call argument type through the shared
    /// argument grammar, mirroring runtime `resolve_call_value` shapes
    /// without needing values. Depth caps at the shared literal limit.
    fn infer_template_arg(
        &mut self,
        src: &str,
        env: &mut Env,
        scope: &Scope,
        depth: usize,
        literal_depth: usize,
    ) -> Result<StaticTy> {
        use oxdock_parser::template::{
            MAX_TEMPLATE_ARG_DEPTH, ScalarLit, TemplateArg, classify_template_arg,
            dollar_qualified_error, parse_string_literal, parse_var_path,
        };
        if literal_depth > MAX_TEMPLATE_ARG_DEPTH {
            bail!("placeholder call argument nests too deep (limit {MAX_TEMPLATE_ARG_DEPTH})");
        }
        match classify_template_arg(src) {
            TemplateArg::DollarQualified { alias, name, keys } => {
                Err(anyhow::anyhow!(dollar_qualified_error(alias, name, &keys)))
            }
            TemplateArg::Dollar(rest) => {
                let Some(parts) = parse_var_path(rest) else {
                    bail!("invalid placeholder call argument: empty variable reference");
                };
                let (base, segs) = parts.split_first().expect("split always yields one");
                let expr = if segs.is_empty() {
                    Expr::Var {
                        name: base.clone(),
                        is_const: false,
                    }
                } else {
                    Expr::KeyPath {
                        base: base.clone(),
                        keys: segs.to_vec(),
                        is_const: false,
                    }
                };
                self.infer_expr(&expr, env, scope, scope.idx, depth + 1)
            }
            TemplateArg::Str(literal_src) => {
                let literal = parse_string_literal(literal_src)
                    .expect("classify_template_arg only yields Str for string shapes");
                let _ = literal.map_err(|err| anyhow::anyhow!("{err:#}"))?;
                Ok(StaticTy::Ty(TypeTag::String))
            }
            TemplateArg::Map(inner) => {
                self.infer_template_map(inner, env, scope, depth, literal_depth)
            }
            TemplateArg::List(inner) => {
                self.infer_template_list(inner, env, scope, depth, literal_depth)
            }
            TemplateArg::Scalar(scalar_src) => {
                let Some(lit) = oxdock_parser::template::parse_scalar_literal(scalar_src) else {
                    bail!("invalid placeholder call argument '{scalar_src}'")
                };
                Ok(StaticTy::Ty(match lit {
                    ScalarLit::Bool(_) => TypeTag::Bool,
                    ScalarLit::Int(_) => TypeTag::Int,
                    ScalarLit::Float(_) => TypeTag::Float,
                }))
            }
            TemplateArg::ConstRef { alias, name } => {
                let expr = Expr::ConstRef {
                    alias: alias.to_string(),
                    name: name.to_string(),
                };
                self.infer_expr(&expr, env, scope, scope.idx, depth + 1)
            }
            TemplateArg::KeyPath { base, keys } => {
                let expr = Expr::KeyPath {
                    base: base.to_string(),
                    keys,
                    is_const: true,
                };
                self.infer_expr(&expr, env, scope, scope.idx, depth + 1)
            }
            TemplateArg::Var(name) => {
                let expr = Expr::Var {
                    name: name.to_string(),
                    is_const: true,
                };
                self.infer_expr(&expr, env, scope, scope.idx, depth + 1)
            }
            TemplateArg::Invalid(invalid) => {
                bail!("invalid placeholder call argument '{invalid}'")
            }
        }
    }

    /// Infer one `{key: value}` literal: keys validate through the
    /// shared key parser with duplicate rejection, values recurse.
    /// Homogeneous value types yield `MapOf`, heterogeneous or empty
    /// yield bare `Map` so `check_exact` enforces shaped parameters.
    fn infer_template_map(
        &mut self,
        inner: &str,
        env: &mut Env,
        scope: &Scope,
        depth: usize,
        literal_depth: usize,
    ) -> Result<StaticTy> {
        use oxdock_parser::template::{parse_map_key, split_map_entry, split_top_level};
        if inner.trim().is_empty() {
            return Ok(StaticTy::Ty(TypeTag::Map));
        }
        let mut seen = std::collections::HashSet::new();
        let mut value_tags: Vec<TypeTag> = Vec::new();
        for entry in split_top_level(inner, ',') {
            let Some((key_src, value_src)) = split_map_entry(&entry) else {
                bail!("invalid map literal entry '{entry}': use key: value");
            };
            let key = parse_map_key(&key_src)?;
            if !seen.insert(key.clone()) {
                bail!("duplicate key '{key}' in map literal");
            }
            let value_ty =
                self.infer_template_arg(value_src.trim(), env, scope, depth, literal_depth + 1)?;
            let StaticTy::Ty(tag) = value_ty else {
                bail!("VOID has no value to pass");
            };
            value_tags.push(tag);
        }
        let mut iter = value_tags.into_iter();
        let Some(first) = iter.next() else {
            return Ok(StaticTy::Ty(TypeTag::Map));
        };
        if iter.all(|tag| tags_equal(&tag, &first)) {
            return Ok(StaticTy::Ty(TypeTag::MapOf(std::sync::Arc::new(first))));
        }
        Ok(StaticTy::Ty(TypeTag::Map))
    }

    /// Infer one `[item]` literal: elements recurse, homogeneous element
    /// types yield `ListOf`, heterogeneous or empty yield bare `List`.
    fn infer_template_list(
        &mut self,
        inner: &str,
        env: &mut Env,
        scope: &Scope,
        depth: usize,
        literal_depth: usize,
    ) -> Result<StaticTy> {
        use oxdock_parser::template::split_top_level;
        if inner.trim().is_empty() {
            return Ok(StaticTy::Ty(TypeTag::List));
        }
        let mut element_tags: Vec<TypeTag> = Vec::new();
        for item in split_top_level(inner, ',') {
            let element_ty =
                self.infer_template_arg(item.trim(), env, scope, depth, literal_depth + 1)?;
            let StaticTy::Ty(tag) = element_ty else {
                bail!("VOID has no value to pass");
            };
            element_tags.push(tag);
        }
        let mut iter = element_tags.into_iter();
        let Some(first) = iter.next() else {
            return Ok(StaticTy::Ty(TypeTag::List));
        };
        if iter.all(|tag| tags_equal(&tag, &first)) {
            return Ok(StaticTy::Ty(TypeTag::ListOf(std::sync::Arc::new(first))));
        }
        Ok(StaticTy::Ty(TypeTag::List))
    }

    /// Best-effort literal string for `allowed` closed-set checks:
    /// quoted strings only, mirroring runtime literal resolution.
    fn template_arg_literal(&self, src: &str) -> Option<String> {
        use oxdock_parser::template::parse_string_literal;
        parse_string_literal(src)?.ok()
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
    /// `FuncInfo` construction is shared with [`Self::collect_home`]:
    /// one owner for resolved params, declared returns, and owner
    /// tagging, so home entries check exactly like collected ones.
    #[allow(clippy::too_many_arguments)]
    fn make_func_info(
        &self,
        name: &str,
        params: &[(String, String, bool)],
        return_type: &str,
        is_async: bool,
        body: &[Step],
        module: Option<&str>,
        line: usize,
        col: usize,
    ) -> Result<FuncInfo> {
        let resolved = resolve_script_params(self.exec, name, params)?;
        let declared = self
            .exec
            .resolve_tag(return_type)
            .map_err(|err| anyhow::anyhow!("function `{name}`: {err:#}"))?;
        self.exec
            .check_return_tag(&format!("function `{name}` returns"), &declared)
            .map_err(|err| anyhow::anyhow!("function `{name}`: {err:#}"))?;
        Ok(FuncInfo {
            params: resolved,
            declared,
            is_async,
            body: body.to_vec(),
            module: module.map(|m| m.to_string()),
            name: name.to_string(),
            line,
            col,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn define_collected(
        &mut self,
        name: &str,
        keys: Vec<String>,
        params: &[(String, String, bool)],
        return_type: &str,
        is_async: bool,
        body: &[Step],
        module: Option<&str>,
        line: usize,
        col: usize,
    ) -> Result<()> {
        let info =
            self.make_func_info(name, params, return_type, is_async, body, module, line, col)?;
        for key in keys {
            self.funcs.insert(key, info.clone());
        }
        let nested = body.to_vec();
        self.collect_owned(&nested, module)?;
        Ok(())
    }

    /// Index every top-level `FUNC` of one closure file for home-scope
    /// resolution (#319): exported, harvested, and private alike, keyed
    /// by (defining file, base name). Separate from `funcs` so
    /// `LIB::PRIVATE` stays unresolvable externally while home bodies
    /// still validate against the sibling's declared signature.
    /// Last definition wins, mirroring `define_collected` overwrite
    /// and runtime definition order; statically-dead steps never
    /// index, like collection.
    fn collect_home(&mut self, file: &super::import::ClosureFile) -> Result<()> {
        for step in &file.steps {
            if let StepKind::FuncDef {
                name,
                params,
                return_type,
                is_async,
                body,
                ..
            } = &step.kind
            {
                // Harvests bind the first textual definition
                // (mirroring `import_funcs`' guard-blind first match),
                // recorded before the dead-guard skip below so the
                // harvest arm resolves exactly what the runtime
                // registers. Dead defs still fail validation here
                // rather than silently validating: a dead harvest
                // target is a loud error, never a quiet wrong body.
                let key = (file.key.clone(), name.clone());
                if !self.harvest_first.contains_key(&key) {
                    let info = self.make_func_info(
                        name,
                        params,
                        return_type,
                        *is_async,
                        body,
                        Some(file.key.as_str()),
                        step.line,
                        step.col,
                    )?;
                    self.harvest_first.insert(key, info);
                }
            }
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
                let info = self.make_func_info(
                    name,
                    params,
                    return_type,
                    *is_async,
                    body,
                    Some(file.key.as_str()),
                    step.line,
                    step.col,
                )?;
                self.home_funcs
                    .entry((file.key.clone(), name.clone()))
                    .or_default()
                    .push((step.guard.clone(), info));
            }
        }
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
                    step.line,
                    step.col,
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
            StepKind::Switch {
                cases,
                default_body,
                ..
            } => {
                for (_, arm) in cases {
                    self.collect_owned(arm, module)?;
                }
                if let Some(arm) = default_body {
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
    /// initialized state for it to observe. The harvest gate itself
    /// (exported plus defined, both kinds) lives in
    /// [`super::import::gate_harvested_name`], shared with the runtime,
    /// so static and runtime fail identically. Shadowing follows the
    /// last-wins rule as [`collect_owned`](Self::collect_owned); the
    /// runtime registry remains the enforcer for true duplicates.
    /// Types persist bare globally for later `resolve_tag` calls, which
    /// read the global alias map.
    fn collect_site(&mut self, site: &ClosureSite, file: &ClosureFile) -> Result<()> {
        let module = file.key.as_str();
        let steps = file.steps.as_slice();
        let alias = site.alias.as_deref();
        let func_names = site.func_names.as_deref();
        let type_names = site.type_names.as_deref();
        let func_exports = &file.exports.funcs;
        let file_path = file.path.as_str();
        if !self.module_lets.contains_key(module) {
            let lets = module_lets(steps, self.exec)?;
            self.module_lets.insert(module.to_string(), lets);
        }
        if let Some(names) = func_names {
            // Harvest validity lives in `super::import::gate_harvested_name`
            // (single owner, shared with the runtime): export gate first,
            // then the defined check, so static and runtime fail
            // identically. This also fixes the old order here
            // (defined-first) to match every other site.
            for name in names {
                super::import::gate_harvested_name(
                    ExportKind::Func,
                    steps,
                    &file.exports,
                    name,
                    file_path,
                )?;
            }
        }
        if let Some(type_names) = type_names {
            for name in type_names {
                super::import::gate_harvested_name(
                    ExportKind::Type,
                    steps,
                    &file.exports,
                    name,
                    file_path,
                )?;
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
                    Some(_) => {
                        // Canonical file key, never the importer alias:
                        // independent units reuse local aliases without
                        // colliding; resolution maps each unit's alias to
                        // this key through its own targets.
                        self.define_collected(
                            name,
                            vec![super::native::file_func_key(module, name)],
                            params,
                            return_type,
                            *is_async,
                            body,
                            Some(module),
                            step.line,
                            step.col,
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
                            step.line,
                            step.col,
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
        // site with the fix attached. Keys are canonical (defining
        // file, never importer alias).
        if let Some(names) = func_names {
            for name in names {
                let key = match alias {
                    Some(_) => super::native::file_func_key(module, name),
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
            let mut lets: HashMap<String, Vec<Binding>> = HashMap::new();
            for entry in self.module_lets.get(module).cloned().unwrap_or_default() {
                lets.entry(entry.name).or_default().push(Binding {
                    tag: entry.tag,
                    guard: entry.guard,
                    is_const: entry.is_const,
                });
            }
            env.module = Some(ModuleScope {
                key: module.to_string(),
                lets,
                touched: std::rc::Rc::new(std::cell::Cell::new(false)),
            });
        }
        let scope = Scope {
            label,
            ret: RetCtx::Func(KEYWORD_IMPORT.to_string()),
            loops: 0,
            switches: 0,
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
            switches: 0,
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
            let touched = probe.unwrap_or_default();
            env.module = Some(ModuleScope {
                key: module.to_string(),
                lets: seed_map(
                    self.module_lets
                        .get(module)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                ),
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
        } else {
            // Entry functions observe params plus the file's top-level
            // `CONST`s declared before their earliest reachable call
            // (#318): immutable file state is visible by scope, while
            // mutable state crosses only as explicit arguments. Seeds
            // sit beneath the params frame, so param shadowing and the
            // const gate ride the existing frame walk with no special
            // cases. Uncalled bodies keep the full union (dead code
            // stays lenient). The runtime seeds the same view from the
            // caller bottom const frame; keep the two in lockstep.
            let scoped = self
                .entry_let_scopes
                .get(&(info.name.clone(), info.line, info.col));
            let source: &[GuardedLet] = match scoped {
                Some(scope) => scope,
                None => &self.entry_lets,
            };
            let seeds: Vec<GuardedLet> = source.iter().filter(|b| b.is_const).cloned().collect();
            if !seeds.is_empty() {
                env.frames.insert(0, seed_map(&seeds));
                env.env_frames.insert(0, HashSet::new());
                env.func_frames.insert(0, HashMap::new());
            }
            for name in &self.entry_env_union {
                env.declare_env(name.clone());
            }
        }
        let scope = Scope {
            label: key,
            ret: RetCtx::Func(key.to_string()),
            loops: 0,
            switches: 0,
            site: None,
            line: 0,
            col: 0,
            idx: 0,
        };
        // Function-local imports scope to this body (mirroring the
        // runtime dynamic extent): swap in the body's overlay,
        // restoring the unit sets afterward on every path.
        let unit = super::import::unit_key(info.module.as_deref());
        let overlay = self
            .body_scopes
            .get(&(unit.to_string(), info.name.clone(), info.line, info.col))
            .cloned();
        let saved = overlay.map(|targets| std::mem::replace(&mut self.alias_targets, targets));
        let out = self.walk_body(&info.body, &mut env, &scope, depth + 1);
        if let Some(targets) = saved {
            self.alias_targets = targets;
        }
        let out = out?;
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
        let base_func_frames = env.func_frames.len();
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
                switches: scope.switches,
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
                env.push_frame();
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
                if env.func_frames.len() > base_func_frames {
                    env.func_frames.pop();
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
    /// Shared `LET`/`CONST $var: TYPE = <expr>` check: infer, reject
    /// `VOID`, resolve and validate the tag, then declare (`CONST`
    /// via `declare_const`). Everything but the declaration is
    /// family-independent, so the two arms cannot drift.
    #[allow(clippy::too_many_arguments)]
    fn check_plain_rhs(
        &mut self,
        expr: &Expr,
        env: &mut Env,
        scope: &Scope,
        idx: usize,
        depth: usize,
        var: &str,
    ) -> Result<()> {
        let rhs = self.infer_expr(expr, env, scope, idx, depth + 1)?;
        if matches!(rhs, StaticTy::Void) {
            bail!(
                "step {}: VOID block has no value to bind to `${var}`",
                idx + 1
            );
        }
        Ok(())
    }

    /// Shared capture check (`LET`/`CONST $var: TYPE = <command>`):
    /// the captured command checks first, the yield must coerce to
    /// the pre-resolved tag. The caller owns tag resolution and the
    /// final declaration. `is_const` selects the diagnostic word.
    #[allow(clippy::too_many_arguments)]
    fn check_capture_rhs(
        &mut self,
        var: &str,
        tag: &TypeTag,
        cmd: &StepKind,
        env: &mut Env,
        scope: &Scope,
        step: &Step,
        idx: usize,
        depth: usize,
        is_const: bool,
    ) -> Result<()> {
        // The captured command runs first at runtime, so its
        // expressions check before the declaration lands.
        self.walk_wrapped(step, cmd, env, scope, idx, depth + 1)?;
        // Yield check mirroring runtime assign_capture (see
        // capture_yield): without it a TIMEOUT-wrapped (or any
        // non-call) command sails through with an incompatible
        // declaration and fails only at runtime coercion.
        if let Some(yield_tag) = self.capture_yield(
            cmd,
            env.module.as_ref().map(|m| m.key.as_str()),
            &env.func_frames,
            scope.site.as_ref(),
            depth,
        )? && self
            .check_arg(
                tag,
                StaticTy::Ty(yield_tag.clone()),
                &format!("step {}", idx + 1),
            )
            .is_err()
        {
            let kw = if is_const { KEYWORD_CONST } else { KEYWORD_LET };
            bail!(
                "step {}: {kw} ${} captures {} but declares {}",
                idx + 1,
                clean_var(var),
                render_structural(&yield_tag),
                render_structural(tag)
            );
        }
        Ok(())
    }

    /// Shared `AWAIT` capture check: the handle resolves before the
    /// output declares and the payload must fit the pre-resolved tag.
    /// The caller owns tag resolution and the final declaration.
    #[allow(clippy::too_many_arguments)]
    fn check_await_rhs(
        &mut self,
        out_var: &str,
        tag: &TypeTag,
        task_var: &str,
        task_is_const: bool,
        env: &mut Env,
        scope: &Scope,
        idx: usize,
    ) -> Result<()> {
        // The handle resolves before the output declares, through
        // the single `$` vs bare owner so bare `CONST` handles pass.
        let key = clean_var(task_var);
        let prefix = format!("step {}: ", idx + 1);
        let handle_tag =
            check_var_binding_access(env, &key, task_is_const, scope.site.as_ref(), &prefix)?;
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
        if !is_compatible(&payload, tag) && !is_compatible(tag, &payload) {
            // Top-down: capture type must accept the task payload.
            // Allow coercible pairs via check_arg shape.
            if self
                .check_arg(
                    tag,
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
                    render_structural(tag)
                );
            }
        }
        Ok(())
    }

    /// Shared `ASYNC` binding check: the body walks in a forked task
    /// scope and the pre-resolved tag must be `HANDLE`-headed and fit
    /// the task's returns. The caller owns tag resolution and the
    /// final declaration.
    #[allow(clippy::too_many_arguments)]
    fn check_async_rhs(
        &mut self,
        var: &str,
        tag: &TypeTag,
        body: &[Step],
        env: &mut Env,
        scope: &Scope,
        idx: usize,
        depth: usize,
    ) -> Result<()> {
        // Bodies publish through `AWAIT`; returns belong to the task,
        // never the enclosing function. Every ASYNC position is HANDLE-headed.
        let mut fork = env.fork();
        let task = Scope {
            label: scope.label,
            ret: RetCtx::AsyncValue,
            loops: 0,
            switches: 0,
            site: scope.site.clone(),
            line: scope.line,
            col: scope.col,
            idx: scope.idx,
        };
        let inner = self.walk_body(body, &mut fork, &task, depth + 1)?;
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
                render_structural(tag)
            ),
        };
        if inner.bare_return && !matches!(payload, TypeTag::Void) {
            bail!(
                "step {}: ASYNC `${}` declares {} but task has a bare RETURN; bare RETURN is only a void exit",
                idx + 1,
                clean_var(var),
                render_structural(tag)
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
                    render_structural(tag)
                );
            }
        } else {
            for (ty, line, _col) in &inner.types {
                if !is_compatible(ty, &payload) {
                    bail!(
                        "step {}: ASYNC `${}` declares {} but task RETURN at line {line} yields {}",
                        idx + 1,
                        clean_var(var),
                        render_structural(tag),
                        render_structural(ty)
                    );
                }
            }
        }
        Ok(())
    }

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
            StepKind::Break => self.walk_break(KEYWORD_BREAK, scope, idx),
            StepKind::Continue => self.walk_break(KEYWORD_CONTINUE, scope, idx),
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
            StepKind::Switch {
                discriminant,
                cases,
                default_body,
            } => {
                if let Some(disc) = discriminant {
                    self.infer_expr(disc, env, scope, idx, depth + 1)?;
                }
                let mut arms_terminal = true;
                let mut out = live;
                for (values, arm) in cases {
                    for value in values {
                        self.infer_expr(value, env, scope, idx, depth + 1)?;
                    }
                    let mut fork = env.fork();
                    let mut inner_scope = scope.clone();
                    inner_scope.switches += 1;
                    let arm_out = self.walk_body(arm, &mut fork, &inner_scope, depth + 1)?;
                    out.has_return |= arm_out.has_return;
                    out.bare_return |= arm_out.bare_return;
                    out.types.extend(arm_out.types);
                    arms_terminal &= !arm_out.falls_through;
                }
                match default_body {
                    Some(arm) => {
                        let mut fork = env.fork();
                        let mut inner_scope = scope.clone();
                        inner_scope.switches += 1;
                        let arm_out = self.walk_body(arm, &mut fork, &inner_scope, depth + 1)?;
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
                let resolved = resolve_script_params(self.exec, name, params)?;
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
                    name: name.clone(),
                    line: step.line,
                    col: step.col,
                };
                // Register before validating the body: later steps in
                // this walk resolve the nested def (mirroring runtime
                // definition order), including the body's own
                // self-calls. Block forks discard it on exit, exactly
                // like runtime scope pops.
                if let Some(frame) = env.func_frames.last_mut() {
                    frame.insert(name.clone(), info.clone());
                }
                self.check_function_body(name, &info, depth + 1)?;
                Ok(live)
            }
            StepKind::Call { name, args } => {
                self.check_call(name, args, env, scope, idx, depth + 1)?;
                Ok(live)
            }
            StepKind::Bind {
                var,
                decl_type,
                is_const,
                rhs,
            } => {
                // Single declaration owner: resolve the declared tag
                // once, validate the right-hand side per `BindRhs`
                // variant, then validate the position rule and declare
                // once. RHS validation runs before the position rule so
                // `ASYNC` reports HANDLE-headed before VOID position,
                // matching the historical per-form order. No per-form
                // restatement of the tag gate or the const branch.
                let tag = self
                    .exec
                    .resolve_tag(decl_type)
                    .map_err(|err| anyhow::anyhow!("step {}: {err:#}", idx + 1))?;
                match rhs {
                    oxdock_parser::BindRhs::Expr(expr) => {
                        self.check_plain_rhs(expr, env, scope, idx, depth, var)?;
                    }
                    oxdock_parser::BindRhs::Capture(cmd) => {
                        self.check_capture_rhs(
                            var, &tag, cmd, env, scope, step, idx, depth, *is_const,
                        )?;
                    }
                    oxdock_parser::BindRhs::Async(body) => {
                        self.check_async_rhs(var, &tag, body, env, scope, idx, depth)?;
                    }
                    oxdock_parser::BindRhs::Await {
                        task_var,
                        task_is_const,
                    } => {
                        self.check_await_rhs(var, &tag, task_var, *task_is_const, env, scope, idx)?;
                    }
                }
                self.exec
                    .check_declaration_tag(&format!("step {}", idx + 1), &tag)
                    .map_err(|err| anyhow::anyhow!("step {}: {err:#}", idx + 1))?;
                let key = clean_var(var);
                if *is_const {
                    env.declare_const(key, tag, step.guard.as_ref(), idx)?;
                } else {
                    env.declare(key, tag, step.guard.as_ref(), idx)?;
                }
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
                if env.is_const(&key, scope.site.as_ref()) {
                    bail!(
                        "step {}: cannot mutate CONST {key}: CONST bindings are deeply immutable",
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
            StepKind::AsyncBlock { body } => {
                // Bare tasks never publish a value: control never
                // crosses the thread boundary at runtime.
                let mut fork = env.fork();
                let task = Scope {
                    label: scope.label,
                    ret: RetCtx::AsyncStmt,
                    loops: 0,
                    switches: 0,
                    site: scope.site.clone(),
                    line: scope.line,
                    col: scope.col,
                    idx: scope.idx,
                };
                self.walk_body(body, &mut fork, &task, depth + 1)?;
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
            StepKind::Fail { body, .. } => {
                self.check_step_args(&step.kind, env, scope, idx, depth)?;
                let inner = self.walk_body(body, env, scope, depth + 1)?;
                let mut out = live;
                // FAIL never yields a value (it exits or propagates
                // control flow): unlike TIMEOUT, body types do not
                // flow through.
                out.has_return = inner.has_return;
                out.bare_return = inner.bare_return;
                out.falls_through = false;
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
                sealed.push_frame();
                let remote = Scope {
                    label: scope.label,
                    ret: RetCtx::Top,
                    loops: 0,
                    switches: 0,
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
                // when present — mirroring `read_line` exactly, which
                // rejects `CONST` through `mutate_var`.
                let key = clean_var(var);
                if env.is_const(&key, scope.site.as_ref()) {
                    bail!(
                        "step {}: cannot mutate CONST {key}: CONST bindings are deeply immutable",
                        idx + 1
                    );
                }
                if !env.is_declared(&key) {
                    env.insert_top(key, TypeTag::String, step.guard.as_ref());
                }
                Ok(live)
            }
            StepKind::Prompt { var, .. } => {
                // Same declare-or-mutate contract as `read_line`:
                // the answer binds as `STRING`, mutating when present.
                let key = clean_var(var);
                if env.is_const(&key, scope.site.as_ref()) {
                    bail!(
                        "step {}: cannot mutate CONST {key}: CONST bindings are deeply immutable",
                        idx + 1
                    );
                }
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
                    StepKind::Await { var, is_const } | StepKind::Cancel { var, is_const } => {
                        let key = clean_var(var);
                        let prefix = format!("step {}: ", idx + 1);
                        check_var_binding_access(
                            env,
                            &key,
                            *is_const,
                            scope.site.as_ref(),
                            &prefix,
                        )?;
                    }
                    StepKind::WithIoBlock { bindings } => {
                        Self::check_pipe_bindings(env, bindings, scope, idx)?;
                    }
                    StepKind::ListAppend { list, item } => {
                        let key = clean_var(list);
                        if env.is_const(&key, scope.site.as_ref()) {
                            bail!(
                                "step {}: cannot mutate CONST {key}: CONST bindings are deeply immutable",
                                idx + 1
                            );
                        }
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
        // Embedded `{{ ... }}` placeholders in literal text expand
        // at runtime through the same `expand_string` as whole-arg
        // expressions, so they read through the same declaration gate.
        // `Arg::String` and `ArgPart::Text` carry nothing else.
        let mut failed: Option<anyhow::Error> = None;
        kind.walk_raw_strings(&mut |text| {
            if failed.is_none()
                && let Err(err) = self.check_template_env(text, env, scope, depth)
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
        // `$` reads gate through the single owner so `$CONST` fails
        // here (deferred from the parser), not at runtime.
        let key = clean_var(name);
        let prefix = format!("step {}: ", idx + 1);
        check_var_binding_access(env, &key, false, scope.site.as_ref(), &prefix)
    }

    /// Static counterpart of `resolve_pipe_handle`: a pipe endpoint
    /// must be a live `PIPE` binding, mirroring the runtime
    /// `TypeMismatch` one for one. The `is_const` spelling rides in
    /// [`PipeTarget`], so `$CONST` fails through the single owner
    /// exactly like every other read.
    fn check_pipe_bindings(
        env: &Env,
        bindings: &[IoBinding],
        scope: &Scope,
        idx: usize,
    ) -> Result<()> {
        for binding in bindings {
            if let Some(PipeTarget::Var { name, is_const }) = &binding.pipe {
                let key = clean_var(name);
                let prefix = format!("step {}: ", idx + 1);
                let tag =
                    check_var_binding_access(env, &key, *is_const, scope.site.as_ref(), &prefix)?;
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

    /// `BREAK`/`CONTINUE`: legal only inside a loop body, except
    /// `BREAK` which is also legal directly in a `SWITCH` arm (the
    /// switch consumes it). Anywhere else the boundary error mirrors
    /// execution one for one.
    fn walk_break(&mut self, word: &str, scope: &Scope, idx: usize) -> Result<WalkOut> {
        let legal = if word == KEYWORD_BREAK {
            scope.loops > 0 || scope.switches > 0
        } else {
            scope.loops > 0
        };
        if legal {
            return Ok(WalkOut {
                falls_through: true,
                has_return: false,
                bare_return: false,
                types: Vec::new(),
            });
        }
        match &scope.ret {
            RetCtx::Top => {
                if word == KEYWORD_BREAK {
                    bail!("step {}: BREAK outside loop or SWITCH", idx + 1)
                } else {
                    bail!("step {}: {word} outside loop", idx + 1)
                }
            }
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
    fn capture_yield(
        &mut self,
        cmd: &StepKind,
        env_module: Option<&str>,
        nested: &[HashMap<String, FuncInfo>],
        site: Option<&GuardExpr>,
        depth: usize,
    ) -> Result<Option<TypeTag>> {
        let mut current = cmd;
        loop {
            match current {
                StepKind::WithIo { cmd, .. } => current = cmd,
                StepKind::Timeout { body, .. } if body.len() == 1 => {
                    current = &body[0].kind;
                }
                StepKind::Call { name, .. } => {
                    let callee = self.resolve_call(name, env_module, nested, site, depth)?;
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
                        // Optional or defaulted fields may be absent:
                        // binding fills defaults before verifying, so
                        // absence here only fails required fields.
                        None if field.optional || field.default.is_some() => {}
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

    /// #319 missing-import suffix for an unknown function: empty
    /// when no closure file exports the base name. Single owner for
    /// the static exporter view; every `resolve_call` unknown
    /// failure appends this instead of restating the search.
    fn missing_import_hint(&self, name: &str) -> String {
        super::native::missing_import_hint(
            self.module_func_exports
                .iter()
                .map(|(file, exports)| (file.as_str(), exports.as_slice())),
            &self.alias_targets,
            name,
        )
        .unwrap_or_default()
    }

    /// Frame walk shared by call resolution and the template gate
    /// (#322): precedence lives in [`FuncFrameView::walk`], shared
    /// with the runtime walk; this is the static store adapter.
    /// Returns the defining info plus the memoization key; hosts and
    /// canonical file views stay with their existing checks.
    fn find_frame_hit(
        &self,
        unit: &str,
        base: &str,
        nested: &[HashMap<String, FuncInfo>],
        site: Option<&GuardExpr>,
    ) -> Option<(FuncInfo, String)> {
        use super::native::FuncFrameView;
        struct StaticView<'a> {
            unit: &'a str,
            nested: &'a [HashMap<String, FuncInfo>],
            home: &'a HomeFuncs,
            harvested: &'a HashMap<(String, String), String>,
            first: &'a HashMap<(String, String), FuncInfo>,
            site: Option<&'a GuardExpr>,
        }
        impl FuncFrameView for StaticView<'_> {
            type Hit = (FuncInfo, String);
            fn find_nested(&self, base: &str) -> Option<Self::Hit> {
                for frame in self.nested.iter().rev() {
                    if let Some(info) = frame.get(base) {
                        let info = info.clone();
                        let key = format!("nested:{}:{base}:{}:{}", self.unit, info.line, info.col);
                        return Some((info, key));
                    }
                }
                None
            }
            fn find_home(&self, unit: &str, base: &str) -> Option<Self::Hit> {
                let overloads = self.home.get(&(unit.to_string(), base.to_string()))?;
                // Site-aware pick among disjoint guarded overloads,
                // mirroring runtime live-guard filtering; last live
                // wins to preserve the unguarded last-wins rule.
                let mut pick: Option<FuncInfo> = None;
                for (guard, info) in overloads {
                    if oxdock_parser::guards_overlap(guard.as_ref(), self.site) {
                        pick = Some(info.clone());
                    }
                }
                let info = pick?;
                let key = super::native::file_func_key(unit, base);
                Some((info, key))
            }
            fn find_harvest(&self, unit: &str, base: &str) -> Option<Self::Hit> {
                let defining = self.harvested.get(&(unit.to_string(), base.to_string()))?;
                let info = self
                    .first
                    .get(&(defining.clone(), base.to_string()))?
                    .clone();
                let key = super::native::file_func_key(defining, base);
                Some((info, key))
            }
        }
        StaticView {
            unit,
            nested,
            home: &self.home_funcs,
            harvested: &self.harvested,
            first: &self.harvest_first,
            site,
        }
        .walk(unit, base)
    }

    /// Resolve a callee through the shared alias-target matcher;
    /// script functions (bare or `SCRIPT::`) win over host metas.
    /// Unknown names yield `ANY`: the parse-time unknown-function gate
    /// owns that error, not this pass. `env_module` is the owning file
    /// of the body under check (from `env.module`): `SCRIPT::BASE` and
    /// bare `BASE` calls resolve through the frame walk (nested, then
    /// that file, then harvests), so canonically-keyed imports infer
    /// declared returns instead of `ANY`. `nested` carries the live
    /// body-local definitions from the walk environment.
    fn resolve_call(
        &mut self,
        name: &str,
        env_module: Option<&str>,
        nested: &[HashMap<String, FuncInfo>],
        site: Option<&GuardExpr>,
        depth: usize,
    ) -> Result<Callee> {
        // Canonical lookup key through the single alias-target
        // matcher: file-aliased spellings map to the defining file,
        // everything else resolves as spelled.
        let mut lookup = name.to_string();
        {
            match super::native::classify_alias_call(&self.alias_targets, name, &|key| {
                self.funcs.contains_key(key)
            }) {
                super::native::AliasResolution::Global => {}
                super::native::AliasResolution::Canonical(key) => lookup = key,
                super::native::AliasResolution::Unknown => {
                    bail!(
                        "unknown function `{name}`{}",
                        self.missing_import_hint(name)
                    );
                }
                super::native::AliasResolution::Ambiguous { alias, base, files } => {
                    bail!(
                        "{}",
                        super::native::ambiguous_alias_message(&alias, &base, &files)
                    );
                }
                // Disjoint platform-conditional targets: the active
                // branch is unknowable statically. Every file must
                // export the name (a missing branch fails the call on
                // its platform); identical signatures check precisely,
                // differing ones fall back to `ANY`.
                super::native::AliasResolution::Any {
                    base,
                    files,
                    has_host,
                } => {
                    // A host-harvest branch holds no file functions, so
                    // any file-backed call misses there.
                    if has_host {
                        bail!(
                            "unknown function `{name}`{}",
                            self.missing_import_hint(name)
                        );
                    }
                    // Every disjoint target must export the name: a
                    // missing branch fails the call on its platform.
                    let mut canonicals = Vec::new();
                    for file in files {
                        let canonical = super::native::file_func_key(file.as_str(), base.as_str());
                        let Some(info) = self.funcs.get(&canonical) else {
                            bail!(
                                "unknown function `{name}`{}",
                                self.missing_import_hint(name)
                            );
                        };
                        canonicals.push((canonical, info.clone()));
                    }
                    // Deterministic order: `files` is a `HashSet` with
                    // per-process randomized iteration.
                    canonicals.sort_by(|a, b| a.0.cmp(&b.0));
                    // Identical signatures check precisely against the
                    // shared shape; differing ones fall back to `ANY`
                    // since the active branch is unknowable. Calls are
                    // positional, so parameter names never participate.
                    let (_, first) = canonicals.first().expect("Any is never empty");
                    let unified = canonicals.iter().all(|(_, info)| {
                        info.params.len() == first.params.len()
                            && info.params.iter().zip(first.params.iter()).all(
                                |((_, param_tag, optional), (_, first_tag, first_optional))| {
                                    optional == first_optional && tags_equal(param_tag, first_tag)
                                },
                            )
                            && tags_equal(&info.declared, &first.declared)
                    });
                    if !unified {
                        return Ok(Callee {
                            params: None,
                            ret: StaticTy::Ty(TypeTag::Any),
                            host_params: None,
                        });
                    }
                    // Infer every branch and unify: differing inferred
                    // returns (including a bare `ANY` on any branch)
                    // widen to `ANY` instead of picking one `HashSet`
                    // slot at random.
                    let mut inferred = Vec::new();
                    for (key, info) in &canonicals {
                        inferred.push(self.func_return_owned(info, key, depth)?);
                    }
                    let (_, first) = canonicals.first().expect("Any is never empty");
                    let first_ret = inferred.first().expect("Any is never empty");
                    let returns_unified = inferred.iter().all(|ty| match (ty, first_ret) {
                        (StaticTy::Void, StaticTy::Void) => true,
                        (StaticTy::Ty(left), StaticTy::Ty(right)) => tags_equal(left, right),
                        _ => false,
                    });
                    let any_branch = inferred.iter().any(|ty| match ty {
                        StaticTy::Ty(tag) => tags_equal(tag, &TypeTag::Any),
                        StaticTy::Void => false,
                    });
                    if !returns_unified || any_branch {
                        return Ok(Callee {
                            params: None,
                            ret: StaticTy::Ty(TypeTag::Any),
                            host_params: None,
                        });
                    }
                    return Ok(Callee {
                        params: Some(first.params.clone()),
                        ret: first_ret.clone(),
                        host_params: None,
                    });
                }
            }
        }
        if lookup != name {
            // Canonical qualified spelling: the file's own index.
            if let Some(info) = self.funcs.get(&lookup) {
                let info = info.clone();
                let ret = self.func_return_owned(&info, &lookup, depth)?;
                let params = info.params;
                return Ok(Callee {
                    params: Some(params),
                    ret,
                    host_params: None,
                });
            }
        } else if let Some(base) = match name.split_once(MODULE_SEPARATOR) {
            Some((head, tail)) if head == oxdock_parser::SCRIPT_MODULE_NAME => Some(tail),
            Some(_) => None,
            None => Some(name),
        } {
            // Bare and `SCRIPT::` spellings walk the frame stack
            // (#322): innermost nested frame outward, then the
            // owning file frame (entry included), then bare
            // harvests. Entry bodies resolve their own file; module
            // bodies resolve theirs. No flat cross-file reads.
            let unit = super::import::unit_key(env_module);
            if let Some((info, key)) = self.find_frame_hit(unit, base, nested, site) {
                let ret = self.func_return_owned(&info, &key, depth)?;
                let params = info.params;
                return Ok(Callee {
                    params: Some(params),
                    ret,
                    host_params: None,
                });
            }
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
                            param.optional,
                        )
                    })
                    .collect()
            });
            // NOTE: host `allowed` sets are checked from the meta
            // below; the triple here carries shape plus the optional
            // flag for the arity range.
            return Ok(Callee {
                params,
                ret,
                host_params: meta.params.clone(),
            });
        }
        // Unimported qualified calls fail closed on the closure pass:
        // a known-alias miss already bailed above, so anything reaching
        // here with an `ALIAS::FUNC` shape the matcher reads as global
        // (a sibling unit's alias, a typo'd host) is unknown. Bare
        // names stay lenient: the parse-time unknown-function gate owns
        // those. `SCRIPT::` keeps its leniency for the same reason.
        // Single-file and probe checkers stay lenient: without the
        // closure they cannot tell unimported from uncollected.
        if self.strict_qualified
            && let Some((alias, _)) = name.split_once(MODULE_SEPARATOR)
            && alias != oxdock_parser::SCRIPT_MODULE_NAME
            && !self.hosts.contains_key(name)
        {
            bail!(
                "unknown function `{name}`{}",
                self.missing_import_hint(name)
            );
        }
        Ok(Callee {
            params: None,
            ret: StaticTy::Ty(TypeTag::Any),
            host_params: None,
        })
    }

    /// Resolve one `$ALIAS::NAME` read to its `CONST` tag: the static
    /// twin of [`import::resolve_const_ref`](super::import::resolve_const_ref).
    /// Alias matching runs through the shared classifier; only
    /// `EXPORT`ed `CONST` bindings resolve. Unknown aliases stay
    /// lenient without the closure (single-file probes) and fail
    /// closed on it; a `LET` spelling with the same name names the
    /// real error (never exportable) instead of an unknown-name miss.
    fn resolve_const_ref(&self, alias: &str, name: &str, scope: &Scope) -> Result<TypeTag> {
        use super::native::{
            ConstHolders, ambiguous_alias_message, chase_const_forward, const_holder_files,
            missing_const_hint,
        };
        let at = || step_pos(scope.line, scope.col, scope.idx);
        let qualified = oxdock_parser::qualify(alias, name);
        let view = CheckerConstView {
            exports: &self.module_const_exports,
            targets: &self.file_targets,
            lets: &self.module_lets,
        };
        // Holders through the shared walk: the static twin and the
        // runtime twin classify identically, so a read accepted here
        // serves there.
        let files = match const_holder_files(&view, &self.alias_targets, alias, name) {
            ConstHolders::Found(files) => files,
            ConstHolders::NoAlias => {
                if self.strict_qualified {
                    bail!(
                        "{}: unknown CONST `{qualified}`: no import alias `{alias}` exports that name",
                        at()
                    );
                }
                return Ok(TypeTag::Any);
            }
            ConstHolders::Ambiguous { alias, base, files } => {
                bail!(
                    "{}: {}",
                    at(),
                    ambiguous_alias_message(&alias, &base, &files).replacen("function", "CONST", 1)
                );
            }
        };
        // Tag lookup over exporting files, following qualified
        // forwards to the defining files first so the tag and the LET
        // check read terminal bindings, never forwards. Disjoint
        // terminals union like disjoint holders: same tags read, and
        // differing tags fall back to `ANY` since only the live branch
        // initializes.
        let mut tags: Vec<TypeTag> = Vec::new();
        for file in &files {
            let terminals = chase_const_forward(&view, file, name, &qualified)
                .map_err(|err| anyhow::anyhow!("{}: {err}", at()))?;
            for terminal in &terminals {
                let Some(binding) = self
                    .module_lets
                    .get(terminal)
                    .and_then(|lets| lets.iter().find(|entry| entry.name == name))
                else {
                    continue;
                };
                if !binding.is_const {
                    bail!(
                        "{}: `{qualified}` is a LET binding and cannot be exported: only CONST bindings cross the module boundary",
                        at()
                    );
                }
                tags.push(binding.tag.clone());
            }
        }
        if tags.len() > 1 {
            let first = tags[0].clone();
            if !tags.iter().all(|tag| tags_equal(&first, tag)) {
                return Ok(TypeTag::Any);
            }
        }
        match tags.into_iter().next() {
            Some(tag) => Ok(tag),
            None => {
                let hint = missing_const_hint(&view, &self.alias_targets, alias, name);
                match hint {
                    Some(hint) => bail!("{}: unknown CONST `{qualified}`: {hint}", at()),
                    None => bail!(
                        "{}: unknown CONST `{qualified}`: no import alias `{alias}` exports that name",
                        at()
                    ),
                }
            }
        }
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
        let callee = self.resolve_call(
            name,
            env.module.as_ref().map(|m| m.key.as_str()),
            &env.func_frames,
            scope.site.as_ref(),
            depth,
        )?;
        // Arity range comes from the full host metadata (which carries
        // the optional flags); script FUNCs and bare shape lists are
        // always exact. Missing lists (INSPECT-like) skip the check:
        // the dedicated AST path owns that call shape.
        let (required, total) = match callee.host_params.as_deref() {
            Some(host) => (required_arity(host), host.len()),
            None => match callee.params.as_deref() {
                Some(shape) => (Self::script_required(shape), shape.len()),
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
                Some(shape) => (Self::script_required(shape), shape.len()),
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
            let (pname, ptype, _) = expected;
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
                // String literals expand `{{ ... }}` at runtime
                // through `expand_string`, so embedded reads gate here
                // exactly like whole-arg expressions.
                if let Some(text) = value.as_str() {
                    self.check_template_env(text, env, scope, depth)?;
                }
                Ok(StaticTy::Ty(literal_tag(value)))
            }
            Expr::Var { name, is_const } => {
                let key = clean_var(name);
                let prefix = loc(Some(&at()));
                Ok(StaticTy::Ty(check_var_binding_access(
                    env,
                    &key,
                    *is_const,
                    scope.site.as_ref(),
                    &prefix,
                )?))
            }
            Expr::ConstRef { alias, name } => {
                let tag = self.resolve_const_ref(alias, name, scope)?;
                Ok(StaticTy::Ty(tag))
            }
            Expr::Env(name) => {
                self.check_env_name(env, name, "env:", scope)?;
                Ok(StaticTy::Ty(TypeTag::String))
            }
            Expr::KeyPath {
                base,
                keys,
                is_const,
            } => {
                let key = clean_var(base);
                let prefix = loc(Some(&at()));
                let current =
                    check_var_binding_access(env, &key, *is_const, scope.site.as_ref(), &prefix)?;
                let traversed = traverse_static_keypath(current, keys, &format!("{}: ", at()))?;
                Ok(StaticTy::Ty(traversed))
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
                        switches: 0,
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
            Expr::Inspect { name, is_const } => {
                let key = clean_var(name);
                let prefix = loc(Some(&at()));
                check_var_binding_access(env, &key, *is_const, scope.site.as_ref(), &prefix)?;
                Ok(StaticTy::Ty(TypeTag::Any))
            }
            Expr::Compare { op, left, right } => {
                let left_ty = self.infer_expr(left, env, scope, idx, depth + 1)?;
                let right_ty = self.infer_expr(right, env, scope, idx, depth + 1)?;
                match op {
                    // Equality is strict but always BOOL.
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

    /// Required script params: total minus trailing `?` (the parser
    /// enforces trailing-only, so a suffix count is exact). Mirrors
    /// [`required_arity`] for host metadata.
    fn script_required(params: &[(String, TypeTag, bool)]) -> usize {
        params.len() - params.iter().rev().take_while(|entry| entry.2).count()
    }

    /// Ordering (`<`, `<=`, `>`, `>=`) operand rule: numerics pass,
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
                        self.check_template_env(text, env, scope, depth)?;
                    }
                    stack.push(StaticTy::Ty(literal_tag(value)))
                }
                MathOp::Load { name, is_const } => {
                    let key = clean_var(name);
                    let tag =
                        check_var_binding_access(env, &key, *is_const, scope.site.as_ref(), "")?;
                    stack.push(StaticTy::Ty(tag));
                }
                MathOp::LoadConstRef { alias, name } => {
                    let tag = self.resolve_const_ref(alias, name, scope)?;
                    stack.push(StaticTy::Ty(tag));
                }
                MathOp::LoadEnv(name) => {
                    self.check_env_name(env, name, "env:", scope)?;
                    stack.push(StaticTy::Ty(TypeTag::String))
                }
                MathOp::LoadKeyPath {
                    base,
                    keys,
                    is_const,
                } => {
                    let key = clean_var(base);
                    let current =
                        check_var_binding_access(env, &key, *is_const, scope.site.as_ref(), "")?;
                    let traversed = traverse_static_keypath(current, keys, "")?;
                    stack.push(StaticTy::Ty(traversed));
                }
                MathOp::Call { name, arity } => {
                    let before = stack.len();
                    if before < *arity {
                        bail!("internal error: RPN stack underflow");
                    }
                    // `split_off` keeps left-to-right order already,
                    // matching the runtime's pop-then-reverse: no
                    // second reversal here, or multi-arg calls check
                    // positionally swapped.
                    let taken: Vec<StaticTy> = stack.split_off(before - arity);
                    // RPN operands carry no literal forms, so closed
                    // `#[values]` sets cannot be checked here; shape and
                    // arity still are. (Math-position calls are
                    // numeric builtins in practice.)
                    let blanks = vec![None; taken.len()];
                    let callee = self.resolve_call(
                        name,
                        env.module.as_ref().map(|m| m.key.as_str()),
                        &env.func_frames,
                        scope.site.as_ref(),
                        depth,
                    )?;
                    let ret = self.apply_checked(name, &callee, &taken, &blanks, None)?;
                    stack.push(ret);
                }
                MathOp::Inspect { name, is_const } => {
                    let key = clean_var(name);
                    check_var_binding_access(env, &key, *is_const, scope.site.as_ref(), "")?;
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
                    default: None,

                    alias: None,
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
                        default: None,

                        alias: None,
                    },
                    oxdock_parser::Field {
                        name: "opt".to_string(),
                        ty: TypeTag::Int,
                        docs: "",
                        optional: true,
                        default: None,

                        alias: None,
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
