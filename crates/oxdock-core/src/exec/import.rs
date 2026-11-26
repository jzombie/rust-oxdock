//! File imports (#115): load another script as an isolated pipeline.
//!
//! `IMPORT ["<path>" AS <ALIAS>]` resolves `<path>` against the importing
//! file's directory (the entry file's parent at top level, the workspace
//! root for stdin/inline runs), parses the target with the run's module
//! table, registers its top-level `FUNC`s as `ALIAS::NAME` in the
//! caller's scope, and executes its steps in a pushed scope. `RETURN`
//! is the only value channel back; `BREAK`/`CONTINUE` cannot cross the
//! import boundary. Nested aliases register in the outermost import's
//! caller frame, so the importer's `ALIAS::` bodies keep resolving them;
//! the same file under the same alias in the same frame loads once
//! (diamond-safe).

use std::collections::HashMap;

use anyhow::Result;

use super::diagnostics as import_diag;
use oxdock_diagnostics::DiagnosticResult;
use oxdock_fs::{GuardedPath, WorkspaceFs};
use oxdock_parser::{
    ExportKind, Exports, MODULE_SEPARATOR, ModuleTable, Step, StepKind, Value, qualify,
};
use oxdock_process::ProcessManager;

use super::resolve_script_params;
use super::state::{ExecState, RawFuncDef, ScopeAliasFrame};
use super::steps::{Flow, StepCtx, execute_steps};

/// Import-only read exemption (#310.1): the entry tree stays readable
/// for `IMPORT` loads when the script file was explicitly named, even
/// when the entry sits outside the workspace root (workspace stays
/// CWD). Each resolved path stays guard-checked under its own root;
/// general file ops keep workspace containment. Single owner for the
/// fallback so expansion, execution, and dry-run previews agree.
fn read_import_bytes(fs: &dyn WorkspaceFs, resolved: &GuardedPath) -> Result<Vec<u8>> {
    match fs.read_file(resolved) {
        Ok(bytes) => Ok(bytes),
        Err(first) => {
            #[cfg(miri)]
            {
                let _ = resolved;
                Err(first)
            }
            #[cfg(not(miri))]
            {
                // Fallback resolver rooted at the resolved file's own
                // root (entry dir for outside entries): guard-checked
                // like CLI `read_script`, so sibling imports load.
                // Host-backed filesystems only: synthetic backends
                // (mocks, embedded content) fail closed with the
                // original error instead of probing the host disk.
                // Missing files fail here too; return the original
                // workspace error to preserve the containment message.
                if !fs.is_host_backed() {
                    return Err(first);
                }
                let root = resolved.root();
                match oxdock_fs::PathResolver::new(root, root) {
                    Ok(fallback) => match fallback.read_file(resolved) {
                        Ok(bytes) => Ok(bytes),
                        Err(_) => Err(first),
                    },
                    Err(_) => Err(first),
                }
            }
        }
    }
}

/// One previewed step for `--dry-run` (#115, #82): the owning file plus
/// the step itself (line/col ride on the step).
#[derive(Debug, Clone)]
pub struct DryRunStep {
    /// Owning file display path (`<stdin>` for inline runs).
    pub file: String,
    /// The step, with its parsed line/col intact.
    pub step: Step,
}

/// Expand the transitive import closure without executing anything
/// (#115 preview, #82 auditor input): every step of the entry script and
/// of every transitively imported file, pre-order, with `IMPORT` steps
/// included at their position. No handlers run, no processes spawn, no
/// FS writes happen. Cycles and depth overflow fail like execution; parse
/// errors name their file.
/// Maximum nesting depth for file imports. Cycle detection counts exact
/// repetition; this caps deep-but-acyclic chains the way call depth does
/// for functions.
pub(super) const MAX_IMPORT_DEPTH: usize = 32;

