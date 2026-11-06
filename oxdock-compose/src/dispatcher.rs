//! Pipe dispatcher: engine wiring plus the spawn boundary.
//!
//! Rust owns the module set, the child interpreter lookup, and the
//! environment the pipe child inherits. The child script itself is
//! composed in the DSL (`compose.oxfile` `COMPOSE_ALL` plus
//! `PIPE_RUN`); Rust never assembles script text.

use anyhow::{Context, Result};
use oxdock_core::{Engine, ExecIo, HostModule, OxDockFn, TypeTag, Value};
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_func_macro::oxdock_func;

use crate::lifecycle::provision_dist;

/// `run()`'s module set, written once.
///
/// Each entry is `(HeaderIdent, module_path)` registered on the
/// `Engine`, so host calls and template placeholders resolve the
/// same set. These are Rust plugin modules, not lifecycle files
/// (DSL): `COMPOSE_ENGINE` is the only module the engine itself
/// defaults to; `DEFER` because deferred placeholders resolve in
/// every template; `OXDOCK`, `RUST`, and `MARKDOWN` because the
/// shipped OxDock docs lifecycle file calls them, and a different
/// pipeline registers whatever its own plugins need.
///
/// External plugin modules never go here either: pass them to
/// `run_with_plugins`, which registers them alongside this list.
/// The static scripts (`run.oxfile`, `compose.oxfile`,
/// `stages.oxfile`, hook files) declare their own `IMPORT`s; a
/// missing registration fails the import loudly naming the module,
/// and the render suite executes every script on every run.
///
/// A runtime `Vec` cannot be this list: it lives at token level and
/// fans out by `pipeline_split!` into the registrations below. Paths
/// thread through as opaque token runs (never `:path`, which
/// degrades across repeated match cycles) and accumulate space
/// separated, since a trailing comma breaks the final match.
macro_rules! pipeline_modules {
    ($final:ident [$($args:tt)*]) => {
        pipeline_split! {
            $final ; [$($args)*] ;
            (COMPOSE_ENGINE, crate::engine),
            (DEFER, crate::defer),
            (OXDOCK, crate::plugins::oxdock),
            (RUST, crate::plugins::rust),
            (MARKDOWN, oxdock_markdown_plugin)
        }
    };
}

/// Fan one `pipeline_modules!` list out to a `$final!` callback as
/// `{ [args] ; [header idents] ; [(paths)] }`.
macro_rules! pipeline_split {
    ($final:ident ; [$($args:tt)*] ; $(($name:ident, $first:tt $(:: $rest:tt)*)),*) => {
        pipeline_split!(@acc $final ; [$($args)*] ; [] ; [] ; $(($name, $first $(:: $rest)*)),*)
    };
    (@acc $final:ident ; [$($args:tt)*] ; [$($hn:ident),*] ; [$($abag:tt)*]) => {
        $final! { [$($args)*] ; [$($hn),*] ; [$($abag)*] }
    };
    (@acc $final:ident ; [$($args:tt)*] ; [$($hn:ident),*] ; [$($abag:tt)*] ; ($name:ident, $first:tt $(:: $rest:tt)*) $(, $($tail:tt)*)?) => {
        pipeline_split!(@acc $final ; [$($args)*] ; [$($hn,)* $name] ; [$($abag)* ($first $(:: $rest)*)] $(; $($tail)*)?)
    };
}

/// Emit one `register_module` per pipeline module path.
macro_rules! pipeline_emit_register {
    ([$engine:expr] ; [$($hn:ident),*] ; [$(( $first:tt $(:: $rest:tt)* ))*]) => {{
        $($engine.register_module($first $(:: $rest)*::module());)*
    }};
}

/// Minimal example external plugin (`TEST::PING`): proves a
/// `run_with_plugins` module resolves in templates across the pipe
/// without header changes. Registered in both parent and child engines
/// (see [`pipeline_full_engine`]) so the pipe child resolves it: Rust
/// modules cannot cross the process boundary any other way, and real
/// pipelines link their own modules into both binaries the same way.
#[oxdock_func(pure, returns = TypeTag::String)]
fn ping(value: String) -> Result<Value> {
    Ok(Value::string(format!("PONG:{value}")))
}

/// The example external plugin module (see [`ping`]).
fn test_ping_module() -> HostModule<oxdock_process::DefaultProcessManager> {
    HostModule {
        name: "TEST".to_string(),
        funcs: vec![Ping::registration()],
        types: vec![],
        record_schemas: vec![],
        states: vec![],
    }
}

