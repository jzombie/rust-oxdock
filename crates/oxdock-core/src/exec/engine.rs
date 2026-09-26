//! Fluent host-extension facade: register functions and types, run scripts.
//!
//! Raw pipeline setup (filesystem resolver, process manager, IO, registry
//! assembly) lives here, so downstream integrations never touch it:
//!
//! ```rust
//! use oxdock_core::{Engine, OxDockFn, OxDockType, Value};
//! use oxdock_func_macro::{oxdock_func, oxdock_type};
//! use std::fmt;
//!
//! /// Opaque label type.
//! #[oxdock_type(name = "ENGINE_DOCTEST_TAG")]
//! #[derive(Debug, Clone, PartialEq)]
//! struct EngineTag(String);
//!
//! impl fmt::Display for EngineTag {
//!     fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
//!         write!(f, "tag:{}", self.0)
//!     }
//! }
//!
//! /// Mint one opaque label.
//! #[oxdock_func(pure)]
//! fn engine_make_tag() -> anyhow::Result<Value> {
//!     Ok(Value::mint_heap(EngineTag::descriptor(), EngineTag("demo".into())))
//! }
//!
//! fn main() {
//!     let temp = oxdock_fs::GuardedPath::tempdir().unwrap();
//!     let root = temp.as_guarded_path().clone();
//!     let mut engine = Engine::new();
//!     engine.register_type::<EngineTag>();
//!     engine.register_module(oxdock_core::HostModule {
//!         name: "DEMO".to_string(),
//!         funcs: vec![EngineMakeTag::registration()],
//!         types: vec![],
//!     });
//!     let run = engine
//!         .run_script(
//!             &root,
//!             "IMPORT [DEMO]\nLET $t: ENGINE_DOCTEST_TAG = ENGINE_MAKE_TAG()\n",
//!         )
//!         .expect("script runs");
//!     assert!(run.bindings.contains_key("t"));
//! }
//! ```
//!
//! Hosts that need more than the defaults (a sandboxed process manager, a
//! caller-built filesystem, custom IO) stay on the same facade: pick the
//! manager as the type parameter, stage IO with [`Engine::with_io`], and run
//! with [`Engine::run_steps_on`] or [`Engine::run_script_on`]. The free
//! [`run_steps_with_manager_with_modules`](super::run_steps_with_manager_with_modules)
//! function remains for callers that never stage host surface at all.
//!
//! Registration shapes behind the macros (pure vs stateful functions,
//! heap vs inline types):
//!
//! ```rust
//! use oxdock_func_macro::{oxdock_func, oxdock_type};
//! use ::oxdock_core::{OxDockFn, OxDockType};
//! use ::oxdock_process::DefaultProcessManager;
//!
//! /// Echo one value back.
//! #[oxdock_func(pure, name = "ECHO_VAL")]
//! fn echo_val(val: ::oxdock_core::Value) -> ::anyhow::Result<::oxdock_core::Value> {
//!     Ok(val)
//! }
//!
//! /// Read an environment variable, defaulting to empty.
//! #[oxdock_func(name = "ENV_OR", returns = "STRING")]
//! fn env_or<P: ::oxdock_core::ProcessManager>(
//!     cx: &mut ::oxdock_core::StepCtx<P>,
//!     key: String,
//! ) -> ::anyhow::Result<::oxdock_core::Value> {
//!     Ok(::oxdock_core::Value::string(cx.get_env(&key).unwrap_or_default()))
//! }
//!
//! /// Dense vector embedding.
//! ///
//! /// Heap type: one box allocation per word.
//! #[oxdock_type(name = "EMBEDDING")]
//! #[derive(Debug, Clone, PartialEq)]
//! struct Embedding(Vec<f32>);
//!
//! impl ::std::fmt::Display for Embedding {
//!     fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
//!         write!(f, "embedding[{}]", self.0.len())
//!     }
//! }
//!
//! /// Entity handle.
//! ///
//! /// Inline type: zero allocation, rides in the payload.
//! #[oxdock_type(name = "ENTITY", inline)]
//! #[derive(Clone, Copy, PartialEq)]
//! struct EntityId(u64);
//!
//! impl ::std::fmt::Display for EntityId {
//!     fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
//!         write!(f, "entity#{}", self.0)
//!     }
//! }
//! impl ::std::fmt::Debug for EntityId {
//!     fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
//!         write!(f, "EntityId({})", self.0)
//!     }
//! }
//!
//! fn main() {
//!     let entry: ::oxdock_core::HostRegistration<::oxdock_process::DefaultProcessManager> =
//!         EchoVal::registration();
//!     let ::oxdock_core::HostRegistration::Pure { meta, .. } = entry else {
//!         panic!("pure functions register Pure entries");
//!     };
//!     assert_eq!(meta.name, "ECHO_VAL");
//!     assert_eq!(meta.summary, "Echo one value back.");
//!     assert_eq!(meta.params.expect("one param").len(), 1);
//!
//!     let stateful =
//!         <EnvOr as OxDockFn<DefaultProcessManager>>::registration();
//!     let ::oxdock_core::HostRegistration::Stateful { meta, .. } = stateful else {
//!         panic!("context functions register Stateful entries");
//!     };
//!     assert_eq!(meta.name, "ENV_OR");
//!
//!     let descriptor = Embedding::descriptor();
//!     assert_eq!(descriptor.name, "EMBEDDING");
//!     assert_eq!(descriptor.summary, "Dense vector embedding.");
//! }
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use anyhow::Result;
use oxdock_fs::{GuardedPath, PathResolver, WorkspaceFs};
use oxdock_parser::{Step, Value};
use oxdock_process::{DefaultProcessManager, ProcessManager, default_process_manager};