pub fn dry_run_closure(
    steps: &[Step],
    entry_label: &str,
    base: &GuardedPath,
    fs: &dyn WorkspaceFs,
    table: &ModuleTable,
) -> Result<Vec<DryRunStep>> {
    let mut out = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut cache: HashMap<String, Vec<Step>> = HashMap::new();
    dry_run_walk(
        steps,
        entry_label,
        base,
        fs,
        table,
        &mut stack,
        &mut cache,
        &mut out,
    )?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn dry_run_walk(
    steps: &[Step],
    file: &str,
    dir: &GuardedPath,
    fs: &dyn WorkspaceFs,
    table: &ModuleTable,
    stack: &mut Vec<String>,
    cache: &mut HashMap<String, Vec<Step>>,
    out: &mut Vec<DryRunStep>,
) -> Result<()> {
    for step in steps {
        out.push(DryRunStep {
            file: file.to_string(),
            step: step.clone(),
        });
        if let StepKind::ImportFile { path, .. } = &step.kind {
            let (key, body, nested_dir) = dry_run_load(fs, dir, path, table, stack, cache)?;
            stack.push(key.clone());
            dry_run_walk(&body, &key, &nested_dir, fs, table, stack, cache, out)?;
            stack.pop();
        }
        if let StepKind::ImportFuncs {
            path,
            func_names,
            type_names,
            ..
        } = &step.kind
        {
            // Harvests never execute the file body: only its nested
            // imports load, mirroring execution. Host type-only imports
            // have no file; skip the load when the path is a known host
            // module and the file is missing.
            let load_result = dry_run_load(fs, dir, path, table, stack, cache);
            match load_result {
                Ok((key, body, nested_dir)) => {
                    stack.push(key.clone());
                    dry_run_walk_top_imports(
                        &body,
                        &key,
                        &nested_dir,
                        fs,
                        table,
                        stack,
                        cache,
                        out,
                    )?;
                    stack.pop();
                }
                Err(e) => {
                    let is_host_type_only = func_names.is_empty()
                        && !type_names.is_empty()
                        && table.modules.contains_key(path);
                    if !is_host_type_only {
                        return Err(e);
                    }
                    // Host type harvest: no file body to preview.
                }
            }
        }
        dry_run_walk_kind(&step.kind, file, dir, fs, table, stack, cache, out)?;
    }
    Ok(())
}

/// Resolve, cycle-check, and parse one preview target, keyed for the
/// per-run cache. Shared by full and harvest imports. Returns the
/// display key, the parsed body, and the file's parent directory.
#[allow(clippy::too_many_arguments)]
fn dry_run_load(
    fs: &dyn WorkspaceFs,
    dir: &GuardedPath,
    path: &str,
    table: &ModuleTable,
    stack: &[String],
    cache: &mut HashMap<String, Vec<Step>>,
) -> Result<(String, Vec<Step>, GuardedPath)> {
    // Same workspace-side join as execution: imported files live in the
    // source tree, never the snapshot. Top-level staged scripts (entry
    // outside the workspace) fall back to the workspace root like
    // validation and execution, so the preview lists what the run loads.
    let resolved = dry_run_resolve(fs, dir, path, stack.is_empty())?;
    let key = resolved.display();
    if stack.iter().any(|p| p == &key) {
        let mut chain = stack.to_vec();
        chain.push(key);
        return Err(anyhow::Error::new(import_diag::import_cycle(&chain)));
    }
    if stack.len() >= MAX_IMPORT_DEPTH {
        return Err(anyhow::Error::new(import_diag::import_depth_limit(
            MAX_IMPORT_DEPTH,
            path,
        )));
    }
    let nested_dir = resolved.parent().unwrap_or_else(|| dir.clone());
    if let Some(cached) = cache.get(&key) {
        return Ok((key, cached.clone(), nested_dir));
    }
    let parsed = read_and_parse_import(fs, &resolved, path, table)?;
    cache.insert(key.clone(), parsed.clone());
    Ok((key, parsed, nested_dir))
}

/// Workspace root with build-context fallback: the shared default
/// directory behind [`import_base`] and dry-run previews. Single
/// owner for the fallback rule; every other spelling is a call here.
fn workspace_fallback(fs: &dyn WorkspaceFs) -> GuardedPath {
    fs.workspace_root()
        .cloned()
        .unwrap_or_else(|| fs.build_context().clone())
}

/// Single top-level import resolver shared by runtime expansion,
/// validation, and dry-run previews. `entry_parent` probes the entry
/// directory first when set; `fallback` applies only when `dir` sits
/// outside it. Nested imports (`top == false`) stay
/// importer-relative. Probes through the passed filesystem so
/// memory-backed tests answer from their own state.
fn resolve_top_level_with(
    fs: &dyn WorkspaceFs,
    dir: &GuardedPath,
    top: bool,
    entry_parent: Option<&GuardedPath>,
    fallback: Option<&GuardedPath>,
    path: &str,
) -> DiagnosticResult<GuardedPath> {
    if top
        && let Some(eparent) = entry_parent
        && eparent != dir
        && let Ok(candidate) = eparent.join(path)
        && fs.entry_kind(&candidate).is_ok()
    {
        return Ok(candidate);
    }
    if let Ok(candidate) = dir.join(path)
        && fs.entry_kind(&candidate).is_ok()
    {
        return Ok(candidate);
    }
    if top
        && let Some(root) = fallback
        // Both sides arrive canonicalized by construction
        // (`guard_path` canonicalizes through the nearest existing
        // ancestor), so a raw prefix check already compares canonical
        // spellings.
        && root != dir
        && !dir.as_path().starts_with(root.as_path())
        && let Ok(candidate) = root.join(path)
        && fs.entry_kind(&candidate).is_ok()
    {
        return Ok(candidate);
    }
    dir.join(path)
        .map_err(|e| import_diag::import_cannot_resolve(path, &e))
}

/// Preview-phase entry point: no entry handle here, so the workspace
/// root is the fallback directly. Nested imports stay
/// importer-relative.
fn dry_run_resolve(
    fs: &dyn WorkspaceFs,
    dir: &GuardedPath,
    path: &str,
    is_top: bool,
) -> Result<GuardedPath> {
    let fallback = workspace_fallback(fs);
    resolve_top_level_with(fs, dir, is_top, None, is_top.then_some(&fallback), path)
        .map_err(anyhow::Error::from)
}

/// Preview a harvest target: only its top-level nested imports load,
/// mirroring [`import_funcs`].
#[allow(clippy::too_many_arguments)]
fn dry_run_walk_top_imports(
    steps: &[Step],
    file: &str,
    dir: &GuardedPath,
    fs: &dyn WorkspaceFs,
    table: &ModuleTable,
    stack: &mut Vec<String>,
    cache: &mut HashMap<String, Vec<Step>>,
    out: &mut Vec<DryRunStep>,
) -> Result<()> {
    for step in steps {
        match &step.kind {
            StepKind::ImportFile { .. } | StepKind::ImportFuncs { .. } => {
                out.push(DryRunStep {
                    file: file.to_string(),
                    step: step.clone(),
                });
            }
            _ => {}
        }
    }
    for step in steps {
        if let StepKind::ImportFile { path, .. } = &step.kind {
            let (key, body, nested_dir) = dry_run_load(fs, dir, path, table, stack, cache)?;
            stack.push(key.clone());
            dry_run_walk(&body, &key, &nested_dir, fs, table, stack, cache, out)?;
            stack.pop();
        }
        if let StepKind::ImportFuncs {
            path,
            func_names,
            type_names,
            ..
        } = &step.kind
        {
            match dry_run_load(fs, dir, path, table, stack, cache) {
                Ok((key, body, nested_dir)) => {
                    stack.push(key.clone());
                    dry_run_walk_top_imports(
                        &body,
                        &key,
                        &nested_dir,
                        fs,
                        table,
                        stack,
                        cache,
                        out,
                    )?;
                    stack.pop();
                }
                Err(e) => {
                    let is_host_type_only = func_names.is_empty()
                        && !type_names.is_empty()
                        && table.modules.contains_key(path);
                    if !is_host_type_only {
                        return Err(e);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Recurse into a step's nested bodies so the preview names every step,
/// not just top-level ones. Bodies never introduce imports by themselves;
/// `ImportFile` is handled by the caller.
#[allow(clippy::too_many_arguments)]
fn dry_run_walk_kind(
    kind: &StepKind,
    file: &str,
    dir: &GuardedPath,
    fs: &dyn WorkspaceFs,
    table: &ModuleTable,
    stack: &mut Vec<String>,
    cache: &mut HashMap<String, Vec<Step>>,
    out: &mut Vec<DryRunStep>,
) -> Result<()> {
    for body in child_bodies(kind) {
        dry_run_walk(body, file, dir, fs, table, stack, cache, out)?;
    }
    Ok(())
}

/// Resolve, read, and parse one import target for preview: same loader
/// rules as execution, minus validation and registration.
fn read_and_parse_import(
    fs: &dyn WorkspaceFs,
    resolved: &GuardedPath,
    path: &str,
    table: &ModuleTable,
) -> Result<Vec<Step>> {
    let bytes = read_import_bytes(fs, resolved)
        .map_err(|e| anyhow::Error::new(import_diag::import_cannot_read(path, &e)))?;
    let text = std::str::from_utf8(&bytes).map_err(|e| {
        anyhow::Error::new(import_diag::import_bad_utf8(path, &anyhow::Error::new(e)))
    })?;
    let table = table.clone();
    let mut reserved = super::native::builtin_function_names();
    reserved.extend(table.reserved_base_names());
    oxdock_parser::parse_script_with_modules(text, crate::lower_command, reserved, table).map_err(
        |e| {
            anyhow::Error::new(import_diag::import_parse_failed(
                path,
                &anyhow::Error::new(e),
            ))
        },
    )
}

/// Execute `IMPORT [[F, G FROM "<path>" (AS <ALIAS>)?]]`: load the
/// file, register only the named top-level `FUNC`s (bare, or under the
/// alias), and execute the file's own top-level nested imports so the
/// harvested functions keep their helpers. Nothing else in the file
/// runs: no top-level steps execute, so cherry-picking never triggers
/// side effects.
/// Single owner for harvest validity: every `IMPORT [NAME FROM ...]`
/// request, static or runtime, gates here. Export gate first, then the
/// defined check, with the exact historical messages, so all stages
/// fail identically on the same input. A second copy of either scan
/// anywhere else is piecemealing: route it here instead.
pub(crate) fn gate_harvested_name(
    kind: ExportKind,
    steps: &[Step],
    exports: &Exports,
    name: &str,
    path: &str,
) -> DiagnosticResult<()> {
    let (exported, kind, noun, defined) = match kind {
        ExportKind::Func => (
            &exports.funcs,
            "FUNC",
            "function",
            steps.iter().any(|step| {
                matches!(&step.kind, StepKind::FuncDef { name: def, .. } if def.as_str() == name)
            }),
        ),
        ExportKind::Type => (
            &exports.types,
            "TYPE",
            "TYPE",
            steps.iter().any(|step| {
                matches!(&step.kind, StepKind::TypeAlias { name: alias, .. } if alias.as_str() == name)
            }),
        ),
        // `CONST` harvests do not exist: reads arrive as `ALIAS::NAME`
        // expressions through the forward chase, never as harvest
        // requests. The arm exists so callers never restate the kind
        // split; the export scan still applies if one ever arrives.
        ExportKind::Const => (&exports.consts, "CONST", "CONST", true),
    };
    if !exported.iter().any(|e| e == name) {
        return Err(import_diag::import_missing_export(path, kind, name));
    }
    if !defined {
        return Err(import_diag::import_unknown_member(path, noun, name));
    }
    Ok(())
}

/// Execute `IMPORT [[F, G FROM "<path>" (AS <ALIAS>)?]]`: load the
/// file, register only the named top-level `FUNC`s (bare, or under the
/// alias), and execute the file's own top-level nested imports so the
/// harvested functions keep their helpers. Nothing else in the file
/// runs: no top-level steps execute, so cherry-picking never triggers
/// side effects.
pub(crate) fn import_funcs<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    path: &str,
    alias: Option<&str>,
    func_names: &[String],
    type_names: &[String],
) -> Result<()> {
    // Host plugin types use the same `IMPORT [TYPE X FROM MOD]` syntax as
    // script types: when the path names a known host module (not a file)
    // and only types are harvested, validate the types resolve globally
    // (host plus script aliases, like `resolve_tag`) and return. Types
    // are global, so no persistence is needed; the import documents the
    // dependency and fails fast on typos. Mixed func+type harvests from
    // host modules stay unsupported: use `IMPORT [MOD]` for host funcs.
    if func_names.is_empty() && !type_names.is_empty() {
        // Entry-first probe shared with expansion: a sibling file named
        // like a host module routes to the file when present.
        let base = import_base(cx.state);
        let stack = cx.state.import_stack.clone();
        let file_exists = top_level_file_exists(cx.state, &base, &stack, path);
        if !file_exists && cx.state.module_table.modules.contains_key(path) {
            for name in type_names {
                cx.state.resolve_tag(name).map_err(|e| {
                    anyhow::Error::new(import_diag::import_unknown_type(
                        path,
                        name,
                        Some(&anyhow::Error::new(e)),
                    ))
                })?;
            }
            if let Some(alias) = alias {
                // Harvest aliases register like file imports; no funcs
                // to define. Recorded with no defining file so the
                // runtime placeholder gate still treats the alias
                // as known.
                record_executed_alias(cx.state, alias, None);
            }
            return Ok(());
        }
    }
    let LoadedFile {
        resolved,
        steps,
        exports,
    } = load_import(cx.state, path)?;
    let key = resolved.display();
    if let Some(alias) = alias {
        // Record under the importing unit for the runtime placeholder
        // gate (see `import_file`): zero-function harvests leave no
        // registry keys, so only this record keeps the alias known.
        record_executed_alias(cx.state, alias, Some(key.clone()));
    }
    // Join the import stack like a full import: nested loads need this
    // file on the chain for cycle/depth detection and for
    // importer-relative resolution.
    cx.state.import_stack.push(resolved.clone());
    let saved_aliases = cx.state.type_aliases.clone();
    let outcome: Result<()> = (|| {
        let aliases = super::typing::collect_type_aliases(&steps, cx.state)?;
        for (name, target) in aliases {
            cx.state.register_alias(name, target);
        }
        // Proven once, trusted thereafter: files the static closure
        // pass validated (full nested-import knowledge, same cached
        // body) skip the single-file re-proof, which would only
        // false-positive on nested calls. Unseen files still validate.
        if !cx.state.validated_modules.contains(&key) {
            super::static_check::validate_import_body_types(&steps, cx.state, &key)
                .map_err(|e| import_diag::import_failed_validation(path, &anyhow::Error::new(e)))?;
        }
        // Stateful harvests are rejected: the file body never runs, so
        // there is no initialized state for them to observe. Import the
        // file to share its module instead.
        if let Some(offender) =
            super::static_check::harvest_touches_module(cx.state, &key, &steps, func_names)
                .map_err(|e| import_diag::import_failed_validation(path, &anyhow::Error::new(e)))?
        {
            return Err(anyhow::Error::new(import_diag::import_stateful_harvest(
                path, &offender,
            )));
        }
        for name in func_names {
            // Harvest validity lives in `gate_harvested_name` (single
            // owner): unexported means private, undefined means no such
            // function. Both fail here, before anything registers.
            gate_harvested_name(ExportKind::Func, &steps, &exports, name, path)?;
            let Some(def) = steps.iter().find_map(|step| match &step.kind {
                StepKind::FuncDef {
                    name: def_name,
                    params,
                    return_type,
                    is_async,
                    body,
                    ..
                } if def_name == name => {
                    Some((params.clone(), return_type.clone(), *is_async, body.clone()))
                }
                _ => None,
            }) else {
                // Gated above: the scan already proved the def exists.
                // This preserves loud failure without restating it.
                return Err(anyhow::Error::new(import_diag::import_unknown_function(
                    path, name,
                )));
            };
            let resolved_params = resolve_script_params(cx.state, name, &def.0)?;
            let return_tag = cx.state.resolve_tag(&def.1)?;
            cx.state
                .check_return_tag(&format!("function `{name}` returns"), &return_tag)?;
            // Owner-tagged with the module: calls resolve names against
            // the shared store. Re-imports skip already-registered
            // names, so shared views stay idempotent. The guard reads
            // the canonical file key, never the importer alias.
            match alias {
                Some(_) => {
                    let canonical = super::native::file_func_key(&key, name);
                    if cx.state.functions.get(&canonical).is_none() {
                        cx.state.functions.define_file_func(
                            name,
                            &resolved_params,
                            return_tag.clone(),
                            &def.1,
                            def.2,
                            &def.3,
                            &key,
                        )?;
                    }
                }
                None => {
                    if cx.state.functions.get(name).is_none() {
                        cx.state.functions.define_script(
                            name,
                            &resolved_params,
                            return_tag.clone(),
                            &def.1,
                            def.2,
                            &def.3,
                            Some(&key),
                            None,
                        )?;
                    }
                    // Harvest visibility per importing unit (#322):
                    // record the defining file so calls resolve through
                    // the harvest map, surviving init pops that evict
                    // scope frames. Last harvest wins, mirroring the
                    // registry slot it replaces for lookup.
                    let unit = active_unit_key(cx.state);
                    cx.state
                        .harvest_map
                        .insert((unit, name.clone()), key.clone());
                }
            }
        }
        // Explicit TYPE harvests: gate against `EXPORT TYPE`, then
        // persist bare (plus qualified when aliased, exactly like
        // funcs) with collision checks. Harvesting an unexported type
        // fails here, before anything registers.
        {
            let file_aliases = super::typing::collect_type_aliases(&steps, cx.state)?;
            for name in type_names {
                // Same single owner as funcs above: gate first, persist
                // after. The alias fetch below cannot miss (gated), but
                // keeps loud failure instead of restating the scan.
                gate_harvested_name(ExportKind::Type, &steps, &exports, name, path)?;
                let Some(target) = file_aliases.get(name) else {
                    return Err(anyhow::Error::new(import_diag::import_unknown_type(
                        path, name, None,
                    )));
                };
                insert_imported_type(cx.state, name, target, alias, path)?;
            }
        }
        // Nested imports run per their own form with the harvested
        // file on the context stack, so unit attribution matches the
        // static pass.
        cx.state.module_stack.push(Some(key.clone()));
        let mut nested_cause: Option<anyhow::Error> = None;
        for step in &steps {
            match &step.kind {
                StepKind::ImportFile { path, alias } => {
                    if let Err(e) = import_file(cx, path, alias) {
                        nested_cause = Some(e);
                        break;
                    }
                }
                StepKind::ImportFuncs {
                    path,
                    alias,
                    func_names,
                    type_names: nested_types,
                } => {
                    if let Err(e) =
                        import_funcs(cx, path, alias.as_deref(), func_names, nested_types)
                    {
                        nested_cause = Some(e);
                        break;
                    }
                }
                _ => {}
            }
        }
        let _ = cx.state.module_stack.pop();
        if let Some(e) = nested_cause {
            return Err(e);
        }
        // Jail setup for harvested FUNCs (#322): the file body never
        // ran on this path, so a module top-level `WORKSPACE` or
        // `WORKDIR` would be dropped and harvested FUNCs would serve
        // from the snapshot default instead of the module jail.
        // Ensuring init when the top-level sets a jail gives harvests
        // the exact jail whole-file imports observe; cached inits
        // return without re-running. Modules without a top-level jail
        // skip init entirely, so pure harvests stay side-effect free
        // (see `file_import_from_bare_harvest_skips_body`). Stateful
        // harvests already failed above, so only pure names reach
        // their jail.
        if module_top_level_sets_jail(&steps) {
            ensure_module_init(cx, &key, &resolved, &steps, path)?;
        }
        Ok(())
    })();
    // import_funcs pushes no scope (nothing executes), so only the
    // import stack unwinds here.
    let _ = cx.state.import_stack.pop();
    // Harvest persists only the explicitly requested types (plus nested
    // imports' contributions, which already persisted through their own
    // calls): restore the pre-import map, then re-insert what this site
    // imported. Funcs persist through the registry (no restore needed).
    // Types merge bare globally (like host types), so no qualified keys.
    {
        let mut persisted: Vec<(String, String)> = Vec::new();
        for name in type_names {
            // Re-read from the post-import map: nested imports may have
            // run after the initial insert, so capture the final target.
            if let Some(target) = cx.state.type_aliases.get(name).cloned() {
                persisted.push((name.clone(), target));
            }
        }
        cx.state.restore_aliases(saved_aliases);
        for (name, target) in persisted {
            // Collision-checked: re-importing the same target is
            // idempotent, differing targets fail naming both files.
            if let Some(existing) = cx.state.type_aliases.get(&name) {
                if existing != &target {
                    return Err(anyhow::Error::new(import_diag::import_type_collision(
                        path, &name, existing, &target,
                    )));
                }
            } else {
                cx.state.register_alias(name, target);
            }
        }
    }
    outcome
}

/// Insert one harvested or whole-file type into the importer's map (bare
/// global, like host types). Same target re-imports idempotently;
/// differing targets fail. Qualified `ALIAS::TYPE` spellings stay
/// future work: whole-file func views namespace, but types merge
/// globally so `LET $x: PERSON` works after any import.
/// Persist one harvested or exported type under the importer's roof.
/// Unaliased harvests land bare (explicit global request, collision
/// checked); anything imported `AS C` persists only as `C::NAME`,
/// mirroring func views, so no file ever leaks bare names into its
/// importer. Same target re-imports are idempotent, differing targets
/// fail naming both files.
pub(crate) fn insert_imported_type<P: ProcessManager>(
    state: &mut ExecState<P>,
    name: &str,
    target: &str,
    alias: Option<&str>,
    path: &str,
) -> DiagnosticResult<()> {
    let key = match alias {
        Some(alias) => qualify(alias, name),
        None => name.to_string(),
    };
    if let Some(existing) = state.type_aliases.get(&key) {
        if existing != target {
            return Err(import_diag::import_type_collision(
                path, name, existing, target,
            ));
        }
    } else {
        state.register_alias(key, target.to_string());
    }
    Ok(())
}
/// Execute `IMPORT ["<path>" AS <ALIAS>]`: initialize the module on
/// first import (executing its body once and draining its top-level
/// `LET`s into the shared store), then register its `EXPORT`ed
/// `FUNC`s as views under the alias. Later imports of the same file
/// skip execution and reuse the initialized module, binding the
/// cached `RETURN` value for `LET`-capture. Returns the init body's
/// `RETURN` value (`""` on fallthrough).
/// Resolve a file's own top-level TYPE harvests into the global
/// type directory (EXPORT-gated, collision-checked). Lets a file's
/// declared dependencies satisfy its own signatures and bodies
/// without every importer repeating them. Mirrors the `import_funcs`
/// harvest path; harvested funcs and nested execution still happen
/// later. Idempotent (same target) so validation, persistence, and
/// re-import points can all share it. Pushes the file while resolving
/// so nested paths join importer-relative, exactly like execution.
fn resolve_own_type_harvests<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    steps: &[Step],
    resolved: &GuardedPath,
) -> Result<()> {
    let pushed = if cx.state.import_stack.iter().any(|p| p == resolved) {
        false
    } else {
        cx.state.import_stack.push(resolved.clone());
        true
    };
    let outcome = resolve_own_type_harvests_inner(cx, steps);
    if pushed {
        let _ = cx.state.import_stack.pop();
    }
    outcome
}

fn resolve_own_type_harvests_inner<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    steps: &[Step],
) -> Result<()> {
    for step in steps {
        let StepKind::ImportFuncs {
            path: harvest_path,
            alias: harvest_alias,
            func_names: harvest_funcs,
            type_names: harvest_types,
        } = &step.kind
        else {
            continue;
        };
        if harvest_types.is_empty() {
            continue;
        }
        if harvest_funcs.is_empty() {
            let base = import_base(cx.state);
            let resolved_opt = base.join(harvest_path).ok();
            let file_exists = resolved_opt
                .as_ref()
                .is_some_and(|r| cx.state.fs.entry_kind(r).is_ok());
            if !file_exists && cx.state.module_table.modules.contains_key(harvest_path) {
                for name in harvest_types {
                    cx.state.resolve_tag(name).map_err(|e| {
                        anyhow::Error::new(import_diag::import_unknown_type(
                            harvest_path,
                            name,
                            Some(&anyhow::Error::new(e)),
                        ))
                    })?;
                }
                continue;
            }
        }
        let harvested = load_import(cx.state, harvest_path)?;
        let file_aliases = super::typing::collect_type_aliases(&harvested.steps, cx.state)?;
        for name in harvest_types {
            // Same single owner as `import_funcs`: gate first, persist
            // after. The fetch below cannot miss (gated), but keeps loud
            // failure instead of restating the scan.
            gate_harvested_name(
                ExportKind::Type,
                &harvested.steps,
                &harvested.exports,
                name,
                harvest_path,
            )?;
            let Some(target) = file_aliases.get(name) else {
                return Err(anyhow::Error::new(import_diag::import_unknown_type(
                    harvest_path,
                    name,
                    None,
                )));
            };
            insert_imported_type(
                cx.state,
                name,
                target,
                harvest_alias.as_deref(),
                harvest_path,
            )?;
        }
    }
    Ok(())
}

/// Whether a module top-level establishes its own jail (#322): a
/// direct top-level `WORKSPACE` or `WORKDIR` step, whether or not
/// guard-gated. Block scopes (`IF`, `FOR`, `WHILE`, `SWITCH`,
/// `ISOLATED`, `TIMEOUT`) unwind `WORKSPACE` and `WORKDIR` on exit
/// and can never establish a module jail, so nested bodies never
/// count. Nested `FUNC` bodies never count: they run in their own
/// call jail, never the init flow. Single owner for the harvest
/// init-requirement gate, so the classifier can never disagree with
/// the init it triggers.
fn module_top_level_sets_jail(steps: &[Step]) -> bool {
    steps.iter().any(|step| step_sets_jail(&step.kind))
}

/// One step's contribution to [`module_top_level_sets_jail`]: only
/// direct top-level jail setters count. Block-nested setters unwind
/// on block exit and nested `FUNC` bodies run in their own call
/// jail, so neither can establish the module jail.
fn step_sets_jail(kind: &StepKind) -> bool {
    matches!(kind, StepKind::Workspace(_) | StepKind::Workdir(_))
}

/// Run a module top-level init body once per file (#322): the jail
/// the module FUNCs restore on every call. Shared by full imports
/// and harvests so both observe the same module context: a harvest
/// that skipped init would serve its FUNCs from the snapshot root
/// instead of the module top-level `WORKSPACE`/`WORKDIR`. Cached
/// inits return the stored value without re-running. The caller
/// holds no stack entry; this pushes `resolved` for the run and pops
/// it after, restoring every swapped scope on all paths.
fn ensure_module_init<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    key: &str,
    resolved: &GuardedPath,
    steps: &[Step],
    path: &str,
) -> Result<Value> {
    // Cached init: every alias shares the first run's state and
    // `RETURN`. Nothing pushed below, so unwinds stay balanced.
    if let Some(init) = cx
        .state
        .modules
        .get(key)
        .and_then(|record| record.init.as_ref())
    {
        return Ok(init.return_value.clone());
    }
    // The init body observes nothing of the importer: the whole
    // variable stack swaps for a fresh one, exactly like a
    // module-owned call, and restores after the scope pops. The
    // workspace, cwd, and env reset to the snapshot jail and seed
    // first, so the init top-level establishes the module own
    // context from a fixed default, never caller ambient (#322).
    let saved_vars = std::mem::take(&mut cx.state.var_scopes);
    let saved_consts = std::mem::take(&mut cx.state.const_scopes);
    let saved_aliases = cx.state.type_aliases.clone();
    let saved_isolated = std::mem::take(&mut cx.state.isolated_stack);
    let outcome: Result<Value> = (|| {
        let aliases = super::typing::collect_type_aliases(steps, cx.state)?;
        for (name, target) in aliases {
            cx.state.register_alias(name, target);
        }
        cx.state.import_stack.push(resolved.clone());
        // Mark the isolation scope as a module top level (not a
        // lexical block): the init body's own direct imports land on
        // the unit-top map like the entry's, while nested blocks
        // still unwind through scope frames. Removed with the scope
        // below on every path.
        cx.state
            .init_scope_depths
            .push(cx.state.scope_stack.len() + 1);
        cx.state.push_scope();
        // Frame floor for the nested walk (#322): the init body's
        // own frames start here. Popped with the scope below on
        // every path.
        cx.state
            .func_floors
            .push(cx.state.functions.scope_depth().saturating_sub(1));
        // Definition-site jails seal only at file top-level
        // (`call_depth == 0`): a module first imported inside a `FUNC`
        // body would otherwise run its init with `call_depth >= 1`
        // and silently drop every `def_jail`. Reset across init so
        // top-level `FUNC`s seal their site no matter the importer.
        // Balanced calls inside the body increment from zero and
        // restore to zero before the outer depth returns.
        let saved_call_depth = cx.state.call_depth;
        cx.state.call_depth = 0;
        // Executed `INHERIT_ENV` keys accumulate here as they run,
        // drained into `ModuleInit::inherit_names` after execution.
        cx.state.init_inherit_stack.push(Vec::new());
        // Fixed default jail for the module top-level (#322): the
        // snapshot root with the seed env. Caller workspace, cwd,
        // env, and isolated scrubs never flow into init. The init
        // body own WORKSPACE, WORKDIR, and ENV statements then build
        // the module context its FUNCs restore on every call.
        cx.state.fs.switch_to_snapshot();
        cx.state.cwd = cx.state.fs.root().clone();
        cx.state.envs = std::sync::Arc::new((*cx.state.seed).clone());
        cx.state.isolated_stack = Vec::new();
        // Baseline for the call-time env delta: the seed. Every init
        // contribution counts, importer values cannot appear by
        // construction since the live env already reset above.
        let env_before: std::collections::HashMap<String, String> = (*cx.state.seed).clone();
        // The init body runs with its module on the context stack so
        // nested definitions owner-tag correctly; lexical reads still
        // resolve through the pushed frames first.
        cx.state.module_stack.push(Some(key.to_string()));
        let flow = execute_steps(
            cx.state,
            cx.process,
            steps,
            cx.stdin.clone(),
            false,
            cx.out.clone(),
            cx.err.clone(),
            cx.out_pipe.clone(),
            cx.stdin_pipe.clone(),
            false,
        );
        // Unwind the module context on every path: nested definitions
        // below already owner-tagged while it was up. Restore the
        // caller call depth and drain executed `INHERIT_ENV` keys
        // before propagating errors, so failures never leak init
        // state into the importer.
        let _ = cx.state.module_stack.pop();
        let inherit_names = cx.state.init_inherit_stack.pop().unwrap_or_default();
        cx.state.call_depth = saved_call_depth;
        let flow = flow?;
        // Drain the init frame's top-level bindings into the shared
        // store before the scope pops: this is the module's global
        // state, visible to every module-owned function and alias.
        // Capture the init-final jail alongside: the workspace
        // selection and cwd the top-level left behind become the
        // module default its FUNCs restore on entry (#322). The pop
        // below restores the importer afterwards.
        let drained = std::mem::take(
            cx.state
                .var_scopes
                .last_mut()
                .expect("import pushed exactly one var scope"),
        );
        let drained_consts = std::mem::take(
            cx.state
                .const_scopes
                .last_mut()
                .expect("import pushed exactly one const scope"),
        );
        // Capture the init-final jail alongside: the workspace
        // selection and cwd as the top-level left them. Def-carrying
        // calls restore their own definition-site jail instead; this
        // final state serves materialized defs that never executed.
        // The pop below restores the importer afterwards.
        let init_selection = cx.state.fs.selection();
        let init_cwd = cx.state.cwd.clone();
        let value = match flow {
            Flow::Done => Value::string(String::new()),
            Flow::Return { value, .. } => value,
            Flow::Break { idx } => {
                return Err(anyhow::Error::new(
                    import_diag::import_flow_crosses_boundary("BREAK", idx + 1, path),
                ));
            }
            Flow::Continue { idx } => {
                return Err(anyhow::Error::new(
                    import_diag::import_flow_crosses_boundary("CONTINUE", idx + 1, path),
                ));
            }
        };
        let record = cx.state.modules.entry(key.to_string()).or_default();
        // Module call-time env (#115): the session seed plus this
        // init's own contributions. Comparing against the seed keeps
        // importer-established values out, so module functions never
        // inherit caller context. Removals (e.g. `INHERIT_ENV` of a
        // removed name) drop seed rows too. A re-inherit of an
        // already-present value leaves no delta, so `INHERIT_ENV`
        // names are additionally recorded for per-call
        // re-establishment from the host.
        let after = cx.state.envs.clone();
        let mut module_env = (*cx.state.seed).clone();
        for (name, value) in after.iter() {
            if env_before.get(name) != Some(value) {
                module_env.insert(name.clone(), value.clone());
            }
        }
        for name in env_before.keys() {
            if !after.contains_key(name) {
                module_env.remove(name);
            }
        }
        // `inherit_names` were recorded as `INHERIT_ENV` steps actually
        // executed (see `handlers::inherit_env`); no post-hoc guard
        // re-evaluation against final envs, so later `ENV` mutations
        // cannot rewrite which names ran.
        record.init = Some(super::state::ModuleInit {
            vars: drained,
            const_vars: drained_consts,
            return_value: value.clone(),
            envs: module_env,
            inherit_names,
            selection: init_selection,
            cwd: init_cwd,
        });
        Ok(value)
    })();
    let pop_res = cx.state.pop_scope();
    let _ = cx.state.func_floors.pop();
    let init_depth = cx.state.scope_stack.len() + 1;
    cx.state
        .init_scope_depths
        .retain(|depth| *depth != init_depth);
    cx.state.var_scopes = saved_vars;
    cx.state.const_scopes = saved_consts;
    cx.state.isolated_stack = saved_isolated;
    let _ = cx.state.import_stack.pop();
    cx.state.restore_aliases(saved_aliases);
    match (outcome, pop_res) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e),
    }
}

pub(crate) fn import_file<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    path: &str,
    alias: &str,
) -> Result<Value> {
    let LoadedFile {
        resolved,
        steps,
        exports,
    } = load_import(cx.state, path)?;
    let key = resolved.display();
    // Record under the importing unit for the runtime placeholder gate:
    // zero-function imports leave no registry keys, so only this record
    // keeps the alias known (mirrors static `imported_aliases`).
    record_executed_alias(cx.state, alias, Some(key.clone()));
    // Merge the imported file's type aliases for its own validation and
    // execution; restored after the import returns so aliases never leak
    // across the import boundary except through `EXPORT TYPE`.
    let saved_aliases = cx.state.type_aliases.clone();
    // Phase 1 (no pushes): merge aliases, validate, register views.
    // Runs on every import so new aliases attach and bodies re-check.
    let phase_one: Result<Option<Value>> = (|| {
        let aliases = super::typing::collect_type_aliases(&steps, cx.state)?;
        for (name, target) in &aliases {
            cx.state.register_alias(name.clone(), target.clone());
        }
        // Self-sufficiency: the file's own harvests satisfy its own
        // signatures (see `resolve_own_type_harvests`).
        resolve_own_type_harvests(cx, &steps, &resolved)?;
        // Entry parity: statically validate the imported body before its
        // first step runs. Nested imports validate when their own IMPORT
        // steps execute.
        // Proven once, trusted thereafter: files the static closure
        // pass validated (full nested-import knowledge, same cached
        // body) skip the single-file re-proof, which would only
        // false-positive on nested calls. Unseen files still validate.
        if !cx.state.validated_modules.contains(&key) {
            super::static_check::validate_import_body_types(&steps, cx.state, &key)
                .map_err(|e| import_diag::import_failed_validation(path, &anyhow::Error::new(e)))?;
        }
        // Register the imported file's EXPORTed top-level FUNCs under
        // the alias in the caller's scope: unexported means private.
        // Owner-tagged with the module so calls share its state.
        // Skips names already present: re-imports only add new views.
        let func_exported: std::collections::HashSet<&str> =
            exports.funcs.iter().map(|e| e.as_str()).collect();
        for step in &steps {
            if let StepKind::FuncDef {
                name,
                params,
                return_type,
                is_async,
                body,
                ..
            } = &step.kind
                && func_exported.contains(name.as_str())
            {
                let canonical = super::native::file_func_key(&key, name);
                if cx.state.functions.get(&canonical).is_none() {
                    let resolved_params = resolve_script_params(cx.state, name, params)?;
                    let return_tag = cx.state.resolve_tag(return_type)?;
                    cx.state
                        .check_return_tag(&format!("function `{name}` returns"), &return_tag)?;
                    cx.state.functions.define_file_func(
                        name,
                        &resolved_params,
                        return_tag,
                        return_type,
                        *is_async,
                        body,
                        &key,
                    )?;
                }
            }
        }
        // Persist the file's `EXPORT TYPE` names into the importer:
        // bare plus `ALIAS::NAME` (mirroring func views), gated so
        // unexported aliases never leak. Same-target re-imports are
        // idempotent; differing targets fail.
        {
            let type_exported: std::collections::HashSet<&str> =
                exports.types.iter().map(|e| e.as_str()).collect();
            for (name, target) in &aliases {
                if !type_exported.contains(name.as_str()) {
                    continue;
                }
                insert_imported_type(cx.state, name, target, Some(alias), path)?;
            }
        }
        // Initialized modules skip execution: every alias shares the
        // first init's state and `RETURN`. Nothing pushed below, so the
        // outer unwinds stay balanced.
        if let Some(init) = cx
            .state
            .modules
            .get(&key)
            .and_then(|record| record.init.as_ref())
        {
            return Ok(Some(init.return_value.clone()));
        }
        Ok(None)
    })();
    cx.state.restore_aliases(saved_aliases.clone());
    let cached = phase_one?;
    // Whole-file type persistence: exported types survive the restore,
    // bare plus `ALIAS::NAME`, exactly like func views. Runs on every
    // import (including cached re-imports) so new aliases attach.
    {
        if let Ok(aliases) = super::typing::collect_type_aliases(&steps, cx.state) {
            // Temporarily merge full aliases so nested-type targets
            // resolve, then filter to exported for persistence.
            for (name, target) in &aliases {
                cx.state.register_alias(name.clone(), target.clone());
            }
            let type_exported: std::collections::HashSet<&str> =
                exports.types.iter().map(|e| e.as_str()).collect();
            // Capture first to avoid borrow issues, then restore and
            // re-insert only exported.
            let mut to_persist: Vec<(String, String)> = Vec::new();
            for (name, target) in &aliases {
                if type_exported.contains(name.as_str()) {
                    to_persist.push((name.clone(), target.clone()));
                }
            }
            cx.state.restore_aliases(saved_aliases.clone());
            for (name, target) in to_persist {
                insert_imported_type(cx.state, &name, &target, Some(alias), path)?;
            }
        } else {
            cx.state.restore_aliases(saved_aliases.clone());
        }
    }
    if let Some(value) = cached {
        return Ok(value);
    }
    // Phase 2 (init): runs once per file. Shared with harvests so
    // both observe the same module jail (see `ensure_module_init`);
    // the file's own harvest re-application below stays
    // full-import-only.
    let outcome = ensure_module_init(cx, &key, &resolved, &steps, path);
    // Re-apply the file's own harvests after the init restore: the
    // restore drops everything but exports, while harvested funcs
    // persist through the registry. Types persist like funcs do, so
    // the file's bodies resolve at call time exactly as validated.
    if outcome.is_ok() {
        resolve_own_type_harvests(cx, &steps, &resolved)?;
    }
    outcome
}