/// The full pipeline engine: every module this pipeline registers,
/// no script attached. Shared by the main run and the pipe child so
/// the child resolves the same host calls (including `DEFER`).
pub(crate) fn pipeline_full_engine(
    extra_modules: Vec<HostModule<oxdock_process::DefaultProcessManager>>,
    io: ExecIo,
) -> Engine<oxdock_process::DefaultProcessManager> {
    let mut engine = Engine::new().with_io(io);
    pipeline_modules!(pipeline_emit_register[engine]);
    engine.register_module(test_ping_module());
    for module in extra_modules {
        // The example module above already registers TEST; a caller
        // passing its own TEST keeps theirs (last registration wins).
        if module.name != "TEST" {
            engine.register_module(module);
        }
    }
    engine
}

/// Locate the pipe-child interpreter: an explicit override for tests
/// and harnesses, else the `oxdock-compose-child` binary beside the
/// running executable (cargo installs both bins side by side).
/// `INTERPRETER()` alone is unusable here: it returns the pipeline
/// binary, which cannot interpret scripts.
pub(crate) fn child_interpreter() -> Result<String> {
    if let Ok(path) = std::env::var("OXDOCK_COMPOSE_CHILD") {
        return Ok(path);
    }
    let exe = std::env::current_exe().context("locate current executable")?;
    let child = exe
        .parent()
        .context("executable has no parent dir")?
        .join("oxdock-compose-child");
    if child.is_file() {
        Ok(child.to_string_lossy().into_owned())
    } else {
        anyhow::bail!(
            "pipe-child binary missing at {}: set OXDOCK_COMPOSE_CHILD explicitly",
            child.display()
        )
    }
}

/// Resolve the child's workspace root: explicit `--root` wins, then
/// the inherited `OXDOCK_WORKSPACE_ROOT`, then the current directory.
#[allow(clippy::disallowed_types)]
fn child_root() -> Result<std::path::PathBuf> {
    let mut args = std::env::args().skip(1);
    loop {
        match args.next() {
            Some(arg) if arg == "--root" => {
                if let Some(root) = args.next() {
                    return Ok(std::path::PathBuf::from(root));
                }
            }
            Some(_) => {}
            None => break,
        }
    }
    std::env::var(oxdock_fs::env::oxdock::WORKSPACE_ROOT)
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::current_dir().context("child needs a working dir"))
}

/// Resolve the child's config path: explicit `--config` wins, then
/// the inherited `COMPOSE_CONFIG`, then the edge default.
fn child_config() -> Option<String> {
    let mut args = std::env::args().skip(1);
    loop {
        match args.next() {
            Some(arg) if arg == "--config" => {
                if let Some(cfg) = args.next() {
                    return Some(cfg);
                }
            }
            Some(_) => {}
            None => break,
        }
    }
    std::env::var("COMPOSE_CONFIG").ok()
}

/// Run one piped stage script: the child entry point. Reads the full
/// script from stdin, runs it over `--root` (or `OXDOCK_WORKSPACE_ROOT`
/// when the flag is absent, or the current directory) with `--config`
/// (or `COMPOSE_CONFIG`, or the edge default), and fails the process
/// on error so the parent `RUN` propagates the failure.
pub fn run_child_script(script: &str) -> Result<()> {
    run_child_script_on(script, &child_root().context("locate child root")?)
}

/// Run one piped stage script over an explicit root. Seeds the
/// child's environment deterministically: the spawn boundary does
/// not forward the parent's script env, so both paths are absolutized
/// here and injected as overrides.
#[allow(clippy::disallowed_types)]
pub fn run_child_script_on(script: &str, dir: &std::path::Path) -> Result<()> {
    let root = GuardedPath::new_root(dir)?;
    let resolver = PathResolver::new(root.as_path(), root.as_path())?;
    // Standalone child runs provision too; the piped flow already did.
    provision_dist(&resolver, &root).context("provision pipeline files")?;
    let mut io = ExecIo::new();
    // The child's INHERIT_ENV reads these overrides; it never sees the
    // parent's script environment through the spawn boundary. Paths are
    // absolutized here (proper path joining is Rust's job, not the DSL's)
    // so hooks can use them directly regardless of module-relative cwd.
    io.insert_inherit_env(
        oxdock_fs::env::oxdock::WORKSPACE_ROOT,
        dir.to_string_lossy().into_owned(),
    );
    let cfg = child_config().unwrap_or_else(|| "compose.json".to_string());
    // `Path::join` with an absolute second half replaces the first,
    // so one join covers both shapes with no absolute check.
    let cfg_abs = dir.join(cfg).to_string_lossy().into_owned();
    io.insert_inherit_env("COMPOSE_CONFIG", cfg_abs);
    let engine = pipeline_full_engine(vec![], io);
    engine
        .run_script(&root, script)
        .context("child script failed")?;
    Ok(())
}
