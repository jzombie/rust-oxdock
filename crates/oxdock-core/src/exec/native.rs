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
#[derive(Debug, Clone)]
pub struct FuncParam {
    pub name: String,
    pub param_type: Option<TypeTag>,
    pub allowed: Option<&'static [&'static str]>,
    pub docs: &'static str,
    pub options: Option<&'static [ParamOption]>,
}

/// One known key of a `MAP` options parameter: name, value type,
/// whether callers must pass it, the rendered default for optional
/// keys, and what it controls. Declared once per options parameter
/// (macro `#[options(...)]` plus the `# Options` doc section, or
/// inline for hand-built entries) and rendered into signatures,
/// `DESCRIBE` output, and generated references alike.
#[derive(Debug, Clone, Copy)]
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
/// on the AST path with full step context.
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
/// pure scalar function, or a stateful function with step context.
pub(super) enum FuncBody<P: ProcessManager> {
    Script(FuncDefData),
    Pure(PureFn),
    Ctx(NativeFn<P>),
}

// Manual `Clone` (a derive would demand `P: Clone`).
impl<P: ProcessManager> Clone for FuncBody<P> {
    fn clone(&self) -> Self {
        match self {
            FuncBody::Script(def) => FuncBody::Script(def.clone()),
            FuncBody::Pure(func) => FuncBody::Pure(Arc::clone(func)),
            FuncBody::Ctx(func) => FuncBody::Ctx(Arc::clone(func)),
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
}

impl<P: ProcessManager> FunctionRegistry<P> {
    pub(super) fn with_builtins() -> Self {
        let mut reg = Self {
            entries: HashMap::new(),
            pure_shared: Arc::new(HashMap::new()),
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
            Types::registration(),
            TypeDescribe::registration(),
            Glob::registration(),
            LoadToml::registration(),
            LoadJson::registration(),
            ParseToml::registration(),
            ParseJson::registration(),
            PathType::registration(),
            HasKey::registration(),
            MapSet::registration(),
            MergeMaps::registration(),
            ToJson::registration(),
            TypeOf::registration(),
            Functions::registration(),
            Describe::registration(),
            IsTerminal::registration(),
            Eof::registration(),
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
    fn rebuild_pure_shared(&mut self) {
        let mut table: PureTable = HashMap::new();
        for (name, entry) in &self.entries {
            let FuncBody::Pure(ref func) = entry.body else {
                continue;
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
                                param_type: Some(*param_type),
                                allowed: None,
                                // Script `FUNC` params carry no prose:
                                // declarations have no doc syntax.
                                docs: "",
                                options: None,
                            })
                            .collect(),
                    ),
                    returns: None,
                    rpn: false,
                    summary: "DSL-defined function.",
                    docs: "Defined via FUNC in script.",
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
    fn clone_pure_fn(&self, name: &str) -> Option<PureFn> {
        match self.entries.get(name)?.body {
            FuncBody::Pure(ref func) => Some(Arc::clone(func)),
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
    fn clone_ctx_fn(&self, name: &str) -> Option<NativeFn<P>> {
        match self.entries.get(name)?.body {
            FuncBody::Ctx(ref func) => Some(Arc::clone(func)),
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
}

impl<P: ProcessManager> Clone for FunctionRegistry<P> {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            scopes: self.scopes.clone(),
            pure_shared: Arc::clone(&self.pure_shared),
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
/// when integral and finite.
///
#[oxdock_func(pure, returns = TypeTag::Int)]
fn int(
    /// Value to convert to `INT`.
    val: Value,
) -> Result<Value> {
    super::args::int_from_value(val)
}

/// Convert a value to FLOAT.
///
/// Parses f64 (accepts int strings), bails on non-finite or non-numeric.
///
#[oxdock_func(pure, returns = TypeTag::Float)]
fn float(
    /// Value to convert to `FLOAT`.
    val: Value,
) -> Result<Value> {
    super::args::float_from_value(val)
}

/// List workspace paths matching a glob pattern.
///
/// Sorted, root-relative LIST; empty on no match or `..` escape.
///
#[oxdock_func(rpn, returns = TypeTag::ListOf(&TypeTag::String))]
fn glob<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Glob pattern matched against workspace paths.
    pattern: String,
) -> Result<Value> {
    super::args::glob_from_value(&[Value::string(pattern)], cx)
}

/// Load and parse a TOML file.
///
/// Reads a workspace file and parses TOML into a DSL value.
///
#[oxdock_func(rpn, returns = TypeTag::MapOf(&TypeTag::Any))]
fn load_toml<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Workspace file path to load and parse as TOML.
    path: String,
) -> Result<Value> {
    super::args::load_toml_from_value(&[Value::string(path)], cx)
}

/// Load and parse a JSON file.
///
/// Reads a workspace file and parses JSON into a DSL value. Returns
/// `ANY` by design: a top-level array or scalar parses to `LIST` or a
/// scalar word, so a `MAP` tag would lie. `LET` coercion still checks
/// the actual value at assignment.
///
#[oxdock_func(rpn, returns = TypeTag::Any)]
fn load_json<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Workspace file path to load and parse as JSON.
    path: String,
) -> Result<Value> {
    super::args::load_json_from_value(&[Value::string(path)], cx)
}

/// Parse TOML text already held in memory.
///
/// Uses the same conversion as file loading, so fetch bodies, file
/// contents, and captured text share one JSON/TOML shape.
///
#[oxdock_func(pure, returns = TypeTag::MapOf(&TypeTag::Any))]
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
/// ```text
/// LET $doc: ANY = PARSE_JSON($body)
/// ```
///
#[oxdock_func(pure, returns = TypeTag::Any)]
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
fn path_type<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Workspace path of the entry to describe.
    path: String,
) -> Result<Value> {
    super::args::path_type_from_value(&[Value::string(path)], cx)
}

/// Report whether a map holds a key.
///
/// Pure MAP probe so scripts can branch on optional fields without
/// tripping the strict missing-key error.
///
#[oxdock_func(pure, returns = TypeTag::Bool)]
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
#[oxdock_func(pure, returns = TypeTag::MapOf(&TypeTag::Any))]
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

/// Encode a script value as JSON with one trailing newline.
///
/// Maps stay sorted; only template-safe shapes (STRING, INT, FLOAT,
/// BOOL, LIST, MAP) survive, anything else fails here instead of
/// rendering as a silent empty.
///
#[oxdock_func(pure, returns = TypeTag::String)]
fn to_json(
    /// Value to encode as JSON.
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
#[oxdock_func(pure, returns = TypeTag::MapOf(&TypeTag::Any))]
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
#[oxdock_func(returns = TypeTag::ListOf(&TypeTag::String))]
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
/// functions), so the shape holds for every entry. Bare names fail
/// closed: `DESCRIBE` requires the qualified form (except `INSPECT`,
/// which is syntax rather than a registry entry). Errors on unknown
/// function.
///
/// Field table for the `DESCRIBE` result bundle: one source for the
/// shape the host mints, so introspection, enforcement, and
/// documentation cannot drift apart. `const` (not `static`) so the
/// inline `Record` below const-promotes to `'static` like the
/// `TYPE_DESCRIBE` table beside it.
const DESCRIBE_PARAMS_MAP: TypeTag = TypeTag::MapOf(&TypeTag::Any);
const DESCRIBE_PARAMS: TypeTag = TypeTag::ListOf(&DESCRIBE_PARAMS_MAP);

#[oxdock_func(
    returns = TypeTag::Record(&[Field {
        name: "name",
        ty: TypeTag::String,
        docs: "Qualified function name.",
        optional: false,
    }, Field {
        name: "module",
        ty: TypeTag::String,
        docs: "Owning module.",
        optional: false,
    }, Field {
        name: "kind",
        ty: TypeTag::String,
        docs: "Origin label (`host` or `script`).",
        optional: false,
    }, Field {
        name: "params",
        ty: DESCRIBE_PARAMS,
        docs: "Parameter entries (empty for parameterless functions).",
        optional: false,
    }, Field {
        name: "returns",
        ty: TypeTag::String,
        docs: "Structural return shape (empty when untagged).",
        optional: false,
    }, Field {
        name: "rpn",
        ty: TypeTag::Bool,
        docs: "True when callable in RPN position.",
        optional: false,
    }, Field {
        name: "summary",
        ty: TypeTag::String,
        docs: "One-line description.",
        optional: false,
    }]),
    summary = "Describe one function by qualified name."
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
#[oxdock_func(returns = TypeTag::ListOf(&TypeTag::String))]
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
#[oxdock_func(returns = TypeTag::Record(&[Field {
    name: "name",
    ty: TypeTag::String,
    docs: "Queried type name.",
    optional: false,
}, Field {
    name: "summary",
    ty: TypeTag::String,
    docs: "One-line description.",
    optional: false,
}, Field {
    name: "docs",
    ty: TypeTag::String,
    docs: "Full documentation text.",
    optional: false,
}]))]
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
fn is_terminal<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Stream name: `stdin`, `stdout`, or `stderr` (exact match).
    stream: String,
) -> Result<Value> {
    use std::io::IsTerminal;
    let terminal = match stream.as_str() {
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
            "IS_TERMINAL expects \"stdin\", \"stdout\", or \"stderr\", got {stream:?}"
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
/// ```text
/// LET $sem: SEMAPHORE = SEMAPHORE_NEW(10)
/// ```
///
#[oxdock_func(returns = TypeTag::Semaphore)]
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
/// ```text
/// LET $acq: MAP<held: BOOL, permit?: PERMIT> = SEMAPHORE_TRY_ACQUIRE($sem)
/// IF !$acq.held {
///   ECHO "at cap, rejecting"
/// } ELSE {
///   LET $permit: PERMIT = $acq.permit
///   ASYNC { session work }
/// }
/// ```
///
#[oxdock_func(returns = TypeTag::Record(&[Field {
    name: "held",
    ty: TypeTag::Bool,
    docs: "`true` with the permit under `permit`, `false` with no `permit` key.",
    optional: false,
}, Field {
    name: "permit",
    ty: TypeTag::Permit,
    docs: "Permit handle; read only when `held`.",
    optional: true,
}]))]
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
/// ```text
/// LET $free: INT = SEMAPHORE_AVAILABLE($sem)
/// ```
///
#[oxdock_func(pure, returns = TypeTag::Int)]
fn semaphore_available(
    /// Semaphore handle from `SEMAPHORE_NEW`.
    sem: Arc<SemaphoreState>,
) -> Result<Value> {
    Ok(Value::int(sem.available() as i64))
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
/// ```oxdock
/// # Capture two lines, then drain to end of stream with no sentinel line.
/// IMPORT [STD]
/// LET $cap: PIPE
/// WITH_IO [stdout=$cap] {
///     ECHO "one"
///     ECHO "two"
/// }
///
/// # The loop exits on close, and the count proves both lines drained.
/// LET $n: INT = 0
/// WHILE !EOF($cap) {
///     WITH_IO [stdin=$cap] READ_LINE $line
///     $n = $n + 1
/// }
/// ASSERT_EQ $n 2
/// ```
///
#[oxdock_func(returns = TypeTag::Bool)]
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
#[derive(Debug, Clone, Copy)]
pub struct RecordSchema {
    pub name: &'static str,
    pub fields: &'static [Field],
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
            }
        }
        for descriptor in module.types {
            self.register_type(descriptor);
        }
        for schema in module.record_schemas {
            self.register_record_schema(schema.name, schema.fields);
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
