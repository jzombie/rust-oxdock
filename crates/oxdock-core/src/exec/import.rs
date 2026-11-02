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
use oxdock_parser::{ModuleTable, Step, StepKind, Value};
use oxdock_process::ProcessManager;

use super::state::ExecState;
use super::steps::{Flow, StepCtx, execute_steps};

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
    // source tree, never the snapshot.
    let resolved = dir
        .join(path)
        .map_err(|e| anyhow::anyhow!("IMPORT `{path}` cannot be resolved: {e}"))?;
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
    let bytes = fs
        .read_file(resolved)
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
        let base = import_base(cx.state);
        let resolved_opt = base.join(path).ok();
        let file_exists = resolved_opt
            .as_ref()
            .is_some_and(|r| cx.state.fs.entry_kind(r).is_ok());
        if !file_exists && cx.state.module_table.modules.contains_key(path) {
            for name in type_names {
                cx.state.resolve_tag(name).map_err(|e| {
                    anyhow::anyhow!("IMPORT `{path}` has no such TYPE `{name}`: {e:#}")
                })?;
            }
            if let Some(alias) = alias {
                // Harvest aliases register like file imports so later
                // top-level imports re-latch cleanly; no funcs to define.
                let _ = enter_alias_frame(cx.state);
                let _ = alias;
            }
            return Ok(());
        }
    }
    let LoadedFile {
        resolved,
        steps,
        func_exports,
        type_exports,
    } = load_import(cx.state, path)?;
    let key = resolved.display();
    let alias_frame = enter_alias_frame(cx.state);
    // Join the import stack like a full import: nested loads need this
    // file on the chain for cycle/depth detection and for
    // importer-relative resolution.
    cx.state.import_stack.push(resolved);
    let saved_aliases = cx.state.type_aliases.clone();
    let outcome: Result<()> = (|| {
        let aliases = super::typing::collect_type_aliases(&steps, cx.state)?;
        for (name, target) in aliases {
            cx.state.type_aliases.insert(name, target);
        }
        super::static_check::validate_import_body_types(&steps, cx.state, &key)
            .map_err(|e| anyhow::anyhow!("IMPORT `{path}` failed validation: {e}"))?;
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
                    body,
                    ..
                } if def_name == name => Some((params.clone(), body.clone())),
                _ => None,
            }) else {
                bail!("IMPORT `{path}` has no such function `{name}`");
            };
            let mut resolved_params = Vec::with_capacity(def.0.len());
            for (pname, ptype) in &def.0 {
                resolved_params.push((pname.clone(), cx.state.resolve_tag(ptype)?));
            }
            // Owner-tagged with the module: calls resolve names against
            // the shared store. Re-imports skip already-registered
            // names, so shared views stay idempotent.
            match alias {
                Some(alias) => {
                    let qualified = oxdock_parser::qualify(alias, name);
                    if cx.state.functions.get(&qualified).is_none() {
                        cx.state.functions.define_aliased_in(
                            alias_frame,
                            alias,
                            name,
                            &resolved_params,
                            &def.1,
                            Some(&key),
                        )?;
                    }
                }
                None => {
                    if cx.state.functions.get(name).is_none() {
                        cx.state.functions.define_script(
                            name,
                            &resolved_params,
                            &def.1,
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
        // Nested imports are the harvested functions' dependencies: run
        // them per their own form. Anything else stays unrunned.
        for step in &steps {
            match &step.kind {
                StepKind::ImportFile { path, alias } => {
                    import_file(cx, path, alias)?;
                }
                StepKind::ImportFuncs {
                    path,
                    alias,
                    func_names,
                    type_names: nested_types,
                } => {
                    import_funcs(cx, path, alias.as_deref(), func_names, nested_types)?;
                }
                _ => {}
            }
        }
        Ok(())
    })();
    // import_funcs pushes no scope (nothing executes), so only the
    // import stack unwinds here; the alias frame clears exactly like a
    // full import so later top-level imports re-latch cleanly.
    let _ = cx.state.import_stack.pop();
    if cx.state.import_stack.is_empty() {
        cx.state.import_alias_frame = None;
    }
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
        cx.state.type_aliases = saved_aliases;
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
                cx.state.type_aliases.insert(name, target);
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
fn insert_imported_type<P: ProcessManager>(
    state: &mut ExecState<P>,
    name: &str,
    target: &str,
    alias: Option<&str>,
    path: &str,
) -> Result<()> {
    let _ = alias;
    let key = name.to_string();
    if let Some(existing) = state.type_aliases.get(&key) {
        if existing != target {
            anyhow::bail!(
                "IMPORT `{path}` TYPE `{name}` collides: already defined with target `{existing}`, new target `{target}`"
            );
        }
    } else {
        state.type_aliases.insert(key, target.to_string());
    }
    Ok(())
}
/// Outermost-import alias frame (#115): the caller frame, shared by
/// every nested import so `ALIAS::` bodies keep resolving nested
/// aliases after nested frames pop.
fn enter_alias_frame<P: ProcessManager>(state: &mut ExecState<P>) -> usize {
    if state.import_stack.is_empty() {
        let depth = state.functions.scope_depth().saturating_sub(1);
        state.import_alias_frame = Some(depth);
    }
    state
        .import_alias_frame
        .unwrap_or_else(|| state.functions.scope_depth().saturating_sub(1))
}

/// Execute `IMPORT ["<path>" AS <ALIAS>]`: initialize the module on
/// first import (executing its body once and draining its top-level
/// `LET`s into the shared store), then register its `EXPORT`ed
/// `FUNC`s as views under the alias. Later imports of the same file
/// skip execution and reuse the initialized module, binding the
/// cached `RETURN` value for `LET`-capture. Returns the init body's
/// `RETURN` value (`""` on fallthrough).
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
    } = load_import(cx.state, path)?;
    let key = resolved.display();
    let alias_frame = enter_alias_frame(cx.state);
    // Merge the imported file's type aliases for its own validation and
    // execution; restored after the import returns so aliases never leak
    // across the import boundary except through `EXPORT TYPE`.
    let saved_aliases = cx.state.type_aliases.clone();
    // Phase 1 (no pushes): merge aliases, validate, register views.
    // Runs on every import so new aliases attach and bodies re-check.
    let phase_one: Result<Option<Value>> = (|| {
        let aliases = super::typing::collect_type_aliases(&steps, cx.state)?;
        for (name, target) in &aliases {
            cx.state.type_aliases.insert(name.clone(), target.clone());
        }
        // Entry parity: statically validate the imported body before its
        // first step runs. Nested imports validate when their own IMPORT
        // steps execute.
        super::static_check::validate_import_body_types(&steps, cx.state, &key)
            .map_err(|e| anyhow::anyhow!("IMPORT `{path}` failed validation: {e}"))?;
        // Register the imported file's EXPORTed top-level FUNCs under
        // the alias in the caller's scope: unexported means private.
        // Owner-tagged with the module so calls share its state.
        // Skips names already present: re-imports only add new views.
        let func_exported: std::collections::HashSet<&str> =
            func_exports.iter().map(|e| e.as_str()).collect();
        for step in &steps {
            if let StepKind::FuncDef {
                name, params, body, ..
            } = &step.kind
                && func_exported.contains(name.as_str())
            {
                let qualified = oxdock_parser::qualify(alias, name);
                if cx.state.functions.get(&qualified).is_none() {
                    let mut resolved_params = Vec::with_capacity(params.len());
                    for (pname, ptype) in params {
                        resolved_params.push((pname.clone(), cx.state.resolve_tag(ptype)?));
                    }
                    cx.state.functions.define_aliased_in(
                        alias_frame,
                        alias,
                        name,
                        &resolved_params,
                        body,
                        Some(&key),
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
    cx.state.type_aliases = saved_aliases.clone();
    let cached = phase_one?;
    // Whole-file type persistence: exported types survive the restore,
    // bare plus `ALIAS::NAME`, exactly like func views. Runs on every
    // import (including cached re-imports) so new aliases attach.
    {
        if let Ok(aliases) = super::typing::collect_type_aliases(&steps, cx.state) {
            // Temporarily merge full aliases so nested-type targets
            // resolve, then filter to exported for persistence.
            for (name, target) in &aliases {
                cx.state.type_aliases.insert(name.clone(), target.clone());
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
            cx.state.type_aliases = saved_aliases.clone();
            for (name, target) in to_persist {
                insert_imported_type(cx.state, &name, &target, Some(alias), path)?;
            }
        } else {
            cx.state.type_aliases = saved_aliases.clone();
        }
    }
    if let Some(value) = cached {
        return Ok(value);
    }
    // Phase 2 (init): runs once per file. Pushes unwind below.
    let saved_aliases = cx.state.type_aliases.clone();
    let outcome: Result<Value> = (|| {
        let aliases = super::typing::collect_type_aliases(&steps, cx.state)?;
        for (name, target) in aliases {
            cx.state.type_aliases.insert(name, target);
        }
        cx.state.import_stack.push(resolved);
        cx.state.push_scope();
        // Snapshot the pre-init env: the module's call-time env is the
        // seed plus only what this init body contributes. Values the
        // importer established stay out by construction (the delta
        // below is taken against this map, not the seed).
        let env_before: std::collections::HashMap<String, String> = (*cx.state.envs).clone();
        // The init body runs with its module on the context stack so
        // nested definitions owner-tag correctly; lexical reads still
        // resolve through the pushed frames first.
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
            return_value: value.clone(),
            envs: module_env,
            inherit_names,
        });
        Ok(value)
    })();
    let pop_res = cx.state.pop_scope();
    let _ = cx.state.import_stack.pop();
    if cx.state.import_stack.is_empty() {
        cx.state.import_alias_frame = None;
    }
    cx.state.type_aliases = saved_aliases;
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
/// literal spelling from the first import statement, for error messages.
#[derive(Debug, Clone)]
pub(super) struct ClosureFile {
    pub key: String,
    pub path: String,
    pub steps: Vec<Step>,
    pub func_exports: Vec<String>,
    pub type_exports: Vec<String>,
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
                let key = target.resolved.display();
                stack.push(target.resolved);
                record_site(
                    files,
                    key,
                    path.clone(),
                    target.body.clone(),
                    target.func_exports.clone(),
                    target.type_exports.clone(),
                    ClosureSite {
                        alias: Some(alias.clone()),
                        func_names: None,
                        type_names: None,
                    },
                );
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
                // resolve globally and records no file. Mixed func+type
                // harvests from host modules stay unsupported (use
                // `IMPORT [MOD]` for host funcs).
                if func_names.is_empty() && !type_names.is_empty() {
                    let resolved_opt = dir.join(path).ok();
                    let file_exists = resolved_opt
                        .as_ref()
                        .is_some_and(|r| state.fs.entry_kind(r).is_ok());
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
                let key = target.resolved.display();
                stack.push(target.resolved);
                record_site(
                    files,
                    key,
                    path.clone(),
                    target.body.clone(),
                    target.func_exports.clone(),
                    target.type_exports.clone(),
                    ClosureSite {
                        alias: alias.clone(),
                        func_names: Some(func_names.clone()),
                        type_names: Some(type_names.clone()),
                    },
                );
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
                let key = target.resolved.display();
                stack.push(target.resolved);
                record_site(
                    files,
                    key,
                    path.clone(),
                    target.body.clone(),
                    target.func_exports.clone(),
                    target.type_exports.clone(),
                    ClosureSite {
                        alias: Some(alias.clone()),
                        func_names: None,
                        type_names: None,
                    },
                );
                expand_walk(state, &target.body, &target.nested, stack, files)?;
                stack.pop();
            }
            StepKind::ImportFuncs {
                path,
                alias,
                func_names,
                type_names,
            } => {
                // Host type-only harvests record no file (same rule as
                // `expand_walk` above).
                if func_names.is_empty() && !type_names.is_empty() {
                    let resolved_opt = dir.join(path).ok();
                    let file_exists = resolved_opt
                        .as_ref()
                        .is_some_and(|r| state.fs.entry_kind(r).is_ok());
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
                let key = target.resolved.display();
                stack.push(target.resolved);
                record_site(
                    files,
                    key,
                    path.clone(),
                    target.body.clone(),
                    target.func_exports.clone(),
                    target.type_exports.clone(),
                    ClosureSite {
                        alias: alias.clone(),
                        func_names: Some(func_names.clone()),
                        type_names: Some(type_names.clone()),
                    },
                );
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
fn child_bodies(kind: &StepKind) -> Vec<&[Step]> {
    match kind {
        StepKind::WithIo { cmd, .. } | StepKind::AssignCapture { cmd, .. } => child_bodies(cmd),
        StepKind::For { body, .. }
        | StepKind::While { body, .. }
        | StepKind::FuncDef { body, .. }
        | StepKind::Timeout { body, .. }
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
        _ => Vec::new(),
    }
}

/// Record an import site, deduping identical sites (diamonds share one
/// module record; each alias is a view onto it). `func_exports` rides
/// with the first site; every site loads the same parsed body.
fn record_site(
    files: &mut Vec<ClosureFile>,
    key: String,
    path: String,
    steps: Vec<Step>,
    func_exports: Vec<String>,
    type_exports: Vec<String>,
    site: ClosureSite,
) {
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
            steps,
            func_exports,
            type_exports,
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
    let resolved = dir
        .join(path)
        .map_err(|e| anyhow::anyhow!("IMPORT `{path}` cannot be resolved: {e}"))?;
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
    } = load_file(state, dir, stack, path)?;
    let nested = resolved.parent().unwrap_or_else(|| dir.clone());
    Ok(ExpandedImport {
        resolved,
        body,
        func_exports,
        type_exports,
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
    // re-validates on the backend.
    let resolved = base
        .join(path)
        .map_err(|e| anyhow::anyhow!("IMPORT `{path}` cannot be resolved: {e}"))?;
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
        });
    }
    let bytes = state
        .fs
        .read_file(&resolved)
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
    let dir = resolved.parent();
    state.modules.insert(
        key,
        super::state::ModuleRecord {
            steps: parsed.steps.clone(),
            func_exports: parsed.func_exports.clone(),
            type_exports: parsed.type_exports.clone(),
            dir,
            init: None,
        },
    );
    Ok(LoadedFile {
        resolved,
        steps: parsed.steps,
        func_exports: parsed.func_exports,
        type_exports: parsed.type_exports,
    })
}

/// Directory relative `IMPORT` paths resolve against, workspace-side:
/// the importing file's parent, the entry file's parent at top level,
/// or the workspace root (falling back to the build context) for
/// stdin/inline runs with no file. An entry staged outside the
/// workspace (CI runners materialize inline scripts in a temp dir)
/// carries no source-tree context, so it resolves like stdin against
/// the workspace root instead of the staging dir.
fn import_base<P: ProcessManager>(state: &ExecState<P>) -> GuardedPath {
    if let Some(top) = state.import_stack.last() {
        if let Some(parent) = top.parent() {
            return parent;
        }
    } else if let Some(parent) = state.entry_file.as_ref().and_then(|entry| entry.parent()) {
        let base = state
            .fs
            .workspace_root()
            .cloned()
            .unwrap_or_else(|| state.fs.build_context().clone());
        if !parent.as_path().starts_with(base.as_path()) {
            return base;
        }
        return parent;
    }
    if let Some(root) = state.fs.workspace_root() {
        return root.clone();
    }
    state.fs.build_context().clone()
}
