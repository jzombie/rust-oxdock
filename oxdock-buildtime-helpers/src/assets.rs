//! Build-script asset pipeline: executes an OxDock DSL script in a tempdir
//! sandbox, materializes the final workdir under `$OUT_DIR`, and emits the
//! typed module consumed by `oxdock_buildtime_macros::embed!`.
//!
//! Contract:
//! - IDE/miri skip predicates write a placeholder module (typed surface,
//!   `get() -> None`) and emit NO rerun directives, restoring cargo's default
//!   conservative invalidation.
//! - A successful build writes the real module and emits
//!   `cargo:rerun-if-changed` for build.rs, the DSL file (when file-based),
//!   every statically discoverable input, and each `extra_inputs` entry.
//! - Any failure prints a decorated error (full chain + filesystem snapshot)
//!   to stderr so cargo surfaces it before compiling the consumer.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_embed::{emit_embed_module, gather_assets, runtime_support_tokens};
#[allow(clippy::disallowed_types)]
use oxdock_fs::UnguardedPath;
use oxdock_fs::{EntryKind, GuardedPath, PathResolver};

use crate::manifest_paths::module_file_name;
use crate::track::plan_input_directives;

/// Where the DSL script comes from.
#[derive(Clone, Copy)]
pub enum DslSource {
    /// DSL file relative to `$CARGO_MANIFEST_DIR`. Watched via
    /// `cargo:rerun-if-changed`.
    File(&'static str),
    /// Inline literal inside build.rs itself (covered by the build.rs watch).
    Inline(&'static str),
}

/// Description of one embedded asset set.
#[derive(Clone)]
pub struct EmbedSpec {
    /// Logical name; also the generated struct identifier.
    pub name: String,
    pub script: DslSource,
    /// Destination directory under `$OUT_DIR`; defaults to lowercase name.
    pub subdir: Option<String>,
    /// Extra watched files/dirs (manifest-relative).
    pub extra_inputs: Vec<&'static str>,
}

impl EmbedSpec {
    pub fn new(name: impl Into<String>, script: DslSource) -> Self {
        Self {
            name: name.into(),
            script,
            subdir: None,
            extra_inputs: Vec::new(),
        }
    }

    pub fn extra_input(mut self, path: &'static str) -> Self {
        self.extra_inputs.push(path);
        self
    }

    pub fn subdir(mut self, dir: impl Into<String>) -> Self {
        self.subdir = Some(dir.into());
        self
    }
}

/// Description of one prepare-only asset set (no runtime module).
///
/// Materializes under `$OUT_DIR/prepared` by default so a bare `prepare`
/// can never wipe sibling asset directories or generated modules.
#[derive(Clone)]
pub struct PrepareSpec {
    pub script: DslSource,
    /// Destination directory under `$OUT_DIR`; defaults to `prepared`.
    pub subdir: Option<String>,
    pub extra_inputs: Vec<&'static str>,
}

impl PrepareSpec {
    pub fn new(script: DslSource) -> Self {
        Self {
            script,
            subdir: None,
            extra_inputs: Vec::new(),
        }
    }

    pub fn extra_input(mut self, path: &'static str) -> Self {
        self.extra_inputs.push(path);
        self
    }

    pub fn subdir(mut self, dir: impl Into<String>) -> Self {
        self.subdir = Some(dir.into());
        self
    }

    fn subdir_name(&self) -> String {
        self.subdir
            .clone()
            .unwrap_or_else(|| "prepared".to_string())
    }
}

/// True when macro/build execution should be skipped for IDE safety.
///
/// Mirrors the historical proc-macro predicates: rust-analyzer internals env,
/// `--cfg miri` in RUSTFLAGS, a rust-analyzer-named host executable, or a
/// VS Code background task (VSCODE_PID without TERM).
pub fn execution_is_skipped() -> bool {
    execution_is_skipped_with(
        |key| std::env::var(key).ok(),
        || {
            std::env::current_exe()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        },
    )
}

/// Pure form of [`execution_is_skipped`] with injectable lookups so the
/// branch matrix stays unit-testable without mutating process-global state.
pub fn execution_is_skipped_with(
    env: impl Fn(&str) -> Option<String>,
    current_exe: impl FnOnce() -> Option<String>,
) -> bool {
    if env("RUST_ANALYZER_INTERNALS_DO_NOT_USE").is_some() {
        return true;
    }

    if env("RUSTFLAGS")
        .map(|flags| flags.contains("--cfg miri"))
        .unwrap_or(false)
    {
        return true;
    }

    if current_exe()
        .map(|pb| pb.contains("rust-analyzer"))
        .unwrap_or(false)
    {
        return true;
    }

    if env("VSCODE_PID").is_some() && env("TERM").is_none() {
        return true;
    }

    false
}

/// Truthiness rules for debug logging: enabled by `1` or any casing of `true`.
fn debug_enabled_from(value: Option<String>) -> bool {
    value
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Build-script entry point. On failure prints `error[oxdock]: …` plus the
/// decorated executor diagnostics to stderr; cargo then aborts before
/// compiling the consumer.
pub fn embed_assets(spec: &EmbedSpec) -> Result<()> {
    try_embed_assets(spec).inspect_err(|err| {
        eprintln!("error[oxdock]: {err:#}");
    })?;
    Ok(())
}

/// Testable core behind [`embed_assets`]: identical behavior but returns the
/// emitted directive list instead of printing it.
pub fn try_embed_assets(spec: &EmbedSpec) -> Result<Vec<String>> {
    init();
    if execution_is_skipped() {
        init();
        let out_dir = out_dir_root()?;
        let resolver = PathResolver::from_manifest_env()?;
        let module_path = module_guarded_path(&out_dir, &module_file_name(&spec.name))?;
        // Placeholder keeps symbol resolution intact; no directives means
        // cargo's conservative default invalidation still applies.
        resolver.write_file(
            &module_path,
            placeholder_module_source(&spec.name).as_bytes(),
        )?;
        return Ok(Vec::new());
    }

    let directives = run_spec(
        &spec.name,
        spec.script,
        &spec.subdir_name(),
        &spec.extra_inputs,
    )?;

    let out_dir = out_dir_root()?;
    let resolver = PathResolver::from_manifest_env()?;
    let module_path = module_guarded_path(&out_dir, &module_file_name(&spec.name))?;
    let asset_root = out_dir.join(&spec.subdir_name())?;
    let assets = gather_assets(&resolver, &asset_root)
        .map_err(|e| anyhow::anyhow!("failed to collect assets: {e}"))?;
    let name_ident = syn::Ident::new(&spec.name, proc_macro2::Span::call_site());
    let module = emit_embed_module(&name_ident, &assets)?;
    resolver.write_file(&module_path, module.to_string().as_bytes())?;
    Ok(directives)
}

/// Prepare-only entry point: materializes assets into `$OUT_DIR` but writes
/// no runtime module.
pub fn prepare_assets(spec: &PrepareSpec) -> Result<()> {
    try_prepare_assets(spec).inspect_err(|err| {
        eprintln!("error[oxdock]: {err:#}");
    })?;
    Ok(())
}

/// Testable core behind [`prepare_assets`].
pub fn try_prepare_assets(spec: &PrepareSpec) -> Result<Vec<String>> {
    init();
    if execution_is_skipped() {
        return Ok(Vec::new());
    }
    let directives = run_spec(
        "prepare",
        spec.script,
        &spec.subdir_name(),
        &spec.extra_inputs,
    )?;
    Ok(directives)
}

fn init() {
    oxdock_fs::init_temp_gc();
}

fn out_dir_root() -> Result<GuardedPath> {
    let out =
        std::env::var("OUT_DIR").context("OUT_DIR missing (not running as a build script?)")?;
    GuardedPath::new_root_from_str(&out).map_err(|e| anyhow::anyhow!("invalid OUT_DIR {out}: {e}"))
}

fn module_guarded_path(out_dir: &GuardedPath, rel: &str) -> Result<GuardedPath> {
    out_dir
        .join(rel)
        .map_err(|e| anyhow::anyhow!("generated module path rejected: {e}"))
}

impl EmbedSpec {
    fn subdir_name(&self) -> String {
        self.subdir
            .clone()
            .unwrap_or_else(|| self.name.to_lowercase())
    }
}

fn script_text(source: DslSource) -> Result<(String, Option<String>)> {
    match source {
        DslSource::Inline(text) => Ok((text.to_string(), None)),
        DslSource::File(rel) => {
            let resolver = PathResolver::from_manifest_env()
                .context("CARGO_MANIFEST_DIR missing for DSL resolution")?;
            let guarded = resolver
                .root()
                .join(rel)
                .map_err(|e| anyhow::anyhow!("DSL path {rel} escapes manifest dir: {e}"))?;
            let text = resolver
                .read_to_string(&guarded)
                .with_context(|| format!("failed to read DSL script {rel}"))?;
            Ok((text, Some(rel.to_string())))
        }
    }
}

fn run_spec(
    name: &str,
    script: DslSource,
    subdir: &str,
    extra_inputs: &[&'static str],
) -> Result<Vec<String>> {
    let (text, file_rel) = script_text(script)?;

    // Directives are computed even on the skip path callers may inspect them,
    // but only the non-skip path returns them for emission.
    let steps =
        oxdock_parser::parse_script(&text).map_err(|e| anyhow::anyhow!("parse error: {e}"))?;
    let (changed, env_changed) = plan_input_directives(&steps);

    let mut directives: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let push = |directives: &mut Vec<String>, seen: &mut BTreeSet<String>, d: String| {
        if seen.insert(d.clone()) {
            directives.push(d);
        }
    };

    // Emitting ANY rerun directive disables cargo's default invalidate-
    // everything, so watching build.rs is mandatory completeness.
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .context("CARGO_MANIFEST_DIR missing (not running as a build script?)")?;
    push(
        &mut directives,
        &mut seen,
        format!(
            "cargo:rerun-if-changed={}",
            join_manifest(&manifest, "build.rs")
        ),
    );
    if let Some(rel) = &file_rel {
        push(
            &mut directives,
            &mut seen,
            format!("cargo:rerun-if-changed={}", join_manifest(&manifest, rel)),
        );
    }
    for rel in &changed {
        push(
            &mut directives,
            &mut seen,
            format!("cargo:rerun-if-changed={}", join_manifest(&manifest, rel)),
        );
    }
    for key in &env_changed {
        push(
            &mut directives,
            &mut seen,
            format!("cargo:rerun-if-env-changed={key}"),
        );
    }
    for rel in extra_inputs {
        push(
            &mut directives,
            &mut seen,
            format!("cargo:rerun-if-changed={}", join_manifest(&manifest, rel)),
        );
    }

    build_and_materialize(name, &text, subdir)?;

    Ok(directives)
}

fn join_manifest(manifest: &str, rel: &str) -> String {
    format!("{manifest}/{rel}")
}

/// Execute the DSL in a tempdir sandbox and copy the final workdir contents
/// into `$OUT_DIR/<subdir>` after clearing stale entries (no merge-with-stale).
fn build_and_materialize(name: &str, script: &str, subdir: &str) -> Result<()> {
    let debug = debug_enabled_from(std::env::var("OXDOCK_EMBED_DEBUG").ok());

    let tempdir = GuardedPath::tempdir().context("failed to create sandbox tempdir")?;
    let temp_root = tempdir.as_guarded_path().clone();

    let steps =
        oxdock_parser::parse_script(script).map_err(|e| anyhow::anyhow!("parse error: {e}"))?;
    let resolver = PathResolver::from_manifest_env().context("CARGO_MANIFEST_DIR missing")?;
    let workspace_root =
        oxdock_fs::discover_workspace_root().context("failed to discover workspace root")?;

    let final_cwd =
        run_steps_with_context_result_with_io(&temp_root, &workspace_root, &steps, ExecIo::new())
            .map_err(|e| {
            // Alternate formatting keeps the full chain + filesystem snapshot.
            anyhow::anyhow!("execution error: {e:#}")
        })?;

    if debug {
        eprintln!("oxdock: [{name}] final_cwd={}", final_cwd.display());
    }

    // Narrow allowance: crossing into OUT_DIR requires the audited
    // unguarded escape hatch, exactly as in the historical proc-macro flow.
    #[allow(clippy::disallowed_types)]
    let final_external = UnguardedPath::external(final_cwd.as_path().to_path_buf());
    let meta = resolver.metadata_unguarded(&final_external).map_err(|e| {
        anyhow::anyhow!(
            "final workdir missing after build: {} ({e})",
            final_cwd.display()
        )
    })?;
    if !meta.is_dir() {
        bail!("final workdir is not a directory: {}", final_cwd.display());
    }

    let out_dir = out_dir_root()?;
    let target = if subdir.is_empty() {
        out_dir.clone()
    } else {
        out_dir.join(subdir)?
    };

    ensure_materialize_dir(&resolver, &target)?;
    clear_dir(&resolver, &target)?;

    resolver
        .copy_dir_from_unguarded(&final_external, &target)
        .map_err(|e| anyhow::anyhow!("failed to copy final workdir into OUT_DIR: {e}"))?;
    // Sandbox infrastructure files are never user assets.
    for marker in [".oxdock-tempdir", ".oxdock-tempdir.lock"] {
        let guarded = target.join(marker)?;
        if resolver.entry_kind(&guarded).is_ok() {
            resolver.remove_file(&guarded)?;
        }
    }

    if debug {
        eprintln!(
            "oxdock: [{name}] materialized into {}; entries={:?}",
            target.display(),
            resolver.read_dir_entries(&target).ok().map(|v| v.len())
        );
    }
    Ok(())
}

fn ensure_materialize_dir(resolver: &PathResolver, target: &GuardedPath) -> Result<()> {
    if target.as_path().exists() {
        let kind = resolver.entry_kind(target)?;
        if kind != EntryKind::Dir {
            bail!(
                "OUT_DIR destination exists but is not a directory: {}",
                target.display()
            );
        }
        return Ok(());
    }
    resolver
        .create_dir_all(target)
        .map_err(|e| anyhow::anyhow!("failed to create {}: {e}", target.display()))
}

fn clear_dir(resolver: &PathResolver, dir: &GuardedPath) -> Result<()> {
    let entries = resolver
        .read_dir_entries(dir)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.path();
        let guarded = GuardedPath::new(dir.root(), &path)
            .map_err(|e| anyhow::anyhow!("failed to guard {}: {e}", path.display()))?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            resolver.remove_dir_all(&guarded)?;
        } else {
            resolver.remove_file(&guarded)?;
        }
    }
    Ok(())
}

/// Typed placeholder used when IDE-skip predicates short-circuit the build:
/// same module surface (`get()` → `None`, empty `iter()`), no compile error.
fn placeholder_module_source(name: &str) -> String {
    let mod_ident = format!("__oxdock_embed_{name}");
    let runtime_support = runtime_support_tokens();
    format!(
        "#[allow(clippy::disallowed_methods, clippy::disallowed_types, non_snake_case)]\n\
         pub mod {mod_ident} {{\n    {runtime_support}\n\n    pub struct {name};\n\n    \
         impl {name} {{\n        pub fn get(_file: &str) -> Option<EmbeddedFile> {{\n            None\n        }}\n\n        \
         pub fn iter() -> Filenames {{\n            static EMPTY: [&str; 0] = [];\n            Filenames::from_slice(&EMPTY)\n        }}\n    }}\n}}\n\n\
         pub use {mod_ident}::{name};\n"
    )
}