/// One site importing a file in the closure (#115): the alias it
/// registers under (`None` for bare harvests) and the harvested names
/// (`None` for whole-file imports, which register everything).
/// Func and type harvests track separately so `IMPORT [TYPE T, FUNC F
/// FROM "f"]` gates each kind against its own export list.
#[derive(Debug, Clone)]
pub(super) struct ClosureSite {
    pub alias: Option<String>,
    pub func_names: Option<Vec<String>>,
    pub type_names: Option<Vec<String>>,
}

/// One file in the transitive import closure (#115): the resolved path
/// plus the parsed body, with every site that loads it. Sites accumulate
/// across the walk (diamonds), each validated once. `path` is the
/// literal spelling from the first import statement, for error messages;
/// `dir` is the directory nested harvests resolve against (the file's
/// own directory, so per-file validation needs no filesystem).
#[derive(Debug, Clone)]
pub(super) struct ClosureFile {
    pub key: String,
    pub path: String,
    pub dir: GuardedPath,
    pub steps: Vec<Step>,
    pub exports: Exports,
    pub sites: Vec<ClosureSite>,
}

/// Expand the transitive import closure without executing anything:
/// every file reachable from `steps` through `ImportFile`/`ImportFuncs`
/// steps, including conditional ones (validation is conservative).
/// Cycles and depth overflow fail here, before the first step runs.
/// Parsed bodies land in the run's module registry, so execution reuses
/// them instead of re-reading.
pub(crate) fn expand_import_closure<P: ProcessManager>(
    state: &mut ExecState<P>,
    steps: &[Step],
) -> DiagnosticResult<Vec<ClosureFile>> {
    let base = import_base(state);
    let mut stack: Vec<GuardedPath> = Vec::new();
    let mut files: Vec<ClosureFile> = Vec::new();
    expand_walk(state, steps, &base, &mut stack, &mut files)?;
    Ok(files)
}

