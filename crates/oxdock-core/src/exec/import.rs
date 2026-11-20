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

use anyhow::{Result, bail};
use oxdock_fs::{GuardedPath, WorkspaceFs};
use oxdock_parser::{MODULE_SEPARATOR, ModuleTable, Step, StepKind, Value};
use oxdock_process::ProcessManager;

use super::resolve_script_params;
use super::state::ExecState;
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
        bail!("IMPORT cycle detected: {}", chain.join(" -> "));
    }
    if stack.len() >= MAX_IMPORT_DEPTH {
        bail!(
            "IMPORT depth limit exceeded ({}): `{}`",
            MAX_IMPORT_DEPTH,
            path
        );
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
) -> Result<GuardedPath> {
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
        .map_err(|e| anyhow::anyhow!("IMPORT `{path}` cannot be resolved: {e}"))
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
        .map_err(|e| anyhow::anyhow!("IMPORT `{path}` cannot be read: {e}"))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| anyhow::anyhow!("IMPORT `{path}` is not valid UTF-8: {e}"))?;
    let table = table.clone();
    let mut reserved = super::native::builtin_function_names();
    reserved.extend(table.reserved_base_names());
    oxdock_parser::parse_script_with_modules(text, crate::lower_command, reserved, table)
        .map_err(|e| anyhow::anyhow!("IMPORT `{path}` failed to parse: {e}"))
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
                    anyhow::anyhow!("IMPORT `{path}` has no such TYPE `{name}`: {e:#}")
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
        func_exports,
        type_exports,
        const_exports: _,
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
    cx.state.import_stack.push(resolved);
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
                .map_err(|e| anyhow::anyhow!("IMPORT `{path}` failed validation: {e}"))?;
        }
        // Stateful harvests are rejected: the file body never runs, so
        // there is no initialized state for them to observe. Import the
        // file to share its module instead.
        if let Some(offender) =
            super::static_check::harvest_touches_module(cx.state, &key, &steps, func_names)
                .map_err(|e| anyhow::anyhow!("IMPORT `{path}` failed validation: {e}"))?
        {
            bail!("IMPORT `{path}` cannot harvest stateful function `{offender}`: import the file");
        }
        for name in func_names {
            // EXPORT gate (#115): unexported means private. Harvesting a
            // name the file does not export fails here, before anything
            // registers or runs.
            if !func_exports.iter().any(|e| e == name) {
                bail!("IMPORT `{path}` does not export `{name}`");
            }
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
                bail!("IMPORT `{path}` has no such function `{name}`");
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
                        )?;
                    }
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
                if !type_exports.iter().any(|e| e == name) {
                    bail!("IMPORT `{path}` does not export TYPE `{name}`");
                }
                let Some(target) = file_aliases.get(name) else {
                    bail!("IMPORT `{path}` has no such TYPE `{name}`");
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
                    anyhow::bail!(
                        "IMPORT `{path}` TYPE `{name}` collides: already defined with a different target"
                    );
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
) -> Result<()> {
    let key = match alias {
        Some(alias) => format!("{alias}::{name}"),
        None => name.to_string(),
    };
    if let Some(existing) = state.type_aliases.get(&key) {
        if existing != target {
            anyhow::bail!(
                "IMPORT `{path}` TYPE `{name}` collides: already defined with target `{existing}`, new target `{target}`"
            );
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
                        anyhow::anyhow!("IMPORT `{harvest_path}` has no such TYPE `{name}`: {e:#}")
                    })?;
                }
                continue;
            }
        }
        let harvested = load_import(cx.state, harvest_path)?;
        let file_aliases = super::typing::collect_type_aliases(&harvested.steps, cx.state)?;
        for name in harvest_types {
            if !harvested.type_exports.iter().any(|e| e == name) {
                anyhow::bail!("IMPORT `{harvest_path}` does not export TYPE `{name}`");
            }
            let Some(target) = file_aliases.get(name) else {
                anyhow::bail!("IMPORT `{harvest_path}` has no such TYPE `{name}`");
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

pub(crate) fn import_file<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    path: &str,
    alias: &str,
) -> Result<Value> {
    let LoadedFile {
        resolved,
        steps,
        func_exports,
        type_exports,
        const_exports: _,
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
                .map_err(|e| anyhow::anyhow!("IMPORT `{path}` failed validation: {e}"))?;
        }
        // Register the imported file's EXPORTed top-level FUNCs under
        // the alias in the caller's scope: unexported means private.
        // Owner-tagged with the module so calls share its state.
        // Skips names already present: re-imports only add new views.
        let func_exported: std::collections::HashSet<&str> =
            func_exports.iter().map(|e| e.as_str()).collect();
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
                type_exports.iter().map(|e| e.as_str()).collect();
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
                type_exports.iter().map(|e| e.as_str()).collect();
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
    // Phase 2 (init): runs once per file. Pushes unwind below.
    // The init body observes nothing of the importer either: the
    // whole variable stack swaps for a fresh one, exactly like a
    // module-owned call, and restores after the scope pops.
    let saved_vars = std::mem::take(&mut cx.state.var_scopes);
    let saved_consts = std::mem::take(&mut cx.state.const_scopes);
    let saved_aliases = cx.state.type_aliases.clone();
    let outcome: Result<Value> = (|| {
        let aliases = super::typing::collect_type_aliases(&steps, cx.state)?;
        for (name, target) in aliases {
            cx.state.register_alias(name, target);
        }
        cx.state.import_stack.push(resolved.clone());
        cx.state.push_scope();
        // Snapshot the pre-init env: the module's call-time env is the
        // seed plus only what this init body contributes. Values the
        // importer established stay out by construction (the delta
        // below is taken against this map, not the seed).
        let env_before: std::collections::HashMap<String, String> = (*cx.state.envs).clone();
        // The init body runs with its module on the context stack so
        // nested definitions owner-tag correctly; lexical reads still
        // resolve through the pushed frames first. Relative paths in
        // the init body resolve against the ambient cwd like any
        // other code: there is no module-dir anchoring.
        cx.state.module_stack.push(Some(key.clone()));
        let flow = execute_steps(
            cx.state,
            cx.process,
            &steps,
            cx.stdin.clone(),
            false,
            cx.out.clone(),
            cx.err.clone(),
            cx.out_pipe.clone(),
            cx.stdin_pipe.clone(),
            false,
        );
        // Unwind the module context on every path: nested definitions
        // below already owner-tagged while it was up.
        let _ = cx.state.module_stack.pop();
        let flow = flow?;
        // Drain the init frame's top-level bindings into the shared
        // store before the scope pops: this is the module's global
        // state, visible to every module-owned function and alias.
        // Ambient run state (cwd/envs/root) still reverts on pop.
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
        let value = match flow {
            Flow::Done => Value::string(String::new()),
            Flow::Return { value, .. } => value,
            Flow::Break { idx } => {
                bail!(
                    "step {}: BREAK cannot cross import boundary (in IMPORT `{}`)",
                    idx + 1,
                    path
                );
            }
            Flow::Continue { idx } => {
                bail!(
                    "step {}: CONTINUE cannot cross import boundary (in IMPORT `{}`)",
                    idx + 1,
                    path
                );
            }
        };
        let record = cx.state.modules.entry(key.clone()).or_default();
        // Module call-time env (#115): the session seed plus this
        // init's own contributions. Comparing against the pre-init
        // map (not the seed) keeps importer-established values out,
        // so module functions never inherit caller context. Removals
        // (e.g. `INHERIT_ENV` of a removed name) drop seed rows too.
        // A re-inherit of an already-present value leaves no delta, so
        // `INHERIT_ENV` names are additionally recorded for
        // per-call re-establishment from the host.
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
        let mut inherit_names = Vec::new();
        for step in &steps {
            if let StepKind::InheritEnv { keys } = &step.kind {
                inherit_names.extend(keys.iter().cloned());
            }
        }
        record.init = Some(super::state::ModuleInit {
            vars: drained,
            const_vars: drained_consts,
            return_value: value.clone(),
            envs: module_env,
            inherit_names,
        });
        Ok(value)
    })();
    let pop_res = cx.state.pop_scope();
    cx.state.var_scopes = saved_vars;
    cx.state.const_scopes = saved_consts;
    let _ = cx.state.import_stack.pop();
    cx.state.restore_aliases(saved_aliases);
    // Re-apply the file's own harvests after the init restore: the
    // restore drops everything but exports, while harvested funcs
    // persist through the registry. Types persist like funcs do, so
    // the file's bodies resolve at call time exactly as validated.
    if outcome.is_ok() {
        resolve_own_type_harvests(cx, &steps, &resolved)?;
    }
    match (outcome, pop_res) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e),
    }
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
    pub func_exports: Vec<String>,
    pub type_exports: Vec<String>,
    pub const_exports: Vec<String>,
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
) -> Result<Vec<ClosureFile>> {
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
) -> Result<()> {
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
                                anyhow::anyhow!("IMPORT `{path}` has no such TYPE `{name}`: {e:#}")
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
) -> Result<()> {
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
                                anyhow::anyhow!("IMPORT `{path}` has no such TYPE `{name}`: {e:#}")
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
) -> Result<()> {
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
    let mut out: Vec<&[Step]> = match kind {
        StepKind::WithIo { cmd, .. } | StepKind::AssignCapture { cmd, .. } => child_bodies(cmd),
        StepKind::For { body, .. }
        | StepKind::While { body, .. }
        | StepKind::FuncDef { body, .. }
        | StepKind::Timeout { body, .. }
        | StepKind::Fail { body, .. }
        | StepKind::Isolated { body, .. }
        | StepKind::AssignAsync { body, .. }
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
        StepKind::Assign { expr, .. } | StepKind::Set { expr, .. } => {
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
/// module record; each alias is a view onto it). `func_exports` rides
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
            func_exports: target.func_exports.clone(),
            type_exports: target.type_exports.clone(),
            const_exports: target.const_exports.clone(),
            sites: vec![site],
        }),
    }
}
/// One resolved import target during expansion: the file, its parsed
/// body, its `EXPORT` lists (func plus type visibility gates), and the
/// directory nested imports resolve against.
struct ExpandedImport {
    resolved: GuardedPath,
    body: Vec<Step>,
    func_exports: Vec<String>,
    type_exports: Vec<String>,
    const_exports: Vec<String>,
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
) -> Result<ExpandedImport> {
    let resolved = resolve_top_level(state, dir, stack, path)?;
    let key = resolved.display();
    if stack.iter().any(|p| p == &resolved) {
        let mut chain: Vec<String> = stack.iter().map(|p| p.display()).collect();
        chain.push(key);
        bail!("IMPORT cycle detected: {}", chain.join(" -> "));
    }
    if stack.len() >= MAX_IMPORT_DEPTH {
        bail!(
            "IMPORT depth limit exceeded ({}): `{}`",
            MAX_IMPORT_DEPTH,
            path
        );
    }
    // The file must already exist: expansion is the analysis pass,
    // and analysis needs bytes. Probing through the run filesystem so
    // memory-backed test filesystems answer from their own state, not
    // the host disk.
    if state.fs.entry_kind(&resolved).is_err() {
        bail!(
            "IMPORT `{path}` not found: the file must exist when the run starts (generated imports are not supported)"
        );
    }
    let LoadedFile {
        resolved,
        steps: body,
        func_exports,
        type_exports,
        const_exports,
    } = load_file(state, dir, stack, path)?;
    let nested = resolved.parent().unwrap_or_else(|| dir.clone());
    Ok(ExpandedImport {
        resolved,
        body,
        func_exports,
        type_exports,
        const_exports,
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
    load_file(state, &base, &stack, path)
}

/// One resolved import target: the file, its parsed body, and its
/// `EXPORT` / `EXPORT TYPE` visibility gates.
struct LoadedFile {
    resolved: GuardedPath,
    steps: Vec<Step>,
    func_exports: Vec<String>,
    type_exports: Vec<String>,
    const_exports: Vec<String>,
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
) -> Result<LoadedFile> {
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
        bail!("IMPORT cycle detected: {}", chain.join(" -> "));
    }
    if stack.len() >= MAX_IMPORT_DEPTH {
        bail!(
            "IMPORT depth limit exceeded ({}): `{}`",
            MAX_IMPORT_DEPTH,
            path
        );
    }
    if let Some(record) = state.modules.get(&key) {
        return Ok(LoadedFile {
            resolved,
            steps: record.steps.clone(),
            func_exports: record.func_exports.clone(),
            type_exports: record.type_exports.clone(),
            const_exports: record.const_exports.clone(),
        });
    }
    let bytes = read_import_bytes(state.fs.as_ref(), &resolved)
        .map_err(|e| anyhow::anyhow!("IMPORT `{path}` cannot be read: {e}"))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| anyhow::anyhow!("IMPORT `{path}` is not valid UTF-8: {e}"))?;
    let table = state.module_table.clone();
    let mut reserved = super::native::builtin_function_names();
    reserved.extend(table.reserved_base_names());
    let parsed = oxdock_parser::parse_script_with_modules_and_exports(
        text,
        crate::lower_command,
        reserved,
        table,
    )
    .map_err(|e| anyhow::anyhow!("IMPORT `{path}` failed to parse: {e}"))?;
    state.modules.insert(
        key,
        super::state::ModuleRecord {
            steps: parsed.steps.clone(),
            func_exports: parsed.func_exports.clone(),
            type_exports: parsed.type_exports.clone(),
            const_exports: parsed.const_exports.clone(),
            init: None,
        },
    );
    Ok(LoadedFile {
        resolved,
        steps: parsed.steps,
        func_exports: parsed.func_exports,
        type_exports: parsed.type_exports,
        const_exports: parsed.const_exports,
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
) -> Result<GuardedPath> {
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

/// Raw home-scope `FUNC` header plus body (#319): spelled params,
/// return spelling, async flag, body steps, and the defining file key.
/// Stored unregistered so `LIB::PRIVATE` stays unknown while home
/// `PRIVATE()` resolves; callers resolve tags on use like every other
/// definition path.
type HomeFuncdef = (Vec<(String, String, bool)>, String, bool, Vec<Step>, String);
/// Home-scope sibling lookup (#319): the defining file's own `FUNC`
/// definitions, including private and unharvested names, for callee
/// bodies resolving against their home file instead of only the
/// importer's scope. Searches the active module's parsed steps for a
/// top-level `FUNC` with the base name, returning its raw header plus
/// body for on-demand execution without registry pollution (so
/// `LIB::PRIVATE` stays unknown while home `PRIVATE()` works).
/// Guards evaluate like execution (see `execute_steps`): skipped
/// steps never define, so a disabled-platform `FUNC` never resolves
/// and disjoint platform variants resolve the live one; overlapping
/// live duplicates resolve last-wins, mirroring definition order.
/// Only `SCRIPT::BASE` and bare `BASE` spellings qualify; qualified
/// `ALIAS::` and host calls never fall back.
pub(crate) fn find_home_funcdef<P: ProcessManager>(
    state: &ExecState<P>,
    name: &str,
) -> Option<HomeFuncdef> {
    let base = match name.split_once(MODULE_SEPARATOR) {
        Some((module, base)) if module == oxdock_parser::SCRIPT_MODULE_NAME => base,
        Some(_) => return None,
        None => name,
    };
    let module_key = state.current_module()?;
    let record = state.modules.get(&module_key)?;
    let mut found = None;
    for step in &record.steps {
        if !oxdock_parser::guard_option_allows(step.guard.as_ref(), &state.envs) {
            continue;
        }
        if let StepKind::FuncDef {
            name: def_name,
            params,
            return_type,
            is_async,
            body,
            ..
        } = &step.kind
            && def_name == base
        {
            found = Some((
                params.clone(),
                return_type.clone(),
                *is_async,
                body.clone(),
                module_key.clone(),
            ));
        }
    }
    found
}

/// Active unit for runtime alias attribution: the innermost module
/// on the context stack, else the entry.
pub(crate) fn active_unit_key<P: ProcessManager>(state: &ExecState<P>) -> String {
    if let Some(key) = state.current_module() {
        return key;
    }
    ENTRY_UNIT.to_string()
}

/// Alias targets for the active unit: executed top-level imports plus
/// the innermost live call frame for this unit, if any. Mirrors the
/// static per-function overlays (unit top-level plus the body's own
/// imports, appended in the same order): a callee never sees its
/// caller's function-local imports. Conditional imports that never
/// execute stay absent and fall back to unknown.
pub(crate) fn active_unit_alias_targets<P: ProcessManager>(
    state: &ExecState<P>,
) -> super::native::AliasTargets {
    let unit = active_unit_key(state);
    let mut out = state
        .executed_alias_targets
        .get(&unit)
        .cloned()
        .unwrap_or_default();
    if let Some((_, frame)) = state
        .call_alias_frames
        .iter()
        .rev()
        .find(|(key, _)| *key == unit)
    {
        // Append, never overwrite: the static overlay appends the
        // body's own imports onto the unit top-level, so the same
        // alias in both layers overlaps (ambiguous) here too instead
        // of the frame silently shadowing the unit map.
        for (alias, targets) in frame {
            out.entry(alias.clone())
                .or_default()
                .extend(targets.iter().cloned());
        }
    }
    out
}

/// Resolve one `$ALIAS::NAME` read against the active unit's alias
/// targets: the single runtime owner for qualified `CONST` reads
/// (expression, math, and template positions all route here, so the
/// three can never disagree). Only `EXPORT`ed `CONST` bindings
/// resolve: `LET` bindings never cross the boundary, and an alias
/// with several live files exporting the name fails loud instead of
/// picking one.
pub(crate) fn resolve_const_ref<P: ProcessManager>(
    state: &ExecState<P>,
    alias: &str,
    name: &str,
) -> Result<Value> {
    use super::native::{AliasResolution, ambiguous_alias_message, classify_alias_call};
    let qualified = oxdock_parser::qualify(alias, name);
    let targets = active_unit_alias_targets(state);
    // Suffix probe for the matcher's canonical keys
    // (`file::base`): the base is ours, so stripping it recovers the
    // file exactly, and only an exporting file counts as a holder.
    let suffix = format!("::{name}");
    let exports = |canonical: &str| {
        canonical
            .strip_suffix(suffix.as_str())
            .and_then(|file| state.modules.get(file))
            .is_some_and(|record| record.const_exports.iter().any(|n| n == name))
    };
    let files = match classify_alias_call(&targets, &qualified, &exports) {
        AliasResolution::Global | AliasResolution::Unknown => {
            bail!("unknown CONST `${qualified}`: no import alias `{alias}` exports that name");
        }
        AliasResolution::Canonical(canonical) => {
            let Some(file) = canonical.strip_suffix(suffix.as_str()) else {
                bail!("unknown CONST `${qualified}`");
            };
            vec![file.to_string()]
        }
        AliasResolution::Ambiguous { alias, base, files } => {
            bail!(
                "{}",
                ambiguous_alias_message(&alias, &base, &files).replacen("function", "CONST", 1)
            );
        }
        AliasResolution::Any { mut files, .. } => {
            let mut files: Vec<String> = files.drain().collect();
            files.sort();
            files
        }
    };
    // Exporting files only (the matcher already gated on exports, but
    // `Any` carries every disjoint file): exactly one live holder
    // reads; several fail loud instead of picking.
    let mut holders = Vec::new();
    for file in files {
        let Some(record) = state.modules.get(&file) else {
            continue;
        };
        if !record.const_exports.iter().any(|n| n == name) {
            continue;
        }
        holders.push(file);
    }
    if holders.len() > 1 {
        holders.sort();
        let files: std::collections::HashSet<String> = holders.into_iter().collect();
        bail!(
            "{}",
            ambiguous_alias_message(alias, name, &files).replacen("function", "CONST", 1)
        );
    }
    let Some(file) = holders.into_iter().next() else {
        // Hint only from the queried alias's own files: an unexported
        // CONST or private LET in an unrelated module must not supply
        // misleading text on a typo under a different alias.
        let hint = targets
            .get(alias)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|(file, _)| file.as_ref())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
            .iter()
            .filter_map(|file| state.modules.get(*file))
            .filter_map(|record| record.init.as_ref())
            .find_map(|init| {
                init.vars.get(name).map(|_| {
                    if init.const_vars.contains(name) {
                        format!("module holds `{name}` as CONST but does not export it: add `EXPORT CONST {name}`")
                    } else {
                        format!("`{name}` is a LET binding and cannot be exported: only CONST bindings cross the module boundary")
                    }
                })
            });
        match hint {
            Some(hint) => bail!("unknown CONST `${qualified}`: {hint}"),
            None => {
                bail!("unknown CONST `${qualified}`: no import alias `{alias}` exports that name")
            }
        }
    };
    let Some(record) = state.modules.get(&file) else {
        bail!("unknown CONST `${qualified}`");
    };
    let Some(init) = record.init.as_ref() else {
        bail!(
            "`${qualified}` cannot be read: the module was harvested but never initialized; import the file"
        );
    };
    if !init.const_vars.contains(name) {
        bail!(
            "`${qualified}` is a LET binding and cannot be exported: only CONST bindings cross the module boundary"
        );
    }
    init.vars
        .get(name)
        .map(|(_, value)| value.clone())
        .ok_or_else(|| anyhow::anyhow!("unknown CONST `${qualified}`"))
}

/// Record one executed import under the importing unit: alias to its
/// defining file key (`None` for host-only harvests, which keep the
/// alias known with an explicit host branch, mirroring the static
/// host fallback). Inside a function body the record lands on the
/// innermost call frame for the active unit, never the shared
/// unit-top map, so sibling and caller scopes stay isolated.
/// Top-level imports land on the unit-top map. Runtime imports execute
/// unconditionally once taken, so they record no guard.
pub(crate) fn record_executed_alias<P: ProcessManager>(
    state: &mut ExecState<P>,
    alias: &str,
    defining: Option<String>,
) {
    let unit = active_unit_key(state);
    if let Some((_, frame)) = state
        .call_alias_frames
        .iter_mut()
        .rev()
        .find(|(key, _)| *key == unit)
    {
        frame
            .entry(alias.to_string())
            .or_default()
            .push((defining, None));
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
