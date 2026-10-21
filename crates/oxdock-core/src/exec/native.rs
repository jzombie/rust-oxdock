use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use oxdock_func_macro::oxdock_func;
use oxdock_parser::{
    Field, KEYWORD_INSPECT, SCRIPT_MODULE_NAME, STD_MODULE_NAME, SemaphoreState, Step, TypeTag,
    Value, base_name, qualify, render_structural, split_qualified,
};
use oxdock_pipe::PipeHandle;
use oxdock_process::{CommandStdin, DefaultProcessManager, ProcessManager};

use super::io::StreamHandle;
use super::state::ExecState;
use super::steps::StepCtx;
use super::typing::TypeDescriptor;

/// Origin of a callable in the unified function registry. `Script` is an
/// interpreted `FUNC` body; `HostCtx` and `HostPure` are compiled Rust
/// functions (the `#[oxdock_func]` host export macro). Builtins and
/// runtime-registered hosts share the host kinds; only `DESCRIBE` output
/// shows the label, and both host kinds keep rendering as `host`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuncKind {
    Script,
    HostCtx,
    HostPure,
}

impl FuncKind {
    pub fn label(&self) -> &'static str {
        match self {
            FuncKind::Script => "script",
            FuncKind::HostCtx | FuncKind::HostPure => "host",
        }
    }
}

/// One declared parameter of a registered function. `param_type` is `None`
/// for unconstrained `Value` parameters (the macro accepts any value).
/// `allowed` names the closed value set for constrained `STRING`
/// parameters, declared once via `#[values(...)]`: the generated
/// extractor enforces membership and the reference renders the set,
/// so the list cannot rot apart from the check. `docs` carries the
/// parameter's description (parsed from the `# Arguments` doc section
/// for macro functions, written inline for hand-built entries) and
/// renders into every reference surface. `options` declares the known
/// keys of a `MAP` options parameter: same single-source rule as
/// `allowed`, with per-key types, required flags, defaults, and docs.
/// `optional` marks a trailing `MAP` parameter callers may omit: the
/// runtime fills an empty MAP, and signatures render `= {}`. Only
/// trailing `MAP` params may be optional (run-start validation
/// rejects anything else), so omission always means "no options".
/// The macro never emits optional params: only hand-built entries
/// use them, and only for options MAPs.
#[derive(Debug, Clone)]
pub struct FuncParam {
    pub name: String,
    pub param_type: Option<TypeTag>,
    pub allowed: Option<&'static [&'static str]>,
    pub docs: &'static str,
    pub options: Option<&'static [ParamOption]>,
    pub optional: bool,
}

/// Required argument count: total params minus trailing optionals.
/// Callers may pass any count in `required..=total`; omitted trailing
/// MAPs fill with an empty MAP. Shared by the static arity gate and
/// the runtime gate so the two can never disagree.
pub fn required_arity(params: &[FuncParam]) -> usize {
    params
        .iter()
        .rev()
        .take_while(|param| param.optional)
        .fold(params.len(), |count, _| count - 1)
}

/// Fill omitted trailing optional MAP params with empty MAPs. The
/// caller guarantees `values.len()` is within
/// `required_arity(params)..=params.len()`; anything else is a bug,
/// not a user error, so over- and under-counts panic loudly.
pub fn fill_optional_args(params: &[FuncParam], mut values: Vec<Value>) -> Vec<Value> {
    assert!(
        values.len() <= params.len(),
        "arity gate must run before fill_optional_args"
    );
    assert!(
        values.len() >= required_arity(params),
        "arity gate must run before fill_optional_args"
    );
    while values.len() < params.len() {
        values.push(Value::map(BTreeMap::new()));
    }
    values
}

/// One known key of a `MAP` options parameter: name, value type,
/// whether callers must pass it, the rendered default for optional
/// keys, and what it controls. Declared once per options parameter
/// (macro `#[options(...)]` plus the `# Options` doc section, or
/// inline for hand-built entries) and rendered into signatures,
/// `DESCRIBE` output, and generated references alike.
#[derive(Debug, Clone)]
pub struct ParamOption {
    pub name: &'static str,
    pub value: TypeTag,
    pub required: bool,
    pub default: Option<&'static str>,
}

/// Introspectable metadata for one function. Single source for
/// `DESCRIBE(name)` output and the static function reference. `rpn` names
/// whether the function also runs on the compiled math path (`true` for
/// pure functions and opted-in stateful ones); everything runs on the AST
/// path.
#[derive(Debug, Clone)]
pub struct FuncMeta {
    pub name: String,
    /// Owning module (`STD` for builtins, `SCRIPT` for DSL definitions,
    /// the host module name otherwise). `name` is always the qualified
    /// `MODULE::BASE` form, so listings and `DESCRIBE` never lose origin.
    pub module: String,
    pub kind: FuncKind,
    pub params: Option<Vec<FuncParam>>,
    pub returns: Option<TypeTag>,
    pub rpn: bool,
    pub summary: &'static str,
    pub docs: &'static str,
    /// Executable examples, rendered as ```oxdock fences into the
    /// generated reference and executed by docs_conformance like
    /// command examples. The single precondition mechanism: the
    /// returns gate executes these blocks to cover functions blind
    /// sampling cannot reach.
    pub examples: &'static [crate::Example],
}

/// Pure scalar function: no filesystem, no scope, no process access.
/// Usable from both AST evaluation and compiled RPN math.
pub type PureFn = Arc<dyn Fn(Vec<Value>) -> Result<Value> + Send + Sync>;

/// Stateful/IO function with full step context (fs, cwd, envs, vars, pipes).
pub type NativeFn<P> = Arc<dyn Fn(&mut StepCtx<P>, Vec<Value>) -> Result<Value> + Send + Sync>;

/// Export hook for a DSL function, implemented by `#[oxdock_func]` on a
/// registration marker. The engine calls `registration()` and never names
/// a generated symbol.
pub trait OxDockFn<P: ProcessManager> {
    /// The registry entry deriving from the Rust signature plus doc
    /// comments: name, metadata, and entry point, composed.
    fn registration() -> HostRegistration<P>;
}

/// One host-registered function, grouped into a [`HostModule`] and passed
/// to `Engine::register_module`. Build the entry with the `#[oxdock_func]`-
/// generated registration marker, or by hand. `Pure` entries run
/// on both the AST and the compiled RPN math paths; `Stateful` entries run
/// on the AST path with full step context. `Both` entries carry a stateful
/// body plus a pure string projection: every context with step IO runs
/// the body, while `{{ }}` placeholder expansion (which has no step
/// context) runs the projection.
pub enum HostRegistration<P: ProcessManager> {
    Stateful {
        name: String,
        meta: FuncMeta,
        func: NativeFn<P>,
    },
    Pure {
        name: String,
        meta: FuncMeta,
        func: PureFn,
    },
    Both {
        name: String,
        meta: FuncMeta,
        func: NativeFn<P>,
        template: PureFn,
    },
}

// Manual `Clone` (a derive would demand `P: Clone`): entries share their
// function pointers through the `Arc`s and deep-copy the metadata.
impl<P: ProcessManager> Clone for HostRegistration<P> {
    fn clone(&self) -> Self {
        match self {
            HostRegistration::Stateful { name, meta, func } => HostRegistration::Stateful {
                name: name.clone(),
                meta: meta.clone(),
                func: Arc::clone(func),
            },
            HostRegistration::Pure { name, meta, func } => HostRegistration::Pure {
                name: name.clone(),
                meta: meta.clone(),
                func: Arc::clone(func),
            },
            HostRegistration::Both {
                name,
                meta,
                func,
                template,
            } => HostRegistration::Both {
                name: name.clone(),
                meta: meta.clone(),
                func: Arc::clone(func),
                template: Arc::clone(template),
            },
        }
    }
}

impl<P: ProcessManager> HostRegistration<P> {
    /// Introspectable metadata for this entry, for documentation tooling
    /// that reads modules without registering them.
    pub fn meta(&self) -> &FuncMeta {
        match self {
            HostRegistration::Stateful { meta, .. } => meta,
            HostRegistration::Pure { meta, .. } => meta,
            HostRegistration::Both { meta, .. } => meta,
        }
    }
}

/// One user-defined function body (`FUNC NAME($p: TYPE, ...) { ... }`).
/// Param types resolve to tags at definition time, where the type
/// directory is in scope; the registry never holds raw type strings.
#[derive(Debug, Clone)]
pub(super) struct FuncDefData {
    pub(super) params: Vec<(String, TypeTag)>,
    pub(super) body: Vec<Step>,
}

/// Executable body behind one registry entry: an interpreted script, a
/// pure scalar function, a stateful function with step context, or a
/// dual entry whose pure arm serves `{{ }}` placeholders while the ctx
/// arm serves every context that carries step IO.
pub(super) enum FuncBody<P: ProcessManager> {
    Script(FuncDefData),
    Pure(PureFn),
    Ctx(NativeFn<P>),
    Both { pure: PureFn, ctx: NativeFn<P> },
}

// Manual `Clone` (a derive would demand `P: Clone`).
impl<P: ProcessManager> Clone for FuncBody<P> {
    fn clone(&self) -> Self {
        match self {
            FuncBody::Script(def) => FuncBody::Script(def.clone()),
            FuncBody::Pure(func) => FuncBody::Pure(Arc::clone(func)),
            FuncBody::Ctx(func) => FuncBody::Ctx(Arc::clone(func)),
            FuncBody::Both { pure, ctx } => FuncBody::Both {
                pure: Arc::clone(pure),
                ctx: Arc::clone(ctx),
            },
        }
    }
}

/// One entry in the unified function registry: introspectable metadata
/// (mandatory for every entry, backing the pre-evaluation arity gate)
/// plus the executable body.
pub(super) struct FuncEntry<P: ProcessManager> {
    pub(super) meta: FuncMeta,
    pub(super) body: FuncBody<P>,
}

// Manual `Clone` (a derive would demand `P: Clone`).
impl<P: ProcessManager> Clone for FuncEntry<P> {
    fn clone(&self) -> Self {
        Self {
            meta: self.meta.clone(),
            body: self.body.clone(),
        }
    }
}

/// Names defined in one lexical scope frame: `defined` rejects duplicates
/// in the same scope, `shadowed` restores outer definitions on exit.
struct ScopeFrame<P: ProcessManager> {
    defined: HashSet<String>,
    shadowed: Vec<(String, FuncEntry<P>)>,
}

// Manual `Clone` (a derive would demand `P: Clone`).
impl<P: ProcessManager> Clone for ScopeFrame<P> {
    fn clone(&self) -> Self {
        Self {
            defined: self.defined.clone(),
            shadowed: self.shadowed.clone(),
        }
    }
}

/// The single function registry: DSL `FUNC` definitions, builtins, and
/// host extensions share one lookup table, one metadata path, and one
/// dispatch order. Script entries scope lexically (defined names revert on
/// scope exit, shadowing an outer definition restores it); native entries
/// persist for the run. Shared across `fork()` via clone; the entry maps
/// clone while scope frames stay per state.
/// Module to function-name table of pure entries, shared by reference.
/// Nested so placeholder dispatch looks up `table[module][func]` with
/// zero allocation; rebuilt only when native entries register.
pub type PureTable = HashMap<String, HashMap<String, PureFn>>;

pub struct FunctionRegistry<P: ProcessManager> {
    entries: HashMap<String, FuncEntry<P>>,
    scopes: Vec<ScopeFrame<P>>,
    pure_shared: Arc<PureTable>,
    /// Script `#` doc comments keyed by qualified `SCRIPT::NAME`.
    /// Populated by the MCP host after linear execution; the tool
    /// table reads these with fallback to `FuncMeta` summary.
    script_docs: HashMap<String, (String, String)>,
}

impl<P: ProcessManager> FunctionRegistry<P> {
    pub(super) fn with_builtins() -> Self {
        let mut reg = Self {
            entries: HashMap::new(),
            pure_shared: Arc::new(HashMap::new()),
            script_docs: HashMap::new(),
            scopes: vec![ScopeFrame {
                defined: HashSet::new(),
                shadowed: Vec::new(),
            }],
        };
        // Every builtin registers through the same `HostRegistration`
        // entries hosts use: no separate authoring path for engine natives.
        // Builtins land in the `STD` module, exactly like a host module.
        for host in Self::builtin_registrations() {
            match host {
                HostRegistration::Stateful { name, meta, func } => {
                    reg.insert_qualified(STD_MODULE_NAME, name, meta, FuncBody::Ctx(func));
                }
                HostRegistration::Pure { name, meta, func } => {
                    reg.insert_qualified(STD_MODULE_NAME, name, meta, FuncBody::Pure(func));
                }
                HostRegistration::Both {
                    name,
                    meta,
                    func,
                    template,
                } => {
                    reg.insert_qualified(
                        STD_MODULE_NAME,
                        name,
                        meta,
                        FuncBody::Both {
                            pure: template,
                            ctx: func,
                        },
                    );
                }
            }
        }
        reg
    }

    /// All builtins as host-style registrations, built from the same
    /// `#[oxdock_func]`-generated markers hosts use. `with_builtins`
    /// consumes this list, so builtins and hosts share one registration
    /// pathway instead of two authoring models.
    pub(super) fn builtin_registrations() -> Vec<HostRegistration<P>> {
        vec![
            Int::registration(),
            Float::registration(),
            FormatByteCount::registration(),
            ParseByteCount::registration(),
            FormatDuration::registration(),
            ParseDuration::registration(),
            Types::registration(),
            TypeDescribe::registration(),
            Glob::registration(),
            LoadToml::registration(),
            LoadJson::registration(),
            ParseToml::registration(),
            ParseJson::registration(),
            PathType::registration(),
            PathWritable::registration(),
            IsExec::registration(),
            HasKey::registration(),
            MapSet::registration(),
            MergeMaps::registration(),
            Base64Encode::registration(),
            Base64Decode::registration(),
            Rand::registration(),
            ToJson::registration(),
            TypeOf::registration(),
            Functions::registration(),
            Describe::registration(),
            IsTerminal::registration(),
            Eof::registration(),
            TextTrim::registration(),
            TextTrimPrefix::registration(),
            TextTrimSuffix::registration(),
            TextToUpper::registration(),
            TextToLower::registration(),
            TextReplace::registration(),
            TextReplaceRegex::registration(),
            TextSplit::registration(),
            TextJoin::registration(),
            TextLines::registration(),
            TextFields::registration(),
            TextMatch::registration(),
            TextExtract::registration(),
            TextStartsWith::registration(),
            TextEndsWith::registration(),
            TextContains::registration(),
            TextLength::registration(),
            TextCount::registration(),
            TextCountMatches::registration(),
            TextGrep::registration(),
            TextLinesRange::registration(),
            SemaphoreNew::registration(),
            SemaphoreTryAcquire::registration(),
            SemaphoreAvailable::registration(),
        ]
    }

