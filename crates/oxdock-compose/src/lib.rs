//! oxdock-compose: composed artifacts, byte-identical.
//!
//! General-purpose composition pipeline extracted from the workspace docs
//! renderer. The core is now `COMPOSE_ENGINE` (see [`engine`]) plus
//! the core `DEFER` plugin (see [`defer`]); domain plugins (`OXDOCK`,
//! `RUST`, `MARKDOWN`) and the `DOCS` compat alias still register
//! per-pipeline until their extraction slices land.
pub mod defer;
pub mod engine;
pub mod plugins;

use anyhow::{Context, Result};
use oxdock_core::{Engine, ExecIo, HostModule, OxDockFn, TypeTag, Value};
use oxdock_fs::{EntryKind, GuardedPath, PathResolver, WorkspaceFs};
use oxdock_func_macro::oxdock_func;
use oxdock_process::default_process_manager;
#[allow(clippy::disallowed_types)]
use std::path::Path;

/// `run()`'s module set, written once.
///
/// Each entry is `(HeaderIdent, module_path)` registered on the
/// `Engine`, so hook calls and template placeholders resolve the
/// same set. This list is this pipeline's requirements, not engine
/// defaults: the only module the engine itself defaults to is
/// `COMPOSE_ENGINE` (see [`pipeline_engine`]); `DEFER` because
/// deferred placeholders resolve in every template (`DOCS` rides
/// alongside as its compat alias until retired); `OXDOCK`, `RUST`,
/// and `MARKDOWN` because the shipped hooks are the docs pipeline's
/// hooks, and a different pipeline would register a different set
/// alongside its own hooks.
///
/// External plugins never go here either: pass them to
/// [`run_with_plugins`], which registers them alongside this list.
/// The static scripts (`run.oxfile`, `compose.oxfile`,
/// `dispatcher.oxfile`, hook files) declare their own `IMPORT`s; a
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
            (DOCS, crate::plugins::docs),
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

/// Execute the composition pipeline in two static runs sharing one
/// filesystem: the domain preamble first (Rust-aware enrichment over
/// `COMPOSE_CONFIG`: version, member values, generated inputs, then
/// the plain-data domain file), then the core composition (discover,
/// assemble, render every master template through the lifecycle
/// pipe, verify). Static scripts are byte-copied from the tree (see
/// `read_support`); sequencing and all writes live in the DSL while
/// Rust owns only run order plus the executable-bit fixup below.
#[allow(clippy::disallowed_types)]
pub fn run(repo_root: &Path) -> Result<()> {
    run_with_plugins(repo_root, vec![])
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
fn pipeline_full_engine(
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
fn child_interpreter() -> Result<String> {
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

/// Run one piped stage script: the child entry point. Reads the full
/// script from stdin, runs it over `--root` (or `OXDOCK_WORKSPACE_ROOT`
/// when the flag is absent, or the current directory) with `--config`
/// (or `COMPOSE_CONFIG`, or the edge default), and fails the process
/// on error so the parent `RUN` propagates the failure.
pub fn run_child_script(script: &str) -> Result<()> {
    run_child_script_on(script, &child_root().context("locate child root")?)
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
/// Execute the composition pipeline with extra modules registered
/// alongside this pipeline's set.
///
/// External plugins arrive here, never in `pipeline_modules!`: any
/// `{{ CUSTOM::FUNC($var) }}` placeholder in a template or fragment
/// resolves through the live registered snapshot, so no header or
/// script change is needed. Custom *scripts* are a separate path:
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
    // Binary-only repos have no checkout: lay the embedded pipeline
    // files down first (write-if-missing, customizations win), so the
    // entry read below and every DSL import resolve.
    provision_dist(&fs_resolver, &root).context("provision pipeline files")?;
    let engine = pipeline_full_engine(extra_modules, io);
    // The pipeline entry is one static script, byte-copied from the
    // tree: it reads the config, assembles the child from the plugin
    // registry, and pipes it through the interpreter. What runs is
    // what the tree holds, never Rust-assembled text.
    let entry = read_support(&fs_resolver, &root, "crates/oxdock-compose/src/run.oxfile")?;
    let fs: Box<dyn WorkspaceFs> = Box::new(fs_resolver);
    let output = engine
        .run_script_on(fs, &entry, default_process_manager())
        .context("compose pipeline")?;
    apply_executable_bits(output.fs.as_ref(), &root, &config_name)
        .context("apply executable bits")?;
    eprintln!("docs rendered");
    Ok(())
}

/// Pipeline files the binary carries: entry, composer, dispatcher,
/// shared types, and the default hook set. Provisioned into the repo
/// (see [`provision_dist`]) so a bare binary runs anywhere without a
/// source checkout and without per-repo vendoring: byte-copy, never
/// Rust-assembled text.
fn embedded_dist() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "crates/oxdock-compose/src/run.oxfile",
            include_str!("run.oxfile"),
        ),
        (
            "crates/oxdock-compose/src/compose.oxfile",
            include_str!("compose.oxfile"),
        ),
        (
            "crates/oxdock-compose/src/dispatcher.oxfile",
            include_str!("dispatcher.oxfile"),
        ),
        (
            "crates/oxdock-compose/src/types.oxfile",
            include_str!("types.oxfile"),
        ),
        (
            "crates/oxdock-compose/src/hooks/setup.oxfile",
            include_str!("hooks/setup.oxfile"),
        ),
        (
            "crates/oxdock-compose/src/hooks/ingest.oxfile",
            include_str!("hooks/ingest.oxfile"),
        ),
        (
            "crates/oxdock-compose/src/hooks/assemble.oxfile",
            include_str!("hooks/assemble.oxfile"),
        ),
        (
            "crates/oxdock-compose/src/hooks/render.oxfile",
            include_str!("hooks/render.oxfile"),
        ),
        (
            "crates/oxdock-compose/src/hooks/deferred.oxfile",
            include_str!("hooks/deferred.oxfile"),
        ),
        (
            "crates/oxdock-compose/src/hooks/write.oxfile",
            include_str!("hooks/write.oxfile"),
        ),
        (
            "crates/oxdock-compose/src/hooks/verify.oxfile",
            include_str!("hooks/verify.oxfile"),
        ),
    ]
}

