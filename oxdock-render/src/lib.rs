//! oxdock-render: rendered artifacts, byte-identical.
//!
//! General-purpose lifecycle renderer: one context in, deterministic
//! outputs out. The core is `RENDER_ENGINE` (see [`engine`]) plus
//! the `DEFER` two-pass plugin (see [`defer`]); domain content
//! arrives through host plugin modules (`OXDOCK`, `RUST`,
//! `MARKDOWN`) and through lifecycle files (`src/lifecycle`),
//! one file per pipeline participant exposing the same methods.
//!
//! Rust owns only the boundaries the DSL has no primitives for:
//! [`config`] (filename default, plugin registry read),
//! [`lifecycle`] (embedded pipeline serving, uniformity check), and
//! [`dispatcher`] (engine wiring, pipe spawn). Sequencing and all
//! writes live in the DSL as named `FUNC` stages.
pub mod cli;
pub mod config;
pub mod defer;
pub mod dispatcher;
pub mod embedded;
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
use embedded::{EmbeddedFiles, warn_on_shadowed_dist};
use lifecycle::{check_plugin_uniformity, read_support};

/// Execute the render pipeline: serve the embedded pipeline from
/// memory, check every registered plugin exposes the same lifecycle
/// methods, then run the entry script, which assembles the child from
/// the plugin registry and pipes it through the interpreter. Static
/// scripts are served from the binary (see [`embedded`]);
/// sequencing and all writes live in the DSL while Rust owns only
/// the boundaries above.
///
/// Trust boundary: this executes repo-controlled templates,
/// fragments, and lifecycle files, like a build script. Only run on
/// checkouts you would already build, and keep automation at
/// contents read with no secrets in scope.
#[allow(clippy::disallowed_types)]
pub fn run(repo_root: &Path) -> Result<()> {
    run_with_plugins(repo_root, vec![])
}

/// Execute the render pipeline with extra modules registered
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
    let config_name = std::env::var(oxdock_fs::env::render::CONFIG)
        .or_else(|_| resolve_config_name(&fs_resolver, &root))?;
    io.insert_inherit_env(oxdock_fs::env::render::CONFIG, config_name.clone());
    io.insert_inherit_env(
        oxdock_fs::env::render::PRINT_CHILD,
        std::env::var(oxdock_fs::env::render::PRINT_CHILD).unwrap_or_else(|_| "0".to_string()),
    );
    // Validate the config before anything runs: unknown keys and
    // mistyped values fail here naming the fix, so a typo like
    // `scope` for `scopes` can never silently change the pipeline.
    let cfg_path = root
        .join(&config_name)
        .with_context(|| format!("resolve {config_name}"))?;
    let cfg_raw = fs_resolver
        .read_to_string(&cfg_path)
        .with_context(|| format!("read {config_name}"))?;
    let cfg_json: serde_json::Value =
        serde_json::from_str(&cfg_raw).with_context(|| format!("parse {config_name}"))?;
    config::validate_config(&cfg_json, &config_name)?;
    let hooks = config::hooks_of(&cfg_json);
    // The pipeline scripts ride in the binary and resolve from memory,
    // so consumer repos never receive pipeline files on disk. On-disk
    // copies shadowing the embedded paths are ignored; warn naming
    // them so stale provisioned copies get deleted.
    warn_on_shadowed_dist(&fs_resolver, &root);
    let embedded = EmbeddedFiles::new(fs_resolver);
    let engine = pipeline_full_engine(extra_modules, io);
    // Every lifecycle plugin exposes the same methods; registration
    // order is execution order. Fail fast here naming the outlier
    // instead of mid-pipeline at first use.
    check_plugin_uniformity(&embedded, &root, &engine, &config_name)
        .context("check plugin lifecycle methods")?;
    // Pre hooks run before the entry script assembles the child: a
    // failing hook halts the pipeline before any target renders.
    run_hooks(&engine, &embedded, &root, &hooks.pre, config::HOOK_PRE)?;
    // The pipeline entry is one static script, served from memory:
    // it reads the config, assembles the child from the plugin
    // registry, and pipes it through the interpreter. What runs is
    // the binary's version, never a stale on-disk copy.
    let entry = read_support(&embedded, &root, "oxdock-render/src/run.oxfile")?;
    let fs: Box<dyn WorkspaceFs> = Box::new(embedded);
    let post_fs = fs.clone_box();
    engine
        .run_script_on(fs, &entry, default_process_manager())
        .context("render pipeline")?;
    // Post hooks run after every target verifies: a failing hook
    // fails the run even though the artifacts already wrote.
    run_hooks(
        &engine,
        post_fs.as_ref(),
        &root,
        &hooks.post,
        config::HOOK_POST,
    )?;
    eprintln!("docs rendered");
    Ok(())
}

/// Run one hook phase: each script executes top to bottom with the
/// pipeline engine, so hooks read the same modules and resolve the
/// same workspace. The first failure halts the pipeline naming the
/// hook. Each hook runs as its own script file, so nested imports
/// resolve against the hook's directory, exactly like entry scripts.
fn run_hooks(
    engine: &Engine<oxdock_process::DefaultProcessManager>,
    fs: &dyn WorkspaceFs,
    root: &GuardedPath,
    hooks: &[String],
    phase: &str,
) -> Result<()> {
    for rel in hooks {
        let path = root
            .join(rel)
            .with_context(|| format!("{phase} hook '{rel}' cannot be resolved"))?;
        let script = fs
            .read_to_string(&path)
            .with_context(|| format!("{phase} hook '{rel}' cannot be read"))?;
        engine
            .run_script_file_on(fs.clone_box(), &path, &script, default_process_manager())
            .with_context(|| format!("{phase} hook '{rel}' failed"))?;
    }
    Ok(())
}

/// Engine with the engine default registered, for downstream
/// pipelines running their own scripts.
///
/// The only default is `RENDER_ENGINE`: the file-set assembler's
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