/// Walk steps collecting import sites. Full imports recurse into the
/// whole body (conditional nested imports included); harvests recurse
/// into top-level nested imports only, mirroring [`import_funcs`].
fn expand_walk<P: ProcessManager>(
    state: &mut ExecState<P>,
    steps: &[Step],
    dir: &GuardedPath,
    stack: &mut Vec<GuardedPath>,
    files: &mut Vec<ClosureFile>,
) -> DiagnosticResult<()> {
    for step in steps {
        match &step.kind {
            StepKind::ImportFile { path, alias } => {
                let target = expand_load(state, dir, stack, path)?;
                record_site(
                    files,
                    &target,
                    path.clone(),
                    ClosureSite {
                        alias: Some(alias.clone()),
                        func_names: None,
                        type_names: None,
                    },
                );
                stack.push(target.resolved);
                expand_walk(state, &target.body, &target.nested, stack, files)?;
                stack.pop();
            }
            StepKind::ImportFuncs {
                path,
                alias,
                func_names,
                type_names,
            } => {
                // Host plugin types share the harvest syntax: a type-only
                // harvest naming a known host module validates the types
                // resolve globally and records no file. Entry-first probe
                // (see `top_level_file_exists`): a sibling file named like
                // a host module routes to the file when present. Mixed
                // func+type harvests from host modules stay unsupported (use
                // `IMPORT [MOD]` for host funcs).
                if func_names.is_empty() && !type_names.is_empty() {
                    let file_exists = top_level_file_exists(state, dir, stack, path);
                    if !file_exists && state.module_table.modules.contains_key(path) {
                        for name in type_names {
                            state.resolve_tag(name).map_err(|e| {
                                import_diag::import_unknown_type(
                                    path,
                                    name,
                                    Some(&anyhow::Error::new(e)),
                                )
                            })?;
                        }
                        // No file body to recurse into; still walk nested
                        // bodies of the step itself (none for imports).
                        expand_walk_kind(state, &step.kind, dir, stack, files)?;
                        continue;
                    }
                }
                let target = expand_load(state, dir, stack, path)?;
                record_site(
                    files,
                    &target,
                    path.clone(),
                    ClosureSite {
                        alias: alias.clone(),
                        func_names: Some(func_names.clone()),
                        type_names: Some(type_names.clone()),
                    },
                );
                stack.push(target.resolved);
                expand_walk_top(state, &target.body, &target.nested, stack, files)?;
                stack.pop();
            }
            _ => {}
        }
        expand_walk_kind(state, &step.kind, dir, stack, files)?;
    }
    Ok(())
}