    /// Every name the registry answers to. Backs parse-time shadow
    /// validation through `builtin_function_names`.
    pub(super) fn keys(&self) -> HashSet<String> {
        self.entries.keys().cloned().collect()
    }

    /// Single lookup for every callable: script, pure, or stateful.
    pub(super) fn get(&self, name: &str) -> Option<FuncEntry<P>> {
        self.entries.get(name).cloned()
    }

    fn insert_native(&mut self, name: String, meta: FuncMeta, body: FuncBody<P>) {
        self.entries.insert(name, FuncEntry { meta, body });
        self.rebuild_pure_shared();
    }

    /// Rebuild the shared pure table. Runs only on native insertion:
    /// script definitions insert `Script` bodies directly and never
    /// affect the pure set, so this stays correct between rebuilds.
    /// Dual entries contribute their template arm, which is what
    /// `{{ }}` placeholder expansion resolves.
    fn rebuild_pure_shared(&mut self) {
        let mut table: PureTable = HashMap::new();
        for (name, entry) in &self.entries {
            let func = match &entry.body {
                FuncBody::Pure(func) => func,
                FuncBody::Both { pure, .. } => pure,
                FuncBody::Script(_) | FuncBody::Ctx(_) => continue,
            };
            if let Some((module, base)) = split_qualified(name) {
                table
                    .entry(module.to_string())
                    .or_default()
                    .insert(base.to_string(), Arc::clone(func));
            }
        }
        self.pure_shared = Arc::new(table);
    }

    /// Insert under `MODULE::BASE`, stamping provenance on the metadata.
    /// The single choke point for every registry entry: builtins pass
    /// `STD`, hosts pass their module, scripts pass `SCRIPT`.
    fn insert_qualified(
        &mut self,
        module: &str,
        base: String,
        mut meta: FuncMeta,
        body: FuncBody<P>,
    ) {
        meta.name = qualify(module, &base);
        meta.module = module.to_string();
        // Last-write-wins would silently reroute calls, so a repeated
        // qualified name is a programmer error, never a shadow: panic like
        // conflicting type registrations do. `SCRIPT` definitions bypass
        // this path (`define_script` owns their scoped shadowing).
        if self.entries.contains_key(&meta.name) {
            panic!("duplicate function registration `{}`", meta.name);
        }
        self.insert_native(meta.name.clone(), meta, body);
    }

    pub(super) fn register_host(
        &mut self,
        module: &str,
        name: String,
        mut meta: FuncMeta,
        func: NativeFn<P>,
    ) {
        meta.kind = FuncKind::HostCtx;
        self.insert_qualified(module, name, meta, FuncBody::Ctx(func));
    }

    pub(super) fn register_pure_host(
        &mut self,
        module: &str,
        name: String,
        mut meta: FuncMeta,
        func: PureFn,
    ) {
        meta.kind = FuncKind::HostPure;
        self.insert_qualified(module, name, meta, FuncBody::Pure(func));
    }

    pub(super) fn register_both_host(
        &mut self,
        module: &str,
        name: String,
        mut meta: FuncMeta,
        func: NativeFn<P>,
        template: PureFn,
    ) {
        meta.kind = FuncKind::HostCtx;
        self.insert_qualified(
            module,
            name,
            meta,
            FuncBody::Both {
                pure: template,
                ctx: func,
            },
        );
    }

    /// Define a DSL `FUNC`: names colliding with any known base name cannot
    /// shadow, same-scope duplicates cannot redefine, and nested shadowing
    /// of an outer script definition restores on scope exit. Stored as
    /// `SCRIPT::NAME`, exactly like every other qualified entry.
    pub(super) fn define_script(
        &mut self,
        name: &str,
        params: &[(String, TypeTag)],
        body: &[Step],
    ) -> Result<()> {
        let qualified = qualify(SCRIPT_MODULE_NAME, name);
        let shadowable = matches!(
            self.entries.get(&qualified).map(|entry| &entry.body),
            Some(FuncBody::Script(_))
        );
        // Reserved spans every module: compare base names so the message
        // keeps naming the bare script identifier.
        let reserved = self
            .entries
            .keys()
            .any(|key| split_qualified(key).is_some_and(|(_, base)| base == name));
        if reserved && !shadowable {
            anyhow::bail!("cannot shadow reserved function `{name}`");
        }
        if self
            .scopes
            .last()
            .is_some_and(|frame| frame.defined.contains(&qualified))
        {
            anyhow::bail!("duplicate function `{name}` in same scope");
        }
        let old = self.entries.insert(
            qualified.clone(),
            FuncEntry {
                meta: FuncMeta {
                    name: qualified.clone(),
                    module: SCRIPT_MODULE_NAME.to_string(),
                    kind: FuncKind::Script,
                    params: Some(
                        params
                            .iter()
                            .map(|(name, param_type)| FuncParam {
                                name: name.clone(),
                                param_type: Some(param_type.clone()),
                                allowed: None,
                                // Script `FUNC` params carry no prose:
                                // declarations have no doc syntax.
                                docs: "",
                                options: None,
                                optional: false,
                            })
                            .collect(),
                    ),
                    returns: None,
                    rpn: false,
                    summary: "DSL-defined function.",
                    docs: "Defined via FUNC in script.",
                    examples: &[],
                },
                body: FuncBody::Script(FuncDefData {
                    params: params.to_vec(),
                    body: body.to_vec(),
                }),
            },
        );
        if let Some(frame) = self.scopes.last_mut() {
            frame.defined.insert(qualified.clone());
            if let Some(old) = old {
                frame.shadowed.push((qualified, old));
            }
        }
        Ok(())
    }

    /// Open a lexical scope frame for script definitions. Native entries
    /// persist; only script names track here.
    pub(super) fn push_scope(&mut self) {
        self.scopes.push(ScopeFrame {
            defined: HashSet::new(),
            shadowed: Vec::new(),
        });
    }

    /// Close a lexical scope frame: names defined inside revert, and any
    /// outer definition they shadowed is restored.
    pub(super) fn pop_scope(&mut self) {
        let Some(frame) = self.scopes.pop() else {
            return;
        };
        for name in frame.defined {
            self.entries.remove(&name);
        }
        for (name, old) in frame.shadowed {
            self.entries.insert(name, old);
        }
    }

    /// True when `name` is an interpreted script definition (needing call
    /// scoping and depth budgeting through `call_func_value` rather than
    /// inline evaluation).
    pub(super) fn contains_script(&self, name: &str) -> bool {
        matches!(
            self.entries.get(name).map(|entry| &entry.body),
            Some(FuncBody::Script(_))
        )
    }

    /// Clone the pure fn for `name` (ending the registry borrow) so callers
    /// can invoke it without holding `&self` across a `&mut StepCtx` use.
    /// Dual entries answer with their template arm.
    fn clone_pure_fn(&self, name: &str) -> Option<PureFn> {
        match self.entries.get(name)?.body {
            FuncBody::Pure(ref func) => Some(Arc::clone(func)),
            FuncBody::Both { ref pure, .. } => Some(Arc::clone(pure)),
            _ => None,
        }
    }

    /// Shared snapshot of every pure entry, nested by module and base
    /// name. Cloning the `Arc` is refcount-only: placeholder dispatch
    /// borrows this per expansion with no map copies.
    pub(super) fn pure_shared(&self) -> Arc<PureTable> {
        Arc::clone(&self.pure_shared)
    }

    /// Every entry's metadata, scripts included, sorted by name. Backs
    /// run-start type tag validation.
    pub(super) fn all_metas(&self) -> Vec<FuncMeta> {
        self.entries_metas()
    }

    /// Clone the ctx fn for `name` (ending the registry borrow) so callers
    /// can invoke it with `&mut StepCtx` without double-borrowing state.
    /// Dual entries answer with their stateful arm, which is the full
    /// behavior wherever step IO exists.
    fn clone_ctx_fn(&self, name: &str) -> Option<NativeFn<P>> {
        match self.entries.get(name)?.body {
            FuncBody::Ctx(ref func) => Some(Arc::clone(func)),
            FuncBody::Both { ref ctx, .. } => Some(Arc::clone(ctx)),
            _ => None,
        }
    }

    fn meta(&self, name: &str) -> Option<FuncMeta> {
        self.entries.get(name).map(|entry| entry.meta.clone())
    }

    fn native_metas(&self) -> Vec<FuncMeta> {
        let mut out: Vec<FuncMeta> = Vec::new();
        for entry in self.entries.values() {
            if !matches!(entry.body, FuncBody::Script(_)) {
                out.push(entry.meta.clone());
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Every entry's metadata, scripts included, sorted by name. Backs
    /// runtime `FUNCTIONS()` listings.
    fn entries_metas(&self) -> Vec<FuncMeta> {
        let mut out: Vec<FuncMeta> = self
            .entries
            .values()
            .map(|entry| entry.meta.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Install script `#` docs keyed by bare base name. Stored under
    /// qualified `SCRIPT::NAME` keys.
    pub fn set_script_docs(&mut self, docs: &HashMap<String, (String, String)>) {
        for (base, pair) in docs {
            self.script_docs
                .insert(qualify(SCRIPT_MODULE_NAME, base), pair.clone());
        }
    }

    /// Script doc pair for a qualified name, if the parser collected one.
    pub fn script_doc_for(&self, qualified: &str) -> Option<(String, String)> {
        self.script_docs.get(qualified).cloned()
    }

    /// Metadata for single-file `EXPORT` names in source order. Errors
    /// name the first missing export; callers fail fast before serving.
    pub fn exported_metas(&self, exports: &[String]) -> anyhow::Result<Vec<FuncMeta>> {
        let mut out = Vec::with_capacity(exports.len());
        for base in exports {
            let qualified = qualify(SCRIPT_MODULE_NAME, base);
            match self.entries.get(&qualified) {
                Some(entry) => out.push(entry.meta.clone()),
                None => anyhow::bail!("EXPORT `{base}` does not match any defined script function"),
            }
        }
        Ok(out)
    }
}

impl<P: ProcessManager> Clone for FunctionRegistry<P> {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            scopes: self.scopes.clone(),
            pure_shared: Arc::clone(&self.pure_shared),
            script_docs: self.script_docs.clone(),
        }
    }
}

/// Names of all compiled-in builtins plus `INSPECT` (a dedicated AST/RPN
/// node, not a registry entry). Read straight off a stock registry, so the
/// `#[oxdock_func]` annotations stay the single source of truth: adding a
/// builtin extends this set with no parallel list to update. Seeds
/// parse-time shadow validation, so the parser crate keeps zero
/// compile-time knowledge of builtin names.
pub fn builtin_function_names() -> HashSet<String> {
    let mut names = FunctionRegistry::<DefaultProcessManager>::with_builtins().keys();
    names.insert(KEYWORD_INSPECT.to_string());
    names
}

/// Metadata of every builtin function, sorted by name, for static
/// rendering (docs-gen). Same single source as `builtin_function_names`:
/// the `#[oxdock_func]` annotations, never a parallel list.
pub fn builtin_function_metas() -> Vec<FuncMeta> {
    FunctionRegistry::<DefaultProcessManager>::with_builtins().native_metas()
}

/// Stock `STD` module table derived from the `#[oxdock_func]` builtins:
/// the single source of truth for builtin membership and RPN eligibility.
/// Seeds parse-time module resolution, so the parser crate keeps zero
/// compile-time knowledge of builtin names.
pub fn std_module_table() -> oxdock_parser::ModuleTable {
    // Registry names are qualified (`STD::GLOB`); the table holds bases.
    let functions: HashSet<String> = builtin_function_metas()
        .into_iter()
        .map(|meta| base_name(&meta.name).to_string())
        .collect();
    oxdock_parser::ModuleTable {
        modules: HashMap::from([(
            STD_MODULE_NAME.to_string(),
            Some(oxdock_parser::ModuleFuncs { functions }),
        )]),
    }
}

// Builtins below use the `#[oxdock_func]` host export macro, the exact same authoring
// model as host-registered functions: metadata, arity checks, and argument
// unpacking derive from the signature plus doc comments, so adding a native
// means writing one small typed function plus one line each in
// `builtin_registrations` (consumed by `with_builtins`) and the
// `FUNCTIONS`/`DESCRIBE`/`TYPES` surface, which all read the same symbols.

/// Convert a value to INT.
///
/// Trims ASCII whitespace and parses i64. Passes Int through; Float only
/// when integral and finite; Bool as 1/0.
///
#[oxdock_func(pure, returns = TypeTag::Int)]
#[example(
    name = "convert",
    code = r###"
    IMPORT [STD]
    LET $n: INT = INT("42")
    ASSERT_EQ $n 42
    LET $flag: INT = INT(true)
    ASSERT_EQ $flag 1
    "###
)]
fn int(
    /// Value to convert to `INT`.
    #[union(Int, Float, String, Bool)]
    val: Value,
) -> Result<Value> {
    super::args::int_from_value(val)
}

/// Convert a value to FLOAT.
///
/// Parses f64 (accepts int strings), bails on non-finite or non-numeric.
///
#[oxdock_func(pure, returns = TypeTag::Float)]
#[example(
    name = "convert",
    code = r###"
    IMPORT [STD]
    LET $f: FLOAT = FLOAT("1.5")
    ASSERT_EQ $f 1.5
    LET $whole: FLOAT = FLOAT(2)
    ASSERT_EQ $whole 2.0
    "###
)]
fn float(
    /// Value to convert to `FLOAT`.
    #[union(Float, Int, String)]
    val: Value,
) -> Result<Value> {
    super::args::float_from_value(val)
}

