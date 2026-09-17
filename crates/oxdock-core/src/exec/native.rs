use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use oxdock_func_macro::oxdock_func;
use oxdock_parser::{Step, Value};
use oxdock_process::{DefaultProcessManager, ProcessManager};

use super::state::ExecState;
use super::steps::StepCtx;

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
#[derive(Debug, Clone)]
pub struct FuncParam {
    pub name: String,
    pub param_type: Option<String>,
}

/// Introspectable metadata for one function. Single source for
/// `DESCRIBE(name)` output and the static function reference. `rpn` names
/// whether the function also runs on the compiled math path (`true` for
/// pure functions and opted-in stateful ones); everything runs on the AST
/// path.
#[derive(Debug, Clone)]
pub struct FuncMeta {
    pub name: String,
    pub kind: FuncKind,
    pub params: Option<Vec<FuncParam>>,
    pub returns: Option<String>,
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

/// One host-registered function passed to `run_steps_with_manager_with_hosts`.
/// Build the entry with the `#[oxdock_func]`-generated registration marker
/// (passed to `Engine::register_fn`), or by hand. `Pure` entries run
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

/// One user-defined function body (`FUNC NAME($p: TYPE, ...) { ... }`).
#[derive(Debug, Clone)]
pub(super) struct FuncDefData {
    pub(super) params: Vec<(String, String)>,
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
pub struct FunctionRegistry<P: ProcessManager> {
    entries: HashMap<String, FuncEntry<P>>,
    scopes: Vec<ScopeFrame<P>>,
    /// Names registered at runtime via `register_host_fn`. Builtins and
    /// hosts share the host kinds; this set answers which names arrived
    /// after startup (for parse-time reserved-name seeding by hosts).
    host_names: HashSet<String>,
}

impl<P: ProcessManager> FunctionRegistry<P> {
    pub(super) fn with_builtins() -> Self {
        let mut reg = Self {
            entries: HashMap::new(),
            scopes: vec![ScopeFrame {
                defined: HashSet::new(),
                shadowed: Vec::new(),
            }],
            host_names: HashSet::new(),
        };
        // Every builtin registers through the same `HostRegistration`
        // entries hosts use: no separate authoring path for engine natives.
        for host in Self::builtin_registrations() {
            match host {
                HostRegistration::Stateful { name, meta, func } => {
                    reg.insert_native(name, meta, FuncBody::Ctx(func));
                }
                HostRegistration::Pure { name, meta, func } => {
                    reg.insert_native(name, meta, FuncBody::Pure(func));
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
            PathType::registration(),
            Functions::registration(),
            Describe::registration(),
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
    }

    pub(super) fn register_host(&mut self, name: String, mut meta: FuncMeta, func: NativeFn<P>) {
        meta.name = name.clone();
        meta.kind = FuncKind::HostCtx;
        self.insert_native(name.clone(), meta, FuncBody::Ctx(func));
        self.host_names.insert(name);
    }

    pub(super) fn register_pure_host(&mut self, name: String, mut meta: FuncMeta, func: PureFn) {
        meta.name = name.clone();
        meta.kind = FuncKind::HostPure;
        self.insert_native(name.clone(), meta, FuncBody::Pure(func));
        self.host_names.insert(name);
    }

    /// Define a DSL `FUNC`: native and host names cannot shadow, same-scope
    /// duplicates cannot redefine, and nested shadowing of an outer script
    /// definition restores on scope exit.
    pub(super) fn define_script(
        &mut self,
        name: &str,
        params: &[(String, String)],
        body: &[Step],
    ) -> Result<()> {
        let shadowable = matches!(
            self.entries.get(name).map(|entry| &entry.body),
            Some(FuncBody::Script(_))
        );
        if self.entries.contains_key(name) && !shadowable {
            anyhow::bail!("cannot shadow reserved function `{name}`");
        }
        if self
            .scopes
            .last()
            .is_some_and(|frame| frame.defined.contains(name))
        {
            anyhow::bail!("duplicate function `{name}` in same scope");
        }
        let old = self.entries.insert(
            name.to_string(),
            FuncEntry {
                meta: FuncMeta {
                    name: name.to_string(),
                    kind: FuncKind::Script,
                    params: Some(
                        params
                            .iter()
                            .map(|(name, param_type)| FuncParam {
                                name: name.clone(),
                                param_type: Some(param_type.clone()),
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
            frame.defined.insert(name.to_string());
            if let Some(old) = old {
                frame.shadowed.push((name.to_string(), old));
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

    fn host_names(&self) -> HashSet<String> {
        self.host_names.clone()
    }
}

impl<P: ProcessManager> Clone for FunctionRegistry<P> {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            scopes: self.scopes.clone(),
            host_names: self.host_names.clone(),
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
    names.insert("INSPECT".to_string());
    names
}

/// Metadata of every builtin function, sorted by name, for static
/// rendering (docs-gen). Same single source as `builtin_function_names`:
/// the `#[oxdock_func]` annotations, never a parallel list.
pub fn builtin_function_metas() -> Vec<FuncMeta> {
    FunctionRegistry::<DefaultProcessManager>::with_builtins().native_metas()
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
#[oxdock_func(pure, returns = "INT")]
fn int(val: Value) -> Result<Value> {
    super::args::int_from_value(val)
}

/// Convert a value to FLOAT.
///
/// Parses f64 (accepts int strings), bails on non-finite or non-numeric.
#[oxdock_func(pure, returns = "FLOAT")]
fn float(val: Value) -> Result<Value> {
    super::args::float_from_value(val)
}

/// List workspace paths matching a glob pattern.
///
/// Sorted, root-relative LIST; empty on no match or `..` escape.
#[oxdock_func(rpn, returns = "LIST")]
fn glob<P: ProcessManager>(cx: &mut StepCtx<P>, pattern: String) -> Result<Value> {
    super::args::glob_from_value(&[Value::string(pattern)], cx)
}

/// Load and parse a TOML file.
///
/// Reads a workspace file and parses TOML into a DSL value.
#[oxdock_func(rpn, returns = "MAP")]
fn load_toml<P: ProcessManager>(cx: &mut StepCtx<P>, path: String) -> Result<Value> {
    super::args::load_toml_from_value(&[Value::string(path)], cx)
}

/// Load and parse a JSON file.
///
/// Reads a workspace file and parses JSON into a DSL value.
#[oxdock_func(rpn, returns = "MAP")]
fn load_json<P: ProcessManager>(cx: &mut StepCtx<P>, path: String) -> Result<Value> {
    super::args::load_json_from_value(&[Value::string(path)], cx)
}

/// Describe a filesystem entry.
///
/// Reports file, dir, symlink (no-follow), or absent. AST-only by design;
/// there is no RPN arm for filesystem IO.
#[oxdock_func(returns = "STRING")]
fn path_type<P: ProcessManager>(cx: &mut StepCtx<P>, path: String) -> Result<Value> {
    super::args::path_type_from_value(&[Value::string(path)], cx)
}

/// List all visible function names.
///
/// Sorted LIST of DSL-defined plus native plus host-registered names.
#[oxdock_func(returns = "LIST")]
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

/// Describe one function by name.
///
/// Returns a MAP with name, kind, params, returns, and summary. Errors on
/// unknown function.
#[oxdock_func(returns = "MAP")]
fn describe<P: ProcessManager>(cx: &mut StepCtx<P>, name: String) -> Result<Value> {
    cx.state
        .describe_function(&name)
        .ok_or_else(|| anyhow::anyhow!("unknown function {name}"))
}

/// List all known type names.
///
/// Sorted LIST of startup plus host-registered type descriptors. Reads the
/// run's name directory, so it runs on the AST path like the other
/// introspection functions.
#[oxdock_func(returns = "LIST")]
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
#[oxdock_func(returns = "MAP")]
fn type_describe<P: ProcessManager>(cx: &mut StepCtx<P>, name: String) -> Result<Value> {
    cx.state
        .describe_type(&name)
        .map(|descriptor| {
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
            Value::map(map)
        })
        .ok_or_else(|| anyhow::anyhow!("unknown type {name}"))
}

fn meta_to_value(meta: &FuncMeta) -> Value {
    let mut map = BTreeMap::new();
    map.insert("name".to_string(), Value::string(meta.name.clone()));
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
                        Value::string(p.param_type.clone().unwrap_or_default()),
                    );
                    Value::map(entry)
                })
                .collect(),
        ),
        None => Value::string(String::new()),
    };
    map.insert("params".to_string(), params);
    map.insert(
        "returns".to_string(),
        Value::string(meta.returns.clone().unwrap_or_default()),
    );
    map.insert("rpn".to_string(), Value::bool(meta.rpn));
    map.insert(
        "summary".to_string(),
        Value::string(meta.summary.to_string()),
    );
    Value::map(map)
}

impl<P: ProcessManager> ExecState<P> {
    /// Register a host/Rust function callable from the DSL as `NAME(...)`.
    /// Host names share one namespace with DSL and native functions.
    pub fn register_host_fn(&mut self, name: String, meta: FuncMeta, func: NativeFn<P>) {
        self.functions.register_host(name, meta, func);
    }

    /// Register one [`HostRegistration`] entry (either flavor).
    pub fn register_host(&mut self, registration: HostRegistration<P>) {
        match registration {
            HostRegistration::Stateful { name, meta, func } => {
                self.register_host_fn(name, meta, func);
            }
            HostRegistration::Pure { name, meta, func } => {
                self.functions.register_pure_host(name, meta, func);
            }
        }
    }

    /// All visible functions: natives plus hosts plus current DSL definitions.
    pub fn list_functions(&self) -> Vec<FuncMeta> {
        let mut out: Vec<FuncMeta> = self.functions.entries_metas().into_iter().collect();
        if !out.iter().any(|m| m.name == "INSPECT") {
            out.push(FuncMeta {
                name: "INSPECT".to_string(),
                kind: FuncKind::HostCtx,
                params: None,
                returns: Some("MAP".to_string()),
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
        if name == "INSPECT" {
            return Some(meta_to_value(&FuncMeta {
                name: "INSPECT".to_string(),
                kind: FuncKind::HostCtx,
                params: None,
                returns: Some("MAP".to_string()),
                rpn: false,
                summary: "Inspect a variable binding.",
                docs: "INSPECT($var): dedicated AST node taking a variable, not a value.",
            }));
        }
        None
    }

    /// Names of host-registered functions, for parse-time shadow checks.
    pub fn registered_host_names(&self) -> HashSet<String> {
        self.functions.host_names()
    }

    pub(super) fn clone_native_pure(&self, name: &str) -> Option<PureFn> {
        self.functions.clone_pure_fn(name)
    }

    pub(super) fn clone_native_ctx(&self, name: &str) -> Option<NativeFn<P>> {
        self.functions.clone_ctx_fn(name)
    }

    pub(super) fn native_meta(&self, name: &str) -> Option<FuncMeta> {
        self.functions.meta(name)
    }
}
