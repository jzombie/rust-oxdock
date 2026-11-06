//! oxdock-compose: composed artifacts, byte-identical.
//!
//! General-purpose lifecycle renderer: one context in, deterministic
//! outputs out. The core is `COMPOSE_ENGINE` (see [`engine`]) plus
//! the `DEFER` two-pass plugin (see [`defer`]); domain content
//! arrives through host plugin modules (`OXDOCK`, `RUST`,
//! `MARKDOWN`) and through lifecycle files (`src/lifecycle`),
//! one file per pipeline participant exposing the same methods.
//!
//! Rust owns only the boundaries the DSL has no primitives for:
//! [`config`] (filename default, plugin registry read),
//! [`lifecycle`] (embedded file provisioning, uniformity check), and
//! [`dispatcher`] (engine wiring, pipe spawn). Sequencing and all
//! writes live in the DSL as named `FUNC` stages.
pub mod config;
pub mod defer;
pub mod dispatcher;
pub mod engine;
pub mod lifecycle;
pub mod plugins;

use anyhow::{Context, Result};
use oxdock_core::{Engine, ExecIo, HostModule};
use oxdock_fs::{GuardedPath, PathResolver, WorkspaceFs};
use oxdock_process::default_process_manager;
#[allow(clippy::disallowed_types)]
use std::path::Path;

pub use dispatcher::{run_child_script, run_child_script_on};

use config::resolve_config_name;
use dispatcher::{child_interpreter, pipeline_full_engine};
use lifecycle::{check_plugin_uniformity, provision_dist, read_support};

/// Execute the composition pipeline: provision the embedded files,
/// check every registered plugin exposes the same lifecycle
/// methods, then run the entry script, which composes the child from
/// the plugin registry and pipes it through the interpreter. Static
/// scripts are byte-copied from the tree (see [`lifecycle`]);
/// sequencing and all writes live in the DSL while Rust owns only
/// the boundaries above.
#[allow(clippy::disallowed_types)]
pub fn run(repo_root: &Path) -> Result<()> {
    run_with_plugins(repo_root, vec![])
}

/// Execute the composition pipeline with extra modules registered
/// alongside this pipeline's set.
///
/// External host modules arrive here, never in `pipeline_modules!`:
/// any `{{ CUSTOM::FUNC($var) }}` placeholder in a template or
/// fragment resolves through the live registered snapshot, so no
/// header or script change is needed. Lifecycle files are
/// separate: they are DSL files named by the config's `files`
/// registry, not plugin modules. Custom *scripts* are a third path:
/// build them over [`pipeline_engine`] with their own `oxdock!`
/// header naming their own modules.
#[allow(clippy::disallowed_types)]
pub fn run_with_plugins(
    repo_root: &Path,
    extra_modules: Vec<HostModule<oxdock_process::DefaultProcessManager>>,
) -> Result<()> {
    let root = GuardedPath::new_root(repo_root)?;

    let mut fs_resolver = PathResolver::new_guarded(root.clone(), root.clone())?;
    fs_resolver.set_workspace_root(root.clone());
    // Every stage runs through the pipe: the interpreter is always
    // the child binary, never the pipeline binary itself.
    let mut io = ExecIo::new();
    io.set_interpreter(child_interpreter().context("locate pipe child")?);
    // The pipe child resolves every path against this root: inherit it
    // into every spawn so module-relative child launching cannot skew
    // staging and output paths. The config filename rides the same
    // channel with an edge default, never a core assumption.
    io.insert_inherit_env(
        oxdock_fs::env::oxdock::WORKSPACE_ROOT,
        root.as_path().to_string_lossy().into_owned(),
    );
    let config_name =
        std::env::var("COMPOSE_CONFIG").or_else(|_| resolve_config_name(&fs_resolver, &root))?;
    io.insert_inherit_env("COMPOSE_CONFIG", config_name.clone());
    io.insert_inherit_env(
        "COMPOSE_PRINT_CHILD",
        std::env::var("COMPOSE_PRINT_CHILD").unwrap_or_else(|_| "0".to_string()),
    );
    // Binary-only repos have no checkout: lay the embedded pipeline
    // files down first (write-if-missing, customizations win), so the
    // entry read below and every DSL import resolve.
    provision_dist(&fs_resolver, &root).context("provision pipeline files")?;
    let engine = pipeline_full_engine(extra_modules, io);
    // Every lifecycle plugin exposes the same methods; registration
    // order is execution order. Fail fast here naming the outlier
    // instead of mid-pipeline at first use.
    check_plugin_uniformity(&fs_resolver, &root, &engine, &config_name)
        .context("check plugin lifecycle methods")?;
    // The pipeline entry is one static script, byte-copied from the
    // tree: it reads the config, assembles the child from the plugin
    // registry, and pipes it through the interpreter. What runs is
    // what the tree holds, never Rust-assembled text.
    let entry = read_support(&fs_resolver, &root, "oxdock-compose/src/run.oxfile")?;
    let fs: Box<dyn WorkspaceFs> = Box::new(fs_resolver);
    engine
        .run_script_on(fs, &entry, default_process_manager())
        .context("compose pipeline")?;
    eprintln!("docs rendered");
    Ok(())
}

/// Engine with the engine default registered, for downstream
/// pipelines running their own scripts.
///
/// The only default is `COMPOSE_ENGINE`: the file-set assembler's
/// own primitives, needed by every pipeline built on this engine.
/// Domain modules are per-pipeline choices, registered by the
/// pipeline itself: custom scripts declare their own `oxdock!`
/// header explicitly and register `OXDOCK`, `RUST`, `MARKDOWN`, or
/// their own modules on top of this engine. `run()` below is one
/// such pipeline, for OxDock Rust workspaces.
pub fn pipeline_engine() -> Engine<oxdock_process::DefaultProcessManager> {
    let mut engine = Engine::new().with_io(ExecIo::new());
    engine.register_module(engine::module());
    engine
}