/// Format a byte count canonically (`500B`, `10KB`, `2MB`, `1GB`,
/// binary `10KIB` and friends): the inverse of `PARSE_BYTE_COUNT`, so
/// formatted values reparse exactly. Negative inputs fail.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "format and round-trip",
    code = r###"
    # Byte counts render in the largest exact unit.
    IMPORT [STD]
    LET $pretty: STRING = FORMAT_BYTE_COUNT(1_048_576)
    ASSERT_EQ $pretty "1MiB"

    # The pair round-trips through the parser.
    LET $back: INT = PARSE_BYTE_COUNT($pretty)
    ASSERT_EQ $back 1_048_576
    "###
)]
fn format_byte_count(
    /// Byte count to format; negatives fail.
    bytes: i64,
) -> Result<Value> {
    if bytes < 0 {
        return Err(anyhow::anyhow!(
            "FORMAT_BYTE_COUNT needs a non-negative byte count, got {bytes}"
        ));
    }
    Ok(Value::string(oxdock_parser::command::format_byte_count(
        bytes as u64,
    )))
}

/// Parse a byte-count token to `INT` bytes.
///
/// Bare digits are bytes; suffixes `B`/`KB`/`MB`/`GB`/`TB` scale
/// decimal and `KIB`/`MIB`/`GIB`/`TIB` binary, in any case. Plain
/// `INT` values pass through (already bytes); integral finite `FLOAT`
/// values convert. Pairs with `FORMAT_BYTE_COUNT` for the round trip.
///
/// ```oxdock
/// # Units parse case-insensitively; bare digits are bytes.
/// IMPORT [STD]
/// LET $bytes: INT = PARSE_BYTE_COUNT("1Gb")
/// ASSERT_EQ $bytes 1_000_000_000
///
/// # Binary suffixes scale by powers of two.
/// LET $mebi: INT = PARSE_BYTE_COUNT("2mib")
/// ASSERT_EQ $mebi 2_097_152
///
/// # Digit runs group Rust-literal style.
/// LET $grouped: INT = PARSE_BYTE_COUNT("1_000_000")
/// ASSERT_EQ $grouped 1_000_000
///
/// # Plain numbers pass through as bytes already.
/// LET $plain: INT = PARSE_BYTE_COUNT(1_048_576)
/// ASSERT_EQ $plain 1_048_576
///
/// # Bare literals group too.
/// LET $bare: INT = PARSE_BYTE_COUNT(1_000_000)
/// ASSERT_EQ $bare 1_000_000
///
/// # One optional space separates number from unit.
/// LET $spaced: INT = PARSE_BYTE_COUNT("1 GB")
/// ASSERT_EQ $spaced 1_000_000_000
/// ```
///
#[oxdock_func(pure, returns = TypeTag::Int)]
#[example(
    name = "parse",
    code = r###"
    IMPORT [STD]
    LET $n: INT = PARSE_BYTE_COUNT("10KB")
    ASSERT_EQ $n 10_000
    LET $identity: INT = PARSE_BYTE_COUNT(512)
    ASSERT_EQ $identity 512
    "###
)]
fn parse_byte_count(
    /// Byte-count token to parse, or bytes already.
    #[union(Int, Float, String)]
    text: Value,
) -> Result<Value> {
    if let Some(n) = text.as_i64() {
        if n < 0 {
            return Err(anyhow::anyhow!(
                "PARSE_BYTE_COUNT needs a non-negative byte count, got {n}"
            ));
        }
        return Ok(Value::int(n));
    }
    if let Some(f) = text.as_f64() {
        if f.is_finite() && f.fract() == 0.0 && f >= 0.0 && f <= i64::MAX as f64 {
            return Ok(Value::int(f as i64));
        }
        return Err(anyhow::anyhow!(
            "PARSE_BYTE_COUNT needs an integral byte count, got {f:?}"
        ));
    }
    let Some(s) = text.as_str() else {
        return Err(anyhow::anyhow!(
            "PARSE_BYTE_COUNT needs a token or number, found {text:?}"
        ));
    };
    let n = oxdock_parser::command::parse_byte_count(s)?;
    let n = i64::try_from(n)
        .map_err(|_| anyhow::anyhow!("PARSE_BYTE_COUNT out of INT range: {s:?}"))?;
    Ok(Value::int(n))
}

/// Format millisecond durations canonically (`500ms`, `10s`, `2m`,
/// `1h`): the inverse of `PARSE_DURATION`, so formatted values
/// reparse exactly. Negative inputs fail.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "format and round-trip",
    code = r###"
    # Durations render in the largest exact unit.
    IMPORT [STD]
    LET $pretty: STRING = FORMAT_DURATION(90_000)
    ASSERT_EQ $pretty "90s"

    # The pair round-trips through the parser.
    LET $back: INT = PARSE_DURATION($pretty)
    ASSERT_EQ $back 90_000
    "###
)]
fn format_duration(
    /// Milliseconds to format; negatives fail.
    millis: i64,
) -> Result<Value> {
    if millis < 0 {
        return Err(anyhow::anyhow!(
            "FORMAT_DURATION needs non-negative milliseconds, got {millis}"
        ));
    }
    Ok(Value::string(oxdock_parser::command::format_duration(
        &std::time::Duration::from_millis(millis as u64),
    )))
}

/// Parse a duration token to `INT` milliseconds.
///
/// Suffixes `ms`/`s`/`m`/`h`; a bare number means seconds. Pairs with
/// `FORMAT_DURATION` for the round trip.
///
#[oxdock_func(pure, returns = TypeTag::Int)]
#[example(
    name = "parse",
    code = r###"
    # A bare number means seconds, like TIMEOUT.
    IMPORT [STD]
    LET $ms: INT = PARSE_DURATION("2m")
    ASSERT_EQ $ms 120_000

    # Milliseconds stay exact.
    LET $exact: INT = PARSE_DURATION("1500ms")
    ASSERT_EQ $exact 1_500

    # One optional space separates number from unit.
    LET $spaced: INT = PARSE_DURATION("1 h")
    ASSERT_EQ $spaced 3_600_000
    "###
)]
fn parse_duration(
    /// Duration token to parse.
    text: String,
) -> Result<Value> {
    let duration = oxdock_parser::command::parse_duration(&text)?;
    let millis = i64::try_from(duration.as_millis())
        .map_err(|_| anyhow::anyhow!("PARSE_DURATION out of INT range: {text:?}"))?;
    Ok(Value::int(millis))
}

/// List workspace paths matching a glob pattern.
///
/// Sorted, root-relative LIST; empty on no match or `..` escape.
///
#[oxdock_func(rpn, returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "match",
    code = r###"
    IMPORT [STD]
    WRITE "notes-a.md" "a"
    WRITE "notes-b.md" "b"
    WRITE "other.txt" "c"
    LET $found: LIST<STRING> = GLOB("notes-*.md")
    ASSERT_EQ $found.0 "notes-a.md"
    ASSERT_EQ $found.1 "notes-b.md"
    "###
)]
fn glob<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Glob pattern matched against workspace paths.
    #[union(String, Path)]
    pattern: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&pattern)?;
    super::args::glob_from_value(&[Value::string(text)], cx)
}

/// Load and parse a TOML file.
///
/// Reads a workspace file and parses TOML into a DSL value.
///
#[oxdock_func(rpn, returns = TypeTag::MapOf(std::sync::Arc::new(TypeTag::Any)))]
#[example(
    name = "load",
    code = r###"
    IMPORT [STD]
    WRITE "cfg.toml" "title = \"demo\""
    LET $cfg: MAP<ANY> = LOAD_TOML("cfg.toml")
    ASSERT_EQ $cfg.title "demo"
    "###
)]
fn load_toml<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Workspace file path to load and parse as TOML.
    #[union(String, Path)]
    path: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&path)?;
    super::args::load_toml_from_value(&[Value::string(text)], cx)
}

/// Load and parse a JSON file.
///
/// Reads a workspace file and parses JSON into a DSL value. Returns
/// `ANY` by design: a top-level array or scalar parses to `LIST` or a
/// scalar word. `LET` coercion still checks the actual value at
/// assignment.
///
#[oxdock_func(rpn, returns = TypeTag::Any)]
#[example(
    name = "load",
    code = r###"
    IMPORT [STD]
    WRITE "data.json" "{\"a\": 1}"
    LET $doc: MAP<ANY> = LOAD_JSON("data.json")
    ASSERT_EQ $doc.a 1
    "###
)]
fn load_json<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Workspace file path to load and parse as JSON.
    #[union(String, Path)]
    path: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&path)?;
    super::args::load_json_from_value(&[Value::string(text)], cx)
}

/// Parse TOML text already held in memory.
///
/// Uses the same conversion as file loading, so fetch bodies, file
/// contents, and captured text share one JSON/TOML shape.
///
#[oxdock_func(pure, returns = TypeTag::MapOf(std::sync::Arc::new(TypeTag::Any)))]
#[example(
    name = "parse",
    code = r###"
    IMPORT [STD]
    LET $doc: MAP<ANY> = PARSE_TOML("title = \"demo\"")
    ASSERT_EQ $doc.title "demo"
    "###
)]
fn parse_toml(
    /// TOML text already held in memory.
    text: String,
) -> Result<Value> {
    super::args::parse_toml_from_value(Value::string(text))
}

/// Parse JSON text already held in memory.
///
/// Uses the same conversion as file loading, so fetch bodies, file
/// contents, and captured text share one JSON value shape. Returns
/// `ANY` by design, matching `LOAD_JSON`: a top-level array or scalar
/// parses to `LIST` or a scalar word. `LET` coercion still checks the
/// actual value at assignment.
///
#[oxdock_func(pure, returns = TypeTag::Any)]
#[example(
    name = "parse",
    code = r###"
    # Parse in-memory JSON text with the same conversion as file loading.
    IMPORT [STD]
    LET $body: STRING = "{\"a\": 1}"
    LET $doc: ANY = PARSE_JSON($body)
    ASSERT_EQ $doc.a 1

    # Top-level arrays parse to LIST, which is why the return is ANY.
    LET $items: LIST<ANY> = PARSE_JSON("[1, 2]")
    ASSERT_EQ $items.0 1
    ASSERT_EQ $items.1 2
    "###
)]
fn parse_json(
    /// JSON text already held in memory.
    text: String,
) -> Result<Value> {
    super::args::parse_json_from_value(Value::string(text))
}

/// Describe a filesystem entry.
///
/// Reports file, dir, symlink (no-follow), or absent. AST-only by design;
/// there is no RPN arm for filesystem IO.
///
#[oxdock_func(returns = TypeTag::String)]
#[example(
    name = "describe",
    code = r###"
    IMPORT [STD]
    WRITE "entry.txt" "words"
    LET $kind: STRING = PATH_TYPE("entry.txt")
    ASSERT_EQ $kind "file"
    LET $missing: STRING = PATH_TYPE("no-such-entry.txt")
    ASSERT_EQ $missing "absent"
    "###
)]
fn path_type<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Workspace path of the entry to describe.
    #[union(String, Path)]
    path: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&path)?;
    super::args::path_type_from_value(&[Value::string(text)], cx)
}

/// Report whether an ordinary file can be created at a path.
///
/// Walks up to the nearest existing ancestor, requires it be a
/// directory, and probes writability by creating and deleting a probe
/// file, so retry loops never litter. The empty string is false.
/// AST-only by design, like `PATH_TYPE`: there is no RPN arm for
/// filesystem IO.
///
#[oxdock_func(returns = TypeTag::Bool)]
#[example(
    name = "probe",
    code = r###"
    # An absent nested path is writable when its ancestor is.
    IMPORT [STD]
    LET $fresh: BOOL = PATH_WRITABLE("probe-check/nested/file.txt")
    ASSERT_EQ $fresh true

    # The empty string never aliases the working directory.
    LET $empty: BOOL = PATH_WRITABLE("")
    ASSERT_EQ $empty false
    "###
)]
fn path_writable<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Workspace path to probe for file creation.
    #[union(String, Path)]
    path: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&path)?;
    super::args::path_writable_from_value(&[Value::string(text)], cx)
}

/// Report whether a name resolves to an executable file (#100).
///
/// Bare command names search `env:PATH` (`IS_EXEC("tar")` finds the
/// first executable `tar`, plus `tar.exe`, on `PATH`); spellings
/// containing a separator check that file directly. True means a real
/// file with an executable bit on Unix, or a `PATHEXT` extension on
/// Windows: each `env:PATHEXT` entry is probed in order (unset or empty
/// probes nothing). Absolute `PATH` entries resolve through the workspace as
/// usual, so host probing wants `WORKSPACE SYSTEM` and an inherited
/// `PATH`; anything unresolvable reads false, never errors. AST-only
/// by design, like `PATH_TYPE`: there is no RPN arm for filesystem IO.
///
#[oxdock_func(returns = TypeTag::Bool)]
#[example(
    name = "probe",
    code = r###"
    # Missing tools read false instead of failing the run.
    IMPORT [STD]
    LET $missing: BOOL = IS_EXEC("no-such-tool-xyz")
    ASSERT_EQ $missing false

    # Plain files are not executable on any platform.
    WRITE tool.txt payload
    LET $plain: BOOL = IS_EXEC("tool.txt")
    ASSERT_EQ $plain false
    "###
)]
fn is_exec<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Command name or file path to probe.
    #[union(String, Path)]
    name: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&name)?;
    super::args::is_exec_from_value(&[Value::string(text)], cx)
}