/// Mirror of [`import_funcs`]' nested handling: only top-level nested
/// imports load. Conditional bodies are skipped here exactly as they
/// are at runtime.
fn expand_walk_top<P: ProcessManager>(
    state: &mut ExecState<P>,
    steps: &[Step],
    dir: &GuardedPath,
    stack: &mut Vec<GuardedPath>,
    files: &mut Vec<ClosureFile>,
) -> DiagnosticResult<()> {
    for step in steps {
        match &step.kind {
            StepKind::ImportFile { path, alias } => {
                let target = expand_load(state, dir, stack, path)?;
                record_site(
                    files,
                    &target,
                    path.clone(),
                    ClosureSite {
                        alias: Some(alias.clone()),
                        func_names: None,
                        type_names: None,
                    },
                );
                stack.push(target.resolved);
                expand_walk(state, &target.body, &target.nested, stack, files)?;
                stack.pop();
            }
            StepKind::ImportFuncs {
                path,
                alias,
                func_names,
                type_names,
            } => {
                // Host type-only harvests record no file (same entry-first
                // probe as `expand_walk` above).
                if func_names.is_empty() && !type_names.is_empty() {
                    let file_exists = top_level_file_exists(state, dir, stack, path);
                    if !file_exists && state.module_table.modules.contains_key(path) {
                        for name in type_names {
                            state.resolve_tag(name).map_err(|e| {
                                import_diag::import_unknown_type(
                                    path,
                                    name,
                                    Some(&anyhow::Error::new(e)),
                                )
                            })?;
                        }
                        continue;
                    }
                }
                let target = expand_load(state, dir, stack, path)?;
                record_site(
                    files,
                    &target,
                    path.clone(),
                    ClosureSite {
                        alias: alias.clone(),
                        func_names: Some(func_names.clone()),
                        type_names: Some(type_names.clone()),
                    },
                );
                stack.push(target.resolved);
                expand_walk_top(state, &target.body, &target.nested, stack, files)?;
                stack.pop();
            }
            _ => {}
        }
    }
    Ok(())
}