/// Lay the embedded pipeline files into the repo: write-if-missing,
/// no-op when identical, and hands off a customized file untouched
/// (its owner wins; delete it to resync). Keeps binary-only repos
/// working with zero vendored maintenance while preserving every
/// on-disk path the DSL imports resolve.
fn provision_dist(resolver: &PathResolver, root: &GuardedPath) -> Result<()> {
    for (rel, contents) in embedded_dist() {
        let path = root.join(rel)?;
        let current: Option<Vec<u8>> = resolver.read_file(&path).ok();
        match current {
            None => {
                if let Some((parent, _)) = rel.rsplit_once('/') {
                    let dir = root.join(parent)?;
                    resolver
                        .create_dir_all(&dir)
                        .context("provision hooks dir")?;
                }
                resolver
                    .write_file(&path, contents.as_bytes())
                    .with_context(|| format!("provision default '{rel}'"))?;
            }
            Some(bytes) if bytes == contents.as_bytes() => {}
            Some(_) => {
                eprintln!("compose: keeping customized '{rel}' (delete it to resync the default)");
            }
        }
    }
    Ok(())
}

/// Byte-copy one static script out of the repo: what runs is what
/// the provisioned files hold, never Rust-assembled text.
fn read_support(resolver: &PathResolver, root: &GuardedPath, rel: &str) -> Result<String> {
    let path = root.join(rel)?;
    resolver
        .read_to_string(&path)
        .with_context(|| format!("read static script '{rel}'"))
}

/// Config filename: explicit `COMPOSE_CONFIG` wins, else the one
/// neutral name. Anything else fails closed naming what was looked
/// for; the DSL never carries a filename default.
fn resolve_config_name(resolver: &PathResolver, root: &GuardedPath) -> Result<String> {
    if let Ok(name) = std::env::var("COMPOSE_CONFIG") {
        return Ok(name);
    }
    let neutral = root.join("compose.json")?;
    if resolver.metadata(&neutral).is_ok() {
        return Ok("compose.json".to_string());
    }
    anyhow::bail!(
        "no compose.json under {}; pass --config or set COMPOSE_CONFIG",
        root.as_path().to_string_lossy()
    )
}

/// Mode re-applied to rendered outputs whose target declares
/// `"executable": true`.
const EXECUTABLE_MODE: u32 = 0o755;

/// Output paths in one target.json manifest requesting the executable
/// bit: every target entry with `"executable": true` contributes its
/// `out`. Strict so a typo degrades to an error, never to a silently
/// non-executable script: a present-but-not-boolean flag, or `true`
/// without a string `out`, fails naming the target.
fn executable_outs(manifest: &serde_json::Value) -> Result<Vec<String>> {
    let mut outs = Vec::new();
    let targets = manifest
        .get("targets")
        .and_then(|targets| targets.as_array())
        .context("target manifest needs a 'targets' array")?;
    for target in targets {
        let name = target
            .get("name")
            .and_then(|name| name.as_str())
            .unwrap_or("<unnamed>");
        match target.get("executable") {
            None | Some(serde_json::Value::Bool(false)) => {}
            Some(serde_json::Value::Bool(true)) => {
                let out = target
                    .get("out")
                    .and_then(|out| out.as_str())
                    .with_context(|| {
                        format!("target '{name}' sets 'executable' without a string 'out'")
                    })?;
                outs.push(out.to_string());
            }
            Some(_) => {
                anyhow::bail!(
                    "target '{name}' sets 'executable' to a non-boolean; use true or omit it"
                )
            }
        }
    }
    Ok(outs)
}