/// Report whether a map holds a key.
///
/// Pure MAP probe so scripts can branch on optional fields without
/// tripping the strict missing-key error.
///
#[oxdock_func(pure, returns = TypeTag::Bool)]
#[example(
    name = "probe",
    code = r###"
    IMPORT [STD]
    LET $m: MAP<ANY> = PARSE_JSON("{\"a\": 1}")
    LET $hit: BOOL = HAS_KEY($m, "a")
    ASSERT_EQ $hit true
    LET $miss: BOOL = HAS_KEY($m, "b")
    ASSERT_EQ $miss false
    "###
)]
fn has_key(
    /// Map to probe.
    map: BTreeMap<String, Value>,
    /// Key to look up.
    key: String,
) -> Result<Value> {
    super::args::has_key_from_value(Value::map(map), &key)
}

/// Insert one key into a map.
///
/// Fails on duplicates so two entries sharing a key fail the run
/// instead of silently shadowing each other.
///
#[oxdock_func(pure, returns = TypeTag::MapOf(std::sync::Arc::new(TypeTag::Any)))]
#[example(
    name = "insert",
    code = r###"
    IMPORT [STD]
    LET $m: MAP<ANY> = MAP_SET({seed: 0}, "a", 1)
    ASSERT_EQ $m.a 1
    "###
)]
fn map_set(
    /// Map to insert into.
    map: BTreeMap<String, Value>,
    /// Key to insert; duplicates fail.
    key: String,
    /// Value to store.
    value: Value,
) -> Result<Value> {
    super::args::map_set_from_value(Value::map(map), key, value)
}

/// Draw a random FLOAT uniformly from [0, 1), optionally seeded.
///
/// Impure by design: unseeded calls return different values, so this
/// runs on the AST script path only, never as a `{{ }}` placeholder
/// call. A seed draws the same value on every run, for deterministic
/// fixtures. Scale and shift the draw for wider ranges.
///
#[oxdock_func(returns = TypeTag::Float, summary = "Draw a random FLOAT uniformly from [0, 1), optionally seeded.")]
#[example(
    name = "seeded draw",
    code = r###"
    # A seed draws the same value on every run.
    IMPORT [STD]
    LET $one: FLOAT = RAND({seed: 7})
    LET $two: FLOAT = RAND({seed: 7})
    ASSERT_EQ $one $two
    "###
)]
#[example(
    name = "unseeded draw",
    code = r###"
    # Scale a unit draw into a backoff delay between 1 and 11 seconds.
    IMPORT [STD]
    LET $draw: FLOAT = RAND()
    LET $delay: FLOAT = 1.0 + $draw * 10.0
    IF $delay < 1.0 {
        EXIT 1
    }
    IF $delay >= 11.0 {
        EXIT 1
    }
    ECHO "retrying in {{ $delay }}s"
    "###
)]
fn rand<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Draw settings: `seed`. Omittable: a missing options MAP fills `{}`.
    #[options("seed?: INT")]
    #[optional]
    options: BTreeMap<String, Value>,
) -> Result<Value> {
    let _ = cx;
    // `check_options` in the wrapper verified the shape above; the
    // read below narrows infallibly after.
    let seed = options
        .get("seed")
        .map(|value| value.as_i64().expect("check_options verified INT"));
    super::args::rand_float_from_value(seed)
}

/// Encode a STRING as standard base64.
///
/// Padded RFC 4648 alphabet, so encoded output pastes into any
/// standard decoder. Pairs with `BASE64_DECODE` for the round trip.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "encode",
    code = r###"
    # Encode text to standard base64.
    IMPORT [STD]
    LET $encoded: STRING = BASE64_ENCODE("hello")
    ASSERT_EQ $encoded "aGVsbG8="
    "###
)]
fn base64_encode(
    /// Text to encode.
    text: String,
) -> Result<Value> {
    super::args::base64_encode_from_value(Value::string(text))
}

/// Decode standard base64 to a STRING.
///
/// Fails naming the input on invalid alphabet characters or
/// non-UTF-8 payloads instead of rendering a silent empty. Pairs
/// with `BASE64_ENCODE` for the round trip.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "decode",
    code = r###"
    # Decode standard base64 back to text.
    IMPORT [STD]
    LET $decoded: STRING = BASE64_DECODE("aGVsbG8=")
    ASSERT_EQ $decoded "hello"
    "###
)]
fn base64_decode(
    /// Base64 text to decode.
    text: String,
) -> Result<Value> {
    super::args::base64_decode_from_value(Value::string(text))
}

/// Encode a script value as JSON with one trailing newline.
///
/// Maps stay sorted; only template-safe shapes (STRING, INT, FLOAT,
/// BOOL, LIST, MAP) survive, anything else fails here instead of
/// rendering as a silent empty.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "encode",
    code = r###"
    IMPORT [STD]
    LET $json: STRING = TO_JSON({a: 1})
    ASSERT_EQ $json "{\"a\":1}\n"
    "###
)]
fn to_json(
    /// Value to encode as JSON.
    #[union(String, Int, Float, Bool, List, Map)]
    value: Value,
) -> Result<Value> {
    super::args::to_json_from_value(value)
}

/// Name the word a value holds, for data-driven branching.
///
/// Returns the registered type word (`STRING`, `INT`, `FLOAT`, `BOOL`,
/// `LIST`, `MAP`, plus handle words like `PIPE`): the same name the
/// value prints in arity and coercion errors, so scripts can branch
/// on config shapes (a path string or a path list) without failing.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "name the word",
    code = r###"
    IMPORT [STD]
    LET $word: STRING = TYPE_OF(42)
    ASSERT_EQ $word "INT"
    "###
)]
fn type_of(
    /// Value whose word to name.
    value: Value,
) -> Result<Value> {
    super::args::type_of_from_value(value)
}

/// Merge a LIST of MAPs in order under one duplicate policy.
///
/// `fail_on_duplicate` fails naming the repeated key, so two files
/// claiming one placeholder fail the run instead of shadowing each
/// other. `overwrite` lets later files win, for environment overlays.
/// Non MAP elements fail naming their position.
///
#[oxdock_func(pure, returns = TypeTag::MapOf(std::sync::Arc::new(TypeTag::Any)))]
#[example(
    name = "merge",
    code = r###"
    IMPORT [STD]
    LET $m: MAP<ANY> = MERGE_MAPS([{a: 1}, {b: 2}], "overwrite")
    ASSERT_EQ $m.a 1
    ASSERT_EQ $m.b 2
    "###
)]
fn merge_maps(
    /// LIST of MAPs to merge in order.
    maps: Vec<BTreeMap<String, Value>>,
    /// Duplicate policy.
    #[values("fail_on_duplicate", "overwrite")]
    policy: String,
) -> Result<Value> {
    super::args::merge_maps_from_value(
        Value::list(maps.into_iter().map(Value::map).collect()),
        policy,
    )
}

/// List all visible function names.
///
/// Sorted LIST of qualified `MODULE::NAME` entries: DSL-defined plus native
/// plus host-registered names.
#[oxdock_func(returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "list",
    code = r###"
    IMPORT [STD]
    LET $names: LIST<STRING> = FUNCTIONS()
    ASSERT_CONTAINS $names "STD::INT"
    "###
)]
fn functions<P: ProcessManager>(cx: &mut StepCtx<P>) -> Result<Value> {
    let mut names: Vec<String> = cx
        .state
        .list_functions()
        .into_iter()
        .map(|meta| meta.name)
        .collect();
    names.sort();
    names.dedup();
    Ok(Value::list(names.into_iter().map(Value::string).collect()))
}

/// Describe one function by qualified name.
///
/// Returns a MAP with name, module, kind, params, returns, rpn, and
/// summary. `params` is always a LIST (empty for parameterless
/// functions) of entries carrying name, param_type, allowed, docs,
/// options, and optional flags, so the shape holds for every entry. Bare names
/// fail closed: `DESCRIBE` requires the qualified form (except
/// `INSPECT`, which is syntax rather than a registry entry). Errors
/// on unknown function.
///
/// Field table for the `DESCRIBE` result bundle: one source for the
/// shape the host mints, so introspection, enforcement, and
/// documentation cannot drift apart.

#[oxdock_func(
    returns = TypeTag::Record(std::sync::Arc::from([Field {
        name: "name".to_string(),
        ty: TypeTag::String,
        docs: "Qualified function name.",
        optional: false,
    }, Field {
        name: "module".to_string(),
        ty: TypeTag::String,
        docs: "Owning module.",
        optional: false,
    }, Field {
        name: "kind".to_string(),
        ty: TypeTag::String,
        docs: "Origin label (`host` or `script`).",
        optional: false,
    }, Field {
        name: "params".to_string(),
        ty: TypeTag::ListOf(std::sync::Arc::new(TypeTag::MapOf(std::sync::Arc::new(
            TypeTag::Any,
        )))),
        docs: "Parameter entries (empty for parameterless functions).",
        optional: false,
    }, Field {
        name: "returns".to_string(),
        ty: TypeTag::String,
        docs: "Structural return shape (empty when untagged).",
        optional: false,
    }, Field {
        name: "rpn".to_string(),
        ty: TypeTag::Bool,
        docs: "True when callable in RPN position.",
        optional: false,
    }, Field {
        name: "summary".to_string(),
        ty: TypeTag::String,
        docs: "One-line description.",
        optional: false,
    }]))
    summary = "Describe one function by qualified name."
)]
#[example(
    name = "describe",
    code = r###"
    IMPORT [STD]
    LET $info: MAP<ANY> = DESCRIBE("STD::INT")
    ASSERT_EQ $info.name "STD::INT"
    ASSERT_EQ $info.returns "INT"
    "###
)]
fn describe<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Qualified function name (`MODULE::NAME`).
    name: String,
) -> Result<Value> {
    if split_qualified(&name).is_none() && name != KEYWORD_INSPECT {
        anyhow::bail!(
            "unknown function `{name}`: DESCRIBE requires a qualified name (e.g. `STD::{name}`)"
        );
    }
    cx.state
        .describe_function(&name)
        .ok_or_else(|| anyhow::anyhow!("unknown function {name}"))
}

/// List all known type names.
///
/// Sorted LIST of startup plus host-registered type descriptors. Reads the
/// run's name directory, so it runs on the AST path like the other
/// introspection functions.
#[oxdock_func(returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "list",
    code = r###"
    IMPORT [STD]
    LET $names: LIST<STRING> = TYPES()
    ASSERT_CONTAINS $names "STRING"
    "###
)]
fn types<P: ProcessManager>(cx: &mut StepCtx<P>) -> Result<Value> {
    Ok(Value::list(
        cx.state
            .type_names()
            .into_iter()
            .map(Value::string)
            .collect(),
    ))
}

/// Describe one type by name.
///
/// Returns a MAP with name, summary, and docs. Errors on unknown type.
/// Reads the run's name directory, so it runs on the AST path.
///
#[oxdock_func(returns = TypeTag::Record(std::sync::Arc::from([Field {
    name: "name".to_string(),
    ty: TypeTag::String,
    docs: "Queried type name.",
    optional: false,
}, Field {
    name: "summary".to_string(),
    ty: TypeTag::String,
    docs: "One-line description.",
    optional: false,
}, Field {
    name: "docs".to_string(),
    ty: TypeTag::String,
    docs: "Full documentation text.",
    optional: false,
}])))]
#[example(
    name = "describe",
    code = r###"
    IMPORT [STD]
    LET $info: MAP<ANY> = TYPE_DESCRIBE("STRING")
    ASSERT_EQ $info.name "STRING"
    "###
)]
fn type_describe<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Type name to describe.
    name: String,
) -> Result<Value> {
    if let Some(descriptor) = cx.state.describe_type(&name) {
        let mut map = BTreeMap::new();
        map.insert(
            "name".to_string(),
            Value::string(descriptor.name.to_string()),
        );
        map.insert(
            "summary".to_string(),
            Value::string(descriptor.summary.to_string()),
        );
        map.insert(
            "docs".to_string(),
            Value::string(descriptor.docs.to_string()),
        );
        return Ok(Value::map(map));
    }
    // Script aliases have no descriptor but introspect the same way:
    // the entry names the alias and its canonical target spelling.
    if let Some(target) = cx.state.alias_target(&name) {
        let mut map = BTreeMap::new();
        map.insert("name".to_string(), Value::string(name.clone()));
        map.insert(
            "summary".to_string(),
            Value::string(format!("Script type alias for {target}.")),
        );
        map.insert(
            "docs".to_string(),
            Value::string(format!(
                "Declared with TYPE {name} = {target}. The alias expands structurally at every declaration."
            )),
        );
        return Ok(Value::map(map));
    }
    Err(anyhow::anyhow!("unknown type {name}"))
}