/// Recurse into a step's nested bodies so conditional imports join the
/// closure too (validation is conservative: anything that might load
/// gets checked).
fn expand_walk_kind<P: ProcessManager>(
    state: &mut ExecState<P>,
    kind: &StepKind,
    dir: &GuardedPath,
    stack: &mut Vec<GuardedPath>,
    files: &mut Vec<ClosureFile>,
) -> DiagnosticResult<()> {
    for body in child_bodies(kind) {
        expand_walk(state, body, dir, stack, files)?;
    }
    Ok(())
}

/// Immediate child bodies of a step, shared by expansion and dry-run.
/// Boxed sub-kinds unwrap one level; block bodies recurse per step by
/// the callers.
/// Immediate child step bodies of one step: block bodies plus inline
/// block expressions, which run in the enclosing dynamic scope (a
/// `FUNC` body does not: it owns an execution frame). Single owner
/// for nested-step traversal: closure expansion, dry-run previews,
/// harvest merging, and per-unit import walks all route through
/// here. Only direct child expressions are inspected; blocks nested
/// deeper inside collection literals never contain imports.
pub(crate) fn child_bodies(kind: &StepKind) -> Vec<&[Step]> {
    use oxdock_parser::BindRhs;
    let mut out: Vec<&[Step]> = match kind {
        StepKind::WithIo { cmd, .. } => child_bodies(cmd),
        StepKind::Bind { rhs, .. } => match rhs {
            BindRhs::Capture(cmd) => child_bodies(cmd),
            BindRhs::Async(body) => vec![body],
            BindRhs::Expr(_) | BindRhs::Await { .. } => Vec::new(),
        },
        StepKind::For { body, .. }
        | StepKind::While { body, .. }
        | StepKind::FuncDef { body, .. }
        | StepKind::Timeout { body, .. }
        | StepKind::Fail { body, .. }
        | StepKind::Isolated { body, .. }
        | StepKind::AsyncBlock { body, .. }
        | StepKind::RemoteBlock { body, .. } => vec![body],
        StepKind::If {
            then_body,
            else_ifs,
            else_body,
            ..
        } => {
            let mut out = vec![then_body.as_slice()];
            for (_, arm) in else_ifs {
                out.push(arm);
            }
            if let Some(arm) = else_body {
                out.push(arm);
            }
            out
        }
        StepKind::Switch {
            cases,
            default_body,
            ..
        } => {
            let mut out: Vec<&[Step]> = Vec::new();
            for (_, arm) in cases {
                out.push(arm);
            }
            if let Some(arm) = default_body {
                out.push(arm);
            }
            out
        }
        _ => Vec::new(),
    };
    // Inline blocks run in the enclosing dynamic scope, so their
    // contents belong to the enclosing walk, exactly like child
    // bodies above. Only direct child expressions are inspected;
    // blocks nested deeper inside collection literals never contain
    // imports. (`Return` boxes its expression; the rest do not.)
    match kind {
        StepKind::Bind {
            rhs: oxdock_parser::BindRhs::Expr(expr),
            ..
        }
        | StepKind::Set { expr, .. } => {
            if let oxdock_parser::Expr::Block(body) = expr {
                out.push(body);
            }
        }
        StepKind::Return { expr, .. } => {
            if let oxdock_parser::Expr::Block(body) = expr.as_ref() {
                out.push(body);
            }
        }
        StepKind::Call { args, .. } => {
            for arg in args {
                if let oxdock_parser::Expr::Block(body) = arg {
                    out.push(body);
                }
            }
        }
        _ => {}
    }
    out
}

/// Record an import site, deduping identical sites (diamonds share one
/// module record; each alias is a view onto it). `exports` rides
/// with the first site; every site loads the same parsed body.
fn record_site(
    files: &mut Vec<ClosureFile>,
    target: &ExpandedImport,
    path: String,
    site: ClosureSite,
) {
    let key = target.resolved.display();
    match files.iter_mut().find(|f| f.key == key) {
        Some(found) => {
            if !found.sites.iter().any(|s| {
                s.alias == site.alias
                    && s.func_names == site.func_names
                    && s.type_names == site.type_names
            }) {
                found.sites.push(site);
            }
        }
        None => files.push(ClosureFile {
            key,
            path,
            dir: target.nested.clone(),
            steps: target.body.clone(),
            exports: target.exports.clone(),
            sites: vec![site],
        }),
    }
}
/// One resolved import target during expansion: the file, its parsed
/// body, its `EXPORT` metadata (func plus type visibility gates, const
/// forwarding), and the directory nested imports resolve against.
struct ExpandedImport {
    resolved: GuardedPath,
    body: Vec<Step>,
    exports: Exports,
    nested: GuardedPath,
}

/// Load one import target during expansion. The file must exist when
/// the run starts: there is no deferred path. A script cannot
/// generate-then-import; analysis requires bytes, and bytes that do
/// not exist yet cannot be checked. Guard, cycle, depth, and absence
/// failures are all hard, before the first step runs.
fn expand_load<P: ProcessManager>(
    state: &mut ExecState<P>,
    dir: &GuardedPath,
    stack: &[GuardedPath],
    path: &str,
) -> DiagnosticResult<ExpandedImport> {
    let resolved = resolve_top_level(state, dir, stack, path)?;
    let key = resolved.display();
    if stack.iter().any(|p| p == &resolved) {
        let mut chain: Vec<String> = stack.iter().map(|p| p.display()).collect();
        chain.push(key);
        return Err(import_diag::import_cycle(&chain));
    }
    if stack.len() >= MAX_IMPORT_DEPTH {
        return Err(import_diag::import_depth_limit(MAX_IMPORT_DEPTH, path));
    }
    // The file must already exist: expansion is the analysis pass,
    // and analysis needs bytes. Probing through the run filesystem so
    // memory-backed test filesystems answer from their own state, not
    // the host disk.
    if state.fs.entry_kind(&resolved).is_err() {
        return Err(import_diag::import_not_found(path));
    }
    let LoadedFile {
        resolved,
        steps: body,
        exports,
    } = load_file(state, dir, stack, path)?;
    let nested = resolved.parent().unwrap_or_else(|| dir.clone());
    Ok(ExpandedImport {
        resolved,
        body,
        exports,
        nested,
    })
}

/// Load and parse an import target: resolve against the importer dir,
/// reject cycles and depth overflow, and reuse the per-run parse cache.
/// Returns the resolved file; the caller pushes the
/// scope frame and the import-stack entry around execution.
fn load_import<P: ProcessManager>(state: &mut ExecState<P>, path: &str) -> Result<LoadedFile> {
    let base = import_base(state);
    let stack = state.import_stack.clone();
    load_file(state, &base, &stack, path).map_err(anyhow::Error::from)
}

/// One resolved import target: the file, its parsed body, and its
/// `EXPORT` visibility gates.
struct LoadedFile {
    resolved: GuardedPath,
    steps: Vec<Step>,
    exports: Exports,
}

/// Shared loader core: `stack` is the active chain for cycle/depth
/// checks (the runtime stack, or the expansion walk's local one).
/// Parsed bodies and their `EXPORT` lists land in the run's module
/// registry either way, so expansion and execution never read a file
/// twice; initialization state rides the same record.
fn load_file<P: ProcessManager>(
    state: &mut ExecState<P>,
    base: &GuardedPath,
    stack: &[GuardedPath],
    path: &str,
) -> DiagnosticResult<LoadedFile> {
    // Workspace-side resolution: imported files live next to the
    // importing file in the source tree (not in the snapshot), so join
    // lexically under the importer's own guard instead of routing
    // through the run resolver's snapshot root. `join` enforces the
    // base root's containment (`..` escapes fail here); `read_file`
    // re-validates on the backend. Top-level outside-workspace
    // entries try the entry dir first (see `resolve_top_level`),
    // preserving the staged-temp fallback to the workspace root.
    let resolved = resolve_top_level(state, base, stack, path)?;
    let key = resolved.display();
    if stack.iter().any(|p| p == &resolved) {
        let mut chain: Vec<String> = stack.iter().map(|p| p.display()).collect();
        chain.push(key.clone());
        return Err(import_diag::import_cycle(&chain));
    }
    if stack.len() >= MAX_IMPORT_DEPTH {
        return Err(import_diag::import_depth_limit(MAX_IMPORT_DEPTH, path));
    }
    if let Some(record) = state.modules.get(&key) {
        return Ok(LoadedFile {
            resolved,
            steps: record.steps.clone(),
            exports: record.exports.clone(),
        });
    }
    let bytes = read_import_bytes(state.fs.as_ref(), &resolved)
        .map_err(|e| import_diag::import_cannot_read(path, &e))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| import_diag::import_bad_utf8(path, &anyhow::Error::new(e)))?;
    let table = state.module_table.clone();
    let mut reserved = super::native::builtin_function_names();
    reserved.extend(table.reserved_base_names());
    let parsed = oxdock_parser::parse_script_with_modules_and_exports(
        text,
        crate::lower_command,
        reserved,
        table,
    )
    .map_err(|e| import_diag::import_parse_failed(path, &anyhow::Error::new(e)))?;
    state.modules.insert(
        key,
        super::state::ModuleRecord {
            steps: parsed.steps.clone(),
            exports: parsed.exports.clone(),
            init: None,
            top_funcs: collect_top_funcs(&parsed.steps),
        },
    );
    Ok(LoadedFile {
        resolved,
        steps: parsed.steps,
        exports: parsed.exports,
    })
}

