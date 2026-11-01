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
        if let StepKind::ImportFuncs { path, .. } = &step.kind {
            // Harvests never execute the file body: only its nested
            // imports load, mirroring execution.
            let (key, body, nested_dir) = dry_run_load(fs, dir, path, table, stack, cache)?;
            stack.push(key.clone());
            dry_run_walk_top_imports(&body, &key, &nested_dir, fs, table, stack, cache, out)?;
            stack.pop();
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
        if let StepKind::ImportFuncs { path, .. } = &step.kind {
            let (key, body, nested_dir) = dry_run_load(fs, dir, path, table, stack, cache)?;
            stack.push(key.clone());
            dry_run_walk_top_imports(&body, &key, &nested_dir, fs, table, stack, cache, out)?;
            stack.pop();
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
    fn bodies(kind: &StepKind) -> Vec<&[Step]> {
        match kind {
            StepKind::WithIo { cmd, .. } | StepKind::AssignCapture { cmd, .. } => bodies(cmd),
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
    for body in bodies(kind) {
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
    names: &[String],
) -> Result<()> {
    let (resolved, steps) = load_import(cx.state, path)?;
    let alias_frame = enter_alias_frame(cx.state);
    // Module semantics: the same harvest in the same frame loads once.
    // The key shares the file slot with full imports under a bare
    // marker (no alias can be empty), so either form satisfies the other.
    let completed_key = format!(
        "{alias_frame}\0{}\0{}",
        alias.unwrap_or(""),
        resolved.display()
    );
    if cx.state.import_completed.contains(&completed_key) {
        return Ok(());
    }
    // Join the import stack like a full import: nested loads need this
    // file on the chain for cycle/depth detection and for
    // importer-relative resolution. Skipped (completed) harvests never
    // reach here, so the push/pop stays balanced.
    cx.state.import_stack.push(resolved);
    let saved_aliases = cx.state.type_aliases.clone();
    let outcome: Result<()> = (|| {
        let aliases = super::typing::collect_type_aliases(&steps, cx.state)?;
        for (name, target) in aliases {
            cx.state.type_aliases.insert(name, target);
        }
        super::static_check::validate_import_body_types(&steps, cx.state)
            .map_err(|e| anyhow::anyhow!("IMPORT `{path}` failed validation: {e}"))?;
        for name in names {
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
            match alias {
                Some(alias) => cx.state.functions.define_aliased_in(
                    alias_frame,
                    alias,
                    name,
                    &resolved_params,
                    &def.1,
                )?,
                None => cx
                    .state
                    .functions
                    .define_script(name, &resolved_params, &def.1)?,
            }
        }
        // Nested imports are the harvested functions' dependencies: run
        // them per their own form. Anything else stays unrunned.
        for step in &steps {
            match &step.kind {
                StepKind::ImportFile { path, alias } => {
                    import_file(cx, path, alias)?;
                }
                StepKind::ImportFuncs { path, alias, names } => {
                    import_funcs(cx, path, alias.as_deref(), names)?;
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
    cx.state.type_aliases = saved_aliases;
    match outcome {
        Ok(()) => {
            cx.state.import_completed.insert(completed_key);
            Ok(())
        }
        Err(e) => Err(e),
    }
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

/// Execute `IMPORT ["<path>" AS <ALIAS>]`: load, parse, register, and run
/// the imported file, returning its `RETURN` value (`""` on fallthrough).
pub(crate) fn import_file<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    path: &str,
    alias: &str,
) -> Result<Value> {
    let (resolved, steps) = load_import(cx.state, path)?;
    let alias_frame = enter_alias_frame(cx.state);
    // Module semantics: the same file under the same alias in the same
    // frame loads once. Diamond imports share it instead of redefining
    // it; a skipped `LET`-capture binds `""`.
    let completed_key = format!("{alias_frame}\0{alias}\0{}", resolved.display());
    if cx.state.import_completed.contains(&completed_key) {
        return Ok(Value::string(String::new()));
    }
    // Merge the imported file's type aliases for its own validation and
    // execution; restored after the import returns so aliases never leak
    // across the import boundary.
    let saved_aliases = cx.state.type_aliases.clone();
    let outcome: Result<Value> = (|| {
        let aliases = super::typing::collect_type_aliases(&steps, cx.state)?;
        for (name, target) in aliases {
            cx.state.type_aliases.insert(name, target);
        }
        // Entry parity: statically validate the imported body before its
        // first step runs. Nested imports validate when their own IMPORT
        // steps execute.
        super::static_check::validate_import_body_types(&steps, cx.state)
            .map_err(|e| anyhow::anyhow!("IMPORT `{path}` failed validation: {e}"))?;
        // Register the imported file's top-level FUNCs under the alias in
        // the caller's scope: they persist after the import returns, while
        // the body's own SCRIPT:: definitions scope to the pushed frame.
        for step in &steps {
            if let StepKind::FuncDef {
                name, params, body, ..
            } = &step.kind
            {
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
                )?;
            }
        }
        cx.state.import_stack.push(resolved);
        cx.state.push_scope();
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
        )?;
        match flow {
            Flow::Done => Ok(Value::string(String::new())),
            Flow::Return { value, .. } => Ok(value),
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
        }
    })();
    let pop_res = cx.state.pop_scope();
    let _ = cx.state.import_stack.pop();
    if cx.state.import_stack.is_empty() {
        cx.state.import_alias_frame = None;
    }
    cx.state.type_aliases = saved_aliases;
    match (outcome, pop_res) {
        (Ok(value), Ok(())) => {
            cx.state.import_completed.insert(completed_key);
            Ok(value)
        }
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e),
    }
}

/// Load and parse an import target: resolve against the importer dir,
/// reject cycles and depth overflow, and reuse the per-run parse cache.
/// Returns the resolved path plus the parsed body; the caller pushes the
/// scope frame and the import-stack entry around execution.
fn load_import<P: ProcessManager>(
    state: &mut ExecState<P>,
    path: &str,
) -> Result<(GuardedPath, Vec<Step>)> {
    // Workspace-side resolution: imported files live next to the
    // importing file in the source tree (not in the snapshot), so join
    // lexically under the importer's own guard instead of routing
    // through the run resolver's snapshot root. `join` enforces the
    // base root's containment (`..` escapes fail here); `read_file`
    // re-validates on the backend.
    let base = import_base(state);
    let resolved = base
        .join(path)
        .map_err(|e| anyhow::anyhow!("IMPORT `{path}` cannot be resolved: {e}"))?;
    let key = resolved.display();
    if state.import_stack.iter().any(|p| p == &resolved) {
        let mut chain: Vec<String> = state.import_stack.iter().map(|p| p.display()).collect();
        chain.push(key.clone());
        bail!("IMPORT cycle detected: {}", chain.join(" -> "));
    }
    if state.import_stack.len() >= MAX_IMPORT_DEPTH {
        bail!(
            "IMPORT depth limit exceeded ({}): `{}`",
            MAX_IMPORT_DEPTH,
            path
        );
    }
    if let Some(cached) = state.import_cache.get(&key) {
        return Ok((resolved, cached.clone()));
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
    let steps =
        oxdock_parser::parse_script_with_modules(text, crate::lower_command, reserved, table)
            .map_err(|e| anyhow::anyhow!("IMPORT `{path}` failed to parse: {e}"))?;
    state.import_cache.insert(key, steps.clone());
    Ok((resolved, steps))
}

/// Directory relative `IMPORT` paths resolve against, workspace-side:
/// the importing file's parent, the entry file's parent at top level,
/// or the workspace root (falling back to the build context) for
/// stdin/inline runs with no file.
fn import_base<P: ProcessManager>(state: &ExecState<P>) -> GuardedPath {
    if let Some(top) = state.import_stack.last() {
        if let Some(parent) = top.parent() {
            return parent;
        }
    } else if let Some(parent) = state.entry_file.as_ref().and_then(|entry| entry.parent()) {
        return parent;
    }
    if let Some(root) = state.fs.workspace_root() {
        return root.clone();
    }
    state.fs.build_context().clone()
}