/// Report whether a standard stream is a terminal.
///
/// `IS_TERMINAL("stdin")`, `IS_TERMINAL("stdout")`, or `IS_TERMINAL("stderr")`
/// answers for the step's stream as currently bound, so scripts can adapt
/// prompts, colors, and progress output. Anything diverted from the
/// terminal reports false without touching host handles: `WITH_IO` pipe
/// bindings (script backends and OS pairs), `LET`-capture sinks, staged
/// runner sinks, and any materialized stdin stream (only a directly
/// inherited fd falls back to the process check). A transparent root tee
/// still answers the session question via the process check. The name
/// matches exactly (no case folding): anything else bails. AST-only:
/// reads the step context like the other introspection functions.
///
#[oxdock_func(returns = TypeTag::Bool)]
#[example(
    name = "null stdin",
    code = r###"
    # A null stdin is never a terminal, in every harness.
    IMPORT [STD]
    LET $term: BOOL = IS_TERMINAL("stdin")
    ASSERT_EQ $term false
    "###
)]
fn is_terminal<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Stream name: `stdin`, `stdout`, or `stderr` (exact match).
    #[values("stdin", "stdout", "stderr")]
    stream_name: String,
) -> Result<Value> {
    use std::io::IsTerminal;
    let terminal = match stream_name.as_str() {
        "stdin" => {
            // A script-pipe backend is definitive; Null is /dev/null.
            // Only a directly inherited fd answers the process check: a
            // materialized Stream is always a binding (staged input,
            // WITH_IO pipe, or OS half), never the raw fd.
            if cx.stdin_pipe.is_some() {
                false
            } else {
                match &cx.stdin {
                    CommandStdin::Null => false,
                    #[cfg(not(miri))]
                    CommandStdin::OsPipe(_) => false,
                    CommandStdin::Stream(_) => false,
                    CommandStdin::Inherit => std::io::stdin().is_terminal(),
                }
            }
        }
        "stdout" => {
            if cx.out_pipe.is_some() {
                // WITH_IO script-pipe binding: never a terminal.
                false
            } else if cx.state.io.stdout().is_some() {
                // Staged runner sink: the root tee diverts bytes to the
                // sink only, never to real stdout.
                false
            } else {
                match &cx.out {
                    // Unbound: inherited straight through.
                    None => std::io::stdout().is_terminal(),
                    // OS kernel pipe: never a terminal.
                    #[cfg(not(miri))]
                    Some(StreamHandle::Os(_)) => false,
                    // Root tee (transparent: forwards to real stdout when
                    // unstaged, so terminal-ness survives) versus a genuine
                    // diversion. WITH_IO bindings never reach here (script
                    // pipes trip out_pipe, OS pipes trip Os, staged sinks
                    // trip above). LET-capture cannot reach here either:
                    // assign_capture installs no sink for a bare NAME(...)
                    // call, so a direct query always observes the ambient
                    // routing. The process check answers the session
                    // question.
                    Some(StreamHandle::Stream(_)) => std::io::stdout().is_terminal(),
                }
            }
        }
        "stderr" => {
            if cx.state.io.stderr().is_some() {
                // Staged runner sink: diverted, never a terminal.
                false
            } else {
                match &cx.err {
                    // Unbound: inherited straight through.
                    None => std::io::stderr().is_terminal(),
                    // OS kernel pipe: never a terminal.
                    #[cfg(not(miri))]
                    Some(StreamHandle::Os(_)) => false,
                    // Root never tees stderr and LET never captures it,
                    // so a bound handle here is always a WITH_IO binding.
                    Some(StreamHandle::Stream(_)) => false,
                }
            }
        }
        _ => anyhow::bail!(
            // Unreachable through the macro path: the generated
            // extractor rejects non-members before the body runs, and
            // the static pre-pass rejects bad literals earlier still.
            // Kept as a bail (never a panic) for hand-built callers.
            "IS_TERMINAL expects \"stdin\", \"stdout\", or \"stderr\", got {stream_name:?}"
        ),
    };
    Ok(Value::bool(terminal))
}

/// Create a counting semaphore admitting at most `max` concurrent holders.
///
/// Non-positive maxima bail. The word names a shared backend: every clone
/// observes the same count, and admission runs through
/// `SEMAPHORE_TRY_ACQUIRE`, never through the `SEMAPHORE_AVAILABLE`
/// readout.
///
#[oxdock_func(returns = TypeTag::Semaphore)]
#[example(
    name = "mint",
    code = r###"
    # Mint a semaphore admitting at most two holders.
    IMPORT [STD]
    LET $sem: SEMAPHORE = SEMAPHORE_NEW(2)

    # A fresh semaphore reports its full cap as available.
    LET $free: INT = SEMAPHORE_AVAILABLE($sem)
    ASSERT_EQ $free 2
    "###
)]
fn semaphore_new<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Maximum concurrent holders; must be positive.
    max: i64,
) -> Result<Value> {
    let _ = cx;
    if max <= 0 {
        return Err(anyhow::anyhow!(
            "SEMAPHORE_NEW() requires a positive max, got {max}"
        ));
    }
    Ok(Value::semaphore(max as usize))
}

/// Attempt one non-blocking acquire, always answering a MAP.
///
/// `held` is `true` with the permit under the `permit` key, or `false`
/// with no `permit` key: branch on `$m.held` directly. The DSL has no
/// null, so the absent key is the miss shape. Do not read `$m.permit`
/// unless `held`: missing-key access bails strictly.
/// Never waits, so no wait can wedge.
///
#[oxdock_func(returns = TypeTag::Record(std::sync::Arc::from([Field {
    name: "held".to_string(),
    ty: TypeTag::Bool,
    docs: "`true` with the permit under `permit`, `false` with no `permit` key.",
    optional: false,
}, Field {
    name: "permit".to_string(),
    ty: TypeTag::Permit,
    docs: "Permit handle; read only when `held`.",
    optional: true,
}])))]
#[example(
    name = "acquire",
    code = r###"
    # First acquire holds with a permit to bind.
    IMPORT [STD]
    LET $sem: SEMAPHORE = SEMAPHORE_NEW(1)
    LET $first: MAP<held: BOOL, permit?: PERMIT> = SEMAPHORE_TRY_ACQUIRE($sem)
    ASSERT_EQ $first.held true
    LET $permit: PERMIT = $first.permit

    # A capped semaphore answers held false with no permit key.
    LET $second: MAP<held: BOOL, permit?: PERMIT> = SEMAPHORE_TRY_ACQUIRE($sem)
    ASSERT_EQ $second.held false
    LET $has_permit: BOOL = HAS_KEY($second, "permit")
    ASSERT_EQ $has_permit false
    "###
)]
fn semaphore_try_acquire<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Semaphore handle from `SEMAPHORE_NEW`.
    sem: Arc<SemaphoreState>,
) -> Result<Value> {
    let _ = cx;
    let mut map = BTreeMap::new();
    if sem.try_acquire() {
        map.insert("held".to_string(), Value::bool(true));
        map.insert("permit".to_string(), Value::permit(&sem));
    } else {
        map.insert("held".to_string(), Value::bool(false));
    }
    Ok(Value::map(map))
}

/// Read free permits under the lock, with no mutation.
///
/// Observability only (audit lines, healthchecks: `active = max - free`).
/// Exact at read time and stale the instant the caller acts on it, so it
/// must never drive admission: that is `SEMAPHORE_TRY_ACQUIRE`'s job.
///
#[oxdock_func(pure, returns = TypeTag::Int)]
#[example(
    name = "readout",
    code = r###"
    # Hold one permit on a cap of two.
    IMPORT [STD]
    LET $sem: SEMAPHORE = SEMAPHORE_NEW(2)
    LET $acq: MAP<held: BOOL, permit?: PERMIT> = SEMAPHORE_TRY_ACQUIRE($sem)
    ASSERT_EQ $acq.held true

    # One held permit leaves exactly one free.
    LET $free: INT = SEMAPHORE_AVAILABLE($sem)
    ASSERT_EQ $free 1
    "###
)]
fn semaphore_available(
    /// Semaphore handle from `SEMAPHORE_NEW`.
    sem: Arc<SemaphoreState>,
) -> Result<Value> {
    Ok(Value::int(sem.available() as i64))
}

/// Trim Unicode whitespace from both ends of text.
///
/// Accepts `STRING` or `PIPE`; the return mirrors the input kind.
/// Pipes stream on a pump thread with O(chunk) memory: leading
/// whitespace falls at the stream head while trailing whitespace
/// holds in the spill substrate until the next segment proves it
/// interior, so unbounded streams never buffer unbounded memory.
///
#[oxdock_func(
    rpn,
    returns = TypeTag::union(vec![TypeTag::String, TypeTag::Pipe]),
    template = text_trim_string
)]
#[example(
    name = "trim",
    code = r###"
    # Leading and trailing whitespace falls away.
    IMPORT [STD]
    LET $clean: STRING = TEXT_TRIM("  hello  ")
    ASSERT_EQ $clean "hello"
    "###
)]
#[example(
    name = "trim pipe mirrors",
    code = r###"
    # Pipe input mints pipe output with the same transform.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "  hello  "
    }
    LET $out: PIPE = TEXT_TRIM($cap)
    WITH_IO [stdin=$out] READ_LINE $line
    ASSERT_EQ $line "hello"
    "###
)]
#[example(
    name = "trim template",
    code = r###"
    # String inputs run inside placeholders too.
    IMPORT [STD]
    LET $msg: STRING = 'clean:{{ STD::TEXT_TRIM("  hello  ") }}'
    ASSERT_EQ $msg "clean:hello"
    "###
)]
#[example(
    name = "trim file template",
    code = r###"
    # EXPAND templates resolve the same registry.
    IMPORT [STD]
    WRITE "tpl.txt" "clean:\{{ STD::TEXT_TRIM(\"  hello  \") }}"
    LET $out: STRING = EXPAND "tpl.txt"
    ASSERT_EQ $out "clean:hello"
    "###
)]
fn text_trim<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Text to trim.
    #[union(String, Pipe)]
    text: Value,
) -> Result<Value> {
    super::args::mirror_stream(cx, "TEXT_TRIM", &text, super::args::TextPump::Trim)
}

/// Template projection of `TEXT_TRIM`: the same trim for `{{ }}`
/// placeholders, which carry no step IO for pipes.
fn text_trim_string(text: Value) -> Result<Value> {
    super::args::mirror_string("TEXT_TRIM", &text, super::args::TextPump::Trim)
}

/// Strip one leading prefix, returning the input unchanged when absent.
///
/// Accepts `STRING` or `PIPE`; the return mirrors the input kind.
/// Pipes decide on the first `prefix.len()` bytes and stream the rest
/// untouched. Matching is case-sensitive; there is no
/// case-insensitive mode.
/// Used for tag prefix strips such as `v1.2.3` to `1.2.3`.
///
#[oxdock_func(
    rpn,
    returns = TypeTag::union(vec![TypeTag::String, TypeTag::Pipe]),
    template = text_trim_prefix_string
)]
#[example(
    name = "strip prefix",
    code = r###"
    # A present prefix falls away; absence returns the input unchanged.
    IMPORT [STD]
    LET $bare: STRING = TEXT_TRIM_PREFIX("v1.2.3", "v")
    ASSERT_EQ $bare "1.2.3"

    # Missing prefixes pass through untouched.
    LET $same: STRING = TEXT_TRIM_PREFIX("1.2.3", "v")
    ASSERT_EQ $same "1.2.3"
    "###
)]
#[example(
    name = "strip prefix pipe mirrors",
    code = r###"
    # Pipe input mints pipe output with the same strip.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "v1.2.3"
    }
    LET $trimmed: PIPE = TEXT_TRIM($cap)
    LET $out: PIPE = TEXT_TRIM_PREFIX($trimmed, "v")
    WITH_IO [stdin=$out] READ_LINE $line
    ASSERT_EQ $line "1.2.3"
    "###
)]
#[example(
    name = "strip prefix template",
    code = r###"
    # String inputs run inside placeholders too.
    IMPORT [STD]
    LET $msg: STRING = 'bare:{{ STD::TEXT_TRIM_PREFIX("v1.2.3", "v") }}'
    ASSERT_EQ $msg "bare:1.2.3"
    "###
)]
fn text_trim_prefix<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Text to strip.
    #[union(String, Pipe)]
    text: Value,
    /// Leading prefix to remove once.
    prefix: String,
) -> Result<Value> {
    super::args::mirror_stream(
        cx,
        "TEXT_TRIM_PREFIX",
        &text,
        super::args::TextPump::TrimPrefix(prefix),
    )
}

/// Template projection of `TEXT_TRIM_PREFIX`: the same strip for
/// `{{ }}` placeholders, which carry no step IO for pipes.
fn text_trim_prefix_string(text: Value, prefix: String) -> Result<Value> {
    super::args::mirror_string(
        "TEXT_TRIM_PREFIX",
        &text,
        super::args::TextPump::TrimPrefix(prefix),
    )
}

/// Strip one trailing suffix, returning the input unchanged when absent.
///
/// Accepts `STRING` or `PIPE`; the return mirrors the input kind.
/// Pipes hold a `suffix.len()` trailing window and stream the rest.
/// Matching is case-sensitive; there is no case-insensitive mode.
///
#[oxdock_func(
    rpn,
    returns = TypeTag::union(vec![TypeTag::String, TypeTag::Pipe]),
    template = text_trim_suffix_string
)]
#[example(
    name = "strip suffix",
    code = r###"
    # A present suffix falls away; absence returns the input unchanged.
    IMPORT [STD]
    LET $bare: STRING = TEXT_TRIM_SUFFIX("archive.tar.gz", ".gz")
    ASSERT_EQ $bare "archive.tar"

    # Missing suffixes pass through untouched.
    LET $same: STRING = TEXT_TRIM_SUFFIX("archive.tar", ".gz")
    ASSERT_EQ $same "archive.tar"
    "###
)]
#[example(
    name = "strip suffix pipe mirrors",
    code = r###"
    # Pipe input mints pipe output with the same strip.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "archive.tar.gz"
    }
    LET $trimmed: PIPE = TEXT_TRIM($cap)
    LET $out: PIPE = TEXT_TRIM_SUFFIX($trimmed, ".gz")
    WITH_IO [stdin=$out] READ_LINE $line
    ASSERT_EQ $line "archive.tar"
    "###
)]
#[example(
    name = "strip suffix template",
    code = r###"
    # String inputs run inside placeholders too.
    IMPORT [STD]
    LET $msg: STRING = 'bare:{{ STD::TEXT_TRIM_SUFFIX("archive.tar.gz", ".gz") }}'
    ASSERT_EQ $msg "bare:archive.tar"
    "###
)]
fn text_trim_suffix<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Text to strip.
    #[union(String, Pipe)]
    text: Value,
    /// Trailing suffix to remove once.
    suffix: String,
) -> Result<Value> {
    super::args::mirror_stream(
        cx,
        "TEXT_TRIM_SUFFIX",
        &text,
        super::args::TextPump::TrimSuffix(suffix),
    )
}

/// Template projection of `TEXT_TRIM_SUFFIX`: the same strip for
/// `{{ }}` placeholders, which carry no step IO for pipes.
fn text_trim_suffix_string(text: Value, suffix: String) -> Result<Value> {
    super::args::mirror_string(
        "TEXT_TRIM_SUFFIX",
        &text,
        super::args::TextPump::TrimSuffix(suffix),
    )
}