/// Directory relative `IMPORT` paths resolve against, workspace-side:
/// the importing file's parent, the entry file's parent at top level,
/// or the workspace root (falling back to the build context) for
/// stdin/inline runs with no file. An entry staged outside the
/// workspace (CI runners materialize inline scripts in a temp dir)
/// carries no source-tree context, so it resolves like stdin against
/// the workspace root instead of the staging dir.
pub(crate) fn import_base<P: ProcessManager>(state: &ExecState<P>) -> GuardedPath {
    if let Some(top) = state.import_stack.last() {
        if let Some(parent) = top.parent() {
            return parent;
        }
    } else if let Some(parent) = state.entry_file.as_ref().and_then(|entry| entry.parent()) {
        let base = workspace_fallback(state.fs.as_ref());
        if !parent.as_path().starts_with(base.as_path()) {
            return base;
        }
        return parent;
    }
    workspace_fallback(state.fs.as_ref())
}

/// Runtime entry point for top-level import resolution: entry-dir
/// probe, no workspace fallback. Thin wrapper over the shared
/// [`resolve_top_level_with`]; nested imports (`stack` non-empty)
/// always stay importer-relative. Shared by expansion
/// (`expand_load`, `load_file`), runtime host-type probes, and the
/// static closure pass, so every top-level import resolves
/// identically.
pub(crate) fn resolve_top_level<P: ProcessManager>(
    state: &ExecState<P>,
    dir: &GuardedPath,
    stack: &[GuardedPath],
    path: &str,
) -> DiagnosticResult<GuardedPath> {
    resolve_top_level_with(
        state.fs.as_ref(),
        dir,
        stack.is_empty(),
        state
            .entry_file
            .as_ref()
            .and_then(|entry| entry.parent())
            .as_ref(),
        None,
        path,
    )
}

/// Unit key for entry steps in
/// [`ExecState::executed_alias_targets`]. File keys are resolved
/// display paths, which never equal this sentinel.
pub(crate) const ENTRY_UNIT: &str = "entry";

/// Frame-1 file scope, indexed once per load (#322): every top-level
/// `FUNC` of one step body in definition order, guards unevaluated.
/// Guards ride along and are evaluated with the live env at use time,
/// so gated definitions behave exactly like the previous per-call
/// step scan. Single owner for the file-frame shape; the runtime
/// frame walk reads this, never the steps directly.
pub(super) fn collect_top_funcs(steps: &[Step]) -> HashMap<String, Vec<RawFuncDef>> {
    let mut out: HashMap<String, Vec<RawFuncDef>> = HashMap::new();
    for step in steps {
        if let StepKind::FuncDef {
            name,
            params,
            return_type,
            is_async,
            body,
            ..
        } = &step.kind
        {
            out.entry(name.clone()).or_default().push(RawFuncDef {
                params: params.clone(),
                return_spelling: return_type.clone(),
                is_async: *is_async,
                body: body.clone(),
                guard: step.guard.clone(),
                jail: None,
            });
        }
    }
    out
}

/// Live top-level def of one file frame (#322): last definition wins,
/// gated definitions filtered against the live env. Mirrors the
/// previous scan's forward-overwrite order exactly.
pub(super) fn live_top_func(
    defs: &[RawFuncDef],
    envs: &impl oxdock_parser::EnvLookup,
) -> Option<RawFuncDef> {
    let mut found = None;
    for def in defs {
        if oxdock_parser::guard_option_allows(def.guard.as_ref(), envs) {
            found = Some(def.clone());
        }
    }
    found
}

/// One script resolution site (#322): the single frame walk both
/// the runtime dispatcher and the expression router share, so they
/// can never disagree. Variants carry owned data; callers build
/// entries or route on presence alone.
pub(super) enum ScriptSite<P: ProcessManager> {
    /// Live nested definition from scope frames (covers entry
    /// top-level and nested defs positionally).
    Live(super::native::FuncEntry<P>),
    /// Owning-file top-level definition: raw def plus the defining
    /// unit key for jail attribution (`None` for the entry unit,
    /// whose calls push no module context).
    File {
        raw: RawFuncDef,
        unit: Option<String>,
    },
}

/// Runtime frame-walk adapter: the [`super::native::FuncFrameView`] implementation
/// over live [`ExecState`] stores, so static and runtime share the
/// nested, home, harvest precedence through [`super::native::FuncFrameView::walk`].
struct RuntimeFrame<'a, P: ProcessManager> {
    state: &'a ExecState<P>,
    current: Option<String>,
}

impl<P: ProcessManager> super::native::FuncFrameView for RuntimeFrame<'_, P> {
    type Hit = ScriptSite<P>;
    fn find_nested(&self, base: &str) -> Option<Self::Hit> {
        // Uniform floor: the innermost call/init boundary, or 0 at
        // the top level.
        let floor = self.state.func_floors.last().copied().unwrap_or(0);
        self.state
            .functions
            .get_scoped(base, floor)
            .map(ScriptSite::Live)
    }
    fn find_home(&self, unit: &str, base: &str) -> Option<Self::Hit> {
        // Owning file frame through the one unit lookup. Entry hits
        // carry no unit key so calls push no module context.
        let tops = self.state.unit_top_funcs(unit)?;
        let defs = tops.get(base)?;
        let raw = live_top_func(defs, &self.state.envs)?;
        let unit = self.current.clone();
        Some(ScriptSite::File { raw, unit })
    }
    fn find_harvest(&self, unit: &str, base: &str) -> Option<Self::Hit> {
        // Per-unit harvests: first textual definition wins.
        let defining = self
            .state
            .harvest_map
            .get(&(unit.to_string(), base.to_string()))?;
        let raw = self
            .state
            .modules
            .get(defining)
            .and_then(|r| r.top_funcs.get(base))
            .and_then(|defs| defs.first().cloned())?;
        Some(ScriptSite::File {
            raw,
            unit: Some(defining.clone()),
        })
    }
}

/// Single frame walk for bare and `SCRIPT::` call spellings (#322):
/// nested frames down to the call/init floor, then the owning file
/// frame (entry included via its precomputed index), then the
/// per-unit harvest map. Caller frames below the floor are
/// unreachable by construction; at entry the live frames hold only
/// entry-owned and explicitly harvested defs (inits pop). Qualified
/// non-`SCRIPT` spellings and canonical matches never reach here:
/// the caller handles those through the alias matcher first.
/// Returns `None` when only hosts/natives (or nothing) answer.
pub(super) fn find_script_site<P: ProcessManager>(
    state: &ExecState<P>,
    name: &str,
) -> Result<Option<ScriptSite<P>>> {
    use super::native::FuncFrameView;
    let base = match name.split_once(MODULE_SEPARATOR) {
        Some((module, base)) if module == oxdock_parser::SCRIPT_MODULE_NAME => base,
        Some(_) => return Ok(None),
        None => name,
    };
    let current = state.current_module();
    let view = RuntimeFrame {
        state,
        current: current.clone(),
    };
    Ok(view.walk(unit_key(current.as_deref()), base))
}

/// Runtime #319 missing-import suffix for an unknown function:
/// empty when no loaded module exports the base name. Single
/// owner for the runtime exporter view; every runtime
/// unknown-function error appends this instead of restating the
/// module scan.
pub(crate) fn missing_import_hint<P: ProcessManager>(state: &ExecState<P>, name: &str) -> String {
    super::native::missing_import_hint(
        state
            .modules
            .iter()
            .map(|(file, record)| (file.as_str(), record.exports.funcs.as_slice())),
        &active_unit_alias_targets(state),
        name,
    )
    .unwrap_or_default()
}

/// Unit key for an owning file: the module key, or the entry
/// sentinel when `None`. Single owner for the `unwrap_or(ENTRY_UNIT)`
/// fallback, so runtime and static lookups can never restate it.
pub(crate) fn unit_key(owner: Option<&str>) -> &str {
    owner.unwrap_or(ENTRY_UNIT)
}

/// Active unit for runtime alias attribution: the innermost module
/// on the context stack, else the entry.
pub(crate) fn active_unit_key<P: ProcessManager>(state: &ExecState<P>) -> String {
    unit_key(state.current_module().as_deref()).to_string()
}

/// Alias targets for one unit: executed unit-top imports, plus the
/// live block-scope frames for that unit (outer-to-inner), plus the
/// innermost call frame for that unit, if any. Scope frames merge
/// only when deeper than the innermost live call frame's entry depth
/// (a stale forked frame carries no floor), so a caller block's
/// imports never leak into a same-unit callee. Single owner behind
/// [`active_unit_alias_targets`], so forward-chasing reads the same
/// tables the direct read does.
pub(crate) fn unit_alias_targets<P: ProcessManager>(
    state: &ExecState<P>,
    unit: &str,
) -> super::native::AliasTargets {
    let mut out = state
        .executed_alias_targets
        .get(unit)
        .cloned()
        .unwrap_or_default();
    // Innermost frame for the unit, preferring a live one (entry
    // depth at or below the current scope depth; a forked child
    // restarts its stack, so deeper cloned frames are stale). A
    // stale frame still merges its targets (inherited visibility),
    // but carries no floor: only a live frame bounds the scope
    // frames to its own invocation.
    let live_floor = {
        let mut floor = 0;
        let mut inherited: Option<&super::native::AliasTargets> = None;
        for frame in state.call_alias_frames.iter().rev() {
            if frame.unit == unit {
                if frame.depth <= state.scope_stack.len() {
                    for (alias, targets) in &frame.targets {
                        out.entry(alias.clone())
                            .or_default()
                            .extend(targets.iter().cloned());
                    }
                    floor = frame.depth;
                    inherited = None;
                    break;
                }
                inherited.get_or_insert(&frame.targets);
            }
        }
        if let Some(targets) = inherited {
            for (alias, alias_targets) in targets {
                out.entry(alias.clone())
                    .or_default()
                    .extend(alias_targets.iter().cloned());
            }
        }
        floor
    };
    for frame in state
        .scope_alias_frames
        .iter()
        .filter(|frame| frame.unit == unit && frame.depth > live_floor)
    {
        // Append, never overwrite: the static walk layers block
        // imports over the unit table the same way, so the same
        // alias in both layers overlaps (ambiguous) here too instead
        // of the frame silently shadowing the unit map.
        for (alias, targets) in &frame.targets {
            out.entry(alias.clone())
                .or_default()
                .extend(targets.iter().cloned());
        }
    }
    out
}

