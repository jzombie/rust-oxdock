//! Build-script asset pipeline: executes an OxDock DSL script in a tempdir
//! sandbox, materializes the final workdir under `$OUT_DIR`, and emits the
//! typed module consumed by `oxdock_macros::embed!`.
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
use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_embed::{emit_embed_module, gather_assets, runtime_support_tokens};
#[allow(clippy::disallowed_types)]
use oxdock_fs::UnguardedPath;
use oxdock_fs::{EntryKind, GuardedPath, PathResolver};
use sha2::{Digest, Sha256};

use crate::manifest_paths::module_file_name;
use crate::track::{collect_env_references, plan_input_directives};

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

/// Environment variable whose value is folded into the asset fingerprint
/// when defined, forcing exactly one rebuild per change.
pub const FINGERPRINT_SALT_ENV: &str = "OXDOCK_EMBED_FINGERPRINT_SALT";

/// Environment variable that bypasses the `.oxdock_hash` cache read when
/// truthy (`"1"` or case-insensitive `"true"`).
pub const FORCE_REBUILD_ENV: &str = "OXDOCK_EMBED_FORCE_REBUILD";

/// Pure truthiness rules for [`FORCE_REBUILD_ENV`].
pub fn embed_force_rebuild_from(value: Option<String>) -> bool {
    value
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Process-env form of [`embed_force_rebuild_from`].
pub fn embed_force_rebuild() -> bool {
    embed_force_rebuild_from(std::env::var(FORCE_REBUILD_ENV).ok())
}

/// True when `OXDOCK_EMBED_DEBUG` requests verbose asset-pipeline logging
/// (`1` or any casing of `true`).
pub fn embed_debug_enabled() -> bool {
    debug_enabled_from(std::env::var("OXDOCK_EMBED_DEBUG").ok())
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
        oxdock_core::parse_script(&text).map_err(|e| anyhow::anyhow!("parse error: {e}"))?;
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
        oxdock_core::parse_script(script).map_err(|e| anyhow::anyhow!("parse error: {e}"))?;
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
    stage_materialize(&resolver, &final_external, &target)?;

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

/// Content fingerprint deciding whether `<out_dir>` may be reused.
///
/// Digest inputs: the raw script text, the out-dir key, every statically
/// discoverable input file (contents; directories walked sorted; missing
/// paths recorded as markers), and every referenced environment variable
/// resolved to `KEY=VALUE` (or `KEY=<unset>`) so environment drift — including
/// `[env:KEY]` guard gating — invalidates the cache.
///
/// `resolver` must be manifest-rooted (e.g. `PathResolver::from_manifest_env`);
/// `envs` should be the builtin env map (`BuiltinEnv::collect(...).into_envs()`),
/// with `std::env` consulted as fallback for keys it does not define.
pub fn asset_input_fingerprint(
    resolver: &PathResolver,
    script_text: &str,
    steps: &[oxdock_parser::Step],
    out_dir_key: &str,
    envs: &HashMap<String, String>,
) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(script_text.as_bytes());
    hasher.update(b"\0");
    hasher.update(out_dir_key.as_bytes());
    hasher.update(b"\0");

    let root = resolver.root().clone();
    let (changed, _env_changed) = plan_input_directives(steps);
    for rel in &changed {
        hash_input_path(resolver, &root, rel, &mut hasher)?;
    }

    for key in collect_env_references(steps) {
        let value = envs.get(&key).cloned().or_else(|| std::env::var(&key).ok());
        match value {
            Some(v) => hasher.update(format!("{key}={v}\0").as_bytes()),
            None => hasher.update(format!("{key}=<unset>\0").as_bytes()),
        }
    }

    // Precision invalidation lever: a defined salt shifts the expected digest,
    // forcing exactly one rebuild; absent salt contributes nothing so existing
    // caches stay valid across the upgrade.
    let salt = envs
        .get(FINGERPRINT_SALT_ENV)
        .cloned()
        .or_else(|| std::env::var(FINGERPRINT_SALT_ENV).ok());
    if let Some(salt) = salt {
        hasher.update(format!("SALT={salt}\0").as_bytes());
    }

    let digest = hasher.finalize();
    let bytes: &[u8] = digest.as_ref();
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn hash_input_path(
    resolver: &PathResolver,
    root: &GuardedPath,
    rel: &str,
    hasher: &mut Sha256,
) -> Result<()> {
    let guarded = root.join(rel)?;
    match resolver.entry_kind(&guarded) {
        Ok(EntryKind::File) => {
            hasher.update(b"F\0");
            hasher.update(rel.as_bytes());
            hasher.update(b"\0");
            hasher.update(&resolver.read_file(&guarded)?);
        }
        Ok(EntryKind::Dir) => {
            hasher.update(b"D\0");
            hasher.update(rel.as_bytes());
            hasher.update(b"\0");
            let mut entries = resolver.read_dir_entries(&guarded)?;
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let name = entry.file_name().to_string_lossy().to_string();
                let child_rel = format!("{rel}/{name}");
                hash_input_path(resolver, root, &child_rel, hasher)?;
            }
        }
        Err(_) => {
            hasher.update(b"MISSING\0");
            hasher.update(rel.as_bytes());
            hasher.update(b"\0");
        }
    }
    Ok(())
}

/// Internal sandbox marker files stripped from materialized output.
const SANDBOX_MARKERS: [&str; 2] = [".oxdock-tempdir", ".oxdock-tempdir.lock"];

/// Atomically-ish materialize `final_external` into `target`.
///
/// Copies the built tree into `<target>/.oxdock-staging` first, then syncs it
/// into `target` file-by-file (overwrite-copy — no destructive wipe of the
/// live directory, no exclusive-handle renames), removes top-level entries
/// that are no longer part of the build output, and finally deletes the
/// staging directory. Windows-safe by construction: no remove-then-rename
/// races against AV/indexer handle locks.
#[allow(clippy::disallowed_types)]
pub fn stage_materialize(
    resolver: &PathResolver,
    final_external: &UnguardedPath,
    target: &GuardedPath,
) -> Result<()> {
    let stage = target.join(".oxdock-staging")?;
    if resolver.entry_kind(&stage).is_ok() {
        resolver.remove_dir_all(&stage)?;
    }
    resolver.create_dir_all(&stage)?;

    resolver
        .copy_dir_from_unguarded(final_external, &stage)
        .map_err(|e| anyhow::anyhow!("failed to copy build output into staging: {e}"))?;
    for marker in SANDBOX_MARKERS {
        let marker_path = stage.join(marker)?;
        if resolver.entry_kind(&marker_path).is_ok() {
            resolver.remove_file(&marker_path)?;
        }
    }

    sync_tree(resolver, &stage, target)?;

    // Staging cleanup must complete before the caller records the cache hash.
    if resolver.entry_kind(&stage).is_ok() {
        resolver.remove_dir_all(&stage)?;
    }
    Ok(())
}

/// Overwrite-copy merge of `src` into `dst`: files are copied over existing
/// destinations, directories are created as needed and recursed, and entries
/// present in `dst` but absent from `src` are removed (best-effort) so the
/// destination never accumulates stale artifacts.
pub fn sync_tree(resolver: &PathResolver, src: &GuardedPath, dst: &GuardedPath) -> Result<()> {
    resolver.create_dir_all(dst)?;

    let src_entries = resolver.read_dir_entries(src)?;
    let mut src_names: std::collections::BTreeSet<String> = Default::default();
    for entry in &src_entries {
        src_names.insert(entry.file_name().to_string_lossy().into_owned());
    }

    for entry in &src_entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let src_child = src.join(&name)?;
        let dst_child = dst.join(&name)?;
        match entry.file_type()? {
            ft if ft.is_dir() => {
                resolver.create_dir_all(&dst_child)?;
                sync_tree(resolver, &src_child, &dst_child)?;
            }
            _ => {
                resolver.copy_file(&src_child, &dst_child)?;
            }
        }
    }

    // Remove destination extras the new output no longer contains.
    for entry in resolver.read_dir_entries(dst)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if src_names.contains(&name) || name == ".oxdock_hash" {
            continue;
        }
        let dst_child = dst.join(&name)?;
        match resolver.entry_kind(&dst_child) {
            Ok(EntryKind::Dir) => {
                let _ = resolver.remove_dir_all(&dst_child);
            }
            Ok(EntryKind::File) => {
                let _ = resolver.remove_file(&dst_child);
            }
            Err(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod fingerprint_tests {
    use super::*;
    use oxdock_parser::{Arg, StepKind, parse_script};
    use std::collections::HashMap;

    fn test_lower(name: &str, args: Vec<Arg>) -> Result<StepKind> {
        match name {
            "COPY" => {
                let from = args
                    .first()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("COPY requires source"))?;
                let to = args
                    .get(1)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("COPY requires destination"))?;
                Ok(StepKind::Copy {
                    from_current_workspace: false,
                    from,
                    to,
                })
            }
            "WRITE" => {
                let path = args
                    .first()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("WRITE requires path"))?;
                let contents = args.get(1).cloned();
                Ok(StepKind::Write { path, contents })
            }
            "ECHO" => {
                let msg = args
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("ECHO requires arg"))?;
                Ok(StepKind::Echo(msg))
            }
            "ENV" => {
                let arg = args
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("ENV requires key=val"))?;
                let (k, v) = arg
                    .as_str()
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("ENV requires key=val"))?;
                Ok(StepKind::Env {
                    key: k.to_string(),
                    value: Arg::String(v.to_string(), false),
                })
            }
            _ => anyhow::bail!("unknown command: {name}"),
        }
    }

    fn ctx() -> (oxdock_fs::GuardedTempDir, PathResolver) {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");
        resolver
            .write_file(&root.join("in.txt").unwrap(), b"payload")
            .ok();
        resolver
            .write_file(
                &root.join("script.oxdock").unwrap(),
                b"COPY \"in.txt\" out.txt",
            )
            .ok();
        (temp, resolver)
    }

    const SCRIPT: &str = "COPY in.txt out.txt";

    #[test]
    fn fingerprint_is_deterministic_and_sensitive() -> Result<()> {
        // Hermetic: pin the process-level salt away from other parallel tests.
        let _salt = oxdock_sys_test_utils::TestEnvGuard::remove(FINGERPRINT_SALT_ENV);
        let (_t, resolver) = ctx();
        let steps = parse_script(SCRIPT, test_lower).unwrap();
        let mut envs = HashMap::new();

        let a = asset_input_fingerprint(&resolver, SCRIPT, &steps, "out", &envs).unwrap();
        // Environment drift only applies to keys the script actually
        // references; use an env-referencing variant for that scenario.
        const ENV_SCRIPT: &str = "WRITE out.txt \"{{ env:TAG }}\"";
        let env_steps = parse_script(ENV_SCRIPT, test_lower).unwrap();
        let base_env =
            asset_input_fingerprint(&resolver, ENV_SCRIPT, &env_steps, "out", &envs).unwrap();
        let b = asset_input_fingerprint(&resolver, SCRIPT, &steps, "out", &envs).unwrap();
        assert_eq!(a, b, "deterministic");

        let script_edit =
            asset_input_fingerprint(&resolver, "COPY in.txt other.txt", &steps, "out", &envs)
                .unwrap();
        assert_ne!(a, script_edit, "script edit must invalidate");

        let out_edit = asset_input_fingerprint(&resolver, SCRIPT, &steps, "other", &envs).unwrap();
        assert_ne!(a, out_edit, "out-dir key change must invalidate");

        // Input file content change.
        let root = resolver.root().clone();
        resolver.write_file(&root.join("in.txt").unwrap(), b"payload2")?;
        let input_edit = asset_input_fingerprint(&resolver, SCRIPT, &steps, "out", &envs).unwrap();
        assert_ne!(a, input_edit, "input content edit must invalidate");

        // Environment drift: value change invalidates for referenced keys.
        envs.insert("TAG".into(), "v1".into());
        let env_v1 =
            asset_input_fingerprint(&resolver, ENV_SCRIPT, &env_steps, "out", &envs).unwrap();
        envs.insert("TAG".into(), "v2".into());
        let env_v2 =
            asset_input_fingerprint(&resolver, ENV_SCRIPT, &env_steps, "out", &envs).unwrap();
        assert_ne!(base_env, env_v1, "unset->set must invalidate");
        assert_ne!(env_v1, env_v2, "env value drift must invalidate");
        let _ = env_edit_placeholder(&a);
        Ok(())
    }

    fn env_edit_placeholder(a: &str) -> &str {
        a
    }

    #[test]
    fn salt_changes_digest_only_when_defined() -> Result<()> {
        let (_t, resolver) = ctx();
        let steps = parse_script(SCRIPT, test_lower).unwrap();
        let envs: HashMap<String, String> = HashMap::new();

        let baseline = asset_input_fingerprint(&resolver, SCRIPT, &steps, "out", &envs).unwrap();

        // Defined salt (even empty) shifts the digest...
        let mut salted_empty = envs.clone();
        salted_empty.insert(FINGERPRINT_SALT_ENV.into(), String::new());
        let with_empty_salt =
            asset_input_fingerprint(&resolver, SCRIPT, &steps, "out", &salted_empty).unwrap();
        assert_ne!(baseline, with_empty_salt, "empty-but-defined salt counts");

        // ...and different values differ from each other.
        let mut salted_a = envs.clone();
        salted_a.insert(FINGERPRINT_SALT_ENV.into(), "a".into());
        let salt_a = asset_input_fingerprint(&resolver, SCRIPT, &steps, "out", &salted_a).unwrap();
        assert_ne!(with_empty_salt, salt_a);

        // Unsalted baseline is unaffected by an unrelated process env value
        // because absent-from-map + absent-from-process contributes nothing.
        Ok(())
    }

    #[test]
    fn salt_map_entry_overrides_process_env() -> Result<()> {
        use oxdock_sys_test_utils::TestEnvGuard;

        let (_t, resolver) = ctx();
        let steps = parse_script(SCRIPT, test_lower).unwrap();
        const SCRIPT_REF: &str = SCRIPT;

        let mut map_only = HashMap::new();
        map_only.insert(FINGERPRINT_SALT_ENV.into(), "from-map".into());

        // Process env says something different; the explicit map must win.
        let _proc = TestEnvGuard::set(FINGERPRINT_SALT_ENV, "from-process");
        let with_proc =
            asset_input_fingerprint(&resolver, SCRIPT_REF, &steps, "out", &map_only).unwrap();
        drop(_proc);

        let without_proc =
            asset_input_fingerprint(&resolver, SCRIPT_REF, &steps, "out", &map_only).unwrap();
        assert_eq!(
            with_proc, without_proc,
            "map entry must override process env"
        );
        Ok(())
    }

    #[test]
    fn force_rebuild_truthiness_matrix() {
        use super::{FORCE_REBUILD_ENV, embed_force_rebuild_from};
        let f = embed_force_rebuild_from;
        assert!(f(Some("1".into())));
        assert!(f(Some("true".into())));
        assert!(f(Some("TRUE".into())));
        assert!(f(Some("True".into())));
        assert!(!f(Some("0".into())));
        assert!(!f(Some("yes".into())));
        assert!(!f(Some(String::new())));
        assert!(!f(None));
        // Constant sanity so renames break loudly.
        assert_eq!(FORCE_REBUILD_ENV, "OXDOCK_EMBED_FORCE_REBUILD");
    }

    #[test]
    fn guard_referenced_keys_affect_fingerprint() -> Result<()> {
        // Hermetic: pin the process-level salt away from other parallel tests.
        let _salt = oxdock_sys_test_utils::TestEnvGuard::remove(FINGERPRINT_SALT_ENV);
        let (_t, resolver) = ctx();
        let steps = parse_script("[env:MODE] ECHO on", test_lower).unwrap();
        let mut envs = HashMap::new();
        let unset = asset_input_fingerprint(&resolver, "", &steps, "o", &envs).unwrap();
        envs.insert("MODE".into(), "on".into());
        let set = asset_input_fingerprint(&resolver, "", &steps, "o", &envs).unwrap();
        assert_ne!(unset, set, "guard key unset->set must invalidate");
        Ok(())
    }
}