/// Uppercase text with full Unicode mapping.
///
/// Accepts `STRING` or `PIPE`; the return mirrors the input kind.
/// Pipes map chunk by chunk across a char-boundary carry, so
/// multibyte characters split across reads still fold correctly.
///
#[oxdock_func(
    rpn,
    returns = TypeTag::union(vec![TypeTag::String, TypeTag::Pipe]),
    template = text_to_upper_string
)]
#[example(
    name = "uppercase",
    code = r###"
    # Text folds to uppercase.
    IMPORT [STD]
    LET $loud: STRING = TEXT_TO_UPPER("hello")
    ASSERT_EQ $loud "HELLO"
    "###
)]
#[example(
    name = "uppercase pipe mirrors",
    code = r###"
    # Pipe input mints pipe output with the same fold.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "hello"
    }
    LET $out: PIPE = TEXT_TO_UPPER($cap)
    WITH_IO [stdin=$out] READ_LINE $line
    ASSERT_EQ $line "HELLO"
    "###
)]
#[example(
    name = "uppercase template",
    code = r###"
    # String inputs run inside placeholders too.
    IMPORT [STD]
    LET $msg: STRING = 'loud:{{ STD::TEXT_TO_UPPER("hello") }}'
    ASSERT_EQ $msg "loud:HELLO"
    "###
)]
fn text_to_upper<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Text to uppercase.
    #[union(String, Pipe)]
    text: Value,
) -> Result<Value> {
    super::args::mirror_stream(cx, "TEXT_TO_UPPER", &text, super::args::TextPump::ToUpper)
}

/// Template projection of `TEXT_TO_UPPER`: the same fold for `{{ }}`
/// placeholders, which carry no step IO for pipes.
fn text_to_upper_string(text: Value) -> Result<Value> {
    super::args::mirror_string("TEXT_TO_UPPER", &text, super::args::TextPump::ToUpper)
}

/// Lowercase text with full Unicode mapping.
///
/// Accepts `STRING` or `PIPE`; the return mirrors the input kind.
/// Pipes map chunk by chunk across a char-boundary carry, like
/// `TEXT_TO_UPPER`.
///
#[oxdock_func(
    rpn,
    returns = TypeTag::union(vec![TypeTag::String, TypeTag::Pipe]),
    template = text_to_lower_string
)]
#[example(
    name = "lowercase",
    code = r###"
    # Text folds to lowercase.
    IMPORT [STD]
    LET $quiet: STRING = TEXT_TO_LOWER("HELLO")
    ASSERT_EQ $quiet "hello"
    "###
)]
#[example(
    name = "lowercase pipe mirrors",
    code = r###"
    # Pipe input mints pipe output with the same fold.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "HELLO"
    }
    LET $out: PIPE = TEXT_TO_LOWER($cap)
    WITH_IO [stdin=$out] READ_LINE $line
    ASSERT_EQ $line "hello"
    "###
)]
#[example(
    name = "lowercase template",
    code = r###"
    # String inputs run inside placeholders too.
    IMPORT [STD]
    LET $msg: STRING = 'quiet:{{ STD::TEXT_TO_LOWER("HELLO") }}'
    ASSERT_EQ $msg "quiet:hello"
    "###
)]
fn text_to_lower<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Text to lowercase.
    #[union(String, Pipe)]
    text: Value,
) -> Result<Value> {
    super::args::mirror_stream(cx, "TEXT_TO_LOWER", &text, super::args::TextPump::ToLower)
}

/// Template projection of `TEXT_TO_LOWER`: the same fold for `{{ }}`
/// placeholders, which carry no step IO for pipes.
fn text_to_lower_string(text: Value) -> Result<Value> {
    super::args::mirror_string("TEXT_TO_LOWER", &text, super::args::TextPump::ToLower)
}

/// Replace every literal occurrence of `from` with `to`.
///
/// Accepts `STRING` or `PIPE`; the return mirrors the input kind.
/// Pipes scan through a `from.len()` sliding window, so needles split
/// across chunk reads still rewrite.
/// Literal sliding search, not a pattern: an empty `from` fails
/// closed, and regex work with captures belongs to
/// `TEXT_REPLACE_REGEX`. Matching is case-sensitive; there is no
/// case-insensitive mode.
///
#[oxdock_func(
    rpn,
    returns = TypeTag::union(vec![TypeTag::String, TypeTag::Pipe]),
    template = text_replace_string
)]
#[example(
    name = "replace",
    code = r###"
    # Every literal occurrence rewrites in place.
    IMPORT [STD]
    LET $fixed: STRING = TEXT_REPLACE("hello world", "world", "there")
    ASSERT_EQ $fixed "hello there"
    "###
)]
#[example(
    name = "replace pipe mirrors",
    code = r###"
    # Pipe input mints pipe output with the same rewrite.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "hello world"
    }
    LET $trimmed: PIPE = TEXT_TRIM($cap)
    LET $out: PIPE = TEXT_REPLACE($trimmed, "world", "there")
    WITH_IO [stdin=$out] READ_LINE $line
    ASSERT_EQ $line "hello there"
    "###
)]
#[example(
    name = "replace template",
    code = r###"
    # String inputs run inside placeholders too.
    IMPORT [STD]
    LET $msg: STRING = 'fixed:{{ STD::TEXT_REPLACE("hello world", "world", "there") }}'
    ASSERT_EQ $msg "fixed:hello there"
    "###
)]
fn text_replace<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Text to rewrite.
    #[union(String, Pipe)]
    text: Value,
    /// Literal text to search for; empty fails closed.
    from: String,
    /// Literal replacement text.
    to: String,
) -> Result<Value> {
    super::args::mirror_stream(
        cx,
        "TEXT_REPLACE",
        &text,
        super::args::TextPump::ReplaceLiteral { from, to },
    )
}

/// Template projection of `TEXT_REPLACE`: the same rewrite for `{{ }}`
/// placeholders, which carry no step IO for pipes.
fn text_replace_string(text: Value, from: String, to: String) -> Result<Value> {
    super::args::mirror_string(
        "TEXT_REPLACE",
        &text,
        super::args::TextPump::ReplaceLiteral { from, to },
    )
}

/// Replace every regex match, expanding `$1` and `$name` captures.
///
/// Accepts `STRING` or `PIPE`; the return mirrors the input kind.
/// Eager-only framing: matches span chunks, so pipe inputs stage
/// through the spill substrate instead of streaming.
/// Bad patterns fail closed naming the pattern and its position.
/// Matching is case-sensitive unless the pattern carries an inline
/// `(?i)` flag, which makes uppercase and lowercase letters match
/// each other.
/// The literal counterpart is `TEXT_REPLACE`.
///
#[oxdock_func(
    rpn,
    returns = TypeTag::union(vec![TypeTag::String, TypeTag::Pipe]),
    template = text_replace_regex_string
)]
#[example(
    name = "replace with captures",
    code = r###"
    # Capture groups expand in the replacement.
    IMPORT [STD]
    LET $swapped: STRING = TEXT_REPLACE_REGEX("2026-10-13", "(\\d+)-(\\d+)-(\\d+)", "$3/$2/$1")
    ASSERT_EQ $swapped "13/10/2026"
    "###
)]
#[example(
    name = "replace regex pipe mirrors",
    code = r###"
    # Pipe input mints pipe output with the same rewrite.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "2026-10-13"
    }
    LET $trimmed: PIPE = TEXT_TRIM($cap)
    LET $out: PIPE = TEXT_REPLACE_REGEX($trimmed, "(\\d+)-(\\d+)-(\\d+)", "$3/$2/$1")
    WITH_IO [stdin=$out] READ_LINE $line
    ASSERT_EQ $line "13/10/2026"
    "###
)]
#[example(
    name = "replace regex template",
    code = r###"
    # String inputs run inside placeholders too.
    IMPORT [STD]
    LET $msg: STRING = 'swapped:{{ STD::TEXT_REPLACE_REGEX("2026-10-13", "(\\d+)-(\\d+)-(\\d+)", "$3/$2/$1") }}'
    ASSERT_EQ $msg "swapped:13/10/2026"
    "###
)]
fn text_replace_regex<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Text to rewrite.
    #[union(String, Pipe)]
    text: Value,
    /// Regex pattern to match.
    pattern: String,
    /// Replacement with `$1` and `$name` capture expansion.
    replacement: String,
) -> Result<Value> {
    if let Some(s) = text.as_str() {
        let out =
            super::args::text_replace_regex_str(s, &pattern, &replacement, "TEXT_REPLACE_REGEX")?;
        return Ok(Value::string(out));
    }
    let content = super::args::read_pipe_eager(cx, "TEXT_REPLACE_REGEX", &text)?;
    let out = super::args::text_replace_regex_str(
        &content,
        &pattern,
        &replacement,
        "TEXT_REPLACE_REGEX",
    )?;
    super::args::write_completed_pipe(cx, "TEXT_REPLACE_REGEX", out)
}

/// Template projection of `TEXT_REPLACE_REGEX`: the same rewrite for
/// `{{ }}` placeholders, which carry no step IO for pipes.
fn text_replace_regex_string(text: Value, pattern: String, replacement: String) -> Result<Value> {
    let Some(s) = text.as_str() else {
        return Err(super::args::pipe_needs_step_io("TEXT_REPLACE_REGEX"));
    };
    let out = super::args::text_replace_regex_str(s, &pattern, &replacement, "TEXT_REPLACE_REGEX")?;
    Ok(Value::string(out))
}

/// Split text on a literal delimiter into a `LIST<STRING>`.
///
/// An empty delimiter splits into single characters. Eager-only by
/// design: splitting into a list has no line framing to stream.
/// Delimiters match case-sensitively; there is no
/// case-insensitive mode.
///
#[oxdock_func(pure, returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "split",
    code = r###"
    # Delimiters split into list items.
    IMPORT [STD]
    LET $parts: LIST<STRING> = TEXT_SPLIT("a,b,c", ",")
    ASSERT_EQ $parts.0 "a"
    ASSERT_EQ $parts.1 "b"
    ASSERT_EQ $parts.2 "c"
    "###
)]
fn text_split(
    /// Text to split.
    text: String,
    /// Literal delimiter; empty splits into characters.
    delimiter: String,
) -> Result<Value> {
    Ok(super::args::text_list_value(super::args::text_split_str(
        &text, &delimiter,
    )))
}

/// Join a `LIST<STRING>` with a literal separator into a `STRING`.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "join",
    code = r###"
    # List items join with the separator between them.
    IMPORT [STD]
    LET $joined: STRING = TEXT_JOIN(["a", "b", "c"], ",")
    ASSERT_EQ $joined "a,b,c"
    "###
)]
fn text_join(
    /// List elements to join.
    elements: Vec<String>,
    /// Literal delimiter placed between elements.
    delimiter: String,
) -> Result<Value> {
    Ok(Value::string(elements.join(delimiter.as_str())))
}

/// Split text into lines with universal newline handling.
///
/// Normalizes through the line-ending crate first so LF, CRLF, CR,
/// and mixed endings all split, then delegates the split itself to
/// `LineEnding::split` instead of hand-rolled scanning.
/// Returns a `LIST<STRING>`; eager-only like `TEXT_SPLIT`.
///
#[oxdock_func(pure, returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "lines",
    code = r###"
    # Newlines split into line items.
    IMPORT [STD]
    LET $lines: LIST<STRING> = TEXT_LINES("one\ntwo\nthree")
    ASSERT_EQ $lines.0 "one"
    ASSERT_EQ $lines.1 "two"
    ASSERT_EQ $lines.2 "three"

    # Windows endings split the same way after normalization.
    LET $crlf: LIST<STRING> = TEXT_LINES("one\r\ntwo\r\nthree")
    ASSERT_EQ $crlf.0 "one"
    ASSERT_EQ $crlf.1 "two"
    ASSERT_EQ $crlf.2 "three"

    # Old Mac endings split too, and mixed endings split together because the input normalizes to LF before splitting.
    LET $mixed: LIST<STRING> = TEXT_LINES("a\r\nb\nc\rd")
    ASSERT_EQ $mixed.0 "a"
    ASSERT_EQ $mixed.1 "b"
    ASSERT_EQ $mixed.2 "c"
    ASSERT_EQ $mixed.3 "d"
    "###
)]
fn text_lines(
    /// Text to split into lines.
    text: String,
) -> Result<Value> {
    Ok(super::args::text_list_value(super::args::text_lines_str(
        &text,
    )))
}

/// Split text into whitespace-separated fields, ignoring empties.
///
/// Returns a `LIST<STRING>`; the checksum-line parser that replaces
/// `cut -d' ' -f1` in CI legs.
///
#[oxdock_func(pure, returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "fields",
    code = r###"
    # Runs of whitespace separate fields; empties never appear.
    IMPORT [STD]
    LET $fields: LIST<STRING> = TEXT_FIELDS("  abc  def ")
    ASSERT_EQ $fields.0 "abc"
    ASSERT_EQ $fields.1 "def"
    "###
)]
fn text_fields(
    /// Text to split into fields.
    text: String,
) -> Result<Value> {
    Ok(super::args::text_list_value(super::args::text_fields_str(
        &text,
    )))
}

/// Read lines `from` through `to` as a `LIST<STRING>`: the `sed -n
/// 'A,Bp'` window over universal newlines, for reading one file
/// section without holding the surrounding text.
///
/// Lines are 1-based and inclusive on both ends. Ranges past the end
/// clamp like sed instead of failing; a start before line 1 or past
/// `to` fails closed, since no line reading satisfies it.
///
#[oxdock_func(pure, returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "line range",
    code = r###"
    # Lines 2 through 3 come back as a list, 1-based inclusive.
    IMPORT [STD]
    LET $window: LIST<STRING> = TEXT_LINES_RANGE("one\ntwo\nthree\nfour", 2, 3)
    ASSERT_EQ $window.0 "two"
    ASSERT_EQ $window.1 "three"

    # Ranges past the end clamp instead of failing.
    LET $tail: LIST<STRING> = TEXT_LINES_RANGE("one\ntwo", 2, 9)
    ASSERT_EQ $tail.0 "two"
    "###
)]
fn text_lines_range(
    /// Text whose lines to window.
    text: String,
    /// First line, 1-based inclusive.
    from: i64,
    /// Last line, inclusive; past the end clamps.
    to: i64,
) -> Result<Value> {
    Ok(super::args::text_list_value(
        super::args::text_lines_range_str(&text, from, to, "TEXT_LINES_RANGE")?,
    ))
}

