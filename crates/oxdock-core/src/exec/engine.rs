//! Fluent host-extension facade: register functions and types, run scripts.
//!
//! Raw pipeline setup (filesystem resolver, process manager, IO, registry
//! assembly) lives here, so downstream integrations never touch it:
//!
//! ```rust
//! use oxdock_core::{Engine, OxDockType, Value};
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
//!     engine.register_fn(EngineMakeTag);
//!     let run = engine
//!         .run_script(&root, "LET $t: ENGINE_DOCTEST_TAG = ENGINE_MAKE_TAG()\n")
//!         .expect("script runs");
//!     assert!(run.bindings.contains_key("t"));
//! }
//! ```
//!
//! Hosts that need more than the defaults (a sandboxed process manager, a
//! caller-built filesystem, custom IO) stay on the same facade: pick the
//! manager as the type parameter, stage IO with [`Engine::with_io`], and run
//! with [`Engine::run_steps_on`] or [`Engine::run_script_on`]. The free
//! [`run_steps_with_manager_with_hosts`](super::run_steps_with_manager_with_hosts)
//! function remains for callers that never stage host surface at all.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::Result;
use oxdock_fs::{GuardedPath, PathResolver, WorkspaceFs};
use oxdock_parser::{Step, Value};
use oxdock_process::{DefaultProcessManager, ProcessManager, default_process_manager};

use super::io::ExecIo;
use super::native::{HostRegistration, OxDockFn};
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
    funcs: Vec<HostRegistration<P>>,
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
            funcs: Vec::new(),
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

    /// Register a `#[oxdock_func]` marker (for example `MakeTag` for
    /// `make_tag`). Chainable.
    pub fn register_fn<F>(&mut self, _func: F) -> &mut Self
    where
        F: OxDockFn<P>,
    {
        self.funcs.push(F::registration());
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

    /// Register one prebuilt [`HostRegistration`] entry (either flavor).
    /// Chainable. This is how callers stage entries built outside marker
    /// reach, such as a `registrations()` vector.
    pub fn register_host(&mut self, registration: HostRegistration<P>) -> &mut Self {
        self.funcs.push(registration);
        self
    }

    /// Register many prebuilt entries at once. Chainable.
    pub fn register_hosts(&mut self, registrations: Vec<HostRegistration<P>>) -> &mut Self {
        self.funcs.extend(registrations);
        self
    }

    /// Parse and run `script` on a caller-built filesystem with `process`
    /// as the manager and the registered host surface available.
    pub fn run_script_on(
        &self,
        fs: Box<dyn WorkspaceFs>,
        script: &str,
        process: P,
    ) -> Result<EngineOutput> {
        let steps = crate::parse_script(script)?;
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
        let (cwd, fs, bindings) = super::run_steps_with_manager_with_hosts(
            fs,
            steps,
            process,
            self.io.clone(),
            self.funcs.clone(),
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
            funcs: Vec::new(),
            types: Vec::new(),
            io: ExecIo::new(),
        }
    }

    /// Parse and run `script` with `root` as the workspace, with the
    /// registered host surface available. Uses the default process manager
    /// and a resolver built from `root`.
    pub fn run_script(&self, root: &GuardedPath, script: &str) -> Result<EngineOutput> {
        let steps = crate::parse_script(script)?;
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