#[cfg(test)]
mod staging_tests {
    use super::*;
    #[allow(clippy::disallowed_types)]
    use oxdock_fs::UnguardedPath;
    use oxdock_fs::{GuardedPath, PathResolver};

    #[test]
    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir and stage_materialize use real filesystem ops"
    )]
    fn staged_materialize_overwrites_removes_stale_and_strips_markers() -> Result<()> {
        let temp = GuardedPath::tempdir()?;
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone())?;

        // Build output tree.
        let src = root.join("__src")?;
        resolver.create_dir_all(&src)?;
        resolver.write_file(&src.join("keep.txt")?, b"new")?;
        resolver.write_file(&src.join("nested/deep.txt")?, b"d")?;

        // Pre-existing target with stale content + markers.
        let target = root.join("assets")?;
        resolver.create_dir_all(&target)?;
        resolver.write_file(&target.join("keep.txt")?, b"old")?;
        resolver.write_file(&target.join("stale.txt")?, b"stale")?;
        resolver.write_file(&target.join(".oxdock-tempdir")?, b"m")?;

        #[allow(clippy::disallowed_types)]
        let external = UnguardedPath::external(src.as_path().to_path_buf());
        stage_materialize(&resolver, &external, &target)?;

        assert_eq!(
            resolver.read_to_string(&target.join("keep.txt")?)?,
            "new",
            "existing files must be overwritten"
        );
        assert_eq!(
            resolver.read_to_string(&target.join("nested/deep.txt")?)?,
            "d"
        );
        assert!(
            resolver.entry_kind(&target.join("stale.txt")?).is_err(),
            "stale top-level entries must be removed"
        );
        assert!(
            resolver
                .entry_kind(&target.join(".oxdock-tempdir")?)
                .is_err(),
            "sandbox markers must be stripped"
        );
        assert!(
            resolver
                .entry_kind(&target.join(".oxdock-staging")?)
                .is_err(),
            "no staging residue may remain"
        );
        Ok(())
    }
}