/// Report whether text matches a regex pattern.
///
/// Returns `BOOL`. Bad patterns fail closed naming the pattern and
/// its position. Matching is case-sensitive unless the pattern
/// carries an inline `(?i)` flag, which makes uppercase and lowercase
/// letters match each other.
///
#[oxdock_func(pure, returns = TypeTag::Bool)]
#[example(
    name = "match",
    code = r###"
    # Regex matches report true; misses report false.
    IMPORT [STD]
    LET $hit: BOOL = TEXT_MATCH("abc123", "\\d+")
    ASSERT_EQ $hit true

    # A miss is false, not a failure.
    LET $miss: BOOL = TEXT_MATCH("abc", "^\\d+$")
    ASSERT_EQ $miss false
    "###
)]
fn text_match(
    /// Text to test.
    text: String,
    /// Regex pattern to match.
    pattern: String,
) -> Result<Value> {
    let re = super::args::text_compile_pattern("TEXT_MATCH", &pattern)?;
    Ok(Value::bool(re.is_match(&text)))
}

/// Extract capture groups for the first regex match.
///
/// Returns a `LIST<STRING>` where index 0 is the full match and the
/// rest are groups in order; non-participating groups bind empty so
/// indexes stay stable. No match binds an empty list. Matching is
/// case-sensitive unless the pattern carries an inline `(?i)` flag,
/// which makes uppercase and lowercase letters match each other.
///
#[oxdock_func(pure, returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "extract",
    code = r###"
    # Index zero is the full match; groups follow in order.
    IMPORT [STD]
    LET $caps: LIST<STRING> = TEXT_EXTRACT("abc123", "([a-z]+)(\\d+)")
    ASSERT_EQ $caps.0 "abc123"
    ASSERT_EQ $caps.1 "abc"
    ASSERT_EQ $caps.2 "123"
    "###
)]
fn text_extract(
    /// Text to search.
    text: String,
    /// Regex pattern with capture groups.
    pattern: String,
) -> Result<Value> {
    let groups = super::args::text_extract_str(&text, &pattern, "TEXT_EXTRACT")?;
    Ok(super::args::text_list_value(groups))
}

/// Report whether text starts with a literal prefix.
///
/// Returns `BOOL`; the literal check behind tag and target branching
/// such as `matrix.target` suffix and prefix tests. Matching is
/// case-sensitive; there is no case-insensitive mode.
///
#[oxdock_func(pure, returns = TypeTag::Bool)]
#[example(
    name = "starts with",
    code = r###"
    # Present prefixes report true; misses report false.
    IMPORT [STD]
    LET $hit: BOOL = TEXT_STARTS_WITH("linux-musl-x64", "linux")
    ASSERT_EQ $hit true

    # A miss is false, not a failure.
    LET $miss: BOOL = TEXT_STARTS_WITH("macos-arm", "linux")
    ASSERT_EQ $miss false
    "###
)]
fn text_starts_with(
    /// Text to test.
    text: String,
    /// Literal prefix to look for.
    prefix: String,
) -> Result<Value> {
    Ok(Value::bool(text.starts_with(prefix.as_str())))
}

/// Report whether text ends with a literal suffix.
///
/// Returns `BOOL`; the `endsWith(matrix.target, 'linux-musl')`
/// check in CI legs. Matching is case-sensitive; there is no
/// case-insensitive mode.
///
#[oxdock_func(pure, returns = TypeTag::Bool)]
#[example(
    name = "ends with",
    code = r###"
    # Present suffixes report true; misses report false.
    IMPORT [STD]
    LET $hit: BOOL = TEXT_ENDS_WITH("x86_64-linux-musl", "linux-musl")
    ASSERT_EQ $hit true

    # A miss is false, not a failure.
    LET $miss: BOOL = TEXT_ENDS_WITH("x86_64-linux-gnu", "linux-musl")
    ASSERT_EQ $miss false
    "###
)]
fn text_ends_with(
    /// Text to test.
    text: String,
    /// Literal suffix to look for.
    suffix: String,
) -> Result<Value> {
    Ok(Value::bool(text.ends_with(suffix.as_str())))
}

/// Report whether text holds a literal substring.
///
/// Returns `BOOL`; the substring search behind target filtering
/// alongside the prefix and suffix tests. Matching is
/// case-sensitive; there is no case-insensitive mode.
///
#[oxdock_func(pure, returns = TypeTag::Bool)]
#[example(
    name = "contains",
    code = r###"
    # Present needles report true; misses report false.
    IMPORT [STD]
    LET $hit: BOOL = TEXT_CONTAINS("x86_64-linux-musl", "musl")
    ASSERT_EQ $hit true

    # A miss is false, not a failure.
    LET $miss: BOOL = TEXT_CONTAINS("x86_64-linux-gnu", "musl")
    ASSERT_EQ $miss false
    "###
)]
fn text_contains(
    /// Text to search.
    text: String,
    /// Literal substring to look for.
    needle: String,
) -> Result<Value> {
    Ok(Value::bool(text.contains(needle.as_str())))
}

/// Count the characters in text as Unicode scalars.
///
/// Multibyte characters count once each, so this is not the byte
/// length: `"héllo"` binds 5, not 6. The `${#var}` and `wc -m`
/// counterpart for scripts that size tags and digests.
///
#[oxdock_func(pure, returns = TypeTag::Int)]
#[example(
    name = "length",
    code = r###"
    # Plain text counts one per character.
    IMPORT [STD]
    LET $n: INT = TEXT_LENGTH("hello")
    ASSERT_EQ $n 5

    # Multibyte characters count once each, not per byte.
    LET $wide: INT = TEXT_LENGTH("héllo")
    ASSERT_EQ $wide 5
    "###
)]
fn text_length(
    /// Text to measure.
    text: String,
) -> Result<Value> {
    Ok(Value::int(text.chars().count() as i64))
}

/// Count non-overlapping literal occurrences of a needle.
///
/// Matches never share bytes, so `"aaa"` holds one `"aa"`, matching
/// `grep -o 'aa' | wc -l`. An empty needle fails closed instead of
/// binding every character boundary. Matching is case-sensitive;
/// there is no case-insensitive mode.
///
#[oxdock_func(pure, returns = TypeTag::Int)]
#[example(
    name = "count",
    code = r###"
    # Every non-overlapping occurrence counts once.
    IMPORT [STD]
    LET $n: INT = TEXT_COUNT("hello world", "o")
    ASSERT_EQ $n 2

    # Overlapping candidates never double count.
    LET $once: INT = TEXT_COUNT("aaa", "aa")
    ASSERT_EQ $once 1
    "###
)]
fn text_count(
    /// Text to search.
    text: String,
    /// Literal needle to count; empty fails closed.
    needle: String,
) -> Result<Value> {
    let n = super::args::text_count_str(&text, &needle, "TEXT_COUNT")?;
    Ok(Value::int(n))
}

/// Count lines matching a regex pattern.
///
/// Lines split with universal newline handling, like `TEXT_LINES`,
/// and each line reports independently: this is `grep -c`, so a line
/// with two matches still counts once. Bad patterns fail closed
/// naming the pattern and its position. Matching is case-sensitive
/// unless the pattern carries an inline `(?i)` flag, which makes
/// uppercase and lowercase letters match each other.
///
#[oxdock_func(pure, returns = TypeTag::Int)]
#[example(
    name = "count matches",
    code = r###"
    # One line matches once no matter how many times it hits.
    IMPORT [STD]
    LET $n: INT = TEXT_COUNT_MATCHES("test a: test\ntest b: nope\ntest c: test", ": test")
    ASSERT_EQ $n 2

    # A miss binds zero, never an error.
    LET $none: INT = TEXT_COUNT_MATCHES("test b: nope", ": test$")
    ASSERT_EQ $none 0
    "###
)]
fn text_count_matches(
    /// Text whose lines to test.
    text: String,
    /// Regex pattern tested per line.
    pattern: String,
) -> Result<Value> {
    let n = super::args::text_count_matches_str(&text, &pattern, "TEXT_COUNT_MATCHES")?;
    Ok(Value::int(n))
}

/// Keep the lines matching a regex pattern.
///
/// Splits with universal newline handling, like `TEXT_LINES`, and
/// returns every matching line in order: plain `grep`, so a miss
/// binds an empty list instead of failing. A trailing options MAP
/// carries `invert`: `TEXT_GREP($text, $pattern, {invert: true})`
/// keeps every non-matching line instead, the `grep -v` shape. The
/// MAP omits freely; a missing MAP keeps matches. Matching is
/// case-sensitive unless the pattern carries an inline `(?i)` flag,
/// which makes uppercase and lowercase letters match each other. Bad
/// patterns fail closed naming the pattern and its position.
///
#[oxdock_func(pure, returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "grep",
    code = r###"
    # Matching lines come back as a list, in order.
    IMPORT [STD]
    LET $hits: LIST<STRING> = TEXT_GREP("one\ntwo\none more", "^one")
    ASSERT_EQ $hits.0 "one"
    ASSERT_EQ $hits.1 "one more"

    # A miss binds an empty list, never an error.
    LET $miss: LIST<STRING> = TEXT_GREP("one\ntwo", "^z")
    LET $joined: STRING = TEXT_JOIN($miss, ",")
    ASSERT_EQ $joined ""

    # The (?i) flag ignores case, so uppercase lines match too.
    LET $folded: LIST<STRING> = TEXT_GREP("One\ntwo\nONE more", "(?i)^one")
    ASSERT_EQ $folded.0 "One"
    ASSERT_EQ $folded.1 "ONE more"
    "###
)]
#[example(
    name = "grep invert",
    code = r###"
    # The invert option keeps every non-matching line instead.
    IMPORT [STD]
    LET $kept: LIST<STRING> = TEXT_GREP("one\ntwo\none more", "^one", {invert: true})
    ASSERT_EQ $kept.0 "two"

    # A pattern matching every line binds an empty list, never an error.
    LET $none: LIST<STRING> = TEXT_GREP("one\ntwo", "^", {invert: true})
    LET $joined: STRING = TEXT_JOIN($none, ",")
    ASSERT_EQ $joined ""

    # Options ride into placeholders as MAP literals. Placeholder
    # strings unescape only backslash and quote, so multiline text
    # arrives through a variable.
    LET $two_lines: STRING = "one\ntwo"
    LET $msg: STRING = 'kept:{{ STD::TEXT_GREP($two_lines, "^one", {invert: true}) }}'
    ASSERT_EQ $msg "kept:two"
    "###
)]
fn text_grep(
    /// Text whose lines to filter.
    text: String,
    /// Regex pattern keeping a line.
    pattern: String,
    /// Filter settings: `invert`. Omittable: a missing options MAP fills `{}`.
    #[options("invert?: BOOL")]
    #[optional]
    options: BTreeMap<String, Value>,
) -> Result<Value> {
    // `check_options` in the wrapper verified the shape above (keys
    // and value types); the read below narrows infallibly after.
    let invert = super::args::text_invert_flag(&options);
    Ok(super::args::text_list_value(super::args::text_grep_str(
        &text,
        &pattern,
        invert,
        "TEXT_GREP",
    )?))
}

/// True when a pipe sits at end of stream: closed with nothing buffered,
/// so the next `READ_LINE` would bind `""` via EOF rather than a line.
/// Live writers, pinned keepers, and buffered bytes all answer false.
/// Never blocks: a reader already blocked stays blocked, so branch on
/// `EOF` before reading, not after. OS pairs and unbound handles answer
/// best-effort (kernel bytes are invisible there; see `INSPECT`).
/// Non-pipe arguments bail at the boundary: the parameter declares
/// `PIPE`, so the extractor and the static pass reject them before
/// the body runs.
///
#[oxdock_func(returns = TypeTag::Bool)]
#[example(
    name = "drain to end",
    code = r###"
    # Capture two lines, then drain to end of stream with no sentinel line.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "one"
        ECHO "two"
    }

    # The loop exits on close, and the count proves both lines drained.
    LET $n: INT = 0
    WHILE !EOF($cap) {
        WITH_IO [stdin=$cap] READ_LINE $line
        $n = $n + 1
    }
    ASSERT_EQ $n 2
    "###
)]
fn eof<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Pipe handle to query for end of stream.
    pipe: PipeHandle,
) -> Result<Value> {
    let info = cx.state.io.inspect_pipe(&pipe);
    Ok(Value::bool(info.closed && info.buffered == 0))
}

fn meta_to_value(meta: &FuncMeta) -> Value {
    let mut map = BTreeMap::new();
    map.insert("name".to_string(), Value::string(meta.name.clone()));
    map.insert("module".to_string(), Value::string(meta.module.clone()));
    map.insert(
        "kind".to_string(),
        Value::string(meta.kind.label().to_string()),
    );
    let params = match &meta.params {
        Some(params) => Value::list(
            params
                .iter()
                .map(|p| {
                    let mut entry = BTreeMap::new();
                    entry.insert("name".to_string(), Value::string(p.name.clone()));
                    entry.insert(
                        "param_type".to_string(),
                        Value::string(
                            p.param_type
                                .as_ref()
                                .map(render_structural)
                                .unwrap_or_default(),
                        ),
                    );
                    entry.insert(
                        "allowed".to_string(),
                        Value::string(
                            p.allowed
                                .map(|values| values.join(", "))
                                .unwrap_or_default(),
                        ),
                    );
                    entry.insert("docs".to_string(), Value::string(String::new()));
                    entry.insert("optional".to_string(), Value::bool(p.optional));
                    entry.insert(
                        "options".to_string(),
                        match p.options {
                            Some(keys) => Value::list(
                                keys.iter()
                                    .map(|key| {
                                        let mut option = BTreeMap::new();
                                        option.insert(
                                            "name".to_string(),
                                            Value::string(key.name.to_string()),
                                        );
                                        option.insert(
                                            "value_type".to_string(),
                                            Value::string(render_structural(&key.value)),
                                        );
                                        option.insert(
                                            "required".to_string(),
                                            Value::bool(key.required),
                                        );
                                        option.insert(
                                            "default".to_string(),
                                            Value::string(
                                                key.default.unwrap_or_default().to_string(),
                                            ),
                                        );
                                        Value::map(option)
                                    })
                                    .collect(),
                            ),
                            None => Value::string(String::new()),
                        },
                    );
                    Value::map(entry)
                })
                .collect(),
        ),
        // Parameterless functions mint an empty LIST, never an empty
        // STRING: the `DESCRIBE` return tag promises `LIST<MAP<ANY>>`
        // for every entry, and the boundary enforces it.
        None => Value::list(Vec::new()),
    };
    map.insert("params".to_string(), params);
    map.insert(
        "returns".to_string(),
        Value::string(
            meta.returns
                .as_ref()
                .map(render_structural)
                .unwrap_or_default(),
        ),
    );
    map.insert("rpn".to_string(), Value::bool(meta.rpn));
    map.insert(
        "summary".to_string(),
        Value::string(meta.summary.to_string()),
    );
    Value::map(map)
}