/// Alias targets for the active unit: executed unit-top imports,
/// live block-scope frames, plus the innermost live call frame for
/// this unit, if any. Mirrors the static per-function overlays (unit
/// top-level plus the body's own imports, appended in the same order):
/// a callee never sees its caller's function-local imports.
/// Block imports unwind with their block: imports that never execute
/// stay absent and fall back to unknown.
pub(crate) fn active_unit_alias_targets<P: ProcessManager>(
    state: &ExecState<P>,
) -> super::native::AliasTargets {
    unit_alias_targets(state, &active_unit_key(state))
}

/// Live [`ModuleGraphView`](super::native::ModuleGraphView): raw export
/// lists, per-file unit targets, and store bindings straight off the
/// executed modules.
impl<P: ProcessManager> super::native::ModuleGraphView for ExecState<P> {
    fn const_exports(&self, file: &str) -> Option<&[String]> {
        self.modules
            .get(file)
            .map(|record| record.exports.consts.as_slice())
    }

    fn alias_targets(&self, file: &str) -> super::native::AliasTargets {
        unit_alias_targets(self, file)
    }

    fn binding(&self, file: &str, name: &str) -> super::native::BindingPresence {
        use super::native::BindingPresence;
        match self
            .modules
            .get(file)
            .and_then(|record| record.init.as_ref())
        {
            Some(init) => match init.vars.get(name) {
                Some(_) if init.const_vars.contains(name) => BindingPresence::Const,
                Some(_) => BindingPresence::Let,
                None => BindingPresence::Absent,
            },
            None => BindingPresence::Absent,
        }
    }
}

/// Resolve one `$ALIAS::NAME` read against the active unit's alias
/// targets: the single runtime owner for qualified `CONST` reads
/// (expression, math, and template positions all route here, so the
/// three can never disagree). Only `EXPORT`ed `CONST` bindings
/// resolve: `LET` bindings never cross the boundary, and an alias
/// with several live files exporting the name fails loud instead of
/// picking one. Holders and forwards resolve through the shared
/// [`native`](super::native) graph walk, so a re-export reads exactly
/// the value a direct read would.
pub(crate) fn resolve_const_ref<P: ProcessManager>(
    state: &ExecState<P>,
    alias: &str,
    name: &str,
) -> Result<Value> {
    use super::native::{
        ConstHolders, chase_const_forward, const_holder_files, missing_const_hint,
    };
    let qualified = oxdock_parser::qualify(alias, name);
    let targets = active_unit_alias_targets(state);
    // Holder files only: exactly one live holder reads; several fail
    // loud instead of picking.
    let holders = match const_holder_files(state, &targets, alias, name) {
        ConstHolders::Found(holders) => holders,
        ConstHolders::NoAlias => {
            return Err(anyhow::Error::new(import_diag::import_const_no_alias(
                &qualified, alias,
            )));
        }
        ConstHolders::Ambiguous { alias, base, files } => {
            return Err(anyhow::Error::new(import_diag::import_const_ambiguous(
                &alias, &base, &files,
            )));
        }
    };
    if holders.len() > 1 {
        let files: std::collections::HashSet<String> = holders.into_iter().collect();
        return Err(anyhow::Error::new(import_diag::import_const_ambiguous(
            alias, name, &files,
        )));
    }
    let Some(file) = holders.into_iter().next() else {
        // Hint owned by `missing_const_hint`: unexported CONST or
        // private LET under the queried alias only.
        let hint = missing_const_hint(state, &targets, alias, name);
        match hint {
            Some(hint) => {
                return Err(anyhow::Error::new(import_diag::import_const_hinted(
                    &qualified, &hint,
                )));
            }
            None => {
                return Err(anyhow::Error::new(import_diag::import_const_no_alias(
                    &qualified, alias,
                )));
            }
        }
    };
    // Follow qualified forwards to the defining files: a re-export
    // reads exactly the value a direct read would, frozen flag
    // included (the clone below shares the frozen buffer). Runtime
    // targets carry no guards, so disjoint branches cannot co-exist
    // here: several terminals fail loud like several holders.
    let mut terminals = chase_const_forward(state, &file, name, &qualified)?;
    if terminals.len() > 1 {
        let files: std::collections::HashSet<String> = terminals.drain(..).collect();
        return Err(anyhow::Error::new(import_diag::import_const_ambiguous(
            alias, name, &files,
        )));
    }
    let Some(terminal) = terminals.into_iter().next() else {
        return Err(anyhow::Error::new(import_diag::import_const_unknown(
            &qualified,
        )));
    };
    let Some(record) = state.modules.get(&terminal) else {
        return Err(anyhow::Error::new(import_diag::import_const_unknown(
            &qualified,
        )));
    };
    let Some(init) = record.init.as_ref() else {
        return Err(anyhow::Error::new(import_diag::import_const_uninitialized(
            &qualified,
        )));
    };
    if !init.const_vars.contains(name) {
        return Err(anyhow::Error::new(import_diag::import_const_is_let(
            &qualified,
        )));
    }
    init.vars
        .get(name)
        .map(|(_, value)| value.clone())
        .ok_or_else(|| anyhow::Error::new(import_diag::import_const_unknown(&qualified)))
}

/// Record one executed import under the importing unit: alias to its
/// defining file key (`None` for host-only harvests, which keep the
/// alias known with an explicit host branch, mirroring the static
/// host fallback). Inside a function body the record lands on the
/// innermost call frame for the active unit when executing at the
/// call's own top level (scope depth equals the frame's entry
/// depth), never the shared unit-top map, so sibling and caller
/// scopes stay isolated. Inside a lexical block (`IF`/`FOR`/`WHILE`/
/// `SWITCH`/...) the record lands on the innermost scope frame for
/// the active unit at the current scope depth, unwinding with the
/// block on scope exit — including blocks nested in function bodies.
/// A module init's own top level and the entry top level land on the
/// unit-top map. Runtime imports execute unconditionally once taken,
/// so they record no guard.
pub(crate) fn record_executed_alias<P: ProcessManager>(
    state: &mut ExecState<P>,
    alias: &str,
    defining: Option<String>,
) {
    let unit = active_unit_key(state);
    let depth = state.scope_stack.len();
    if let Some(frame) = state
        .call_alias_frames
        .iter_mut()
        .rev()
        .find(|frame| frame.unit == unit)
        && frame.depth == depth
    {
        frame
            .targets
            .entry(alias.to_string())
            .or_default()
            .push((defining, None));
        return;
    }
    if !state.scope_stack.is_empty() && !state.init_scope_depths.contains(&depth) {
        if let Some(frame) = state
            .scope_alias_frames
            .iter_mut()
            .rev()
            .find(|frame| frame.depth == depth && frame.unit == unit)
        {
            frame
                .targets
                .entry(alias.to_string())
                .or_default()
                .push((defining, None));
            return;
        }
        let mut targets = super::native::AliasTargets::new();
        targets
            .entry(alias.to_string())
            .or_default()
            .push((defining, None));
        state.scope_alias_frames.push(ScopeAliasFrame {
            depth,
            unit,
            targets,
        });
        return;
    }
    let targets = state
        .executed_alias_targets
        .entry(unit)
        .or_default()
        .entry(alias.to_string())
        .or_default();
    targets.push((defining, None));
}

/// Static-phase twin of [`resolve_top_level`]: no filesystem reads.
/// Tries the entry dir first for entry-unit imports when it differs
/// from `dir`, matching on closure keys (recorded by `expand_load`
/// through `resolve_top_level`); falls back to `dir.join`.
/// Nested file units pass `is_entry=false` and resolve purely
/// against their own directory.
pub(crate) fn resolve_static_top_level(
    entry_parent: Option<&GuardedPath>,
    dir: &GuardedPath,
    files: &[ClosureFile],
    path: &str,
    is_entry: bool,
) -> Option<String> {
    if is_entry
        && let Some(eparent) = entry_parent
        && eparent != dir
        && let Ok(candidate) = eparent.join(path)
    {
        let key = candidate.display();
        if files.iter().any(|f| f.key == key) {
            return Some(key);
        }
    }
    dir.join(path).ok().and_then(|resolved| {
        let key = resolved.display();
        files.iter().any(|f| f.key == key).then_some(key)
    })
}

/// Top-level file-existence probe shared by host-type harvest checks:
/// entry dir first (outside-workspace siblings), then `dir`. Mirrors
/// [`resolve_top_level`] without failing on unresolvable joins, so a
/// file named like a host module routes to the file when present.
pub(crate) fn top_level_file_exists<P: ProcessManager>(
    state: &ExecState<P>,
    dir: &GuardedPath,
    stack: &[GuardedPath],
    path: &str,
) -> bool {
    resolve_top_level(state, dir, stack, path)
        .is_ok_and(|resolved| state.fs.entry_kind(&resolved).is_ok())
}