use super::io::ExecIo;
use super::native::{HostModule, HostRegistration, std_module_table};
use super::typing::{OxDockType, TypeDescriptor};

/// Output of one [`Engine`] run: the final working directory, the filesystem
/// handle the run executed against, and the top-level script variable
/// bindings captured at completion.
pub struct EngineOutput {
    /// Working directory when the last step finished.
    pub cwd: GuardedPath,
    /// Filesystem handle the run executed against.
    pub fs: Box<dyn WorkspaceFs>,
    /// Top-level script variables (`FUNC` bodies and loop scopes excluded).
    pub bindings: BTreeMap<String, Value>,
}

impl fmt::Debug for EngineOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineOutput")
            .field("cwd", &self.cwd.as_path().display().to_string())
            .field("bindings", &self.bindings)
            .finish_non_exhaustive()
    }
}

/// Host-extension engine: the single front door for registering Rust
/// functions and types and running scripts against them.
///
/// The type parameter selects the process manager: the default
/// [`DefaultProcessManager`] runs real subprocesses, while test harnesses
/// pass their mock (`Engine::<MockProcessManager>::new_custom()` in
/// `crates/oxdock-core/tests/integration/custom_types.rs` shows the shape).
/// Staged IO applies to every run; the process manager and the filesystem
/// are chosen per run, so one engine serves many roots.
pub struct Engine<P: ProcessManager = DefaultProcessManager> {
    modules: Vec<HostModule<P>>,
    types: Vec<&'static TypeDescriptor>,
    io: ExecIo,
}

impl<P: ProcessManager> Engine<P> {
    /// Empty engine for a custom process manager, with default IO and no
    /// host surface. Prefer [`Engine::new`] for the default manager: a
    /// bare `Engine::new()` pins the manager type for inference, while
    /// this constructor needs the turbofish (`Engine::<Mock>::new_custom()`).
    /// `Engine::<P>::default()` works the same way.
    pub fn new_custom() -> Self {
        Self {
            modules: Vec::new(),
            types: Vec::new(),
            io: ExecIo::new(),
        }
    }

    /// Stage custom IO (for example inherited environment overrides) for
    /// every run. Chainable.
    pub fn with_io(mut self, io: ExecIo) -> Self {
        self.io = io;
        self
    }

    /// Stage the transport behind `REMOTE` blocks for every run.
    /// Chainable. Without this every `REMOTE` step bails naming the NET
    /// plugin; the CLI registers its SSH session here at startup.
    pub fn set_remote_runner(
        &mut self,
        runner: Arc<dyn super::remote::RemoteRunner>,
    ) -> &mut Self {
        self.io.set_remote_runner(runner);
        self
    }