/// One named record schema: a shape host functions produce and scripts
/// bind by name (`LET $s: SSH_SESSION_INFO`). Travels with its module
/// so schemas register exactly when their producer does.
#[derive(Debug, Clone)]
pub struct RecordSchema {
    pub name: &'static str,
    pub fields: std::sync::Arc<[Field]>,
}

/// One host library: functions and types registered under a single module
/// name. `Engine::register_module` stages these; runs expose them as
/// `MODULE::NAME` calls with `MODULE` provenance on every entry.
#[derive(Clone)]
pub struct HostModule<P: ProcessManager> {
    pub name: String,
    pub funcs: Vec<HostRegistration<P>>,
    pub types: Vec<&'static TypeDescriptor>,
    pub record_schemas: Vec<RecordSchema>,
    /// Shared module state (endpoint registries and the like), stashed
    /// at registration and reachable from bodies through
    /// `StepCtx::module_state`. Lets every entry stay `#[oxdock_func]`:
    /// the macro only generates closers-over-nothing, so state that
    /// cannot ride in a value travels here instead of a hand-built
    /// closure. Prefer `.with_state(...)` over spelling this field.
    pub states: Vec<ModuleState>,
}

/// One shared state object traveling with its module. Keyed by the
/// owning module name plus the value type at registration, so two
/// modules sharing one state type never collide.
#[derive(Clone)]
pub struct ModuleState {
    id: std::any::TypeId,
    state: std::sync::Arc<dyn std::any::Any + Send + Sync>,
}

/// A pre-run steps inspection: pure function of the parsed steps,
///
/// shared so the struct field stays under the complexity lint.
pub type PreRunInspector = dyn Fn(&[Step]) -> Result<()> + Send + Sync;

/// A pre-run check riding in a module's `states`: the runner executes
/// every staged check against the parsed steps before the first step
/// runs, so unmapped services fail fast with zero side effects. Modules
/// attach one with `.with_state(Arc::new(check))`; the runner discovers
/// them by type, so no struct field or plugin knowledge is needed here.
#[derive(Clone)]
pub struct PreRunCheck {
    check: Arc<PreRunInspector>,
}

impl PreRunCheck {
    /// Wrap a steps inspector as a pre-run check.
    pub fn new(check: impl Fn(&[Step]) -> Result<()> + Send + Sync + 'static) -> Self {
        Self {
            check: Arc::new(check),
        }
    }

    /// Run the inspection: `Ok` lets execution start.
    pub fn run(&self, steps: &[Step]) -> Result<()> {
        (self.check)(steps)
    }
}

impl<P: ProcessManager> HostModule<P> {
    /// Attach shared state for bodies to fetch with
    /// `cx.module_state::<T>("MODULE")`. Chainable, like registration.
    pub fn with_state<T: Send + Sync + 'static>(mut self, state: std::sync::Arc<T>) -> Self {
        self.states.push(ModuleState {
            id: std::any::TypeId::of::<T>(),
            state,
        });
        self
    }

    /// Run every staged [`PreRunCheck`] against already-parsed steps.
    /// The runner calls this before the first step executes; the first
    /// failing check aborts the run with no side effects.
    pub fn check_pre_run(&self, steps: &[Step]) -> Result<()> {
        for state in &self.states {
            if let Some(check) = state.state.downcast_ref::<PreRunCheck>() {
                check.run(steps)?;
            }
        }
        Ok(())
    }
}

impl<P: ProcessManager> ExecState<P> {
    /// Register one [`HostModule`]: every function becomes callable as
    /// `MODULE::NAME`, every type joins the run's name directory.
    pub fn register_module(&mut self, module: HostModule<P>) {
        for registration in module.funcs {
            match registration {
                HostRegistration::Stateful { name, meta, func } => {
                    self.functions.register_host(&module.name, name, meta, func);
                }
                HostRegistration::Pure { name, meta, func } => {
                    self.functions
                        .register_pure_host(&module.name, name, meta, func);
                }
                HostRegistration::Both {
                    name,
                    meta,
                    func,
                    template,
                } => {
                    self.functions
                        .register_both_host(&module.name, name, meta, func, template);
                }
            }
        }
        for descriptor in module.types {
            self.register_type(descriptor);
        }
        for schema in module.record_schemas {
            self.register_record_schema(schema.name, schema.fields);
        }
        for state in module.states {
            self.module_states
                .insert((module.name.clone(), state.id), state.state);
        }
    }

    /// All visible functions: natives plus hosts plus current DSL definitions.
    pub fn list_functions(&self) -> Vec<FuncMeta> {
        let mut out: Vec<FuncMeta> = self.functions.entries_metas().into_iter().collect();
        // TODO: Make a "virtual function" and don't hardcode
        if !out.iter().any(|m| m.name == KEYWORD_INSPECT) {
            out.push(FuncMeta {
                name: KEYWORD_INSPECT.to_string(),
                module: STD_MODULE_NAME.to_string(),
                kind: FuncKind::HostCtx,
                params: None,
                returns: Some(TypeTag::Map),
                rpn: false,
                summary: "Inspect a variable binding.",
                docs: "INSPECT($var): dedicated AST node taking a variable, not a value.",
                examples: &[],
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Describe one function by name, or `None` when unknown.
    pub fn describe_function(&self, name: &str) -> Option<Value> {
        if let Some(meta) = self.functions.meta(name) {
            return Some(meta_to_value(&meta));
        }
        // TODO: Make a "virtual function" and don't hardcode
        if name == KEYWORD_INSPECT {
            return Some(meta_to_value(&FuncMeta {
                name: KEYWORD_INSPECT.to_string(),
                module: STD_MODULE_NAME.to_string(),
                kind: FuncKind::HostCtx,
                params: None,
                returns: Some(TypeTag::Map),
                rpn: false,
                summary: "Inspect a variable binding.",
                docs: "INSPECT($var): dedicated AST node taking a variable, not a value.",
                examples: &[],
            }));
        }
        None
    }

    pub(super) fn clone_native_pure(&self, name: &str) -> Option<PureFn> {
        self.functions.clone_pure_fn(name)
    }

    /// Shared snapshot of every registered pure function for
    /// placeholder and host driven expansion. Public so host crates
    /// resolve calls through the live registry instead of hardcoding
    /// module or function names. Cloning the `Arc` never copies the map.
    pub fn pure_function_table(&self) -> Arc<PureTable> {
        self.functions.pure_shared()
    }

    pub(super) fn clone_native_ctx(&self, name: &str) -> Option<NativeFn<P>> {
        self.functions.clone_ctx_fn(name)
    }

    pub(super) fn native_meta(&self, name: &str) -> Option<FuncMeta> {
        self.functions.meta(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_entry_renders_union_spellings() {
        // Union params and returns render through the same structural
        // renderer every other tag uses: `DESCRIBE` (and the generated
        // reference built on it) names the admission set, never a
        // bare word or an empty string. Built by macro, so the
        // spelling cannot drift from the members.
        let union = oxdock_parser::union_tag!(String, Pipe);
        let meta = FuncMeta {
            name: "T::F".to_string(),
            module: "T".to_string(),
            kind: FuncKind::HostPure,
            params: Some(vec![FuncParam {
                name: "text".to_string(),
                param_type: Some(union.clone()),
                allowed: None,
                docs: "",
                options: None,
                optional: false,
            }]),
            returns: Some(union),
            rpn: false,
            summary: "test-only",
            docs: "test-only",
            examples: &[],
        };
        let value = meta_to_value(&meta);
        let map = value.as_map().expect("describe value is a map");
        let params = map
            .get("params")
            .expect("params key")
            .as_list()
            .expect("params is a list");
        let first = params[0].as_map().expect("entry is a map");
        assert_eq!(
            first.get("param_type").and_then(Value::as_str),
            Some("STRING|PIPE"),
            "param renders the set"
        );
        assert_eq!(
            map.get("returns").and_then(Value::as_str),
            Some("STRING|PIPE"),
            "returns render the set"
        );
    }

    #[test]
    fn describe_entry_carries_optional_flag() {
        // The `optional` key on params entries is the machine-readable
        // half of the `?` rendering: pin both values through the same
        // builder the runtime serves, not through prose.
        let meta = FuncMeta {
            name: "T::F".to_string(),
            module: "T".to_string(),
            kind: FuncKind::HostPure,
            params: Some(vec![
                FuncParam {
                    name: "url".to_string(),
                    param_type: Some(TypeTag::String),
                    allowed: None,
                    docs: "",
                    options: None,
                    optional: false,
                },
                FuncParam {
                    name: "options".to_string(),
                    param_type: Some(TypeTag::Map),
                    allowed: None,
                    docs: "",
                    options: None,
                    optional: true,
                },
            ]),
            returns: Some(TypeTag::String),
            rpn: false,
            summary: "test-only",
            docs: "test-only",
            examples: &[],
        };
        let value = meta_to_value(&meta);
        let map = value.as_map().expect("describe value is a map");
        let params = map
            .get("params")
            .expect("params key")
            .as_list()
            .expect("params is a list");
        assert_eq!(params.len(), 2);
        let first = params[0].as_map().expect("entry is a map");
        let second = params[1].as_map().expect("entry is a map");
        assert_eq!(
            first.get("optional").and_then(Value::as_bool),
            Some(false),
            "required param must read false"
        );
        assert_eq!(
            second.get("optional").and_then(Value::as_bool),
            Some(true),
            "omittable param must read true"
        );
    }

    #[test]
    fn template_twins_mirror_string_paths_and_refuse_pipes() {
        // Every template twin answers strings through the shared pump
        // semantics and refuses pipes naming the step IO they need:
        // placeholder evaluation carries no step context to pump
        // through. Table-driven so a twinless mirror op fails here
        // naming it.
        let pipe = Value::pipe_fresh();
        type TwinCase = (&'static str, Box<dyn Fn() -> Result<Value>>);
        let cases: Vec<TwinCase> = vec![
            (
                "TEXT_TRIM",
                Box::new(|| text_trim_string(Value::string("  hello  ".to_string()))),
            ),
            (
                "TEXT_TRIM_PREFIX",
                Box::new(|| {
                    text_trim_prefix_string(Value::string("v1".to_string()), "v".to_string())
                }),
            ),
            (
                "TEXT_TRIM_SUFFIX",
                Box::new(|| {
                    text_trim_suffix_string(Value::string("a.gz".to_string()), "x".to_string())
                }),
            ),
            (
                "TEXT_TO_UPPER",
                Box::new(|| text_to_upper_string(Value::string("hi".to_string()))),
            ),
            (
                "TEXT_TO_LOWER",
                Box::new(|| text_to_lower_string(Value::string("HI".to_string()))),
            ),
            (
                "TEXT_REPLACE",
                Box::new(|| {
                    text_replace_string(
                        Value::string("ab".to_string()),
                        "b".to_string(),
                        "c".to_string(),
                    )
                }),
            ),
            (
                "TEXT_REPLACE_REGEX",
                Box::new(|| {
                    text_replace_regex_string(
                        Value::string("ab".to_string()),
                        "b".to_string(),
                        "c".to_string(),
                    )
                }),
            ),
        ];
        assert_eq!(cases.len(), 7, "every mirror op needs a twin row");
        for (name, run) in &cases {
            let value = run().unwrap_or_else(|e| panic!("{name} twin must answer strings: {e:#}"));
            assert!(
                value.as_str().is_some(),
                "{name} twin must answer STRING, got {}",
                value.type_name()
            );
        }
        assert_eq!(
            text_trim_string(Value::string("  hello  ".to_string()))
                .expect("trim twin")
                .as_str(),
            Some("hello")
        );
        // Pipes refuse through the shared message on every twin.
        let pipe_cases: Vec<(&str, Result<Value>)> = vec![
            ("TEXT_TRIM", text_trim_string(pipe.clone())),
            (
                "TEXT_TRIM_PREFIX",
                text_trim_prefix_string(pipe.clone(), "v".to_string()),
            ),
            (
                "TEXT_TRIM_SUFFIX",
                text_trim_suffix_string(pipe.clone(), ".gz".to_string()),
            ),
            ("TEXT_TO_UPPER", text_to_upper_string(pipe.clone())),
            ("TEXT_TO_LOWER", text_to_lower_string(pipe.clone())),
            (
                "TEXT_REPLACE",
                text_replace_string(pipe.clone(), "a".to_string(), "b".to_string()),
            ),
            (
                "TEXT_REPLACE_REGEX",
                text_replace_regex_string(pipe.clone(), "a".to_string(), "b".to_string()),
            ),
        ];
        for (name, result) in pipe_cases {
            let err = result.expect_err(&format!("{name} twin must refuse PIPE"));
            assert!(
                format!("{err:#}").contains("step IO"),
                "{name} refusal names the missing context: {err:#}"
            );
        }
    }
}