/// Collect every target.json path under one docs-gen scope, relative
/// to the repo root. Symlinked directories are never descended, so a
/// link cycle cannot loop the walk; a symlinked target.json file
/// itself still resolves through the normal read.
fn collect_target_manifests(
    fs: &dyn WorkspaceFs,
    root: &GuardedPath,
    rel: &str,
    out: &mut Vec<String>,
) -> Result<()> {
    let dir = if rel == "." {
        root.clone()
    } else {
        root.join(rel)?
    };
    let entries = fs
        .read_dir_entries(&dir)
        .with_context(|| format!("list '{rel}' for executable-bit pass"))?;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = if rel == "." {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if name == "target.json" {
            out.push(child);
            continue;
        }
        let guarded = root.join(&child)?;
        if matches!(fs.entry_kind_no_follow(&guarded)?, EntryKind::Dir) {
            collect_target_manifests(fs, root, &child, out)?;
        }
    }
    Ok(())
}

/// Re-apply the executable bit to every rendered output whose target
/// declares `"executable": true`. Fresh files arrive `0644` from the
/// renderer, which silently drops the `0755` the smoke workflow needs
/// to execute `./install.sh`; re-applying here keeps regeneration
/// self-healing instead of relying on a committed mode bit
/// surviving. The bit itself lands through
/// `set_permissions_mode_unix`, so Windows and Miri runs succeed as
/// no-ops and only POSIX trees change.
fn apply_executable_bits(
    fs: &dyn WorkspaceFs,
    root: &GuardedPath,
    config_name: &str,
) -> Result<()> {
    let cfg_raw = fs
        .read_to_string(&root.join(config_name)?)
        .with_context(|| format!("read {config_name} for executable-bit pass"))?;
    let cfg: serde_json::Value = serde_json::from_str(&cfg_raw)
        .with_context(|| format!("parse {config_name} for executable-bit pass"))?;
    let scopes = cfg
        .get("scopes")
        .and_then(|scopes| scopes.as_array())
        .with_context(|| format!("{config_name} needs a 'scopes' array"))?;
    let mut manifests = Vec::new();
    for scope in scopes {
        let scope = scope
            .as_str()
            .with_context(|| format!("{config_name} 'scopes' entries must be strings"))?;
        collect_target_manifests(fs, root, scope, &mut manifests)?;
    }
    for manifest_rel in &manifests {
        let raw = fs
            .read_to_string(&root.join(manifest_rel)?)
            .with_context(|| format!("read '{manifest_rel}' for executable-bit pass"))?;
        let manifest: serde_json::Value = serde_json::from_str(&raw)
            .with_context(|| format!("parse '{manifest_rel}' for executable-bit pass"))?;
        for out in executable_outs(&manifest)? {
            let path = root.join(&out)?;
            fs.set_permissions_mode_unix(&path, EXECUTABLE_MODE)
                .with_context(|| format!("set executable bit on '{out}'"))?;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_outs_selects_flagged_targets() {
        let manifest = serde_json::json!({"targets": [
            {"name": "a", "out": "a.sh", "executable": true},
            {"name": "b", "out": "b.md"},
            {"name": "c", "out": "c.ps1", "executable": false},
        ]});
        assert_eq!(
            executable_outs(&manifest).expect("select"),
            vec!["a.sh".to_string()]
        );
    }

    #[test]
    fn executable_outs_rejects_non_boolean_flag() {
        let manifest = serde_json::json!({"targets": [
            {"name": "a", "out": "a.sh", "executable": "yes"},
        ]});
        let err = format!(
            "{:#}",
            executable_outs(&manifest).expect_err("string flag must fail")
        );
        assert!(err.contains("'a'"), "names the target: {err}");
    }

    #[test]
    fn executable_outs_rejects_flag_without_out() {
        let manifest = serde_json::json!({"targets": [{"name": "a", "executable": true}]});
        let err = format!(
            "{:#}",
            executable_outs(&manifest).expect_err("missing out must fail")
        );
        assert!(err.contains("'a'"), "names the target: {err}");
    }
}