    /// Register a host library module: its functions become callable as
    /// `MODULE::NAME`, its types join the run's name directory. Chainable.
    /// Group `#[oxdock_func]` markers (for example `MakeTag` for
    /// `make_tag`) with their `#[oxdock_type]` payloads here. Panics on a
    /// duplicate qualified name, including collisions with `STD` builtins;
    /// the registry re-checks at run time for direct state users.
    pub fn register_module(&mut self, module: HostModule<P>) -> &mut Self {
        let mut seen: std::collections::HashSet<String> = super::builtin_function_names();
        for staged in self.modules.iter().chain(std::iter::once(&module)) {
            for registration in &staged.funcs {
                let base = match registration {
                    HostRegistration::Stateful { name, .. }
                    | HostRegistration::Pure { name, .. } => name,
                };
                let qualified = format!("{}::{base}", staged.name);
                if !seen.insert(qualified.clone()) {
                    panic!("duplicate function registration `{qualified}`");
                }
            }
        }
        self.modules.push(module);
        self
    }

    /// Register a `#[oxdock_type]` payload struct. Chainable. Staged for
    /// the run: values mint straight from the payload type's own
    /// descriptor, which needs no registration to exist.
    pub fn register_type<T>(&mut self) -> &mut Self
    where
        T: OxDockType,
    {
        self.types.push(T::descriptor());
        self
    }

    /// Parse-time module table for this engine: stock `STD` builtins plus
    /// every staged host module (function names and RPN eligibility).
    /// Unknown modules stay unknown: the `oxdock!` macro declares its own
    /// opaque modules via the `modules:` prefix instead.
    pub fn module_table(&self) -> oxdock_parser::ModuleTable {
        let mut table = std_module_table();
        for module in &self.modules {
            let mut functions = std::collections::HashSet::new();
            for registration in &module.funcs {
                let name = match registration {
                    HostRegistration::Stateful { name, .. } => name,
                    HostRegistration::Pure { name, .. } => name,
                };
                functions.insert(name.clone());
            }
            table.modules.insert(
                module.name.clone(),
                Some(oxdock_parser::ModuleFuncs { functions }),
            );
        }
        table
    }

    /// Parse and run `script` on a caller-built filesystem with `process`
    /// as the manager and the registered host surface available.
    pub fn run_script_on(
        &self,
        fs: Box<dyn WorkspaceFs>,
        script: &str,
        process: P,
    ) -> Result<EngineOutput> {
        let steps = crate::parse_script_with_modules(script, self.module_table())?;
        self.run_steps_on(fs, &steps, process)
    }

    /// Run already parsed `steps` (for example from the `oxdock!` macro,
    /// which builds the same DSL at compile time) on a caller-built
    /// filesystem with `process` as the manager and the registered host
    /// surface available.
    pub fn run_steps_on(
        &self,
        fs: Box<dyn WorkspaceFs>,
        steps: &[Step],
        process: P,
    ) -> Result<EngineOutput> {
        let (cwd, fs, bindings) = super::run_steps_with_manager_with_modules(
            fs,
            steps,
            process,
            self.io.clone(),
            self.modules.clone(),
            self.types.clone(),
        )?;
        Ok(EngineOutput { cwd, fs, bindings })
    }
}

impl Engine<DefaultProcessManager> {
    /// Empty engine with default IO and no host surface. Concrete by
    /// construction, so `let mut engine = Engine::new()` needs no
    /// annotation; custom managers use `Engine::<P>::new_custom()`.
    pub fn new() -> Self {
        Self {
            modules: Vec::new(),
            types: Vec::new(),
            io: ExecIo::new(),
        }
    }

    /// Parse and run `script` with `root` as the workspace, with the
    /// registered host surface available. Uses the default process manager
    /// and a resolver built from `root`.
    pub fn run_script(&self, root: &GuardedPath, script: &str) -> Result<EngineOutput> {
        let steps = crate::parse_script_with_modules(script, self.module_table())?;
        self.run_steps(root, &steps)
    }

    /// Run already parsed `steps` (for example from the `oxdock!` macro,
    /// which builds the same DSL at compile time) with `root` as the
    /// workspace and the registered host surface available. Uses the
    /// default process manager and a resolver built from `root`.
    pub fn run_steps(&self, root: &GuardedPath, steps: &[Step]) -> Result<EngineOutput> {
        let resolver = PathResolver::new_guarded(root.clone(), root.clone())?;
        let fs: Box<dyn WorkspaceFs> = Box::new(resolver);
        self.run_steps_on(fs, steps, default_process_manager())
    }
}

impl<P: ProcessManager> Default for Engine<P> {
    fn default() -> Self {
        Self::new_custom()
    }
}
