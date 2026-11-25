use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use oxdock_diagnostics::DiagnosticResult;
use oxdock_fs::to_forward_slashes;
use oxdock_fs::{GuardedPath, WorkspaceSelection};
use oxdock_func_macro::oxdock_func;
use oxdock_parser::value::HandleValue;
use oxdock_parser::{
    Field, GuardExpr, KEYWORD_INSPECT, MODULE_SEPARATOR, SCRIPT_MODULE_NAME, STD_MODULE_NAME,
    SemaphoreState, Step, TypeTag, Value, base_name, guards_overlap, qualify, render_structural,
    split_qualified,
};
use oxdock_pipe::PipeHandle;
use oxdock_process::{CommandStdin, DefaultProcessManager, ProcessManager};

use super::diagnostics as std_diag;
use super::function_names::{
    FUNC_BOOL, FUNC_BOOL_OR, FUNC_PARSE_ARGS, FUNC_PARSE_FLAGS, FUNC_PARSE_POSITIONALS,
};
use super::io::StreamHandle;
use super::state::ExecState;
use super::steps::StepCtx;
use super::typing::TypeDescriptor;

/// Origin of a callable in the unified function registry. `Script` is an
/// interpreted `FUNC` body; `HostCtx` and `HostPure` are compiled Rust
/// functions (the `#[oxdock_func]` host export macro). Builtins and
/// runtime-registered hosts share the host kinds; only `DESCRIBE` output
/// shows the label, and both host kinds keep rendering as `host`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuncKind {
    Script,
    HostCtx,
    HostPure,
}

/// Canonical registry key for a file-imported script function:
/// defining file plus base name, through the shared [`qualify`]
/// owner. Importer aliases never appear here.
pub(super) fn file_func_key(defining_file: &str, name: &str) -> String {
    qualify(defining_file, name)
}

/// Loud ambiguity error for one unit importing the same alias from
/// several files. `files` sorts for deterministic rendering.
pub(super) fn ambiguous_alias_message(alias: &str, name: &str, files: &HashSet<String>) -> String {
    let mut files: Vec<&str> = files.iter().map(|file| file.as_str()).collect();
    files.sort();
    format!(
        "function `{alias}::{name}` is ambiguous: alias `{alias}` is imported from multiple files ({})",
        files.join(", ")
    )
}

/// One `ALIAS::FUNC` spelling's fate under a unit's alias targets.
/// Single owner for alias-target matching: every resolution path
/// (static calls, template gates, runtime dispatch, placeholder
/// misses, snapshots, re-exports) classifies here and acts on the
/// result, never restating the singleton/ambiguity branches.
pub(super) enum AliasResolution {
    /// Resolve as spelled (bare, `SCRIPT::`, hosts, unknown-to-unit).
    Global,
    /// Canonical defining-file key.
    Canonical(String),
    /// Known alias with no usable target: the caller reports unknown.
    Unknown,
    /// One alias, several files with the name in overlapping guards:
    /// fail loud, naming the colliding spelling.
    Ambiguous {
        alias: String,
        base: String,
        files: HashSet<String>,
    },
    /// Several files hold the name under disjoint guards: the caller
    /// validates every target instead of failing outright
    /// (platform-conditional imports). `has_host` marks a disjoint
    /// host-harvest branch (a `(None, guard)` target) alongside the
    /// files: that branch holds no file functions, so any file-backed
    /// call misses there.
    Any {
        base: String,
        files: HashSet<String>,
        has_host: bool,
    },
}

/// One unit's alias targets: defining file plus the import's guard.
/// Single owner for the target-set shape; static and runtime tables
/// share it. Guards decide ambiguity (overlapping guards importing
/// different files under one alias are ambiguous); unguarded imports
/// overlap everything. Runtime records `None` (its imports execute
/// once taken).
pub(super) type AliasTargets = HashMap<String, Vec<(Option<String>, Option<GuardExpr>)>>;

/// Shared frame-walk precedence for script resolution (#322):
/// innermost nested frames, then the owning file top-level, then
/// per-unit harvests. Static checking and runtime dispatch implement
/// this trait over their own stores and share [`FuncFrameView::walk`],
/// so precedence lives once and the twins cannot drift.
pub(super) trait FuncFrameView {
    /// Resolution hit for this layer (entry, info+key, etc.).
    type Hit;
    /// Innermost nested frames outward.
    fn find_nested(&self, base: &str) -> Option<Self::Hit>;
    /// Owning file top-level (entry included via its unit key).
    fn find_home(&self, unit: &str, base: &str) -> Option<Self::Hit>;
    /// Per-unit bare harvests.
    fn find_harvest(&self, unit: &str, base: &str) -> Option<Self::Hit>;
    /// Precedence owner: nested, home, harvest.
    fn walk(&self, unit: &str, base: &str) -> Option<Self::Hit> {
        self.find_nested(base)
            .or_else(|| self.find_home(unit, base))
            .or_else(|| self.find_harvest(unit, base))
    }
}

/// Classify one call spelling under a unit's alias targets. Single
/// entry point for alias-target matching: splits the spelling,
/// resolves `SCRIPT::` and bare names globally, and maps file aliases
/// through the unit's recorded targets. `exists` probes the caller's
/// function map for a canonical key.
pub(super) fn classify_alias_call(
    unit_targets: &AliasTargets,
    name: &str,
    exists: &dyn Fn(&str) -> bool,
) -> AliasResolution {
    let Some((alias, base)) = split_qualified(name) else {
        return AliasResolution::Global;
    };
    if alias == SCRIPT_MODULE_NAME {
        return AliasResolution::Global;
    }
    let Some(entries) = unit_targets.get(alias) else {
        return AliasResolution::Global;
    };
    if entries.is_empty() {
        return AliasResolution::Unknown;
    }
    // Distinct defining files in first-seen order; `(None, guard)`
    // entries are host-harvest branches holding no file functions.
    let mut files: Vec<(&str, Vec<Option<&GuardExpr>>)> = Vec::new();
    let mut host_guards: Vec<Option<&GuardExpr>> = Vec::new();
    for (file, guard) in entries {
        match file {
            Some(path) => match files.iter_mut().find(|(known, _)| *known == path) {
                Some((_, guards)) => guards.push(guard.as_ref()),
                None => files.push((path, vec![guard.as_ref()])),
            },
            None => host_guards.push(guard.as_ref()),
        }
    }
    if files.is_empty() {
        return AliasResolution::Unknown;
    }
    let overlaps = |left: &[Option<&GuardExpr>], right: &[Option<&GuardExpr>]| {
        left.iter()
            .any(|a| right.iter().any(|b| guards_overlap(*a, *b)))
    };
    if files.len() == 1 {
        // A lone file target stays canonical unless a host-harvest
        // branch is disjoint from it: on that platform the alias
        // holds no file function, so the call misses there.
        let disjoint_host = host_guards
            .iter()
            .any(|host| !overlaps(&files[0].1, &[*host]));
        if !disjoint_host {
            return AliasResolution::Canonical(file_func_key(files[0].0, base));
        }
        let mut all = HashSet::new();
        all.insert(files[0].0.to_string());
        return AliasResolution::Any {
            base: base.to_string(),
            files: all,
            has_host: true,
        };
    }
    let mut ambiguous = HashSet::new();
    for (i, (file, guards)) in files.iter().enumerate() {
        for (other, other_guards) in files.iter().skip(i + 1) {
            if overlaps(guards, other_guards)
                && (exists(&file_func_key(file, base)) || exists(&file_func_key(other, base)))
            {
                ambiguous.insert((*file).to_string());
                ambiguous.insert((*other).to_string());
            }
        }
    }
    if !ambiguous.is_empty() {
        return AliasResolution::Ambiguous {
            alias: alias.to_string(),
            base: base.to_string(),
            files: ambiguous,
        };
    }
    let all: HashSet<String> = files.iter().map(|(file, _)| (*file).to_string()).collect();
    if all.iter().any(|file| exists(&file_func_key(file, base))) {
        return AliasResolution::Any {
            base: base.to_string(),
            files: all,
            has_host: !host_guards.is_empty(),
        };
    }
    AliasResolution::Unknown
}

/// Missing-import hint for an unknown function (#319): when `base`
/// is `EXPORT`ed by already-loaded files, name them so the error
/// directs the author to the fix instead of dead-ending. `files`
/// pairs each loaded file's key with its exported func names (only
/// exported names suggest: privates would fail the `EXPORT` gate);
/// `unit_targets` is the failing unit's alias map for `did you mean
/// ALIAS::BASE` suggestions. Bare and `SCRIPT::` spellings search
/// the same way. Labels sort for determinism. Only unknown-function
/// errors append this; nothing else restates the exporter search.
pub(super) fn missing_import_hint<'a>(
    files: impl IntoIterator<Item = (&'a str, &'a [String])>,
    unit_targets: &AliasTargets,
    name: &str,
) -> Option<String> {
    let base = match split_qualified(name) {
        Some((_, base)) => base,
        None => name,
    };
    if base.is_empty() {
        return None;
    }
    let mut exporters: Vec<&str> = files
        .into_iter()
        .filter(|(_, exports)| exports.iter().any(|exported| exported == base))
        .map(|(file, _)| file)
        .collect();
    exporters.sort();
    exporters.dedup();
    if exporters.is_empty() {
        return None;
    }
    // Aliases the failing unit already imports each exporter under.
    let imported_as = |file: &str| {
        let mut aliases: Vec<&str> = unit_targets
            .iter()
            .filter(|(_, targets)| {
                targets
                    .iter()
                    .any(|(target, _)| target.as_deref() == Some(file))
            })
            .map(|(alias, _)| alias.as_str())
            .collect();
        aliases.sort();
        aliases
    };
    if exporters.len() == 1 {
        let file = exporters[0];
        let aliases = imported_as(file);
        // Prefer a working spelling: when the file is imported under
        // several aliases, an alias spelling the failed call
        // differently resolves, so suggest it before falling back to
        // the self-view diagnostic.
        let import_alias = aliases
            .iter()
            .find(|alias| qualify(alias, base) != name)
            .or(aliases.first())
            .copied();
        let suggestion = match import_alias {
            Some(import_alias) => {
                let spelling = qualify(import_alias, base);
                if spelling == name {
                    // Disjoint-guard view: the file exports the name
                    // but this alias resolves it elsewhere here.
                    format!(
                        "file \"{file}\" exports `{base}` but the `{import_alias}` view does not resolve it here"
                    )
                } else {
                    format!("did you mean `{spelling}`? (file \"{file}\" exports `{base}`)")
                }
            }
            None => {
                // Forward slashes: a raw Windows path would emit
                // invalid escapes inside the quoted suggestion.
                let path = to_forward_slashes(file);
                format!(
                    "file \"{file}\" exports `{base}`: add `[FUNC {base} FROM \"{path}\"]` to an IMPORT"
                )
            }
        };
        return Some(format!("; {suggestion}"));
    }
    let mut files = exporters.join("\", \"");
    files.insert(0, '"');
    files.push('"');
    Some(format!(
        "; files {files} export `{base}`: qualify the call with the importing alias"
    ))
}

/// Whether one `EXPORT CONST` entry serves reads of stored `name`
/// (the sigil-stripped binding key): the entry itself, or a
/// qualified `ALIAS::NAME` forward of the same base. Single owner
/// for the cover rule every qualified-`CONST` path matches on.
pub(super) fn const_export_covers(entry: &str, name: &str) -> bool {
    base_name(entry) == name
}

/// How a module holds a top-level binding, for broken-forward
/// diagnostics: a `CONST`, a never-exportable `LET`, or nothing.
pub(super) enum BindingPresence {
    Const,
    Let,
    Absent,
}

/// Missing-`CONST` hint from the queried alias's own files: an
/// unexported `CONST` or a private `LET` with the same base names the
/// real fault instead of a bare unknown-name miss. Single owner for
/// the hint every qualified-`CONST` miss renders, so static and
/// runtime agree word for word. Reads only `view.binding`, never a
/// twin-local store scan.
pub(super) fn missing_const_hint<S: ModuleGraphView>(
    view: &S,
    targets: &AliasTargets,
    alias: &str,
    name: &str,
) -> Option<String> {
    targets
        .get(alias)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(file, _)| file.as_ref())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
        .iter()
        .find_map(|file| match view.binding(file, name) {
            BindingPresence::Const => Some(format!(
                "module holds `{name}` as CONST but does not export it: add `EXPORT CONST ${name}`"
            )),
            BindingPresence::Let => Some(format!(
                "`{name}` is a LET binding and cannot be exported: only CONST bindings cross the module boundary"
            )),
            BindingPresence::Absent => None,
        })
}

/// Read-only view of the module graph one qualified-`CONST`
/// resolution needs. Implemented once for live runtime stores and
/// once for static closure tables, so the walk below runs
/// identically on both and the twins cannot drift: static
/// validation, eager closure checks, and serving all chase through
/// this one owner.
pub(super) trait ModuleGraphView {
    /// Raw `EXPORT CONST` entries (stored bare names and qualified forwards).
    fn const_exports(&self, file: &str) -> Option<&[String]>;
    /// Outbound alias targets for `file`'s own unit: the namespace
    /// each forward hop resolves in.
    fn alias_targets(&self, file: &str) -> AliasTargets;
    /// How `file` holds top-level `name`, for broken-link detail.
    fn binding(&self, file: &str, name: &str) -> BindingPresence;
}

/// One `ALIAS::NAME` read's holder files: the unit's alias targets
/// classified through the shared matcher, kept only when the file
/// covers the base. Single owner for the holder half of
/// qualified-`CONST` resolution; the chase half is
/// [`chase_const_forward`].
#[derive(Debug)]
pub(super) enum ConstHolders {
    /// Covering files, sorted, possibly empty (callers run their
    /// own missing-name hints then).
    Found(Vec<String>),
    /// The alias is unknown to this unit.
    NoAlias,
    /// Several overlapping files hold the base: fail loud.
    Ambiguous {
        alias: String,
        base: String,
        files: HashSet<String>,
    },
}

/// Suffix probe for the matcher's canonical keys (`file::base`):
/// the base is ours, so stripping it recovers the file exactly,
/// and only a covering file counts as a holder. Single owner for
/// the suffix shape, so call sites never restate the separator.
fn const_canonical_suffix(name: &str) -> String {
    format!("{MODULE_SEPARATOR}{name}")
}

/// Classify one qualified-`CONST` read to its holder files.
pub(super) fn const_holder_files<S: ModuleGraphView>(
    view: &S,
    unit_targets: &AliasTargets,
    alias: &str,
    name: &str,
) -> ConstHolders {
    let qualified = qualify(alias, name);
    let suffix = const_canonical_suffix(name);
    // Suffix probe for the matcher's canonical keys
    // (`file::base`): the base is ours, so stripping it recovers the
    // file exactly, and only a covering file counts as a holder.
    let exports = |canonical: &str| {
        canonical
            .strip_suffix(suffix.as_str())
            .and_then(|file| view.const_exports(file))
            .is_some_and(|names| names.iter().any(|entry| const_export_covers(entry, name)))
    };
    let mut holders = match classify_alias_call(unit_targets, &qualified, &exports) {
        AliasResolution::Global | AliasResolution::Unknown => return ConstHolders::NoAlias,
        AliasResolution::Ambiguous { alias, base, files } => {
            return ConstHolders::Ambiguous { alias, base, files };
        }
        AliasResolution::Canonical(canonical) => {
            let Some(file) = canonical.strip_suffix(suffix.as_str()) else {
                return ConstHolders::NoAlias;
            };
            vec![file.to_string()]
        }
        AliasResolution::Any { mut files, .. } => {
            let mut files: Vec<String> = files.drain().collect();
            files.sort();
            files
        }
    };
    holders.retain(|file| {
        view.const_exports(file)
            .is_some_and(|names| names.iter().any(|entry| const_export_covers(entry, name)))
    });
    ConstHolders::Found(holders)
}

/// Failure modes of [`chase_const_forward`], rendered by each caller
/// with its own position context (static line/col, runtime none,
/// eager file attribution). Wording lives here once, on [`std::fmt::Display`],
/// so diagnostics cannot drift between the twins.
#[derive(Debug)]
pub(super) enum ForwardError {
    /// The holder file itself is gone from the view.
    Unknown { original: String },
    /// A forward hop lands where the base is missing.
    Broken {
        original: String,
        hop: String,
        detail: BrokenDetail,
    },
    /// One file forwards the same base from several aliases.
    Split { original: String, hops: Vec<String> },
    /// Hops revisit a spelling. Import cycles are rejected before
    /// resolution, so this is defense in depth.
    Cycle { chain: Vec<String> },
    /// One hop resolves to several overlapping files.
    HopAmbiguous {
        alias: String,
        base: String,
        files: HashSet<String>,
    },
}

/// Why a forward hop lands nowhere.
#[derive(Debug)]
pub(super) enum BrokenDetail {
    /// The terminal file holds the base as an unexported `CONST`.
    UnexportedConst,
    /// The terminal file holds the base as a `LET`.
    Let,
    /// Nothing there answers to the base.
    Absent,
}

impl std::fmt::Display for ForwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForwardError::Unknown { original } => write!(f, "unknown CONST `{original}`"),
            ForwardError::Broken {
                original,
                hop,
                detail,
            } => {
                let base = base_name(hop);
                let detail = match detail {
                    BrokenDetail::UnexportedConst => format!(
                        "module holds `{base}` as CONST but does not export it: add `EXPORT CONST ${base}`"
                    ),
                    BrokenDetail::Let => format!(
                        "`{base}` is a LET binding and cannot be exported: only CONST bindings cross the module boundary"
                    ),
                    BrokenDetail::Absent => {
                        let (forward, _) = split_qualified(hop).unwrap_or(("", base));
                        format!("no import alias `{forward}` exports `{base}`")
                    }
                };
                write!(
                    f,
                    "unknown CONST `{original}`: re-export `{hop}` does not resolve: {detail}"
                )
            }
            ForwardError::Split { original, hops } => write!(
                f,
                "ambiguous CONST `{original}`: re-exported from {}; export exactly one source",
                hops.join(" and ")
            ),
            ForwardError::Cycle { chain } => {
                write!(f, "CONST re-export cycle detected: {}", chain.join(" -> "))
            }
            ForwardError::HopAmbiguous { alias, base, files } => write!(
                f,
                "{}",
                ambiguous_alias_message(alias, base, files).replacen("function", "CONST", 1)
            ),
        }
    }
}

impl std::error::Error for ForwardError {}

/// Follow qualified `EXPORT CONST $ALIAS::NAME` forwards from `file`
/// to the files holding the stored bare binding, resolving each hop
/// in the exporting file's own unit. A local bare definition shadows
/// forwards; disjoint-guard branches each chase to their own
/// terminal, so all terminals return (callers apply their own
/// multi-holder policy). Each hop preserves the base name.
pub(super) fn chase_const_forward<S: ModuleGraphView>(
    view: &S,
    file: &str,
    name: &str,
    original: &str,
) -> Result<Vec<String>, ForwardError> {
    let mut terminals: Vec<String> = Vec::new();
    chase_from(view, file, name, original, &mut Vec::new(), &mut terminals)?;
    terminals.sort();
    terminals.dedup();
    Ok(terminals)
}

fn chase_from<S: ModuleGraphView>(
    view: &S,
    current: &str,
    name: &str,
    original: &str,
    chain: &mut Vec<String>,
    terminals: &mut Vec<String>,
) -> Result<(), ForwardError> {
    let suffix = const_canonical_suffix(name);
    let covering = |canonical: &str| {
        canonical
            .strip_suffix(suffix.as_str())
            .and_then(|file| view.const_exports(file))
            .is_some_and(|names| names.iter().any(|entry| const_export_covers(entry, name)))
    };
    let mut current = current.to_string();
    loop {
        let Some(names) = view.const_exports(current.as_str()) else {
            return Err(ForwardError::Unknown {
                original: original.to_string(),
            });
        };
        if names.iter().any(|entry| entry == name) {
            terminals.push(current);
            return Ok(());
        }
        let mut aliases: Vec<&str> = Vec::new();
        for entry in names {
            if let Some((alias, base)) = split_qualified(entry)
                && base == name
                && !aliases.contains(&alias)
            {
                aliases.push(alias);
            }
        }
        if aliases.is_empty() {
            if chain.is_empty() {
                return Err(ForwardError::Unknown {
                    original: original.to_string(),
                });
            }
            let hop = chain.last().expect("chain never empty").clone();
            let detail = match view.binding(current.as_str(), name) {
                BindingPresence::Const => BrokenDetail::UnexportedConst,
                BindingPresence::Let => BrokenDetail::Let,
                BindingPresence::Absent => BrokenDetail::Absent,
            };
            return Err(ForwardError::Broken {
                original: original.to_string(),
                hop,
                detail,
            });
        }
        if aliases.len() > 1 {
            let mut hops: Vec<String> = aliases.iter().map(|alias| qualify(alias, name)).collect();
            hops.sort();
            return Err(ForwardError::Split {
                original: original.to_string(),
                hops,
            });
        }
        let forward = aliases[0];
        let hop = qualify(forward, name);
        if chain.contains(&hop) {
            chain.push(hop);
            return Err(ForwardError::Cycle {
                chain: chain.clone(),
            });
        }
        chain.push(hop.clone());
        let targets = view.alias_targets(current.as_str());
        match classify_alias_call(&targets, &hop, &covering) {
            AliasResolution::Global | AliasResolution::Unknown => {
                return Err(ForwardError::Broken {
                    original: original.to_string(),
                    hop,
                    detail: BrokenDetail::Absent,
                });
            }
            AliasResolution::Ambiguous { alias, base, files } => {
                return Err(ForwardError::HopAmbiguous { alias, base, files });
            }
            AliasResolution::Canonical(canonical) => {
                let Some(next) = canonical.strip_suffix(suffix.as_str()) else {
                    return Err(ForwardError::Unknown {
                        original: original.to_string(),
                    });
                };
                current = next.to_string();
            }
            AliasResolution::Any { mut files, .. } => {
                let mut covering: Vec<String> = files
                    .drain()
                    .filter(|file| {
                        view.const_exports(file).is_some_and(|names| {
                            names.iter().any(|entry| const_export_covers(entry, name))
                        })
                    })
                    .collect();
                covering.sort();
                match covering.len() {
                    0 => {
                        return Err(ForwardError::Broken {
                            original: original.to_string(),
                            hop,
                            detail: BrokenDetail::Absent,
                        });
                    }
                    1 => {
                        current = covering.into_iter().next().expect("len checked");
                    }
                    _ => {
                        for branch in covering {
                            let mut branch_chain = chain.clone();
                            chase_from(
                                view,
                                branch.as_str(),
                                name,
                                original,
                                &mut branch_chain,
                                terminals,
                            )?;
                        }
                        return Ok(());
                    }
                }
            }
        }
    }
}
/// Whether a disjoint-alias template call names a script function,
/// owned here so every placeholder path agrees with `resolve_call`:
/// visible only when no host-harvest branch competes (`!has_host`)
/// and **every** candidate file defines `base` (universal, not
/// existential: a missing branch fails the `LET` path, so the
/// template must not suggest it). `exists` probes canonical
/// file-function keys for script definitions.
pub(super) fn disjoint_template_script_visible(
    base: &str,
    files: &HashSet<String>,
    has_host: bool,
    exists: &dyn Fn(&str) -> bool,
) -> bool {
    !has_host && !files.is_empty() && files.iter().all(|file| exists(&file_func_key(file, base)))
}

/// Miss wording for a disjoint-alias placeholder call, shared by the
/// static gate, the lazy runtime path, and (via the script set) the
/// EXPAND snapshot. Script-visible branches read as script misses
/// (the `LET` path); otherwise the alias is known but holds no
/// callable function here. Always renders: `Any` only arises for a
/// known alias, so the generic unknown wording never applies.
pub(super) fn disjoint_alias_placeholder_miss(
    module: &str,
    name: &str,
    base: &str,
    files: &HashSet<String>,
    has_host: bool,
    is_script: &dyn Fn(&str) -> bool,
) -> String {
    let script = disjoint_template_script_visible(base, files, has_host, is_script);
    describe_placeholder_miss(module, name, script, false, !script)
        .expect("disjoint miss always renders")
}

/// Every local spelling of one defining file's function in a unit.
/// Inverted direction of [`classify_alias_call`]; shares its matcher.
/// `is_script` probes the caller's registry for script bodies, so
/// disjoint platform aliases covering every branch with a script
/// definition read as direct spellings (matching the static gate and
/// the lazy path through [`disjoint_alias_placeholder_miss`]).
pub(super) fn spellings_for_defining_file(
    unit_targets: &AliasTargets,
    defining: &str,
    base: &str,
    is_script: &dyn Fn(&str) -> bool,
) -> (Vec<String>, Vec<(String, HashSet<String>)>) {
    let mut direct = Vec::new();
    let mut ambiguous = Vec::new();
    for (alias, files) in unit_targets {
        if !files
            .iter()
            .any(|(file, _)| file.as_deref() == Some(defining))
        {
            continue;
        }
        let spelling = qualify(alias, base);
        match classify_alias_call(unit_targets, &spelling, &|key| {
            key == file_func_key(defining, base)
        }) {
            AliasResolution::Canonical(_) => direct.push(spelling),
            AliasResolution::Ambiguous { files, .. } => {
                ambiguous.push((spelling, files));
            }
            AliasResolution::Any {
                base,
                files,
                has_host,
            } => {
                if disjoint_template_script_visible(&base, &files, has_host, is_script) {
                    direct.push(spelling);
                }
            }
            AliasResolution::Global | AliasResolution::Unknown => {}
        }
    }
    (direct, ambiguous)
}

impl FuncKind {
    pub fn label(&self) -> &'static str {
        match self {
            FuncKind::Script => "script",
            FuncKind::HostCtx | FuncKind::HostPure => "host",
        }
    }
}

/// One declared parameter of a registered function. `param_type` is `None`
/// for unconstrained `Value` parameters (the macro accepts any value).
/// `allowed` names the closed value set for constrained `STRING`
/// parameters, declared once via `#[values(...)]`: the generated
/// extractor enforces membership and the reference renders the set,
/// so the list cannot rot apart from the check. `docs` carries the
/// parameter's description (parsed from the `# Arguments` doc section
/// for macro functions, written inline for hand-built entries) and
/// renders into every reference surface. `options` declares the known
/// keys of a `MAP` options parameter: same single-source rule as
/// `allowed`, with per-key types, required flags, defaults, and docs.
/// `optional` marks a trailing `MAP` parameter callers may omit: the
/// runtime fills an empty MAP, and signatures render `= {}`. Only
/// trailing `MAP` params may be optional (run-start validation
/// rejects anything else), so omission always means "no options".
/// The macro never emits optional params: only hand-built entries
/// use them, and only for options MAPs.
#[derive(Debug, Clone)]
pub struct FuncParam {
    pub name: String,
    pub param_type: Option<TypeTag>,
    pub allowed: Option<&'static [&'static str]>,
    pub docs: &'static str,
    pub options: Option<&'static [ParamOption]>,
    pub optional: bool,
}

/// Required argument count: total params minus trailing optionals.
/// Callers may pass any count in `required..=total`; omitted trailing
/// MAPs fill with an empty MAP. Shared by the static arity gate and
/// the runtime gate so the two can never disagree.
pub fn required_arity(params: &[FuncParam]) -> usize {
    params
        .iter()
        .rev()
        .take_while(|param| param.optional)
        .fold(params.len(), |count, _| count - 1)
}

/// Fill omitted trailing optional MAP params with empty MAPs. The
/// caller guarantees `values.len()` is within
/// `required_arity(params)..=params.len()`; anything else is a bug,
/// not a user error, so over- and under-counts panic loudly.
pub fn fill_optional_args(params: &[FuncParam], mut values: Vec<Value>) -> Vec<Value> {
    assert!(
        values.len() <= params.len(),
        "arity gate must run before fill_optional_args"
    );
    assert!(
        values.len() >= required_arity(params),
        "arity gate must run before fill_optional_args"
    );
    while values.len() < params.len() {
        values.push(Value::map(BTreeMap::new()));
    }
    values
}

/// One known key of a `MAP` options parameter: name, value type,
/// whether callers must pass it, the rendered default for optional
/// keys, and what it controls. Declared once per options parameter
/// (macro `#[options(...)]` plus the `# Options` doc section, or
/// inline for hand-built entries) and rendered into signatures,
/// `DESCRIBE` output, and generated references alike.
#[derive(Debug, Clone)]
pub struct ParamOption {
    pub name: &'static str,
    pub value: TypeTag,
    pub required: bool,
    pub default: Option<&'static str>,
}

/// Introspectable metadata for one function. Single source for
/// `DESCRIBE(name)` output and the static function reference. `rpn` names
/// whether the function also runs on the compiled math path (`true` for
/// pure functions and opted-in stateful ones); everything runs on the AST
/// path.
#[derive(Debug, Clone)]
pub struct FuncMeta {
    pub name: String,
    /// Owning module (`STD` for builtins, `SCRIPT` for DSL definitions,
    /// the host module name otherwise). `name` is always the qualified
    /// `MODULE::BASE` form, so listings and `DESCRIBE` never lose origin.
    pub module: String,
    pub kind: FuncKind,
    pub params: Option<Vec<FuncParam>>,
    pub returns: Option<TypeTag>,
    pub rpn: bool,
    pub summary: &'static str,
    pub docs: &'static str,
    /// Executable examples, rendered as ```oxdock fences into the
    /// generated reference and executed by docs_conformance like
    /// command examples. The single precondition mechanism: the
    /// returns gate executes these blocks to cover functions blind
    /// sampling cannot reach.
    pub examples: &'static [crate::Example],
}

/// Pure scalar function: no filesystem, no scope, no process access.
/// Usable from both AST evaluation and compiled RPN math.
pub type PureFn = Arc<dyn Fn(Vec<Value>) -> Result<Value> + Send + Sync>;

/// Stateful/IO function with full step context (fs, cwd, envs, vars, pipes).
pub type NativeFn<P> = Arc<dyn Fn(&mut StepCtx<P>, Vec<Value>) -> Result<Value> + Send + Sync>;

/// Export hook for a DSL function, implemented by `#[oxdock_func]` on a
/// registration marker. The engine calls `registration()` and never names
/// a generated symbol.
pub trait OxDockFn<P: ProcessManager> {
    /// The registry entry deriving from the Rust signature plus doc
    /// comments: name, metadata, and entry point, rendered.
    fn registration() -> HostRegistration<P>;
}

/// One host-registered function, grouped into a [`HostModule`] and passed
/// to `Engine::register_module`. Build the entry with the `#[oxdock_func]`-
/// generated registration marker, or by hand. `Pure` entries run
/// on both the AST and the compiled RPN math paths; `Stateful` entries run
/// on the AST path with full step context. `Both` entries carry a stateful
/// body plus a pure string projection: every context with step IO runs
/// the body, while `{{ }}` placeholder expansion (which has no step
/// context) runs the projection.
pub enum HostRegistration<P: ProcessManager> {
    Stateful {
        name: String,
        meta: FuncMeta,
        func: NativeFn<P>,
    },
    Pure {
        name: String,
        meta: FuncMeta,
        func: PureFn,
    },
    Both {
        name: String,
        meta: FuncMeta,
        func: NativeFn<P>,
        template: PureFn,
    },
}

// Manual `Clone` (a derive would demand `P: Clone`): entries share their
// function pointers through the `Arc`s and deep-copy the metadata.
impl<P: ProcessManager> Clone for HostRegistration<P> {
    fn clone(&self) -> Self {
        match self {
            HostRegistration::Stateful { name, meta, func } => HostRegistration::Stateful {
                name: name.clone(),
                meta: meta.clone(),
                func: Arc::clone(func),
            },
            HostRegistration::Pure { name, meta, func } => HostRegistration::Pure {
                name: name.clone(),
                meta: meta.clone(),
                func: Arc::clone(func),
            },
            HostRegistration::Both {
                name,
                meta,
                func,
                template,
            } => HostRegistration::Both {
                name: name.clone(),
                meta: meta.clone(),
                func: Arc::clone(func),
                template: Arc::clone(template),
            },
        }
    }
}

impl<P: ProcessManager> HostRegistration<P> {
    /// Registry name this entry answers to. The lock test in `tests.rs`
    /// (`function_names_match_registry`) asserts every
    /// `function_names::FUNC_*` constant against this, so a rename of
    /// the Rust function (which moves the macro-derived name) fails
    /// loudly instead of leaving diagnostics text behind.
    pub fn name(&self) -> &str {
        match self {
            HostRegistration::Stateful { name, .. } => name,
            HostRegistration::Pure { name, .. } => name,
            HostRegistration::Both { name, .. } => name,
        }
    }

    /// Introspectable metadata for this entry, for documentation tooling
    /// that reads modules without registering them.
    pub fn meta(&self) -> &FuncMeta {
        match self {
            HostRegistration::Stateful { meta, .. } => meta,
            HostRegistration::Pure { meta, .. } => meta,
            HostRegistration::Both { meta, .. } => meta,
        }
    }
}

/// One user-defined function body (`FUNC NAME($p: TYPE, ...): RET { ... }`).
/// Param and return types resolve to tags at definition time, where the type
/// directory is in scope; the registry never holds raw type strings.
/// The flag marks trailing `?` params the caller may omit (filled with
/// [`optional_zero_value`] at call time).
#[derive(Debug, Clone)]
pub(super) struct FuncDefData {
    pub(super) params: Vec<(String, TypeTag, bool)>,
    pub(super) return_type: TypeTag,
    pub(super) return_spelling: String,
    pub(super) is_async: bool,
    pub(super) body: Vec<Step>,
    /// Owning file module (#115, resolved display path): module-owned
    /// `FUNC`s resolve names against the module's shared store after
    /// lexical frames miss. `None` for entry/REPL definitions, which
    /// stay params-only.
    pub(super) module: Option<String>,
    /// Definition-site jail (#322): the workspace selection and cwd
    /// as they stood when this `FUNC` was defined. `Some` for
    /// definitions executed at file top level (entry or module init);
    /// `None` for call-time materializations and bodies, which must
    /// never capture caller ambient (the file fallback applies).
    pub(super) def_jail: Option<(WorkspaceSelection, GuardedPath)>,
}

/// Zero value for an omitted trailing optional script param, by
/// resolved tag. Scalars take falsy empties and collections empty
/// shapes (mirroring the host optional-`MAP` fill); records qualify
/// when every field is optional or defaulted (binding fills the
/// defaults). Handles, pipes, custom types, unions, and the
/// remaining words have no zero: omitting those fails at definition,
/// so the caller passes explicitly.
pub fn optional_zero_value(tag: &TypeTag) -> DiagnosticResult<Value> {
    match tag {
        TypeTag::Bool => Ok(Value::bool(false)),
        TypeTag::String | TypeTag::Any => Ok(Value::string(String::new())),
        TypeTag::Int => Ok(Value::int(0)),
        TypeTag::Float => Ok(Value::float(0.0)),
        TypeTag::List | TypeTag::ListOf(_) => Ok(Value::list(Vec::new())),
        TypeTag::Map | TypeTag::MapOf(_) => Ok(Value::map(BTreeMap::new())),
        TypeTag::Record(fields)
            if fields
                .iter()
                .all(|field| field.optional || field.default.is_some()) =>
        {
            Ok(Value::map(BTreeMap::new()))
        }
        other => Err(std_diag::func_zero_value_missing(
            &oxdock_parser::render_structural(other),
        )),
    }
}

/// Resolve script `FUNC` param spellings to tags in one place: tag
/// resolution, declaration checks, and optional zero-value validation.
/// All definition paths (direct, static check, file import) funnel
/// here, so a required-after-optional or zero-less optional fails
/// identically everywhere instead of drifting by call site.
pub fn resolve_script_params<P: ProcessManager>(
    state: &ExecState<P>,
    func_name: &str,
    params: &[(String, String, bool)],
) -> DiagnosticResult<Vec<(String, TypeTag, bool)>> {
    let mut resolved = Vec::with_capacity(params.len());
    for (pname, ptype, optional) in params {
        let clean = pname.trim_start_matches('$').to_string();
        let tag = state
            .resolve_tag(ptype)
            .map_err(|err| std_diag::func_param_type(func_name, &anyhow::Error::new(err)))?;
        state.check_declaration_tag(&format!("function `{func_name}` param `${clean}`"), &tag)?;
        if *optional {
            optional_zero_value(&tag).map(|_| ()).map_err(|err| {
                std_diag::func_optional_param(func_name, &clean, &anyhow::Error::new(err))
            })?;
        }
        resolved.push((clean, tag, *optional));
    }
    Ok(resolved)
}

/// Validate already-resolved script param tags: the zero-value gate
/// for registry insertion paths that never see spellings.
pub fn check_script_param_tags(
    func_name: &str,
    params: &[(String, TypeTag, bool)],
) -> DiagnosticResult<()> {
    for (pname, tag, optional) in params {
        if *optional {
            optional_zero_value(tag).map(|_| ()).map_err(|err| {
                std_diag::func_optional_param(func_name, pname, &anyhow::Error::new(err))
            })?;
        }
    }
    Ok(())
}

/// Executable body behind one registry entry: an interpreted script, a
/// pure scalar function, a stateful function with step context, or a
/// dual entry whose pure arm serves `{{ }}` placeholders while the ctx
/// arm serves every context that carries step IO.
pub(super) enum FuncBody<P: ProcessManager> {
    Script(FuncDefData),
    Pure(PureFn),
    Ctx(NativeFn<P>),
    Both { pure: PureFn, ctx: NativeFn<P> },
}

// Manual `Clone` (a derive would demand `P: Clone`).
impl<P: ProcessManager> Clone for FuncBody<P> {
    fn clone(&self) -> Self {
        match self {
            FuncBody::Script(def) => FuncBody::Script(def.clone()),
            FuncBody::Pure(func) => FuncBody::Pure(Arc::clone(func)),
            FuncBody::Ctx(func) => FuncBody::Ctx(Arc::clone(func)),
            FuncBody::Both { pure, ctx } => FuncBody::Both {
                pure: Arc::clone(pure),
                ctx: Arc::clone(ctx),
            },
        }
    }
}

/// One entry in the unified function registry: introspectable metadata
/// (mandatory for every entry, backing the pre-evaluation arity gate)
/// plus the executable body.
pub(super) struct FuncEntry<P: ProcessManager> {
    pub(super) meta: FuncMeta,
    pub(super) body: FuncBody<P>,
}

// Manual `Clone` (a derive would demand `P: Clone`).
impl<P: ProcessManager> Clone for FuncEntry<P> {
    fn clone(&self) -> Self {
        Self {
            meta: self.meta.clone(),
            body: self.body.clone(),
        }
    }
}

/// Names defined in one lexical scope frame: `defined` rejects duplicates
/// in the same scope, `shadowed` restores outer definitions on exit.
struct ScopeFrame<P: ProcessManager> {
    defined: HashSet<String>,
    shadowed: Vec<(String, FuncEntry<P>)>,
}

// Manual `Clone` (a derive would demand `P: Clone`).
impl<P: ProcessManager> Clone for ScopeFrame<P> {
    fn clone(&self) -> Self {
        Self {
            defined: self.defined.clone(),
            shadowed: self.shadowed.clone(),
        }
    }
}

/// The single function registry: DSL `FUNC` definitions, builtins, and
/// host extensions share one lookup table, one metadata path, and one
/// dispatch order. Script entries scope lexically (defined names revert on
/// scope exit, shadowing an outer definition restores it); native entries
/// persist for the run. Shared across `fork()` via clone; the entry maps
/// clone while scope frames stay per state.
/// Module to function-name table of pure entries, shared by reference.
/// Nested so placeholder dispatch looks up `table[module][func]` with
/// zero allocation; rebuilt only when native entries register.
pub type PureTable = HashMap<String, HashMap<String, PureFn>>;

/// Shared `{{ MODULE::FUNC(args) }}` dispatch over a pure-function table.
///
/// Single implementation for every expansion path (script `EXPAND`,
/// string interpolation, host-driven fragment expansion): nested
/// module then function lookup with zero allocation, unknown names
/// failing with the sorted known-functions listing. Callers keep
/// their own arg resolution and output handling; only dispatch and
/// its error shape live here.
pub fn placeholder_resolver(table: Arc<PureTable>) -> oxdock_process::PlaceholderCall {
    Arc::new(move |module, func, args| {
        let callee = table
            .get(module)
            .and_then(|entries| entries.get(func))
            .ok_or_else(|| {
                let mut known: Vec<String> = table
                    .iter()
                    .flat_map(|(m, entries)| entries.keys().map(move |f| format!("{m}::{f}")))
                    .collect();
                known.sort();
                anyhow::Error::new(std_diag::func_placeholder_unknown(module, func, &known))
            })?;
        callee(args)
    })
}

/// Descriptive `{{ MODULE::FUNC }}` miss wording, shared by the static
/// gate and the runtime fallbacks: known callees are never called
/// "unknown". Returns `None` when the callee is genuinely unknown
/// everywhere (callers keep the unknown-function diagnostic with its
/// known-functions listing). `{{ }}` runs without step context, so
/// the fix always routes through a prior `LET` binding.
pub(super) fn describe_placeholder_miss(
    module: &str,
    name: &str,
    is_script: bool,
    is_stateful: bool,
    alias_known: bool,
) -> Option<String> {
    let qualified = qualify(module, name);
    if is_script {
        let call = if module == SCRIPT_MODULE_NAME {
            format!("{name}(...)")
        } else {
            format!("{qualified}(...)")
        };
        return Some(format!(
            "placeholder function '{qualified}' is a script function and cannot run in {{{{ }}}}; bind the result first with LET $v = {call} then interpolate {{{{ $v }}}}"
        ));
    }
    if is_stateful {
        return Some(format!(
            "placeholder function '{qualified}' is stateful and cannot run in {{{{ }}}}; capture it first with LET then interpolate {{{{ $v }}}}"
        ));
    }
    if alias_known {
        return Some(format!(
            "placeholder function '{qualified}': no function '{name}' under file alias '{module}'; check the file's EXPORT list and the IMPORT harvest, or bind a known function first with LET then interpolate {{{{ $v }}}}"
        ));
    }
    None
}

/// Owned placeholder classes for `'static` closures (notably
/// `EXPAND` streaming), which cannot borrow the registry.
pub(super) struct PlaceholderClasses {
    pub script: HashSet<String>,
    pub stateful: HashSet<String>,
    pub aliases: HashSet<String>,
    /// Ambiguous spellings: alias resolves to several files.
    pub ambiguous: HashMap<String, HashSet<String>>,
}

impl PlaceholderClasses {
    pub(super) fn describe(&self, module: &str, name: &str) -> Option<String> {
        let qualified = qualify(module, name);
        if let Some(files) = self.ambiguous.get(&qualified) {
            return Some(ambiguous_alias_message(module, name, files));
        }
        describe_placeholder_miss(
            module,
            name,
            self.script.contains(&qualified),
            self.stateful.contains(&qualified),
            self.aliases.contains(module),
        )
    }
}

pub struct FunctionRegistry<P: ProcessManager> {
    entries: HashMap<String, FuncEntry<P>>,
    scopes: Vec<ScopeFrame<P>>,
    pure_shared: Arc<PureTable>,
    /// Script `#` doc comments keyed by qualified `SCRIPT::NAME`.
    /// Populated by the MCP host after linear execution; the tool
    /// table reads these with fallback to `FuncMeta` summary.
    script_docs: HashMap<String, (String, String)>,
}

impl<P: ProcessManager> FunctionRegistry<P> {
    pub(super) fn with_builtins() -> Self {
        let mut reg = Self {
            entries: HashMap::new(),
            pure_shared: Arc::new(HashMap::new()),
            script_docs: HashMap::new(),
            scopes: vec![ScopeFrame {
                defined: HashSet::new(),
                shadowed: Vec::new(),
            }],
        };
        // Every builtin registers through the same `HostRegistration`
        // entries hosts use: no separate authoring path for engine natives.
        // Builtins land in the `STD` module, exactly like a host module.
        for host in Self::builtin_registrations() {
            match host {
                HostRegistration::Stateful { name, meta, func } => {
                    reg.insert_qualified(STD_MODULE_NAME, name, meta, FuncBody::Ctx(func));
                }
                HostRegistration::Pure { name, meta, func } => {
                    reg.insert_qualified(STD_MODULE_NAME, name, meta, FuncBody::Pure(func));
                }
                HostRegistration::Both {
                    name,
                    meta,
                    func,
                    template,
                } => {
                    reg.insert_qualified(
                        STD_MODULE_NAME,
                        name,
                        meta,
                        FuncBody::Both {
                            pure: template,
                            ctx: func,
                        },
                    );
                }
            }
        }
        reg
    }

    /// All builtins as host-style registrations, built from the same
    /// `#[oxdock_func]`-generated markers hosts use. `with_builtins`
    /// consumes this list, so builtins and hosts share one registration
    /// pathway instead of two authoring models.
    pub(super) fn builtin_registrations() -> Vec<HostRegistration<P>> {
        vec![
            Int::registration(),
            Float::registration(),
            BoolConvert::registration(),
            BoolOr::registration(),
            FormatByteCount::registration(),
            ParseByteCount::registration(),
            FormatDuration::registration(),
            ParseDuration::registration(),
            Types::registration(),
            TypeDescribe::registration(),
            Glob::registration(),
            LoadToml::registration(),
            LoadJson::registration(),
            HashSha256::registration(),
            Cwd::registration(),
            ParseToml::registration(),
            ParseJson::registration(),
            PathType::registration(),
            PathWritable::registration(),
            IsExec::registration(),
            IsFile::registration(),
            IsDir::registration(),
            Exists::registration(),
            HasKey::registration(),
            Get::registration(),
            MapSet::registration(),
            MergeMaps::registration(),
            Base64Encode::registration(),
            Base64Decode::registration(),
            Rand::registration(),
            ToJson::registration(),
            TypeOf::registration(),
            Functions::registration(),
            Describe::registration(),
            IsMain::registration(),
            IsTerminal::registration(),
            Eof::registration(),
            TaskPoll::registration(),
            Version::registration(),
            Interpreter::registration(),
            Argv::registration(),
            ParseArgs::registration(),
            ParseFlags::registration(),
            ParsePositionals::registration(),
            TextTrim::registration(),
            TextTrimPrefix::registration(),
            TextTrimSuffix::registration(),
            PathJoin::registration(),
            PathBase::registration(),
            PathRel::registration(),
            TextToUpper::registration(),
            TextToLower::registration(),
            TextReplace::registration(),
            TextReplaceRegex::registration(),
            TextSplit::registration(),
            TextJoin::registration(),
            TextLines::registration(),
            TextFields::registration(),
            TextMatch::registration(),
            TextExtract::registration(),
            TextStartsWith::registration(),
            TextEndsWith::registration(),
            Contains::registration(),
            Unique::registration(),
            Len::registration(),
            TextCount::registration(),
            TextCountMatches::registration(),
            TextGrep::registration(),
            TextLinesRange::registration(),
            SemaphoreNew::registration(),
            SemaphoreTryAcquire::registration(),
            SemaphoreAvailable::registration(),
            ShellSplitArgs::registration(),
            ShellJoinArgs::registration(),
        ]
    }

    /// Every name the registry answers to. Backs parse-time shadow
    /// validation through `builtin_function_names`.
    pub(super) fn keys(&self) -> HashSet<String> {
        self.entries.keys().cloned().collect()
    }

    /// Single lookup for every callable: script, pure, or stateful.
    pub(super) fn get(&self, name: &str) -> Option<FuncEntry<P>> {
        self.entries.get(name).cloned()
    }

    /// Number of live function scope frames. Callers record this as
    /// their frame floor so nested lookup stays within the current
    /// call (or init), never reaching caller frames.
    pub(super) fn scope_depth(&self) -> usize {
        self.scopes.len()
    }

    /// Frame-2..N lookup (#322): innermost scope frame at or above
    /// `floor` defining `SCRIPT::BASE`, innermost first. The live
    /// slot always holds the innermost defining frame's value
    /// (save/restore discipline), so a hit here is definitionally
    /// lexical: nested defs shadow everything below them with no
    /// owner tags consulted. Cross-unit exclusion comes from the
    /// caller choosing `floor` (call/init boundary), never from
    /// inspecting whose definition was found.
    pub(super) fn get_scoped(&self, base: &str, floor: usize) -> Option<FuncEntry<P>> {
        let qualified = qualify(SCRIPT_MODULE_NAME, base);
        let len = self.scopes.len();
        let start = floor.min(len);
        for frame in self.scopes[start..].iter().rev() {
            if frame.defined.contains(&qualified) {
                return self.entries.get(&qualified).cloned();
            }
        }
        None
    }

    /// Single lazy miss classifier for runtime placeholder paths with
    /// registry access: one borrow, no `FuncEntry` clones, invoked only
    /// on resolver failure. Classification runs first (matching
    /// `exported_metas`); the shared body helper below owns the
    /// `Script`/`Ctx` wording, so the match appears once.
    pub(super) fn describe_unit_placeholder_miss(
        &self,
        unit_targets: &AliasTargets,
        module: &str,
        name: &str,
    ) -> Option<String> {
        let qualified = qualify(module, name);
        // File-aliased spellings classify through the shared matcher.
        // A canonical miss means the alias is known but holds no such
        // function; disjoint aliases share the static owner's verdict
        // (script-visible branches read as script misses).
        match classify_alias_call(unit_targets, &qualified, &|key| {
            self.entries.contains_key(key)
        }) {
            AliasResolution::Canonical(canonical) => {
                if let Some(entry) = self.entries.get(&canonical) {
                    if let Some(miss) = Self::placeholder_miss_for_body(module, name, &entry.body) {
                        return Some(miss);
                    }
                    None
                } else {
                    describe_placeholder_miss(module, name, false, false, true)
                }
            }
            AliasResolution::Global => {
                if let Some(entry) = self.entries.get(&qualified) {
                    // `SCRIPT` and host spellings resolve globally;
                    // canonical file keys never match a call-site
                    // spelling.
                    if let Some(miss) = Self::placeholder_miss_for_body(module, name, &entry.body) {
                        return Some(miss);
                    }
                }
                None
            }
            AliasResolution::Ambiguous { files, .. } => {
                Some(ambiguous_alias_message(module, name, &files))
            }
            AliasResolution::Unknown => describe_placeholder_miss(module, name, false, false, true),
            // Disjoint aliases share the static owner's verdict through
            // the single miss helper, so lazy inline expansion agrees
            // with the static gate word for word (script-visible
            // branches read as script misses, the rest as known-alias
            // misses, never the generic unknown wording).
            AliasResolution::Any {
                base,
                files,
                has_host,
            } => Some(disjoint_alias_placeholder_miss(
                module,
                name,
                &base,
                &files,
                has_host,
                &|key| matches!(self.entries.get(key), Some(entry) if matches!(entry.body, FuncBody::Script(_))),
            )),
        }
    }

    /// Single owner for the `FuncBody` miss wording: script and
    /// stateful bodies report their kind, template-capable bodies
    /// (`Pure`/`Both`) report no miss.
    fn placeholder_miss_for_body(module: &str, name: &str, body: &FuncBody<P>) -> Option<String> {
        match body {
            FuncBody::Script(_) => describe_placeholder_miss(module, name, true, false, false),
            FuncBody::Ctx(_) => describe_placeholder_miss(module, name, false, true, false),
            FuncBody::Pure(_) | FuncBody::Both { .. } => None,
        }
    }

    /// Owned snapshot for `'static` placeholder closures (notably
    /// `EXPAND` streaming): the resolver closure cannot borrow the
    /// registry, so these name sets are the minimal owned form.
    pub(super) fn placeholder_snapshot(&self, unit_targets: &AliasTargets) -> PlaceholderClasses {
        let mut script = HashSet::new();
        let mut stateful = HashSet::new();
        let mut ambiguous: HashMap<String, HashSet<String>> = HashMap::new();
        for (key, entry) in self.entries.iter() {
            let Some((module, base)) = split_qualified(key) else {
                continue;
            };
            match &entry.body {
                FuncBody::Script(def) => {
                    if module == SCRIPT_MODULE_NAME {
                        // `SCRIPT` and bare-harvest spellings stay global.
                        script.insert(key.clone());
                        continue;
                    }
                    // Canonical file key: every local spelling in this
                    // unit, singletons (plus disjoint platform branches
                    // covered everywhere by script definitions) into
                    // the script set and several files into the
                    // ambiguity map, through the shared matcher so
                    // `describe` matches call sites exactly like the
                    // static gate and lazy path.
                    let defining = def.module.as_deref().unwrap_or(module);
                    let (direct, ambiguous_here) = spellings_for_defining_file(
                        unit_targets,
                        defining,
                        base,
                        &|key| matches!(self.entries.get(key), Some(entry) if matches!(entry.body, FuncBody::Script(_))),
                    );
                    script.extend(direct);
                    for (spelling, files) in ambiguous_here {
                        ambiguous.entry(spelling).or_default().extend(files);
                    }
                }
                FuncBody::Ctx(_) => {
                    stateful.insert(key.clone());
                }
                FuncBody::Pure(_) | FuncBody::Both { .. } => {}
            }
        }
        let aliases = unit_targets.keys().cloned().collect();
        PlaceholderClasses {
            script,
            stateful,
            aliases,
            ambiguous,
        }
    }

    fn insert_native(&mut self, name: String, meta: FuncMeta, body: FuncBody<P>) {
        self.entries.insert(name, FuncEntry { meta, body });
        self.rebuild_pure_shared();
    }

    /// Rebuild the shared pure table. Runs only on native insertion:
    /// script definitions insert `Script` bodies directly and never
    /// affect the pure set, so this stays correct between rebuilds.
    /// Dual entries contribute their template arm, which is what
    /// `{{ }}` placeholder expansion resolves.
    fn rebuild_pure_shared(&mut self) {
        let mut table: PureTable = HashMap::new();
        for (name, entry) in &self.entries {
            let func = match &entry.body {
                FuncBody::Pure(func) => func,
                FuncBody::Both { pure, .. } => pure,
                FuncBody::Script(_) | FuncBody::Ctx(_) => continue,
            };
            if let Some((module, base)) = split_qualified(name) {
                table
                    .entry(module.to_string())
                    .or_default()
                    .insert(base.to_string(), Arc::clone(func));
            }
        }
        self.pure_shared = Arc::new(table);
    }

    /// Insert under `MODULE::BASE`, stamping provenance on the metadata.
    /// The single choke point for every registry entry: builtins pass
    /// `STD`, hosts pass their module, scripts pass `SCRIPT`.
    fn insert_qualified(
        &mut self,
        module: &str,
        base: String,
        mut meta: FuncMeta,
        body: FuncBody<P>,
    ) {
        meta.name = qualify(module, &base);
        meta.module = module.to_string();
        // Last-write-wins would silently reroute calls, so a repeated
        // qualified name is a programmer error, never a shadow: panic like
        // conflicting type registrations do. `SCRIPT` definitions bypass
        // this path (`define_script` owns their scoped shadowing).
        if self.entries.contains_key(&meta.name) {
            panic!("duplicate function registration `{}`", meta.name);
        }
        self.insert_native(meta.name.clone(), meta, body);
    }

    pub(super) fn register_host(
        &mut self,
        module: &str,
        name: String,
        mut meta: FuncMeta,
        func: NativeFn<P>,
    ) {
        meta.kind = FuncKind::HostCtx;
        self.insert_qualified(module, name, meta, FuncBody::Ctx(func));
    }

    pub(super) fn register_pure_host(
        &mut self,
        module: &str,
        name: String,
        mut meta: FuncMeta,
        func: PureFn,
    ) {
        meta.kind = FuncKind::HostPure;
        self.insert_qualified(module, name, meta, FuncBody::Pure(func));
    }

    pub(super) fn register_both_host(
        &mut self,
        module: &str,
        name: String,
        mut meta: FuncMeta,
        func: NativeFn<P>,
        template: PureFn,
    ) {
        meta.kind = FuncKind::HostCtx;
        self.insert_qualified(
            module,
            name,
            meta,
            FuncBody::Both {
                pure: template,
                ctx: func,
            },
        );
    }

    /// Define a DSL `FUNC`: names colliding with any known base name cannot
    /// shadow, same-scope duplicates cannot redefine, and nested shadowing
    /// of an outer script definition restores on scope exit. Stored as
    /// `SCRIPT::NAME`, exactly like every other qualified entry.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn define_script(
        &mut self,
        name: &str,
        params: &[(String, TypeTag, bool)],
        return_type: TypeTag,
        return_spelling: &str,
        is_async: bool,
        body: &[Step],
        module: Option<&str>,
        def_jail: Option<(WorkspaceSelection, GuardedPath)>,
    ) -> DiagnosticResult<()> {
        let qualified = qualify(SCRIPT_MODULE_NAME, name);
        let shadowable = matches!(
            self.entries.get(&qualified).map(|entry| &entry.body),
            Some(FuncBody::Script(_))
        );
        // Reserved spans every module: compare base names so the message
        // keeps naming the bare script identifier. Script-defined entries
        // (including `ALIAS::NAME` file imports, #115) never reserve: only
        // native/host bodies are unshadowable.
        let reserved = self.entries.iter().any(|(key, entry)| {
            split_qualified(key).is_some_and(|(_, base)| base == name)
                && !matches!(entry.body, FuncBody::Script(_))
        });
        if reserved && !shadowable {
            return Err(std_diag::func_shadow_reserved(name));
        }
        if self
            .scopes
            .last()
            .is_some_and(|frame| frame.defined.contains(&qualified))
        {
            return Err(std_diag::func_duplicate_in_scope(name));
        }
        check_script_param_tags(name, params)?;
        let old = self.entries.insert(
            qualified.clone(),
            FuncEntry {
                meta: FuncMeta {
                    name: qualified.clone(),
                    module: SCRIPT_MODULE_NAME.to_string(),
                    kind: FuncKind::Script,
                    params: Some(
                        params
                            .iter()
                            .map(|(pname, param_type, optional)| {
                                Ok(FuncParam {
                                    name: pname.clone(),
                                    param_type: Some(param_type.clone()),
                                    allowed: None,
                                    // Script `FUNC` params carry no prose:
                                    // declarations have no doc syntax.
                                    docs: "",
                                    options: None,
                                    optional: *optional,
                                })
                            })
                            .collect::<DiagnosticResult<_>>()?,
                    ),
                    returns: Some(return_type.clone()),
                    rpn: false,
                    summary: "DSL-defined function.",
                    docs: "Defined via FUNC in script.",
                    examples: &[],
                },
                body: FuncBody::Script(FuncDefData {
                    params: params.to_vec(),
                    return_type,
                    return_spelling: return_spelling.to_string(),
                    is_async,
                    body: body.to_vec(),
                    module: module.map(|m| m.to_string()),
                    def_jail,
                }),
            },
        );
        if let Some(frame) = self.scopes.last_mut() {
            frame.defined.insert(qualified.clone());
            if let Some(old) = old {
                frame.shadowed.push((qualified, old));
            }
        }
        Ok(())
    }

    /// Define a file-imported DSL `FUNC` under its canonical key
    /// ([`file_func_key`]: defining file plus base name, never the
    /// importer alias) in the global registry frame (index 0, never
    /// popped): eviction on scope exit would strand the module cache
    /// and alias targets that still reference it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn define_file_func(
        &mut self,
        name: &str,
        params: &[(String, TypeTag, bool)],
        return_type: TypeTag,
        return_spelling: &str,
        is_async: bool,
        body: &[Step],
        defining: &str,
    ) -> DiagnosticResult<()> {
        // The importer alias selects the defining file at dispatch;
        // the key itself names the defining file so units never share
        // entries.
        let qualified = file_func_key(defining, name);
        let shadowable = matches!(
            self.entries.get(&qualified).map(|entry| &entry.body),
            Some(FuncBody::Script(_))
        );
        // Reserved spans every module: compare base names so the message
        // keeps naming the bare script identifier. Script-defined entries
        // never reserve: only native/host bodies are unshadowable.
        let reserved = self.entries.iter().any(|(key, entry)| {
            split_qualified(key).is_some_and(|(_, base)| base == name)
                && !matches!(entry.body, FuncBody::Script(_))
        });
        if reserved && !shadowable {
            return Err(std_diag::func_shadow_reserved(name));
        }
        // File entries live in the global frame (index 0, never
        // popped): eviction on scope exit would strand the module
        // cache and alias targets that still reference them.
        let frame_idx = 0;
        if self
            .scopes
            .get(frame_idx)
            .is_some_and(|frame| frame.defined.contains(&qualified))
        {
            return Err(std_diag::func_duplicate_in_scope(name));
        }
        check_script_param_tags(name, params)?;
        let old = self.entries.insert(
            qualified.clone(),
            FuncEntry {
                meta: FuncMeta {
                    name: qualified.clone(),
                    module: defining.to_string(),
                    kind: FuncKind::Script,
                    params: Some(
                        params
                            .iter()
                            .map(|(pname, param_type, optional)| {
                                Ok(FuncParam {
                                    name: pname.clone(),
                                    param_type: Some(param_type.clone()),
                                    allowed: None,
                                    docs: "",
                                    options: None,
                                    optional: *optional,
                                })
                            })
                            .collect::<DiagnosticResult<_>>()?,
                    ),
                    returns: Some(return_type.clone()),
                    rpn: false,
                    summary: "DSL-defined function.",
                    docs: "Defined via FUNC in an imported script.",
                    examples: &[],
                },
                body: FuncBody::Script(FuncDefData {
                    params: params.to_vec(),
                    return_type,
                    return_spelling: return_spelling.to_string(),
                    is_async,
                    body: body.to_vec(),
                    module: Some(defining.to_string()),
                    // Harvest registration never captures a jail: it
                    // runs in the importer context, so capturing here
                    // would leak caller ambient into the defining
                    // file. The file fallback applies at call time.
                    def_jail: None,
                }),
            },
        );
        if let Some(frame) = self.scopes.get_mut(frame_idx) {
            frame.defined.insert(qualified.clone());
            if let Some(old) = old {
                frame.shadowed.push((qualified, old));
            }
        }
        Ok(())
    }

    /// Backfill a definition-site jail onto an import-registered
    /// canonical entry (#322). Imports register canonical slots
    /// before init runs, so they start jail-less; the first
    /// top-level definition seals them exactly once, mirroring the
    /// file frame. Later definitions never overwrite. Harvest-only
    /// registrations stay `None`: without init there is no
    /// definition site to seal.
    pub(super) fn seal_canonical_jail(
        &mut self,
        canonical: &str,
        jail: (WorkspaceSelection, GuardedPath),
    ) {
        if let Some(entry) = self.entries.get_mut(canonical)
            && let FuncBody::Script(def) = &mut entry.body
            && def.def_jail.is_none()
        {
            def.def_jail = Some(jail);
        }
    }

    /// Open a lexical scope frame for script definitions. Native entries
    /// persist; only script names track here.
    pub(super) fn push_scope(&mut self) {
        self.scopes.push(ScopeFrame {
            defined: HashSet::new(),
            shadowed: Vec::new(),
        });
    }

    /// Close a lexical scope frame: names defined inside revert, and any
    /// outer definition they shadowed is restored.
    pub(super) fn pop_scope(&mut self) {
        let Some(frame) = self.scopes.pop() else {
            return;
        };
        for name in frame.defined {
            self.entries.remove(&name);
        }
        for (name, old) in frame.shadowed {
            self.entries.insert(name, old);
        }
    }

    /// Registry lookup key for a runtime call-site spelling, through
    /// the shared alias-target matcher.
    pub(super) fn resolve_file_call_key(
        &self,
        unit_targets: &AliasTargets,
        name: &str,
    ) -> DiagnosticResult<Option<String>> {
        match classify_alias_call(unit_targets, name, &|key| self.entries.contains_key(key)) {
            AliasResolution::Global | AliasResolution::Unknown | AliasResolution::Any { .. } => {
                Ok(None)
            }
            AliasResolution::Canonical(key) => Ok(Some(key)),
            AliasResolution::Ambiguous { alias, base, files } => {
                Err(std_diag::func_ambiguous_alias(&alias, &base, &files))
            }
        }
    }

    /// Shared snapshot of every pure entry, nested by module and base
    /// name. Cloning the `Arc` is refcount-only: placeholder dispatch
    /// borrows this per expansion with no map copies.
    pub(super) fn pure_shared(&self) -> Arc<PureTable> {
        Arc::clone(&self.pure_shared)
    }

    /// Every entry's metadata, scripts included, sorted by name. Backs
    /// run-start type tag validation.
    pub(super) fn all_metas(&self) -> Vec<FuncMeta> {
        self.entries_metas()
    }

    fn meta(&self, name: &str) -> Option<FuncMeta> {
        self.entries.get(name).map(|entry| entry.meta.clone())
    }

    fn native_metas(&self) -> Vec<FuncMeta> {
        let mut out: Vec<FuncMeta> = Vec::new();
        for entry in self.entries.values() {
            if !matches!(entry.body, FuncBody::Script(_)) {
                out.push(entry.meta.clone());
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Metadata for template-capable entries only: `Pure` plus `Both`
    /// (dual entries contribute their template arm to `{{ }}`).
    /// `Ctx` and `Script` never resolve in placeholder expansion, so
    /// the static template gate reads this instead of `all_metas`.
    pub(super) fn pure_metas(&self) -> Vec<FuncMeta> {
        let mut out: Vec<FuncMeta> = Vec::new();
        for entry in self.entries.values() {
            if matches!(entry.body, FuncBody::Pure(_) | FuncBody::Both { .. }) {
                out.push(entry.meta.clone());
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Every entry's metadata, scripts included, sorted by name. Backs
    /// runtime `FUNCTIONS()` listings.
    fn entries_metas(&self) -> Vec<FuncMeta> {
        let mut out: Vec<FuncMeta> = self
            .entries
            .values()
            .map(|entry| entry.meta.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Install script `#` docs keyed by bare base name. Stored under
    /// qualified `SCRIPT::NAME` keys.
    pub fn set_script_docs(&mut self, docs: &HashMap<String, (String, String)>) {
        for (base, pair) in docs {
            self.script_docs
                .insert(qualify(SCRIPT_MODULE_NAME, base), pair.clone());
        }
    }

    /// Script doc pair for a qualified name, if the parser collected one.
    pub fn script_doc_for(&self, qualified: &str) -> Option<(String, String)> {
        self.script_docs.get(qualified).cloned()
    }

    /// Metadata for single-file `EXPORT` names in source order. Errors
    /// name the first missing export; callers fail fast before serving.
    /// Qualified names resolve through `resolve_file_call_key`, so
    /// re-exported file functions serve under the export spelling.
    pub fn exported_metas(
        &self,
        func_exports: &[String],
        unit_targets: &AliasTargets,
    ) -> DiagnosticResult<Vec<FuncMeta>> {
        let mut out = Vec::with_capacity(func_exports.len());
        for name in func_exports {
            // Bare names are script FUNCs; qualified names resolve
            // through the unit's alias targets first, falling back to
            // the global spelling for hosts and entry-owned scripts.
            let qualified = match split_qualified(name) {
                None => qualify(SCRIPT_MODULE_NAME, name),
                Some(_) => name.clone(),
            };
            let lookup = self
                .resolve_file_call_key(unit_targets, &qualified)?
                .unwrap_or_else(|| qualified.clone());
            if let Some(entry) = self.entries.get(&lookup) {
                let mut meta = entry.meta.clone();
                if lookup != qualified {
                    meta.name = name.clone();
                }
                out.push(meta);
                continue;
            }
            return Err(std_diag::func_export_unknown(name));
        }
        Ok(out)
    }
}

impl<P: ProcessManager> Clone for FunctionRegistry<P> {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            scopes: self.scopes.clone(),
            pure_shared: Arc::clone(&self.pure_shared),
            script_docs: self.script_docs.clone(),
        }
    }
}

/// Names of all compiled-in builtins plus `INSPECT` (a dedicated AST/RPN
/// node, not a registry entry). Read straight off a stock registry, so the
/// `#[oxdock_func]` annotations stay the single source of truth: adding a
/// builtin extends this set with no parallel list to update. Seeds
/// parse-time shadow validation, so the parser crate keeps zero
/// compile-time knowledge of builtin names.
pub fn builtin_function_names() -> HashSet<String> {
    let mut names = FunctionRegistry::<DefaultProcessManager>::with_builtins().keys();
    names.insert(KEYWORD_INSPECT.to_string());
    names
}

/// Metadata of every builtin function, sorted by name, for static
/// rendering (oxdock-render). Same single source as `builtin_function_names`:
/// the `#[oxdock_func]` annotations, never a parallel list.
pub fn builtin_function_metas() -> Vec<FuncMeta> {
    FunctionRegistry::<DefaultProcessManager>::with_builtins().native_metas()
}

/// Stock `STD` module table derived from the `#[oxdock_func]` builtins:
/// the single source of truth for builtin membership and RPN eligibility.
/// Seeds parse-time module resolution, so the parser crate keeps zero
/// compile-time knowledge of builtin names.
pub fn std_module_table() -> oxdock_parser::ModuleTable {
    // Registry names are qualified (`STD::GLOB`); the table holds bases.
    let func_names: HashSet<String> = builtin_function_metas()
        .into_iter()
        .map(|meta| base_name(&meta.name).to_string())
        .collect();
    oxdock_parser::ModuleTable {
        modules: HashMap::from([(
            STD_MODULE_NAME.to_string(),
            Some(oxdock_parser::ModuleFuncs { func_names }),
        )]),
    }
}

// Builtins below use the `#[oxdock_func]` host export macro, the exact same authoring
// model as host-registered functions: metadata, arity checks, and argument
// unpacking derive from the signature plus doc comments, so adding a native
// means writing one small typed function plus one line each in
// `builtin_registrations` (consumed by `with_builtins`) and the
// `FUNCTIONS`/`DESCRIBE`/`TYPES` surface, which all read the same symbols.

/// Convert a value to INT.
///
/// Trims ASCII whitespace and parses i64. Passes Int through; Float only
/// when integral and finite; Bool as 1/0.
///
#[oxdock_func(pure, returns = TypeTag::Int)]
#[example(
    name = "convert",
    code = r###"
    IMPORT [STD]
    LET $n: INT = INT("42")
    ASSERT_EQ $n 42
    LET $flag: INT = INT(true)
    ASSERT_EQ $flag 1
    "###
)]
fn int(
    /// Value to convert to `INT`.
    #[union(Int, Float, String, Bool)]
    val: Value,
) -> Result<Value> {
    Ok(super::args::int_from_value(val)?)
}

/// Convert a value to FLOAT.
///
/// Parses f64 (accepts int strings), bails on non-finite or non-numeric.
///
#[oxdock_func(pure, returns = TypeTag::Float)]
#[example(
    name = "convert",
    code = r###"
    IMPORT [STD]
    LET $f: FLOAT = FLOAT("1.5")
    ASSERT_EQ $f 1.5
    LET $whole: FLOAT = FLOAT(2)
    ASSERT_EQ $whole 2.0
    "###
)]
fn float(
    /// Value to convert to `FLOAT`.
    #[union(Float, Int, String)]
    val: Value,
) -> Result<Value> {
    Ok(super::args::float_from_value(val)?)
}

/// Format a byte count canonically (`500B`, `10KB`, `2MB`, `1GB`,
/// binary `10KIB` and friends): the inverse of `PARSE_BYTE_COUNT`, so
/// formatted values reparse exactly. Negative inputs fail.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "format and round-trip",
    code = r###"
    # Byte counts render in the largest exact unit.
    IMPORT [STD]
    LET $pretty: STRING = FORMAT_BYTE_COUNT(1_048_576)
    ASSERT_EQ $pretty "1MiB"

    # The pair round-trips through the parser.
    LET $back: INT = PARSE_BYTE_COUNT($pretty)
    ASSERT_EQ $back 1_048_576
    "###
)]
fn format_byte_count(
    /// Byte count to format; negatives fail.
    bytes: i64,
) -> Result<Value> {
    if bytes < 0 {
        return Err(anyhow::Error::new(
            std_diag::std_format_byte_count_negative(bytes),
        ));
    }
    Ok(Value::string(oxdock_parser::command::format_byte_count(
        bytes as u64,
    )))
}

/// Parse a byte-count token to `INT` bytes.
///
/// Bare digits are bytes; suffixes `B`/`KB`/`MB`/`GB`/`TB` scale
/// decimal and `KIB`/`MIB`/`GIB`/`TIB` binary, in any case. Plain
/// `INT` values pass through (already bytes); integral finite `FLOAT`
/// values convert. Pairs with `FORMAT_BYTE_COUNT` for the round trip.
///
/// ```oxdock
/// # Units parse case-insensitively; bare digits are bytes.
/// IMPORT [STD]
/// LET $bytes: INT = PARSE_BYTE_COUNT("1Gb")
/// ASSERT_EQ $bytes 1_000_000_000
///
/// # Binary suffixes scale by powers of two.
/// LET $mebi: INT = PARSE_BYTE_COUNT("2mib")
/// ASSERT_EQ $mebi 2_097_152
///
/// # Digit runs group Rust-literal style.
/// LET $grouped: INT = PARSE_BYTE_COUNT("1_000_000")
/// ASSERT_EQ $grouped 1_000_000
///
/// # Plain numbers pass through as bytes already.
/// LET $plain: INT = PARSE_BYTE_COUNT(1_048_576)
/// ASSERT_EQ $plain 1_048_576
///
/// # Bare literals group too.
/// LET $bare: INT = PARSE_BYTE_COUNT(1_000_000)
/// ASSERT_EQ $bare 1_000_000
///
/// # One optional space separates number from unit.
/// LET $spaced: INT = PARSE_BYTE_COUNT("1 GB")
/// ASSERT_EQ $spaced 1_000_000_000
/// ```
///
#[oxdock_func(pure, returns = TypeTag::Int)]
#[example(
    name = "parse",
    code = r###"
    IMPORT [STD]
    LET $n: INT = PARSE_BYTE_COUNT("10KB")
    ASSERT_EQ $n 10_000
    LET $identity: INT = PARSE_BYTE_COUNT(512)
    ASSERT_EQ $identity 512
    "###
)]
fn parse_byte_count(
    /// Byte-count token to parse, or bytes already.
    #[union(Int, Float, String)]
    text: Value,
) -> Result<Value> {
    if let Some(n) = text.as_i64() {
        if n < 0 {
            return Err(anyhow::Error::new(std_diag::std_parse_byte_count_negative(
                n,
            )));
        }
        return Ok(Value::int(n));
    }
    if let Some(f) = text.as_f64() {
        if f.is_finite() && f.fract() == 0.0 && f >= 0.0 && f <= i64::MAX as f64 {
            return Ok(Value::int(f as i64));
        }
        return Err(anyhow::Error::new(std_diag::std_parse_byte_count_integral(
            f,
        )));
    }
    let Some(s) = text.as_str() else {
        return Err(anyhow::Error::new(std_diag::std_parse_byte_count_shape(
            &text,
        )));
    };
    let n = oxdock_parser::command::parse_byte_count(s)?;
    let n = i64::try_from(n)
        .map_err(|_| anyhow::Error::new(std_diag::std_parse_byte_count_range(s)))?;
    Ok(Value::int(n))
}

/// Format millisecond durations canonically (`500ms`, `10s`, `2m`,
/// `1h`): the inverse of `PARSE_DURATION`, so formatted values
/// reparse exactly. Negative inputs fail.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "format and round-trip",
    code = r###"
    # Durations render in the largest exact unit.
    IMPORT [STD]
    LET $pretty: STRING = FORMAT_DURATION(90_000)
    ASSERT_EQ $pretty "90s"

    # The pair round-trips through the parser.
    LET $back: INT = PARSE_DURATION($pretty)
    ASSERT_EQ $back 90_000
    "###
)]
fn format_duration(
    /// Milliseconds to format; negatives fail.
    millis: i64,
) -> Result<Value> {
    if millis < 0 {
        return Err(anyhow::Error::new(std_diag::std_format_duration_negative(
            millis,
        )));
    }
    Ok(Value::string(oxdock_parser::command::format_duration(
        &std::time::Duration::from_millis(millis as u64),
    )))
}

/// Parse a duration token to `INT` milliseconds.
///
/// Suffixes `ms`/`s`/`m`/`h`; a bare number means seconds. Pairs with
/// `FORMAT_DURATION` for the round trip.
///
#[oxdock_func(pure, returns = TypeTag::Int)]
#[example(
    name = "parse",
    code = r###"
    # A bare number means seconds, like TIMEOUT.
    IMPORT [STD]
    LET $ms: INT = PARSE_DURATION("2m")
    ASSERT_EQ $ms 120_000

    # Milliseconds stay exact.
    LET $exact: INT = PARSE_DURATION("1500ms")
    ASSERT_EQ $exact 1_500

    # One optional space separates number from unit.
    LET $spaced: INT = PARSE_DURATION("1 h")
    ASSERT_EQ $spaced 3_600_000
    "###
)]
fn parse_duration(
    /// Duration token to parse.
    text: String,
) -> Result<Value> {
    let duration = oxdock_parser::command::parse_duration(&text)?;
    let millis = i64::try_from(duration.as_millis())
        .map_err(|_| anyhow::Error::new(std_diag::std_parse_duration_range(&text)))?;
    Ok(Value::int(millis))
}

/// List workspace paths matching a glob pattern.
///
/// Sorted, root-relative LIST; empty on no match or `..` escape.
///
#[oxdock_func(rpn, returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "match",
    code = r###"
    IMPORT [STD]
    WRITE "notes-a.md" "a"
    WRITE "notes-b.md" "b"
    WRITE "other.txt" "c"
    LET $found: LIST<STRING> = GLOB("notes-*.md")
    ASSERT_EQ $found.0 "notes-a.md"
    ASSERT_EQ $found.1 "notes-b.md"
    "###
)]
fn glob<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Glob pattern matched against workspace paths.
    #[union(String, Path)]
    pattern: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&pattern)?;
    Ok(super::args::glob_from_value(&[Value::string(text)], cx)?)
}

/// Load and parse a TOML file.
///
/// Reads a workspace file and parses TOML into a DSL value.
///
#[oxdock_func(rpn, returns = TypeTag::MapOf(std::sync::Arc::new(TypeTag::Any)))]
#[example(
    name = "load",
    code = r###"
    IMPORT [STD]
    WRITE "cfg.toml" "title = \"demo\""
    LET $cfg: MAP<ANY> = LOAD_TOML("cfg.toml")
    ASSERT_EQ $cfg.title "demo"
    "###
)]
fn load_toml<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Workspace file path to load and parse as TOML.
    #[union(String, Path)]
    path: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&path)?;
    Ok(super::args::load_toml_from_value(
        &[Value::string(text)],
        cx,
    )?)
}

/// Load and parse a JSON file.
///
/// Reads a workspace file and parses JSON into a DSL value. Returns
/// `ANY` by design: a top-level array or scalar parses to `LIST` or a
/// scalar word. `LET` coercion still checks the actual value at
/// assignment.
///
#[oxdock_func(rpn, returns = TypeTag::Any)]
#[example(
    name = "load",
    code = r###"
    IMPORT [STD]
    WRITE "data.json" "{\"a\": 1}"
    LET $doc: MAP<ANY> = LOAD_JSON("data.json")
    ASSERT_EQ $doc.a 1
    "###
)]
fn load_json<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Workspace file path to load and parse as JSON.
    #[union(String, Path)]
    path: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&path)?;
    Ok(super::args::load_json_from_value(
        &[Value::string(text)],
        cx,
    )?)
}

/// Hash a workspace file or directory with SHA-256.
///
/// The argument is always a path: reads through the workspace guard
/// like the file-loading builtins, so relative paths resolve against
/// the script cwd. To hash a literal string's bytes instead, use
/// `ASSERT_EQ --hash <digest> <string>`, which hashes the actual
/// directly. Returns the 64 lowercase hex chars with no trailing
/// newline.
///
#[oxdock_func(rpn, returns = TypeTag::String)]
#[example(
    name = "hash",
    code = r###"
    IMPORT [STD]
    WRITE payload.txt hello
    LET $digest: STRING = HASH_SHA256("payload.txt")
    ASSERT_EQ $digest "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    "###
)]
fn hash_sha256<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Workspace file or directory path to hash.
    #[union(String, Path)]
    path: Value,
) -> Result<Value> {
    use sha2::Digest;

    let text = super::args::string_or_path_text(&path)?;
    let target = cx.state.fs.resolve_read(&cx.state.cwd, &text)?;
    let mut hasher = sha2::Sha256::new();
    super::fs_ops::hash_path(cx.state.fs.as_ref(), &target, "", &mut hasher)?;
    let digest = hasher.finalize();
    let bytes: &[u8] = digest.as_ref();
    Ok(Value::string(oxdock_utils::hex_encode(bytes)))
}

/// Current working directory as a workspace-absolute path.
///
/// Returns `STRING` because the result is already resolved: a `PATH`
/// value is workspace-relative text awaiting guard checks at use time,
/// while this directory is already checked and concrete, or absent
/// entirely under a pending snapshot. Compare or display it as text.
///
/// Reports the snapshot-pending sentinel when no concrete directory
/// exists yet, so snapshot scripts keep working unchanged.
///
#[oxdock_func(rpn, returns = TypeTag::String)]
#[example(
    name = "cwd",
    code = r###"
    IMPORT [STD]
    WORKDIR sub
    LET $c: STRING = CWD()
    ASSERT_CONTAINS $c "sub"
    "###
)]
fn cwd<P: ProcessManager>(cx: &mut StepCtx<P>) -> Result<Value> {
    if cx.state.fs.is_snapshot_pending() {
        return Ok(Value::string(super::SNAPSHOT_PENDING_DISPLAY.to_string()));
    }
    let concrete = cx.state.fs.concretize_cwd(&cx.state.cwd);
    let real = super::fs_ops::canonical_cwd(cx.state.fs.as_ref(), &concrete)?;
    Ok(Value::string(real))
}

/// Parse TOML text already held in memory.
///
/// Uses the same conversion as file loading, so fetch bodies, file
/// contents, and captured text share one JSON/TOML shape.
///
#[oxdock_func(pure, returns = TypeTag::MapOf(std::sync::Arc::new(TypeTag::Any)))]
#[example(
    name = "parse",
    code = r###"
    IMPORT [STD]
    LET $doc: MAP<ANY> = PARSE_TOML("title = \"demo\"")
    ASSERT_EQ $doc.title "demo"
    "###
)]
fn parse_toml(
    /// TOML text already held in memory.
    text: String,
) -> Result<Value> {
    Ok(super::args::parse_toml_from_value(Value::string(text))?)
}

/// Parse JSON text already held in memory.
///
/// Uses the same conversion as file loading, so fetch bodies, file
/// contents, and captured text share one JSON value shape. Returns
/// `ANY` by design, matching `LOAD_JSON`: a top-level array or scalar
/// parses to `LIST` or a scalar word. `LET` coercion still checks the
/// actual value at assignment.
///
#[oxdock_func(pure, returns = TypeTag::Any)]
#[example(
    name = "parse",
    code = r###"
    # Parse in-memory JSON text with the same conversion as file loading.
    IMPORT [STD]
    LET $body: STRING = "{\"a\": 1}"
    LET $doc: ANY = PARSE_JSON($body)
    ASSERT_EQ $doc.a 1

    # Top-level arrays parse to LIST, which is why the return is ANY.
    LET $items: LIST<ANY> = PARSE_JSON("[1, 2]")
    ASSERT_EQ $items.0 1
    ASSERT_EQ $items.1 2
    "###
)]
fn parse_json(
    /// JSON text already held in memory.
    text: String,
) -> Result<Value> {
    Ok(super::args::parse_json_from_value(Value::string(text))?)
}

/// Describe a filesystem entry.
///
/// Reports file, dir, symlink (no-follow), or absent. AST-only by design;
/// there is no RPN arm for filesystem IO.
///
#[oxdock_func(returns = TypeTag::String)]
#[example(
    name = "describe",
    code = r###"
    IMPORT [STD]
    WRITE "entry.txt" "words"
    LET $kind: STRING = PATH_TYPE("entry.txt")
    ASSERT_EQ $kind "file"
    LET $missing: STRING = PATH_TYPE("no-such-entry.txt")
    ASSERT_EQ $missing "absent"
    "###
)]
fn path_type<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Workspace path of the entry to describe.
    #[union(String, Path)]
    path: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&path)?;
    Ok(super::args::path_type_from_value(
        &[Value::string(text)],
        cx,
    )?)
}

/// Report whether an ordinary file can be created at a path.
///
/// Walks up to the nearest existing ancestor, requires it be a
/// directory, and probes writability by creating and deleting a probe
/// file, so retry loops never litter. The empty string is false.
/// AST-only by design, like `PATH_TYPE`: there is no RPN arm for
/// filesystem IO.
///
#[oxdock_func(returns = TypeTag::Bool)]
#[example(
    name = "probe",
    code = r###"
    # An absent nested path is writable when its ancestor is.
    IMPORT [STD]
    LET $fresh: BOOL = PATH_WRITABLE("probe-check/nested/file.txt")
    ASSERT_EQ $fresh true

    # The empty string never aliases the working directory.
    LET $empty: BOOL = PATH_WRITABLE("")
    ASSERT_EQ $empty false
    "###
)]
fn path_writable<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Workspace path to probe for file creation.
    #[union(String, Path)]
    path: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&path)?;
    Ok(super::args::path_writable_from_value(
        &[Value::string(text)],
        cx,
    )?)
}

/// Report whether a name resolves to an executable file (#100).
///
/// Bare command names search `env:PATH` (`IS_EXEC("tar")` finds the
/// first executable `tar`, plus `tar.exe`, on `PATH`); spellings
/// containing a separator check that file directly. True means a real
/// file with an executable bit on Unix, or a `PATHEXT` extension on
/// Windows: each `env:PATHEXT` entry is probed in order (unset or empty
/// probes nothing). Absolute `PATH` entries resolve through the workspace as
/// usual, so host probing wants `WORKSPACE SYSTEM` and an inherited
/// `PATH`; anything unresolvable reads false, never errors. AST-only
/// by design, like `PATH_TYPE`: there is no RPN arm for filesystem IO.
///
#[oxdock_func(returns = TypeTag::Bool)]
#[example(
    name = "probe",
    code = r###"
    # Missing tools read false instead of failing the run.
    IMPORT [STD]
    LET $missing: BOOL = IS_EXEC("no-such-tool-xyz")
    ASSERT_EQ $missing false

    # Plain files are not executable on any platform.
    WRITE tool.txt payload
    LET $plain: BOOL = IS_EXEC("tool.txt")
    ASSERT_EQ $plain false
    "###
)]
fn is_exec<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Command name or file path to probe.
    #[union(String, Path)]
    name: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&name)?;
    Ok(super::args::is_exec_from_value(&[Value::string(text)], cx)?)
}

/// Report whether a workspace path names an ordinary file.
///
/// No-follow by design like `PATH_TYPE`: a symlink is neither a file
/// nor a directory, so it reads false here. Missing or unresolvable
/// paths read false, never error. AST-only by design, like `PATH_TYPE`:
/// there is no RPN arm for filesystem IO.
///
#[oxdock_func(returns = TypeTag::Bool)]
#[example(
    name = "probe",
    code = r###"
    # Ordinary files read true; missing paths read false.
    IMPORT [STD]
    WRITE "entry.txt" "words"
    LET $hit: BOOL = IS_FILE("entry.txt")
    ASSERT_EQ $hit true
    LET $miss: BOOL = IS_FILE("no-such-entry.txt")
    ASSERT_EQ $miss false
    "###
)]
fn is_file<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Workspace path to probe.
    #[union(String, Path)]
    path: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&path)?;
    Ok(Value::bool(super::args::path_is_file(cx, &text)))
}

/// Report whether a workspace path names a directory.
///
/// No-follow by design like `PATH_TYPE`: a symlink is neither a file
/// nor a directory, so it reads false here. Missing or unresolvable
/// paths read false, never error. AST-only by design, like `PATH_TYPE`:
/// there is no RPN arm for filesystem IO.
///
#[oxdock_func(returns = TypeTag::Bool)]
#[example(
    name = "probe",
    code = r###"
    # Directories read true; files read false.
    IMPORT [STD]
    LET $hit: BOOL = IS_DIR(".")
    ASSERT_EQ $hit true
    WRITE "entry.txt" "words"
    LET $miss: BOOL = IS_DIR("entry.txt")
    ASSERT_EQ $miss false
    "###
)]
fn is_dir<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Workspace path to probe.
    #[union(String, Path)]
    path: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&path)?;
    Ok(Value::bool(super::args::path_is_dir(cx, &text)))
}

/// Report whether a workspace path exists in any form.
///
/// True for files, directories, and symlinks; false for absent or
/// unresolvable paths, never an error. AST-only by design, like
/// `PATH_TYPE`: there is no RPN arm for filesystem IO.
///
#[oxdock_func(returns = TypeTag::Bool)]
#[example(
    name = "probe",
    code = r###"
    # Present entries read true; absent paths read false.
    IMPORT [STD]
    WRITE "entry.txt" "words"
    LET $hit: BOOL = EXISTS("entry.txt")
    ASSERT_EQ $hit true
    LET $miss: BOOL = EXISTS("no-such-entry.txt")
    ASSERT_EQ $miss false
    "###
)]
fn exists<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Workspace path to probe.
    #[union(String, Path)]
    path: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&path)?;
    Ok(Value::bool(super::args::path_exists(cx, &text)))
}

/// Report whether a map holds a key.
///
/// Pure MAP probe so scripts can branch on optional fields without
/// tripping the strict missing-key error.
///
#[oxdock_func(pure, returns = TypeTag::Bool)]
#[example(
    name = "probe",
    code = r###"
    IMPORT [STD]
    LET $m: MAP<ANY> = PARSE_JSON("{\"a\": 1}")
    LET $hit: BOOL = HAS_KEY($m, "a")
    ASSERT_EQ $hit true
    LET $miss: BOOL = HAS_KEY($m, "b")
    ASSERT_EQ $miss false
    "###
)]
fn has_key(
    /// Map to probe.
    map: BTreeMap<String, Value>,
    /// Key to look up.
    key: String,
) -> Result<Value> {
    Ok(super::args::has_key_from_value(Value::map(map), &key)?)
}

/// Read one map entry with a default for missing keys.
///
/// Pure MAP getter so optional flags cost one line instead of a
/// `HAS_KEY` + index + rebind triple: present keys return their value
/// verbatim (no coercion), absent keys return `default`.
#[oxdock_func(pure, returns = TypeTag::Any)]
#[example(
    name = "present and absent",
    code = r###"
    IMPORT [STD]
    LET $m: MAP<ANY> = PARSE_JSON("{\"a\": 1}")
    LET $hit: INT = GET($m, "a", 0)
    ASSERT_EQ $hit 1
    LET $miss: INT = GET($m, "b", 0)
    ASSERT_EQ $miss 0
    "###
)]
fn get(
    /// Map to read.
    map: BTreeMap<String, Value>,
    /// Key to look up.
    key: String,
    /// Default for absent keys.
    default: Value,
) -> Result<Value> {
    Ok(map.get(&key).cloned().unwrap_or(default))
}

/// Insert one key into a map, returning the extended map.
///
/// Fails on duplicates so two entries sharing a key fail the run
/// instead of silently shadowing each other.
///
/// A frozen (`CONST`-derived) map fails instead of deriving: `CONST`
/// bindings never produce a mutable map, so snapshot explicitly
/// through a `LET` copy of unfrozen data if derivation is intended.
///
#[oxdock_func(pure, returns = TypeTag::MapOf(std::sync::Arc::new(TypeTag::Any)))]
#[example(
    name = "insert",
    code = r###"
    IMPORT [STD]
    LET $m: MAP<ANY> = MAP_SET({seed: 0}, "a", 1)
    ASSERT_EQ $m.a 1
    "###
)]
fn map_set(
    /// Map to insert into.
    #[reject_frozen]
    map: BTreeMap<String, Value>,
    /// Key to insert; duplicates fail.
    key: String,
    /// Value to store.
    value: Value,
) -> Result<Value> {
    Ok(super::args::map_set_from_value(
        Value::map(map),
        key,
        value,
    )?)
}

/// Draw a random FLOAT uniformly from [0, 1), optionally seeded.
///
/// Impure by design: unseeded calls return different values, so this
/// runs on the AST script path only, never as a `{{ }}` placeholder
/// call. A seed draws the same value on every run, for deterministic
/// fixtures. Scale and shift the draw for wider ranges.
///
#[oxdock_func(returns = TypeTag::Float, summary = "Draw a random FLOAT uniformly from [0, 1), optionally seeded.")]
#[example(
    name = "seeded draw",
    code = r###"
    # A seed draws the same value on every run.
    IMPORT [STD]
    LET $one: FLOAT = RAND({seed: 7})
    LET $two: FLOAT = RAND({seed: 7})
    ASSERT_EQ $one $two
    "###
)]
#[example(
    name = "unseeded draw",
    code = r###"
    # Scale a unit draw into a backoff delay between 1 and 11 seconds.
    IMPORT [STD]
    LET $draw: FLOAT = RAND()
    LET $delay: FLOAT = 1.0 + $draw * 10.0
    IF $delay < 1.0 {
        FAIL "trap: RAND draw below range"
    }
    IF $delay >= 11.0 {
        FAIL "trap: RAND draw above range"
    }
    ECHO "retrying in {{ $delay }}s"
    "###
)]
fn rand<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Draw settings: `seed`. Omittable: a missing options MAP fills `{}`.
    #[options("seed?: INT")]
    #[optional]
    options: BTreeMap<String, Value>,
) -> Result<Value> {
    let _ = cx;
    // `check_options` in the wrapper verified the shape above; the
    // read below narrows infallibly after.
    let seed = options
        .get("seed")
        .map(|value| value.as_i64().expect("check_options verified INT"));
    super::args::rand_float_from_value(seed)
}

/// Encode a STRING as standard base64.
///
/// Padded RFC 4648 alphabet, so encoded output pastes into any
/// standard decoder. Pairs with `BASE64_DECODE` for the round trip.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "encode",
    code = r###"
    # Encode text to standard base64.
    IMPORT [STD]
    LET $encoded: STRING = BASE64_ENCODE("hello")
    ASSERT_EQ $encoded "aGVsbG8="
    "###
)]
fn base64_encode(
    /// Text to encode.
    text: String,
) -> Result<Value> {
    Ok(super::args::base64_encode_from_value(Value::string(text))?)
}

/// Decode standard base64 to a STRING.
///
/// Fails naming the input on invalid alphabet characters or
/// non-UTF-8 payloads instead of rendering a silent empty. Pairs
/// with `BASE64_ENCODE` for the round trip.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "decode",
    code = r###"
    # Decode standard base64 back to text.
    IMPORT [STD]
    LET $decoded: STRING = BASE64_DECODE("aGVsbG8=")
    ASSERT_EQ $decoded "hello"
    "###
)]
fn base64_decode(
    /// Base64 text to decode.
    text: String,
) -> Result<Value> {
    Ok(super::args::base64_decode_from_value(Value::string(text))?)
}

/// Encode a script value as JSON with one trailing newline.
///
/// Maps stay sorted; only template-safe shapes (STRING, INT, FLOAT,
/// BOOL, LIST, MAP) survive, anything else fails here instead of
/// rendering as a silent empty.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "encode",
    code = r###"
    IMPORT [STD]
    LET $json: STRING = TO_JSON({a: 1})
    ASSERT_EQ $json "{\"a\":1}\n"
    "###
)]
fn to_json(
    /// Value to encode as JSON.
    #[union(String, Int, Float, Bool, List, Map)]
    value: Value,
) -> Result<Value> {
    Ok(super::args::to_json_from_value(value)?)
}

/// Name the word a value holds, for data-driven branching.
///
/// Returns the registered type word (`STRING`, `INT`, `FLOAT`, `BOOL`,
/// `LIST`, `MAP`, plus handle words like `PIPE`): the same name the
/// value prints in arity and coercion errors, so scripts can branch
/// on config shapes (a path string or a path list) without failing.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "name the word",
    code = r###"
    IMPORT [STD]
    LET $word: STRING = TYPE_OF(42)
    ASSERT_EQ $word "INT"
    "###
)]
fn type_of(
    /// Value whose word to name.
    value: Value,
) -> Result<Value> {
    super::args::type_of_from_value(value)
}

/// Merge a LIST of MAPs in order under one duplicate policy.
///
/// `fail_on_duplicate` fails naming the repeated key, so two files
/// claiming one placeholder fail the run instead of shadowing each
/// other. `overwrite` lets later files win, for environment overlays.
/// Non MAP elements fail naming their position.
///
/// A frozen (`CONST`-derived) list or element fails instead of
/// deriving, mirroring `MAP_SET`: `CONST` bindings never produce a
/// mutable map.
///
#[oxdock_func(pure, returns = TypeTag::MapOf(std::sync::Arc::new(TypeTag::Any)))]
#[example(
    name = "merge",
    code = r###"
    IMPORT [STD]
    LET $m: MAP<ANY> = MERGE_MAPS([{a: 1}, {b: 2}], "overwrite")
    ASSERT_EQ $m.a 1
    ASSERT_EQ $m.b 2
    "###
)]
fn merge_maps(
    /// LIST of MAPs to merge in order.
    #[reject_frozen]
    maps: Vec<BTreeMap<String, Value>>,
    /// Duplicate policy.
    #[values("fail_on_duplicate", "overwrite")]
    policy: String,
) -> Result<Value> {
    Ok(super::args::merge_maps_from_value(
        Value::list(maps.into_iter().map(Value::map).collect()),
        policy,
    )?)
}

/// Report the running engine version.
///
/// Returns the workspace version baked in at compile time, so scripts
/// and agents can branch on engine behavior instead of probing it.
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "report",
    code = r###"
    # The engine reports its own build version as a dotted string.
    IMPORT [STD]
    LET $v: STRING = VERSION()
    ASSERT_CONTAINS $v "."
    "###
)]
fn version() -> Result<Value> {
    Ok(Value::string(env!("CARGO_PKG_VERSION").to_string()))
}

/// List all visible function names.
///
/// Sorted LIST of qualified `MODULE::NAME` entries: DSL-defined plus native
/// plus host-registered names.
#[oxdock_func(returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "list",
    code = r###"
    IMPORT [STD]
    LET $names: LIST<STRING> = FUNCTIONS()
    ASSERT_CONTAINS $names "STD::INT"
    "###
)]
fn functions<P: ProcessManager>(cx: &mut StepCtx<P>) -> Result<Value> {
    let mut names: Vec<String> = cx
        .state
        .list_functions()
        .into_iter()
        .map(|meta| meta.name)
        .collect();
    names.sort();
    names.dedup();
    Ok(Value::list(names.into_iter().map(Value::string).collect()))
}

/// Report the verbatim CLI tail after the script position.
///
/// Staged per run from `Options.script_args` (#226): everything after
/// the script file (or `--script <file>`), with one leading `--`
/// stripped. Never merged with interpreter flags, so `--dry-run`
/// after the script arrives here instead of enabling dry-run.
#[oxdock_func(returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "empty",
    code = r###"
    # With no staged args the list is empty.
    IMPORT [STD]
    LET $args: LIST<STRING> = ARGV()
    LET $n: INT = LEN($args)
    ASSERT_EQ $n 0
    "###
)]
fn argv<P: ProcessManager>(cx: &mut StepCtx<P>) -> Result<Value> {
    Ok(Value::list(
        cx.state
            .io
            .script_args()
            .iter()
            .cloned()
            .map(Value::string)
            .collect(),
    ))
}

/// Parse an argv tail against `TYPE FLAGS`, returning flags and positionals.
///
/// Consumes a `LIST<STRING>` (usually `STD::ARGV()`) with the `lexopt`
/// tokenizer. Long `--key=value` binds the string value. Bare long
/// `--key` binds `true`. Short `-a` binds `true`. One `--` entry ends
/// option parsing, so the rest lands in `positionals`. Duplicate keys
/// overwrite in order, so the last occurrence wins.
///
/// The schema is always `TYPE FLAGS`: a `MAP` shape with scalar
/// fields. Present flags coerce to their target (`BOOL` accepts
/// `true`/`false` spellings, `INT`/`FLOAT` parse their strings); bare
/// `--key` for a non-`BOOL` target fails naming the flag. Missing
/// flags fill their `= default`, missing required flags fail, and
/// unknown flags fail. Defaults are literals only: `INT`, `FLOAT`,
/// `BOOL`, and double-quoted `STRING`. A variable or expression in
/// place of a literal fails the `TYPE` statement, so defaults stay
/// plain data in the shape. Absent optional flags without a default stay
/// absent (read them with a typed `GET` default). `positionals` is
/// omitted when empty, so a flags only `MAP<flags: FLAGS>` binds clean.
pub struct ParseArgs;

impl<P: ProcessManager> OxDockFn<P> for ParseArgs {
    fn registration() -> HostRegistration<P> {
        let func: NativeFn<P> = Arc::new(|cx: &mut StepCtx<P>, values: Vec<Value>| {
            if values.len() != 1 {
                return Err(anyhow::Error::new(std_diag::std_parse_args_arity(
                    values.len(),
                )));
            }
            let mut iter = values.into_iter();
            let args_val = iter.next().expect("arity checked above");
            let args =
                string_list_arg(&args_val, &format!("{FUNC_PARSE_ARGS}() argument `$args`"))?;
            let specs = resolve_flags_specs(cx)?;
            let (raw, positionals) = tokenize_raw_args(args)?;
            let coerced = coerce_flags(&specs, &raw)?;
            let mut out = BTreeMap::new();
            out.insert("flags".to_string(), Value::map(coerced));
            if !positionals.is_empty() {
                out.insert("positionals".to_string(), Value::list(positionals));
            }
            Ok(Value::map(out))
        });
        HostRegistration::Stateful {
            name: FUNC_PARSE_ARGS.to_string(),
            meta: FuncMeta {
                name: FUNC_PARSE_ARGS.to_string(),
                module: String::new(),
                kind: FuncKind::HostPure,
                params: Some(vec![FuncParam {
                    name: "args".to_string(),
                    param_type: Some(TypeTag::ListOf(Arc::new(TypeTag::String))),
                    allowed: None,
                    docs: "Argv tail to parse, usually from `STD::ARGV()`.",
                    options: None,
                    optional: false,
                }]),
                returns: Some(TypeTag::Record(Arc::from([
                    Field {
                        name: "flags".to_string(),
                        ty: TypeTag::Map,
                        docs: "Flags coerced to `TYPE FLAGS`.",
                        optional: false,
                        default: None,

                        alias: None,
                    },
                    Field {
                        name: "positionals".to_string(),
                        ty: TypeTag::ListOf(Arc::new(TypeTag::String)),
                        docs: "Trailing values and entries after the first double dash. Omitted when empty.",
                        optional: true,
                        default: None,

                        alias: None,
                    },
                ]))),
                rpn: true,
                summary: "Parse an argv tail against TYPE FLAGS.",
                docs: "Consumes a `LIST<STRING>` with the `lexopt` tokenizer and coerces flags through `TYPE FLAGS`. See the function reference for the coercion and omission rules.",
                examples: &[
                    crate::Example {
                        name: "flags and positionals",
                        fence_meta: None,
                        code: indoc::indoc! {r#"
                            IMPORT [STD]
                            # One schema TYPE drives coercion; the pair binds both slots.
                            TYPE FLAGS = MAP<model: STRING, verbose?: BOOL>
                            TYPE POSITIONS = LIST<STRING>
                            TYPE PARSED = MAP<flags: FLAGS, positionals?: POSITIONS>
                            LET $parsed: PARSED = PARSE_ARGS(["--model=gpt-4", "--verbose", "serve"])
                            LET $flags: FLAGS = $parsed.flags
                            ASSERT_EQ $flags.model "gpt-4"
                            ASSERT_EQ $flags.verbose true
                            ASSERT_EQ $parsed.positionals.0 "serve"
                        "#},
                    },
                    crate::Example {
                        name: "optional stays absent",
                        fence_meta: None,
                        code: indoc::indoc! {r#"
                            IMPORT [STD]
                            # Absent optionals stay absent; read them with a typed GET default.
                            TYPE FLAGS = MAP<model: STRING, propose?: BOOL>
                            TYPE POSITIONS = LIST<STRING>
                            TYPE PARSED = MAP<flags: FLAGS, positionals?: POSITIONS>
                            LET $parsed: PARSED = PARSE_ARGS(["--model=x"])
                            LET $flags: FLAGS = $parsed.flags
                            LET $propose: BOOL = GET($flags, "propose", false)
                            ASSERT_EQ $propose false
                        "#},
                    },
                    crate::Example {
                        name: "flags only omits positionals",
                        fence_meta: None,
                        code: indoc::indoc! {r#"
                            IMPORT [STD]
                            # Empty positionals are omitted, so a flags only shape binds clean.
                            TYPE FLAGS = MAP<model: STRING>
                            TYPE PARSED = MAP<flags: FLAGS>
                            LET $parsed: PARSED = PARSE_ARGS(["--model=x"])
                            LET $flags: FLAGS = $parsed.flags
                            ASSERT_EQ $flags.model "x"
                        "#},
                    },
                    crate::Example {
                        name: "literal defaults",
                        fence_meta: None,
                        code: indoc::indoc! {r#"
                            IMPORT [STD]
                            # Defaults are literals only: INT, FLOAT, BOOL,
                            # and double-quoted STRING fill absent flags.
                            TYPE FLAGS = MAP<retries: INT = 2, ratio: FLOAT = 2.5, verbose: BOOL = true, model: STRING = "gpt-4">
                            TYPE PARSED = MAP<flags: FLAGS>
                            LET $parsed: PARSED = PARSE_ARGS([])
                            LET $flags: FLAGS = $parsed.flags
                            ASSERT_EQ $flags.retries 2
                            ASSERT_EQ $flags.verbose true
                            ASSERT_EQ $flags.model "gpt-4"
                            LET $ratio_ok: BOOL = $flags.ratio == 2.5
                            ASSERT_EQ $ratio_ok true
                        "#},
                    },
                    crate::Example {
                        name: "variable default fails",
                        fence_meta: Some("expect_error:\"expected default literal\""),
                        code: indoc::indoc! {r#"
                            IMPORT [STD]
                            # Defaults are literals: a variable in place
                            # of a literal fails instead of evaluating.
                            TYPE FLAGS = MAP<n: INT = $x>
                            TYPE PARSED = MAP<flags: FLAGS>
                            LET $p: PARSED = PARSE_ARGS([])
                        "#},
                    },
                    crate::Example {
                        name: "missing required",
                        fence_meta: Some("expect_error:\"missing required flag\""),
                        code: indoc::indoc! {r#"
                            IMPORT [STD]
                            # Absent required flags fail naming the flag.
                            TYPE FLAGS = MAP<model: STRING>
                            TYPE PARSED = MAP<flags: FLAGS>
                            LET $parsed: PARSED = PARSE_ARGS([])
                        "#},
                    },
                    crate::Example {
                        name: "unknown flag",
                        fence_meta: Some("expect_error:\"unknown flag\""),
                        code: indoc::indoc! {r#"
                            IMPORT [STD]
                            # Undeclared flags fail instead of passing through.
                            TYPE FLAGS = MAP<model: STRING>
                            TYPE PARSED = MAP<flags: FLAGS>
                            LET $parsed: PARSED = PARSE_ARGS(["--model=x", "--nope=1"])
                        "#},
                    },
                    crate::Example {
                        name: "alias accepted",
                        fence_meta: None,
                        code: indoc::indoc! {r#"
                            IMPORT [STD]
                            # One canonical key binds both spellings; the
                            # alias never appears as a separate key.
                            TYPE FLAGS = MAP<h: BOOL ALIAS help>
                            TYPE PARSED = MAP<flags: FLAGS>
                            LET $a: PARSED = PARSE_ARGS(["--help"])
                            ASSERT_EQ $a.flags.h true
                            LET $b: PARSED = PARSE_ARGS(["--h"])
                            ASSERT_EQ $b.flags.h true
                        "#},
                    },
                    crate::Example {
                        name: "alias duplicate fails",
                        fence_meta: Some("expect_error:\"duplicate flag\""),
                        code: indoc::indoc! {r#"
                            IMPORT [STD]
                            # Canonical and alias together fail naming both.
                            TYPE FLAGS = MAP<h: BOOL ALIAS help>
                            TYPE PARSED = MAP<flags: FLAGS>
                            LET $p: PARSED = PARSE_ARGS(["--h", "--help"])
                        "#},
                    },
                    crate::Example {
                        name: "alias missing names both",
                        fence_meta: Some("expect_error:\"--h/--help\""),
                        code: indoc::indoc! {r#"
                            IMPORT [STD]
                            # Missing required flags name both spellings.
                            TYPE FLAGS = MAP<h: BOOL ALIAS help>
                            TYPE PARSED = MAP<flags: FLAGS>
                            LET $p: PARSED = PARSE_ARGS([])
                        "#},
                    },
                ],
            },
            func,
        }
    }
}

/// CLI target types for `PARSE_ARGS` schemas: the four scalar words
/// raw flags can coerce into. No new types without extending the
/// coercion below together with the schema resolution below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliType {
    String,
    Bool,
    Int,
    Float,
}

/// Coerce one raw flag value to its schema target. Raw values are only
/// ever `BOOL true` (bare) or `STRING` (equals form); anything else is
/// a caller bug. Bare flags for non-`BOOL` targets fail naming the
/// flag instead of smuggling `true` into the slot.
fn coerce_cli_flag(target: CliType, flag: &str, raw: Value) -> DiagnosticResult<Value> {
    match target {
        CliType::Bool => {
            if let Some(b) = raw.as_bool() {
                return Ok(Value::bool(b));
            }
            if let Some(s) = raw.as_str() {
                match super::args::parse_bool_str(s) {
                    Some(b) => return Ok(Value::bool(b)),
                    None => {
                        return Err(std_diag::std_parse_args_bool_flag(flag, s));
                    }
                }
            }
            Err(std_diag::std_parse_args_bool_type(flag, raw.type_name()))
        }
        CliType::String => {
            if let Some(s) = raw.as_str() {
                return Ok(Value::string(s.to_string()));
            }
            Err(std_diag::std_parse_args_needs_value(flag))
        }
        CliType::Int => {
            if raw.as_bool().is_some() {
                return Err(std_diag::std_parse_args_needs_value(flag));
            }
            super::args::int_from_value(raw).map_err(|_| std_diag::std_parse_args_expects_int(flag))
        }
        CliType::Float => {
            if raw.as_bool().is_some() {
                return Err(std_diag::std_parse_args_needs_value(flag));
            }
            super::args::float_from_value(raw)
                .map_err(|_| std_diag::std_parse_args_expects_float(flag))
        }
    }
}

/// One CLI flag spec: target type, required bit, filled default, and
/// optional alternate spelling. Single owner so the tokenizer,
/// coercion, and both entry points share one signature.
#[derive(Debug, Clone)]
struct CliSpec {
    target: CliType,
    required: bool,
    default: Option<Value>,
    alias: Option<String>,
}

/// Canonical specs keyed by canonical flag name.
type CliSpecs = BTreeMap<String, CliSpec>;

/// Checked `LIST<STRING>` extraction for the CLI trio. Single owner
/// so all three functions admit exactly the same argv shape.
fn string_list_arg(val: &Value, what: &str) -> DiagnosticResult<Vec<String>> {
    crate::check_value_at(&TypeTag::ListOf(Arc::new(TypeTag::String)), val, what)
        .map_err(|err| std_diag::std_parse_args_arg_shape(&err))?;
    Ok(val
        .as_list()
        .expect("check_value_at enforces LIST")
        .iter()
        .map(|v| {
            v.as_str()
                .expect("check_value_at enforces STRING")
                .to_string()
        })
        .collect())
}

/// Shared argv tokenizer. Single owner so flags versus positionals
/// split identically for all three entry points. Raw values are only
/// `BOOL true` (bare) or `STRING` (equals form).
fn tokenize_raw_args(args: Vec<String>) -> DiagnosticResult<(BTreeMap<String, Value>, Vec<Value>)> {
    use lexopt::Arg::{Long, Short, Value as ArgValue};
    let mut parser = lexopt::Parser::from_args(args);
    let mut flags: BTreeMap<String, Value> = BTreeMap::new();
    let mut positionals: Vec<Value> = Vec::new();
    let utf8 = |value: std::ffi::OsString| -> DiagnosticResult<String> {
        value
            .into_string()
            .map_err(|_| std_diag::std_parse_args_not_utf8())
    };
    loop {
        match parser.next() {
            Ok(None) => break,
            Ok(Some(Long(name))) => {
                let key = name.to_owned();
                if let Some(raw) = parser.optional_value() {
                    flags.insert(key, Value::string(utf8(raw)?));
                } else {
                    flags.insert(key, Value::bool(true));
                }
            }
            Ok(Some(Short(ch))) => {
                flags.insert(ch.to_string(), Value::bool(true));
            }
            Ok(Some(ArgValue(raw))) => {
                positionals.push(Value::string(utf8(raw)?));
            }
            Err(lexopt::Error::UnexpectedValue { option, value }) => {
                let key = option
                    .strip_prefix("--")
                    .or_else(|| option.strip_prefix('-'))
                    .unwrap_or(option.as_str());
                flags.insert(key.to_string(), Value::string(utf8(value)?));
            }
            Err(err) => return Err(std_diag::std_parse_args_lex_error(&err)),
        }
    }
    Ok((flags, positionals))
}

/// Shared flag coercion. Single owner so `PARSE_ARGS` and
/// `PARSE_FLAGS` fill, require, and reject identically: present flags
/// (canonical or alias) coerce, missing defaulted flags fill, missing
/// required flags fail, missing optionals stay absent, unknown flags
/// fail. One canonical key lands in the parsed map; alias spellings
/// ride the command line only. Missing and duplicate errors name both
/// spellings when an alias exists.
fn coerce_flags(
    specs: &CliSpecs,
    raw: &BTreeMap<String, Value>,
) -> DiagnosticResult<BTreeMap<String, Value>> {
    // Alias reverse map: alias spelling to canonical key. Validated
    // unique at type resolution, so a duplicate insert is unreachable.
    let mut alias_to_canonical: BTreeMap<&str, &str> = BTreeMap::new();
    for (canonical, spec) in specs.iter() {
        if let Some(alias) = spec.alias.as_ref() {
            alias_to_canonical.insert(alias.as_str(), canonical.as_str());
        }
    }
    let mut coerced: BTreeMap<String, Value> = BTreeMap::new();
    for (flag, spec) in specs.iter() {
        if flag.is_empty() {
            return Err(std_diag::std_parse_args_schema_flag_empty());
        }
        let canonical_raw = raw.get(flag);
        let alias_raw = spec.alias.as_ref().and_then(|alias| raw.get(alias));
        if canonical_raw.is_some() && alias_raw.is_some() {
            return Err(std_diag::std_parse_args_duplicate_flag(
                flag,
                spec.alias.as_deref(),
            ));
        }
        if let Some(raw_val) = canonical_raw.or(alias_raw) {
            coerced.insert(
                flag.clone(),
                coerce_cli_flag(spec.target, flag, raw_val.clone())?,
            );
        } else if let Some(d) = spec.default.as_ref() {
            coerced.insert(flag.clone(), d.clone());
        } else if spec.required {
            return Err(std_diag::std_parse_args_missing_flag(
                flag,
                spec.alias.as_deref(),
            ));
        }
    }
    for flag in raw.keys() {
        if !specs.contains_key(flag) && !alias_to_canonical.contains_key(flag.as_str()) {
            return Err(std_diag::std_parse_args_unknown_flag(flag));
        }
    }
    Ok(coerced)
}

/// Resolve the fixed `TYPE FLAGS` schema into coercion specs: one entry
/// per record field, targets from scalar field types, required unless
/// optional or defaulted, defaults filled from the shape, aliases
/// carried for command-line acceptance. Single owner so both pair and
/// flags only entry points admit exactly the same set. Non-record
/// types and non-scalar fields fail naming the flag.
fn resolve_flags_specs<P: ProcessManager>(cx: &mut StepCtx<P>) -> DiagnosticResult<CliSpecs> {
    let name = "FLAGS";
    let tag = cx
        .state
        .resolve_tag(name)
        .map_err(|err| std_diag::std_parse_args_schema_type(name, &anyhow::Error::new(err)))?;
    let TypeTag::Record(fields) = tag else {
        return Err(std_diag::std_parse_args_schema_shape(name));
    };
    let mut specs = BTreeMap::new();
    for field in fields.iter() {
        let target = match field.ty {
            TypeTag::String => CliType::String,
            TypeTag::Bool => CliType::Bool,
            TypeTag::Int => CliType::Int,
            TypeTag::Float => CliType::Float,
            _ => {
                return Err(std_diag::std_parse_args_schema_field_type(
                    &field.name,
                    field.ty.name(),
                ));
            }
        };
        specs.insert(
            field.name.clone(),
            CliSpec {
                target,
                required: !field.optional && field.default.is_none(),
                default: field.default.clone(),
                alias: field.alias.clone(),
            },
        );
    }
    Ok(specs)
}

/// Parse flags only against `TYPE FLAGS`.
///
/// Returns the flags `MAP` directly: no pair wrapper. Positional
/// arguments fail instead of dropping silently, so a space-separated
/// value (`--model gpt-4`) names the mistake instead of binding
/// `--model` to `true` and losing `gpt-4`.
/// Same tokenizer, coercion, defaults, and errors as `PARSE_ARGS`.
pub struct ParseFlags;

impl<P: ProcessManager> OxDockFn<P> for ParseFlags {
    fn registration() -> HostRegistration<P> {
        let func: NativeFn<P> = Arc::new(|cx: &mut StepCtx<P>, values: Vec<Value>| {
            if values.len() != 1 {
                return Err(anyhow::Error::new(std_diag::std_parse_flags_arity(
                    values.len(),
                )));
            }
            let mut iter = values.into_iter();
            let args_val = iter.next().expect("arity checked above");
            let args = string_list_arg(&args_val, "PARSE_FLAGS() argument `$args`")?;
            let specs = resolve_flags_specs(cx)?;
            let (raw, positionals) = tokenize_raw_args(args)?;
            if !positionals.is_empty() {
                return Err(anyhow::Error::new(std_diag::std_parse_flags_positional(
                    &positionals
                        .iter()
                        .map(|pos| pos.to_string())
                        .collect::<Vec<_>>(),
                )));
            }
            Ok(Value::map(coerce_flags(&specs, &raw)?))
        });
        HostRegistration::Stateful {
            name: FUNC_PARSE_FLAGS.to_string(),
            meta: FuncMeta {
                name: FUNC_PARSE_FLAGS.to_string(),
                module: String::new(),
                kind: FuncKind::HostPure,
                params: Some(vec![FuncParam {
                    name: "args".to_string(),
                    param_type: Some(TypeTag::ListOf(Arc::new(TypeTag::String))),
                    allowed: None,
                    docs: "Argv tail to parse, usually from `STD::ARGV()`.",
                    options: None,
                    optional: false,
                }]),
                returns: Some(TypeTag::Map),
                rpn: true,
                summary: "Parse flags against TYPE FLAGS.",
                docs: "Consumes a `LIST<STRING>` and coerces flags through `TYPE FLAGS`. Returns the flags `MAP` directly; positional arguments fail.",
                examples: &[crate::Example {
                    name: "flags only",
                    fence_meta: None,
                    code: indoc::indoc! {r#"
                        IMPORT [STD]
                        # Flags only returns the flags MAP directly.
                        TYPE FLAGS = MAP<model: STRING, retries: INT = 2>
                        LET $flags: FLAGS = PARSE_FLAGS(["--model=x"])
                        ASSERT_EQ $flags.model "x"
                        ASSERT_EQ $flags.retries 2
                    "#},
                }],
            },
            func,
        }
    }
}

/// Parse positionals only: no schema, no `TYPE FLAGS` needed.
///
/// Returns trailing values and entries after the first double dash in
/// argv order. Tokenizer matches `PARSE_ARGS` exactly.
pub struct ParsePositionals;

impl<P: ProcessManager> OxDockFn<P> for ParsePositionals {
    fn registration() -> HostRegistration<P> {
        let func: NativeFn<P> = Arc::new(|_: &mut StepCtx<P>, values: Vec<Value>| {
            if values.len() != 1 {
                return Err(anyhow::Error::new(std_diag::std_parse_positionals_arity(
                    values.len(),
                )));
            }
            let mut iter = values.into_iter();
            let args_val = iter.next().expect("arity checked above");
            let args = string_list_arg(
                &args_val,
                &format!("{FUNC_PARSE_POSITIONALS}() argument `$args`"),
            )?;
            let (_raw, positionals) = tokenize_raw_args(args)?;
            Ok(Value::list(positionals))
        });
        HostRegistration::Stateful {
            name: FUNC_PARSE_POSITIONALS.to_string(),
            meta: FuncMeta {
                name: FUNC_PARSE_POSITIONALS.to_string(),
                module: String::new(),
                kind: FuncKind::HostPure,
                params: Some(vec![FuncParam {
                    name: "args".to_string(),
                    param_type: Some(TypeTag::ListOf(Arc::new(TypeTag::String))),
                    allowed: None,
                    docs: "Argv tail to parse, usually from `STD::ARGV()`.",
                    options: None,
                    optional: false,
                }]),
                returns: Some(TypeTag::ListOf(Arc::new(TypeTag::String))),
                rpn: true,
                summary: "Parse positionals in argv order.",
                docs: "Consumes a `LIST<STRING>` and returns positionals in argv order with no schema.",
                examples: &[crate::Example {
                    name: "positionals only",
                    fence_meta: None,
                    code: indoc::indoc! {r#"
                        IMPORT [STD]
                        # No schema needed: positionals are always strings in order.
                        TYPE POSITIONS = LIST<STRING>
                        LET $pos: POSITIONS = PARSE_POSITIONALS(["serve", "--", "--not-a-flag"])
                        ASSERT_EQ $pos.0 "serve"
                        ASSERT_EQ $pos.1 "--not-a-flag"
                    "#},
                }],
            },
            func,
        }
    }
}

/// Describe one function by qualified name.
///
/// Returns a MAP with name, module, kind, params, returns, rpn, and
/// summary. `params` is always a LIST (empty for parameterless
/// functions) of entries carrying name, param_type, allowed, docs,
/// options, and optional flags, so the shape holds for every entry. Bare names
/// fail closed: `DESCRIBE` requires the qualified form (except
/// `INSPECT`, which is syntax rather than a registry entry). Errors
/// on unknown function.
///
/// Field table for the `DESCRIBE` result bundle: one source for the
/// shape the host mints, so introspection, enforcement, and
/// documentation cannot drift apart.

#[oxdock_func(
    returns = TypeTag::Record(std::sync::Arc::from([Field {
        name: "name".to_string(),
        ty: TypeTag::String,
        docs: "Qualified function name.",
        optional: false,
        default: None,

        alias: None,}, Field {
        name: "module".to_string(),
        ty: TypeTag::String,
        docs: "Owning module.",
        optional: false,
        default: None,

        alias: None,}, Field {
        name: "kind".to_string(),
        ty: TypeTag::String,
        docs: "Origin label (`host` or `script`).",
        optional: false,
        default: None,

        alias: None,}, Field {
        name: "params".to_string(),
        ty: TypeTag::ListOf(std::sync::Arc::new(TypeTag::MapOf(std::sync::Arc::new(
            TypeTag::Any,
        )))),
        docs: "Parameter entries (empty for parameterless functions).",
        optional: false,
        default: None,

        alias: None,}, Field {
        name: "returns".to_string(),
        ty: TypeTag::String,
        docs: "Structural return shape (empty when untagged).",
        optional: false,
        default: None,

        alias: None,}, Field {
        name: "rpn".to_string(),
        ty: TypeTag::Bool,
        docs: "True when callable in RPN position.",
        optional: false,
        default: None,

        alias: None,}, Field {
        name: "summary".to_string(),
        ty: TypeTag::String,
        docs: "One-line description.",
        optional: false,
        default: None,

        alias: None,}]))
    summary = "Describe one function by qualified name."
)]
#[example(
    name = "describe",
    code = r###"
    IMPORT [STD]
    LET $info: MAP<ANY> = DESCRIBE("STD::INT")
    ASSERT_EQ $info.name "STD::INT"
    ASSERT_EQ $info.returns "INT"
    "###
)]
fn describe<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Qualified function name (`MODULE::NAME`).
    name: String,
) -> Result<Value> {
    if split_qualified(&name).is_none() && name != KEYWORD_INSPECT {
        return Err(anyhow::Error::new(std_diag::func_describe_unqualified(
            &name,
        )));
    }
    cx.state
        .describe_function(&name)
        .ok_or_else(|| anyhow::Error::new(std_diag::func_unknown(&name)))
}

/// List all known type names.
///
/// Sorted LIST of startup plus host-registered type descriptors. Reads the
/// run's name directory, so it runs on the AST path like the other
/// introspection functions.
#[oxdock_func(returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "list",
    code = r###"
    IMPORT [STD]
    LET $names: LIST<STRING> = TYPES()
    ASSERT_CONTAINS $names "STRING"
    "###
)]
fn types<P: ProcessManager>(cx: &mut StepCtx<P>) -> Result<Value> {
    Ok(Value::list(
        cx.state
            .type_names()
            .into_iter()
            .map(Value::string)
            .collect(),
    ))
}

/// Describe one type by name.
///
/// Returns a MAP with name, summary, and docs. Errors on unknown type.
/// Reads the run's name directory, so it runs on the AST path.
///
#[oxdock_func(returns = TypeTag::Record(std::sync::Arc::from([Field {
    name: "name".to_string(),
    ty: TypeTag::String,
    docs: "Queried type name.",
    optional: false,
    default: None,

    alias: None,}, Field {
    name: "summary".to_string(),
    ty: TypeTag::String,
    docs: "One-line description.",
    optional: false,
    default: None,

    alias: None,}, Field {
    name: "docs".to_string(),
    ty: TypeTag::String,
    docs: "Full documentation text.",
    optional: false,
    default: None,

    alias: None,}])))]
#[example(
    name = "describe",
    code = r###"
    IMPORT [STD]
    LET $info: MAP<ANY> = TYPE_DESCRIBE("STRING")
    ASSERT_EQ $info.name "STRING"
    "###
)]
fn type_describe<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Type name to describe.
    name: String,
) -> Result<Value> {
    if let Some(descriptor) = cx.state.describe_type(&name) {
        let mut map = BTreeMap::new();
        map.insert(
            "name".to_string(),
            Value::string(descriptor.name.to_string()),
        );
        map.insert(
            "summary".to_string(),
            Value::string(descriptor.summary.to_string()),
        );
        map.insert(
            "docs".to_string(),
            Value::string(descriptor.docs.to_string()),
        );
        return Ok(Value::map(map));
    }
    // Script aliases have no descriptor but introspect the same way:
    // the entry names the alias and its canonical target spelling.
    if let Some(target) = cx.state.alias_target(&name) {
        let mut map = BTreeMap::new();
        map.insert("name".to_string(), Value::string(name.clone()));
        map.insert(
            "summary".to_string(),
            Value::string(format!("Script type alias for {target}.")),
        );
        map.insert(
            "docs".to_string(),
            Value::string(format!(
                "Declared with TYPE {name} = {target}. The alias expands structurally at every declaration."
            )),
        );
        return Ok(Value::map(map));
    }
    Err(anyhow::Error::new(std_diag::std_unknown_type(&name)))
}

/// Path of the running interpreter binary.
///
/// Returns the executing binary as a `STRING`, so one script can
/// reinvoke the interpreter on another script without bridging an env
/// var. The child re-parses from bytes in a separate process: no
/// bindings cross back (only stdout and files), and each run pays
/// a spawn. Prefer modules for reuse (statically gated, in-process);
/// reach for reinvocation when the child must run isolated, or under
/// a different binary than the parent.
///
/// Unsupported under Miri isolation: the lookup itself fails there
/// instead of returning a path.
///
#[oxdock_func(returns = TypeTag::String)]
#[example(
    name = "self",
    code = r###"
    # The running interpreter resolves to a non-empty path.
    IMPORT [STD]
    LET $exe: STRING = INTERPRETER()
    LET $nonempty: BOOL = $exe != ""
    ASSERT_EQ $nonempty true
    "###
)]
#[example(
    name = "dynamic assembly",
    code = r###"
    # Assemble the child script dynamically: the parent interpolates
    # its own bindings into the pipe, then runs the interpreter on it.
    # Unescaped {{ }} bakes in at render time; \{{ }} stays literal
    # so the child expands it with its own bindings.
    # The pipe delivers EOF when the producer step ends, so the child
    # reads exactly one script.
    IMPORT [STD]
    LET $exe: STRING = INTERPRETER()
    LET $who: STRING = "Ada"
    LET $child: PIPE
    WITH_IO [stdout=$child] ECHO """
        ECHO "hello {{ $who }}!"
        LET $who: STRING = "Grace"
        ECHO "child sees \{{ $who }}!"
        """
    LET $out: STRING = WITH_IO [stdin=$child] RUN [$exe]
    ASSERT_CONTAINS $out "hello Ada!"
    ASSERT_CONTAINS $out "child sees Grace!"
    "###
)]
fn interpreter<P: ProcessManager>(cx: &mut StepCtx<P>) -> Result<Value> {
    if let Some(staged) = cx.state.io.interpreter_override() {
        return Ok(Value::string(staged));
    }
    Ok(Value::string(oxdock_process::interpreter_exe()?))
}

/// Report whether the current context is the entry script.
///
/// True at entry top level and inside entry-owned functions; false
/// inside module-owned functions and module init bodies. Lets a
/// library file double as a runnable demo: guard the demo with
/// `IF IS_MAIN()` so importing it stays side-effect free.
///
#[oxdock_func(returns = TypeTag::Bool)]
#[example(
    name = "main",
    code = r###"
    # Entry top level runs as main.
    IMPORT [STD]
    LET $m: BOOL = IS_MAIN()
    ASSERT_EQ $m true
    "###
)]
#[example(
    name = "library doubles as demo",
    fence_meta = "roots:unified",
    code = r###"
    # A library doubles as a runnable demo: guard the demo with
    # IF IS_MAIN() so importing it stays side effect free. The
    # parent writes both files, then runs each in a fresh
    # interpreter: direct execution prints the demo, importing
    # it does not. Unified roots: the child inherits one
    # workspace, so parent and child see the same files. The
    # exact stdout match proves the demo line stays out of the
    # importing run.
    IMPORT [STD]

    # The library under test: demo guarded by IS_MAIN.
    WRITE lib.oxfile """
        IMPORT [STD]
        FUNC GREET($x: STRING): STRING { RETURN "hi \{{ $x }}" }
        EXPORT GREET
        IF IS_MAIN() {
            ECHO "demo: hi Ada"
        }
        """

    # The importer: calls the library, prints only its own line.
    WRITE main.oxfile """
        IMPORT ["lib.oxfile" AS LIB]
        LET $g: STRING = LIB::GREET("Bo")
        ECHO "imported: \{{ $g }}"
        """

    # Direct run executes the demo.
    LET $exe: STRING = INTERPRETER()
    LET $direct: STRING = RUN [$exe, "lib.oxfile"]
    ASSERT_CONTAINS $direct "demo: hi Ada"

    # Importing run stays silent apart from its own line.
    LET $via: STRING = RUN [$exe, "main.oxfile"]
    ASSERT_EQ $via "imported: hi Bo\n"
    "###
)]
fn is_main<P: ProcessManager>(cx: &mut StepCtx<P>) -> Result<Value> {
    let main = match cx.state.module_stack.last() {
        None | Some(None) => true,
        Some(Some(_)) => false,
    };
    Ok(Value::bool(main))
}

/// Report whether a standard stream is a terminal.
///
/// `IS_TERMINAL("stdin")`, `IS_TERMINAL("stdout")`, or `IS_TERMINAL("stderr")`
/// answers for the step's stream as currently bound, so scripts can adapt
/// prompts, colors, and progress output. Anything diverted from the
/// terminal reports false without touching host handles: `WITH_IO` pipe
/// bindings (script backends and OS pairs), `LET`-capture sinks, staged
/// runner sinks, and any materialized stdin stream (only a directly
/// inherited fd falls back to the process check). A transparent root tee
/// still answers the session question via the process check. The name
/// matches exactly (no case folding): anything else bails. AST-only:
/// reads the step context like the other introspection functions.
///
#[oxdock_func(returns = TypeTag::Bool)]
#[example(
    name = "null stdin",
    code = r###"
    # A null stdin is never a terminal, in every harness.
    IMPORT [STD]
    LET $term: BOOL = IS_TERMINAL("stdin")
    ASSERT_EQ $term false
    "###
)]
fn is_terminal<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Stream name: `stdin`, `stdout`, or `stderr` (exact match).
    #[values("stdin", "stdout", "stderr")]
    stream_name: String,
) -> Result<Value> {
    use std::io::IsTerminal;
    let terminal = match stream_name.as_str() {
        "stdin" => {
            // A script-pipe backend is definitive; Null is /dev/null.
            // Only a directly inherited fd answers the process check: a
            // materialized Stream is always a binding (staged input,
            // WITH_IO pipe, or OS half), never the raw fd.
            if cx.stdin_pipe.is_some() {
                false
            } else {
                match &cx.stdin {
                    CommandStdin::Null => false,
                    #[cfg(not(miri))]
                    CommandStdin::OsPipe(_) => false,
                    CommandStdin::Stream(_) => false,
                    CommandStdin::Inherit => std::io::stdin().is_terminal(),
                }
            }
        }
        "stdout" => {
            if cx.out_pipe.is_some() {
                // WITH_IO script-pipe binding: never a terminal.
                false
            } else if cx.state.io.stdout().is_some() {
                // Staged runner sink: the root tee diverts bytes to the
                // sink only, never to real stdout.
                false
            } else {
                match &cx.out {
                    // Unbound: inherited straight through.
                    None => std::io::stdout().is_terminal(),
                    // OS kernel pipe: never a terminal.
                    #[cfg(not(miri))]
                    Some(StreamHandle::Os(_)) => false,
                    // Root tee (transparent: forwards to real stdout when
                    // unstaged, so terminal-ness survives) versus a genuine
                    // diversion. WITH_IO bindings never reach here (script
                    // pipes trip out_pipe, OS pipes trip Os, staged sinks
                    // trip above). LET-capture cannot reach here either:
                    // assign_capture installs no sink for a bare NAME(...)
                    // call, so a direct query always observes the ambient
                    // routing. The process check answers the session
                    // question.
                    Some(StreamHandle::Stream(_)) => std::io::stdout().is_terminal(),
                }
            }
        }
        "stderr" => {
            if cx.state.io.stderr().is_some() {
                // Staged runner sink: diverted, never a terminal.
                false
            } else {
                match &cx.err {
                    // Unbound: inherited straight through.
                    None => std::io::stderr().is_terminal(),
                    // OS kernel pipe: never a terminal.
                    #[cfg(not(miri))]
                    Some(StreamHandle::Os(_)) => false,
                    // Root never tees stderr and LET never captures it,
                    // so a bound handle here is always a WITH_IO binding.
                    Some(StreamHandle::Stream(_)) => false,
                }
            }
        }
        _ => {
            return Err(anyhow::Error::new(std_diag::std_is_terminal_expected(
                &stream_name,
            )));
        }
    };
    Ok(Value::bool(terminal))
}

/// Create a counting semaphore admitting at most `max` concurrent holders.
///
/// Non-positive maxima bail. The word names a shared backend: every clone
/// observes the same count, and admission runs through
/// `SEMAPHORE_TRY_ACQUIRE`, never through the `SEMAPHORE_AVAILABLE`
/// readout.
///
#[oxdock_func(returns = TypeTag::Semaphore)]
#[example(
    name = "mint",
    code = r###"
    # Mint a semaphore admitting at most two holders.
    IMPORT [STD]
    LET $sem: SEMAPHORE = SEMAPHORE_NEW(2)

    # A fresh semaphore reports its full cap as available.
    LET $free: INT = SEMAPHORE_AVAILABLE($sem)
    ASSERT_EQ $free 2
    "###
)]
fn semaphore_new<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Maximum concurrent holders; must be positive.
    max: i64,
) -> Result<Value> {
    let _ = cx;
    if max <= 0 {
        return Err(anyhow::Error::new(std_diag::std_semaphore_needs_positive(
            max,
        )));
    }
    Ok(Value::semaphore(max as usize))
}

/// Attempt one non-blocking acquire, always answering a MAP.
///
/// `held` is `true` with the permit under the `permit` key, or `false`
/// with no `permit` key: branch on `$m.held` directly. The DSL has no
/// null, so the absent key is the miss shape. Do not read `$m.permit`
/// unless `held`: missing-key access bails strictly.
/// Never waits, so no wait can wedge.
///
#[oxdock_func(returns = TypeTag::Record(std::sync::Arc::from([Field {
    name: "held".to_string(),
    ty: TypeTag::Bool,
    docs: "`true` with the permit under `permit`, `false` with no `permit` key.",
    optional: false,
    default: None,

    alias: None,}, Field {
    name: "permit".to_string(),
    ty: TypeTag::Permit,
    docs: "Permit handle; read only when `held`.",
    optional: true,
    default: None,

    alias: None,}])))]
#[example(
    name = "acquire",
    code = r###"
    # First acquire holds with a permit to bind.
    IMPORT [STD]
    LET $sem: SEMAPHORE = SEMAPHORE_NEW(1)
    LET $first: MAP<held: BOOL, permit?: PERMIT> = SEMAPHORE_TRY_ACQUIRE($sem)
    ASSERT_EQ $first.held true
    LET $permit: PERMIT = $first.permit

    # A capped semaphore answers held false with no permit key.
    LET $second: MAP<held: BOOL, permit?: PERMIT> = SEMAPHORE_TRY_ACQUIRE($sem)
    ASSERT_EQ $second.held false
    LET $has_permit: BOOL = HAS_KEY($second, "permit")
    ASSERT_EQ $has_permit false
    "###
)]
fn semaphore_try_acquire<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Semaphore handle from `SEMAPHORE_NEW`.
    sem: Arc<SemaphoreState>,
) -> Result<Value> {
    let _ = cx;
    let mut map = BTreeMap::new();
    if sem.try_acquire() {
        map.insert("held".to_string(), Value::bool(true));
        map.insert("permit".to_string(), Value::permit(&sem));
    } else {
        map.insert("held".to_string(), Value::bool(false));
    }
    Ok(Value::map(map))
}

/// Read free permits under the lock, with no mutation.
///
/// Observability only (audit lines, healthchecks: `active = max - free`).
/// Exact at read time and stale the instant the caller acts on it, so it
/// must never drive admission: that is `SEMAPHORE_TRY_ACQUIRE`'s job.
///
#[oxdock_func(pure, returns = TypeTag::Int)]
#[example(
    name = "readout",
    code = r###"
    # Hold one permit on a cap of two.
    IMPORT [STD]
    LET $sem: SEMAPHORE = SEMAPHORE_NEW(2)
    LET $acq: MAP<held: BOOL, permit?: PERMIT> = SEMAPHORE_TRY_ACQUIRE($sem)
    ASSERT_EQ $acq.held true

    # One held permit leaves exactly one free.
    LET $free: INT = SEMAPHORE_AVAILABLE($sem)
    ASSERT_EQ $free 1
    "###
)]
fn semaphore_available(
    /// Semaphore handle from `SEMAPHORE_NEW`.
    sem: Arc<SemaphoreState>,
) -> Result<Value> {
    Ok(Value::int(sem.available() as i64))
}

/// Trim Unicode whitespace from both ends of text.
///
/// Accepts `STRING` or `PIPE`; the return mirrors the input kind.
/// Pipes stream on a pump thread with O(chunk) memory: leading
/// whitespace falls at the stream head while trailing whitespace
/// holds in the spill substrate until the next segment proves it
/// interior, so unbounded streams never buffer unbounded memory.
///
#[oxdock_func(
    rpn,
    returns = TypeTag::union(vec![TypeTag::String, TypeTag::Pipe]),
    template = text_trim_string
)]
#[example(
    name = "trim",
    code = r###"
    # Leading and trailing whitespace falls away.
    IMPORT [STD]
    LET $clean: STRING = TEXT_TRIM("  hello  ")
    ASSERT_EQ $clean "hello"
    "###
)]
#[example(
    name = "trim pipe mirrors",
    code = r###"
    # Pipe input mints pipe output with the same transform.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "  hello  "
    }
    LET $out: PIPE = TEXT_TRIM($cap)
    WITH_IO [stdin=$out] READ_LINE $line
    ASSERT_EQ $line "hello"
    "###
)]
#[example(
    name = "trim template",
    code = r###"
    # String inputs run inside placeholders too.
    IMPORT [STD]
    LET $msg: STRING = 'clean:{{ STD::TEXT_TRIM("  hello  ") }}'
    ASSERT_EQ $msg "clean:hello"
    "###
)]
#[example(
    name = "trim file template",
    code = r###"
    # EXPAND templates resolve the same registry.
    IMPORT [STD]
    WRITE "tpl.txt" "clean:\{{ STD::TEXT_TRIM(\"  hello  \") }}"
    LET $out: STRING = EXPAND "tpl.txt"
    ASSERT_EQ $out "clean:hello"
    "###
)]
fn text_trim<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Text to trim.
    #[union(String, Pipe)]
    text: Value,
) -> Result<Value> {
    super::args::mirror_stream(cx, "TEXT_TRIM", &text, super::args::TextPump::Trim)
}

/// Template projection of `TEXT_TRIM`: the same trim for `{{ }}`
/// placeholders, which carry no step IO for pipes.
fn text_trim_string(text: Value) -> Result<Value> {
    super::args::mirror_string("TEXT_TRIM", &text, super::args::TextPump::Trim)
}

/// Strip one leading prefix, returning the input unchanged when absent.
///
/// Accepts `STRING` or `PIPE`; the return mirrors the input kind.
/// Pipes decide on the first `prefix.len()` bytes and stream the rest
/// untouched. Matching is case-sensitive; there is no
/// case-insensitive mode.
/// Used for tag prefix strips such as `v1.2.3` to `1.2.3`.
///
#[oxdock_func(
    rpn,
    returns = TypeTag::union(vec![TypeTag::String, TypeTag::Pipe]),
    template = text_trim_prefix_string
)]
#[example(
    name = "strip prefix",
    code = r###"
    # A present prefix falls away; absence returns the input unchanged.
    IMPORT [STD]
    LET $bare: STRING = TEXT_TRIM_PREFIX("v1.2.3", "v")
    ASSERT_EQ $bare "1.2.3"

    # Missing prefixes pass through untouched.
    LET $same: STRING = TEXT_TRIM_PREFIX("1.2.3", "v")
    ASSERT_EQ $same "1.2.3"
    "###
)]
#[example(
    name = "strip prefix pipe mirrors",
    code = r###"
    # Pipe input mints pipe output with the same strip.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "v1.2.3"
    }
    LET $trimmed: PIPE = TEXT_TRIM($cap)
    LET $out: PIPE = TEXT_TRIM_PREFIX($trimmed, "v")
    WITH_IO [stdin=$out] READ_LINE $line
    ASSERT_EQ $line "1.2.3"
    "###
)]
#[example(
    name = "strip prefix template",
    code = r###"
    # String inputs run inside placeholders too.
    IMPORT [STD]
    LET $msg: STRING = 'bare:{{ STD::TEXT_TRIM_PREFIX("v1.2.3", "v") }}'
    ASSERT_EQ $msg "bare:1.2.3"
    "###
)]
fn text_trim_prefix<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Text to strip.
    #[union(String, Pipe)]
    text: Value,
    /// Leading prefix to remove once.
    prefix: String,
) -> Result<Value> {
    super::args::mirror_stream(
        cx,
        "TEXT_TRIM_PREFIX",
        &text,
        super::args::TextPump::TrimPrefix(prefix),
    )
}

/// Template projection of `TEXT_TRIM_PREFIX`: the same strip for
/// `{{ }}` placeholders, which carry no step IO for pipes.
fn text_trim_prefix_string(text: Value, prefix: String) -> Result<Value> {
    super::args::mirror_string(
        "TEXT_TRIM_PREFIX",
        &text,
        super::args::TextPump::TrimPrefix(prefix),
    )
}

/// Strip one trailing suffix, returning the input unchanged when absent.
///
/// Accepts `STRING` or `PIPE`; the return mirrors the input kind.
/// Pipes hold a `suffix.len()` trailing window and stream the rest.
/// Matching is case-sensitive; there is no case-insensitive mode.
///
#[oxdock_func(
    rpn,
    returns = TypeTag::union(vec![TypeTag::String, TypeTag::Pipe]),
    template = text_trim_suffix_string
)]
#[example(
    name = "strip suffix",
    code = r###"
    # A present suffix falls away; absence returns the input unchanged.
    IMPORT [STD]
    LET $bare: STRING = TEXT_TRIM_SUFFIX("archive.tar.gz", ".gz")
    ASSERT_EQ $bare "archive.tar"

    # Missing suffixes pass through untouched.
    LET $same: STRING = TEXT_TRIM_SUFFIX("archive.tar", ".gz")
    ASSERT_EQ $same "archive.tar"
    "###
)]
#[example(
    name = "strip suffix pipe mirrors",
    code = r###"
    # Pipe input mints pipe output with the same strip.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "archive.tar.gz"
    }
    LET $trimmed: PIPE = TEXT_TRIM($cap)
    LET $out: PIPE = TEXT_TRIM_SUFFIX($trimmed, ".gz")
    WITH_IO [stdin=$out] READ_LINE $line
    ASSERT_EQ $line "archive.tar"
    "###
)]
#[example(
    name = "strip suffix template",
    code = r###"
    # String inputs run inside placeholders too.
    IMPORT [STD]
    LET $msg: STRING = 'bare:{{ STD::TEXT_TRIM_SUFFIX("archive.tar.gz", ".gz") }}'
    ASSERT_EQ $msg "bare:archive.tar"
    "###
)]
fn text_trim_suffix<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Text to strip.
    #[union(String, Pipe)]
    text: Value,
    /// Trailing suffix to remove once.
    suffix: String,
) -> Result<Value> {
    super::args::mirror_stream(
        cx,
        "TEXT_TRIM_SUFFIX",
        &text,
        super::args::TextPump::TrimSuffix(suffix),
    )
}

/// Template projection of `TEXT_TRIM_SUFFIX`: the same strip for
/// `{{ }}` placeholders, which carry no step IO for pipes.
fn text_trim_suffix_string(text: Value, suffix: String) -> Result<Value> {
    super::args::mirror_string(
        "TEXT_TRIM_SUFFIX",
        &text,
        super::args::TextPump::TrimSuffix(suffix),
    )
}

/// Join a directory and a file name with the host-native separator.
///
/// Both inputs accept `STRING` or workspace `PATH` words; the result
/// is a `STRING` joined with the platform separator, so installers
/// never hand-roll separators in templates. No filesystem access:
/// missing parents fail later at the consumer. Stays `STRING` (not
/// `PATH`) on purpose: the joined value is a not-yet-validated
/// destination string like `$dir` itself, and `PATH` words
/// re-canonicalize to forward slashes on template expansion, which
/// would undo the native join.
#[oxdock_func(returns = TypeTag::String)]
#[example(
    name = "join path",
    code = r###"
    # Directory and file stem join with the native separator.
    # Source `\\` is one backslash at runtime; `/` works on Windows
    # too, native just reads better there.
    IMPORT [STD]
    LET $p: STRING = PATH_JOIN("some-dir", "some-file")
    [family:unix] ASSERT_EQ $p "some-dir/some-file"
    [family:windows] ASSERT_EQ $p "some-dir\\some-file"
    "###
)]
fn path_join<P: ProcessManager>(
    _cx: &mut StepCtx<P>,
    /// Directory half of the destination.
    #[union(String, Path)]
    dir: Value,
    /// File half of the destination.
    #[union(String, Path)]
    name: Value,
) -> Result<Value> {
    let d = super::args::string_or_path_text(&dir)?;
    let n = super::args::string_or_path_text(&name)?;
    Ok(Value::string(oxdock_fs::join_native(&d, &n)))
}

/// Final segment of a path (`PATH_BASE`).
///
/// Pure lexical surgery with no filesystem access: backslashes normalize
/// to slashes, trailing slashes trim (root stays `/`), then the text
/// after the last slash binds. Empty input fails closed. Runs in
/// templates like every pure builtin.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "base",
    code = r###"
    # Final segments bind without filesystem access.
    IMPORT [STD]
    LET $b: STRING = PATH_BASE("some-dir/some-file")
    ASSERT_EQ $b "some-file"

    # Trailing slashes trim before the split.
    LET $d: STRING = PATH_BASE("some-dir/")
    ASSERT_EQ $d "some-dir"
    "###
)]
fn path_base(
    /// Path whose final segment to bind.
    #[union(String, Path)]
    path: Value,
) -> Result<Value> {
    let text = super::args::string_or_path_text(&path)?;
    let base = oxdock_fs::path_base(&text)
        .map_err(|e| anyhow::Error::new(std_diag::std_path_base_failed(&e)))?;
    Ok(Value::string(base))
}

/// Lexical relativization (`PATH_REL`): remainder of a path below a base.
///
/// Pure string surgery with no filesystem access: both sides normalize
/// backslashes to slashes and trim trailing slashes, then `base`
/// segments must prefix `path` segments on a segment boundary. Equal
/// paths bind `.`; non-prefix inputs fail closed naming both sides.
/// Absolute and relative shapes never mix. Runs in templates like
/// every pure builtin.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "relativize",
    code = r###"
    # Prefix bases strip to their remainder.
    IMPORT [STD]
    LET $r: STRING = PATH_REL("some-dir", "some-dir/some-file")
    ASSERT_EQ $r "some-file"

    # Equal paths bind dot.
    LET $dot: STRING = PATH_REL("some-dir", "some-dir")
    ASSERT_EQ $dot "."
    "###
)]
fn path_rel(
    /// Base prefix segments must match.
    #[union(String, Path)]
    base: Value,
    /// Path below the base to relativize.
    #[union(String, Path)]
    path: Value,
) -> Result<Value> {
    let b = super::args::string_or_path_text(&base)?;
    let p = super::args::string_or_path_text(&path)?;
    let rel = oxdock_fs::path_rel(&b, &p)
        .map_err(|e| anyhow::Error::new(std_diag::std_path_rel_failed(&e)))?;
    Ok(Value::string(rel))
}

/// Uppercase text with full Unicode mapping.
///
/// Accepts `STRING` or `PIPE`; the return mirrors the input kind.
/// Pipes map chunk by chunk across a char-boundary carry, so
/// multibyte characters split across reads still fold correctly.
///
#[oxdock_func(
    rpn,
    returns = TypeTag::union(vec![TypeTag::String, TypeTag::Pipe]),
    template = text_to_upper_string
)]
#[example(
    name = "uppercase",
    code = r###"
    # Text folds to uppercase.
    IMPORT [STD]
    LET $loud: STRING = TEXT_TO_UPPER("hello")
    ASSERT_EQ $loud "HELLO"
    "###
)]
#[example(
    name = "uppercase pipe mirrors",
    code = r###"
    # Pipe input mints pipe output with the same fold.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "hello"
    }
    LET $out: PIPE = TEXT_TO_UPPER($cap)
    WITH_IO [stdin=$out] READ_LINE $line
    ASSERT_EQ $line "HELLO"
    "###
)]
#[example(
    name = "uppercase template",
    code = r###"
    # String inputs run inside placeholders too.
    IMPORT [STD]
    LET $msg: STRING = 'loud:{{ STD::TEXT_TO_UPPER("hello") }}'
    ASSERT_EQ $msg "loud:HELLO"
    "###
)]
fn text_to_upper<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Text to uppercase.
    #[union(String, Pipe)]
    text: Value,
) -> Result<Value> {
    super::args::mirror_stream(cx, "TEXT_TO_UPPER", &text, super::args::TextPump::ToUpper)
}

/// Template projection of `TEXT_TO_UPPER`: the same fold for `{{ }}`
/// placeholders, which carry no step IO for pipes.
fn text_to_upper_string(text: Value) -> Result<Value> {
    super::args::mirror_string("TEXT_TO_UPPER", &text, super::args::TextPump::ToUpper)
}

/// Lowercase text with full Unicode mapping.
///
/// Accepts `STRING` or `PIPE`; the return mirrors the input kind.
/// Pipes map chunk by chunk across a char-boundary carry, like
/// `TEXT_TO_UPPER`.
///
#[oxdock_func(
    rpn,
    returns = TypeTag::union(vec![TypeTag::String, TypeTag::Pipe]),
    template = text_to_lower_string
)]
#[example(
    name = "lowercase",
    code = r###"
    # Text folds to lowercase.
    IMPORT [STD]
    LET $quiet: STRING = TEXT_TO_LOWER("HELLO")
    ASSERT_EQ $quiet "hello"
    "###
)]
#[example(
    name = "lowercase pipe mirrors",
    code = r###"
    # Pipe input mints pipe output with the same fold.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "HELLO"
    }
    LET $out: PIPE = TEXT_TO_LOWER($cap)
    WITH_IO [stdin=$out] READ_LINE $line
    ASSERT_EQ $line "hello"
    "###
)]
#[example(
    name = "lowercase template",
    code = r###"
    # String inputs run inside placeholders too.
    IMPORT [STD]
    LET $msg: STRING = 'quiet:{{ STD::TEXT_TO_LOWER("HELLO") }}'
    ASSERT_EQ $msg "quiet:hello"
    "###
)]
fn text_to_lower<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Text to lowercase.
    #[union(String, Pipe)]
    text: Value,
) -> Result<Value> {
    super::args::mirror_stream(cx, "TEXT_TO_LOWER", &text, super::args::TextPump::ToLower)
}

/// Template projection of `TEXT_TO_LOWER`: the same fold for `{{ }}`
/// placeholders, which carry no step IO for pipes.
fn text_to_lower_string(text: Value) -> Result<Value> {
    super::args::mirror_string("TEXT_TO_LOWER", &text, super::args::TextPump::ToLower)
}

/// Replace every literal occurrence of `from` with `to`.
///
/// Accepts `STRING` or `PIPE`; the return mirrors the input kind.
/// Pipes scan through a `from.len()` sliding window, so needles split
/// across chunk reads still rewrite.
/// Literal sliding search, not a pattern: an empty `from` fails
/// closed, and regex work with captures belongs to
/// `TEXT_REPLACE_REGEX`. Matching is case-sensitive; there is no
/// case-insensitive mode.
///
#[oxdock_func(
    rpn,
    returns = TypeTag::union(vec![TypeTag::String, TypeTag::Pipe]),
    template = text_replace_string
)]
#[example(
    name = "replace",
    code = r###"
    # Every literal occurrence rewrites in place.
    IMPORT [STD]
    LET $fixed: STRING = TEXT_REPLACE("hello world", "world", "there")
    ASSERT_EQ $fixed "hello there"
    "###
)]
#[example(
    name = "replace pipe mirrors",
    code = r###"
    # Pipe input mints pipe output with the same rewrite.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "hello world"
    }
    LET $trimmed: PIPE = TEXT_TRIM($cap)
    LET $out: PIPE = TEXT_REPLACE($trimmed, "world", "there")
    WITH_IO [stdin=$out] READ_LINE $line
    ASSERT_EQ $line "hello there"
    "###
)]
#[example(
    name = "replace template",
    code = r###"
    # String inputs run inside placeholders too.
    IMPORT [STD]
    LET $msg: STRING = 'fixed:{{ STD::TEXT_REPLACE("hello world", "world", "there") }}'
    ASSERT_EQ $msg "fixed:hello there"
    "###
)]
fn text_replace<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Text to rewrite.
    #[union(String, Pipe)]
    text: Value,
    /// Literal text to search for; empty fails closed.
    from: String,
    /// Literal replacement text.
    to: String,
) -> Result<Value> {
    super::args::mirror_stream(
        cx,
        "TEXT_REPLACE",
        &text,
        super::args::TextPump::ReplaceLiteral { from, to },
    )
}

/// Template projection of `TEXT_REPLACE`: the same rewrite for `{{ }}`
/// placeholders, which carry no step IO for pipes.
fn text_replace_string(text: Value, from: String, to: String) -> Result<Value> {
    super::args::mirror_string(
        "TEXT_REPLACE",
        &text,
        super::args::TextPump::ReplaceLiteral { from, to },
    )
}

/// Replace every regex match, expanding `$1` and `$name` captures.
///
/// Accepts `STRING` or `PIPE`; the return mirrors the input kind.
/// Eager-only framing: matches span chunks, so pipe inputs stage
/// through the spill substrate instead of streaming.
/// Bad patterns fail closed naming the pattern and its position.
/// Matching is case-sensitive unless the pattern carries an inline
/// `(?i)` flag, which makes uppercase and lowercase letters match
/// each other.
/// The literal counterpart is `TEXT_REPLACE`.
///
#[oxdock_func(
    rpn,
    returns = TypeTag::union(vec![TypeTag::String, TypeTag::Pipe]),
    template = text_replace_regex_string
)]
#[example(
    name = "replace with captures",
    code = r###"
    # Capture groups expand in the replacement.
    IMPORT [STD]
    LET $swapped: STRING = TEXT_REPLACE_REGEX("2026-10-13", "(\\d+)-(\\d+)-(\\d+)", "$3/$2/$1")
    ASSERT_EQ $swapped "13/10/2026"
    "###
)]
#[example(
    name = "replace regex pipe mirrors",
    code = r###"
    # Pipe input mints pipe output with the same rewrite.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "2026-10-13"
    }
    LET $trimmed: PIPE = TEXT_TRIM($cap)
    LET $out: PIPE = TEXT_REPLACE_REGEX($trimmed, "(\\d+)-(\\d+)-(\\d+)", "$3/$2/$1")
    WITH_IO [stdin=$out] READ_LINE $line
    ASSERT_EQ $line "13/10/2026"
    "###
)]
#[example(
    name = "replace regex template",
    code = r###"
    # String inputs run inside placeholders too.
    IMPORT [STD]
    LET $msg: STRING = 'swapped:{{ STD::TEXT_REPLACE_REGEX("2026-10-13", "(\\d+)-(\\d+)-(\\d+)", "$3/$2/$1") }}'
    ASSERT_EQ $msg "swapped:13/10/2026"
    "###
)]
fn text_replace_regex<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Text to rewrite.
    #[union(String, Pipe)]
    text: Value,
    /// Regex pattern to match.
    pattern: String,
    /// Replacement with `$1` and `$name` capture expansion.
    replacement: String,
) -> Result<Value> {
    if let Some(s) = text.as_str() {
        let out =
            super::args::text_replace_regex_str(s, &pattern, &replacement, "TEXT_REPLACE_REGEX")?;
        return Ok(Value::string(out));
    }
    let content = super::args::read_pipe_eager(cx, "TEXT_REPLACE_REGEX", &text)?;
    let out = super::args::text_replace_regex_str(
        &content,
        &pattern,
        &replacement,
        "TEXT_REPLACE_REGEX",
    )?;
    super::args::write_completed_pipe(cx, "TEXT_REPLACE_REGEX", out)
}

/// Template projection of `TEXT_REPLACE_REGEX`: the same rewrite for
/// `{{ }}` placeholders, which carry no step IO for pipes.
fn text_replace_regex_string(text: Value, pattern: String, replacement: String) -> Result<Value> {
    let Some(s) = text.as_str() else {
        return Err(super::args::pipe_needs_step_io("TEXT_REPLACE_REGEX"));
    };
    let out = super::args::text_replace_regex_str(s, &pattern, &replacement, "TEXT_REPLACE_REGEX")?;
    Ok(Value::string(out))
}

/// Split text on a literal delimiter into a `LIST<STRING>`.
///
/// An empty delimiter splits into single characters. Eager-only by
/// design: splitting into a list has no line framing to stream.
/// Delimiters match case-sensitively; there is no
/// case-insensitive mode.
///
#[oxdock_func(pure, returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "split",
    code = r###"
    # Delimiters split into list items.
    IMPORT [STD]
    LET $parts: LIST<STRING> = TEXT_SPLIT("a,b,c", ",")
    ASSERT_EQ $parts.0 "a"
    ASSERT_EQ $parts.1 "b"
    ASSERT_EQ $parts.2 "c"
    "###
)]
fn text_split(
    /// Text to split.
    text: String,
    /// Literal delimiter; empty splits into characters.
    delimiter: String,
) -> Result<Value> {
    Ok(super::args::text_list_value(super::args::text_split_str(
        &text, &delimiter,
    )))
}

/// Join a `LIST<STRING>` with a literal separator into a `STRING`.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "join",
    code = r###"
    # List items join with the separator between them.
    IMPORT [STD]
    LET $joined: STRING = TEXT_JOIN(["a", "b", "c"], ",")
    ASSERT_EQ $joined "a,b,c"
    "###
)]
fn text_join(
    /// List elements to join.
    elements: Vec<String>,
    /// Literal delimiter placed between elements.
    delimiter: String,
) -> Result<Value> {
    Ok(Value::string(elements.join(delimiter.as_str())))
}

/// Split a shell command string into argv words.
///
/// Parses POSIX shell word syntax (single and double quotes, backslash
/// escapes) with the `shell-words` crate, the same splitter the remote
/// plugins use. Returns a `LIST<STRING>`. Unbalanced quotes fail.
/// Inverse of `SHELL_JOIN_ARGS`: joining the result re-splits
/// to the same words.
///
/// Shell syntax only: plain prose with no quoting splits on
/// whitespace like any shell would, which is rarely what prose wants.
///
#[oxdock_func(pure, returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "split args",
    code = r###"
    # Quoted segments stay one word.
    IMPORT [STD]
    LET $argv: LIST<STRING> = SHELL_SPLIT_ARGS("gh issue comment 153 --body-file notes.md")
    ASSERT_EQ $argv.0 "gh"
    ASSERT_EQ $argv.3 "153"
    ASSERT_EQ $argv.5 "notes.md"

    # Quotes and escapes parse the shell way.
    LET $quoted: LIST<STRING> = SHELL_SPLIT_ARGS("say \"hi there\" 'a b'")
    ASSERT_EQ $quoted.0 "say"
    ASSERT_EQ $quoted.1 "hi there"
    ASSERT_EQ $quoted.2 "a b"
    "###
)]
fn shell_split_args(
    /// Shell command string to split into words.
    command: String,
) -> Result<Value> {
    match shell_words::split(&command) {
        Ok(words) => Ok(super::args::text_list_value(words)),
        Err(err) => Err(anyhow::Error::new(std_diag::std_shell_split_failed(&err))),
    }
}

/// Join argv words into one shell-parseable string.
///
/// Quotes each word with POSIX single-quote escaping through the
/// `shell-words` crate, so the result re-splits to the same words.
/// Inverse of `SHELL_SPLIT_ARGS` on the word side: splitting the
/// result returns the input list unchanged.
///
#[oxdock_func(pure, returns = TypeTag::String)]
#[example(
    name = "join args",
    code = r###"
    # Plain words join with spaces and no quoting.
    IMPORT [STD]
    LET $plain: STRING = SHELL_JOIN_ARGS(["gh", "issue", "list"])
    ASSERT_EQ $plain "gh issue list"

    # Words with spaces quote, and the round trip is exact.
    LET $words: LIST<STRING> = ["say", "hi there", "a'b"]
    LET $joined: STRING = SHELL_JOIN_ARGS($words)
    LET $back: LIST<STRING> = SHELL_SPLIT_ARGS($joined)
    ASSERT_EQ $back.0 "say"
    ASSERT_EQ $back.1 "hi there"
    ASSERT_EQ $back.2 "a'b"
    "###
)]
fn shell_join_args(
    /// Words to join into a shell command string.
    words: Vec<String>,
) -> Result<Value> {
    Ok(Value::string(shell_words::join(&words)))
}

/// Split text into lines with universal newline handling.
///
/// Normalizes through the line-ending crate first so LF, CRLF, CR,
/// and mixed endings all split, then delegates the split itself to
/// `LineEnding::split` instead of hand-rolled scanning.
/// Returns a `LIST<STRING>`; eager-only like `TEXT_SPLIT`.
///
#[oxdock_func(pure, returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "lines",
    code = r###"
    # Newlines split into line items.
    IMPORT [STD]
    LET $lines: LIST<STRING> = TEXT_LINES("one\ntwo\nthree")
    ASSERT_EQ $lines.0 "one"
    ASSERT_EQ $lines.1 "two"
    ASSERT_EQ $lines.2 "three"

    # Windows endings split the same way after normalization.
    LET $crlf: LIST<STRING> = TEXT_LINES("one\r\ntwo\r\nthree")
    ASSERT_EQ $crlf.0 "one"
    ASSERT_EQ $crlf.1 "two"
    ASSERT_EQ $crlf.2 "three"

    # Old Mac endings split too, and mixed endings split together because the input normalizes to LF before splitting.
    LET $mixed: LIST<STRING> = TEXT_LINES("a\r\nb\nc\rd")
    ASSERT_EQ $mixed.0 "a"
    ASSERT_EQ $mixed.1 "b"
    ASSERT_EQ $mixed.2 "c"
    ASSERT_EQ $mixed.3 "d"
    "###
)]
fn text_lines(
    /// Text to split into lines.
    text: String,
) -> Result<Value> {
    Ok(super::args::text_list_value(super::args::text_lines_str(
        &text,
    )))
}

/// Split text into whitespace-separated fields, ignoring empties.
///
/// Returns a `LIST<STRING>`; the checksum-line parser that replaces
/// `cut -d' ' -f1` in CI legs.
///
#[oxdock_func(pure, returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "fields",
    code = r###"
    # Runs of whitespace separate fields; empties never appear.
    IMPORT [STD]
    LET $fields: LIST<STRING> = TEXT_FIELDS("  abc  def ")
    ASSERT_EQ $fields.0 "abc"
    ASSERT_EQ $fields.1 "def"
    "###
)]
fn text_fields(
    /// Text to split into fields.
    text: String,
) -> Result<Value> {
    Ok(super::args::text_list_value(super::args::text_fields_str(
        &text,
    )))
}

/// Read lines `from` through `to` as a `LIST<STRING>`: the `sed -n
/// 'A,Bp'` window over universal newlines, for reading one file
/// section without holding the surrounding text.
///
/// Lines are 1-based and inclusive on both ends. Ranges past the end
/// clamp like sed instead of failing; a start before line 1 or past
/// `to` fails closed, since no line reading satisfies it.
///
#[oxdock_func(pure, returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "line range",
    code = r###"
    # Lines 2 through 3 come back as a list, 1-based inclusive.
    IMPORT [STD]
    LET $window: LIST<STRING> = TEXT_LINES_RANGE("one\ntwo\nthree\nfour", 2, 3)
    ASSERT_EQ $window.0 "two"
    ASSERT_EQ $window.1 "three"

    # Ranges past the end clamp instead of failing.
    LET $tail: LIST<STRING> = TEXT_LINES_RANGE("one\ntwo", 2, 9)
    ASSERT_EQ $tail.0 "two"
    "###
)]
fn text_lines_range(
    /// Text whose lines to window.
    text: String,
    /// First line, 1-based inclusive.
    from: i64,
    /// Last line, inclusive; past the end clamps.
    to: i64,
) -> Result<Value> {
    Ok(super::args::text_list_value(
        super::args::text_lines_range_str(&text, from, to, "TEXT_LINES_RANGE")?,
    ))
}

/// Report whether text matches a regex pattern.
///
/// Returns `BOOL`. Bad patterns fail closed naming the pattern and
/// its position. Matching is case-sensitive unless the pattern
/// carries an inline `(?i)` flag, which makes uppercase and lowercase
/// letters match each other.
///
#[oxdock_func(pure, returns = TypeTag::Bool)]
#[example(
    name = "match",
    code = r###"
    # Regex matches report true; misses report false.
    IMPORT [STD]
    LET $hit: BOOL = TEXT_MATCH("abc123", "\\d+")
    ASSERT_EQ $hit true

    # A miss is false, not a failure.
    LET $miss: BOOL = TEXT_MATCH("abc", "^\\d+$")
    ASSERT_EQ $miss false
    "###
)]
fn text_match(
    /// Text to test.
    text: String,
    /// Regex pattern to match.
    pattern: String,
) -> Result<Value> {
    let re = super::args::text_compile_pattern("TEXT_MATCH", &pattern)?;
    Ok(Value::bool(re.is_match(&text)))
}

/// Convert a value to BOOL.
///
/// Trims ASCII whitespace; accepts `true`/`false`, `1`/`0`,
/// `yes`/`no`, `on`/`off` in any case. Passes Bool through. Empty
/// and anything else fail: an empty string is not a boolean, so
/// absent env names (which read empty) must go through `BOOL_OR`
/// with a stated default instead of converting here.
///
#[oxdock_func(pure, name = "BOOL", returns = TypeTag::Bool)]
#[example(
    name = "convert",
    code = r###"
    IMPORT [STD]
    LET $n: BOOL = BOOL("yes")
    ASSERT_EQ $n true
    LET $f: BOOL = BOOL("0")
    ASSERT_EQ $f false
    "###
)]
fn bool_convert(
    /// Value to convert to `BOOL`.
    #[union(Bool, String)]
    val: Value,
) -> Result<Value> {
    if let Some(b) = val.as_bool() {
        return Ok(Value::bool(b));
    }
    let Some(s) = val.as_str() else {
        return Err(anyhow::Error::new(std_diag::std_bool_needs_bool_or_string(
            &val,
        )));
    };
    match super::args::parse_bool_str(s) {
        Some(b) => Ok(Value::bool(b)),
        None => Err(anyhow::Error::new(std_diag::std_bool_cannot_coerce(
            FUNC_BOOL, s,
        ))),
    }
}

/// Coerce text to `BOOL`, substituting a default for empty.
///
/// Returns `BOOL`. Blank input (a declared-but-absent env name, which
/// reads empty) resolves to `default` instead of failing, so each flag
/// site states its own absent polarity explicitly: negative-polarity
/// flags (`dry_run`) default true, positive ones default false. No
/// implicit polarity lives in the coercion. Non-blank garbage still
/// fails naming the input, exactly like `LET $x: BOOL` coercion.
///
#[oxdock_func(pure, returns = TypeTag::Bool)]
#[example(
    name = "absent-default",
    code = r###"
    # Absent names read empty; the default settles the polarity.
    IMPORT [STD]
    INHERIT_ENV [OXDOCK_TEST_ABSENT_FLAG]
    LET $dry: BOOL = BOOL_OR("{{ env:OXDOCK_TEST_ABSENT_FLAG }}", true)
    ASSERT_EQ $dry true

    # Present values coerce strictly, ignoring the default.
    LET $off: BOOL = BOOL_OR("false", true)
    ASSERT_EQ $off false
    "###
)]
fn bool_or(
    /// Text to coerce (`true`/`false`, `1`/`0`, `yes`/`no`/`on`/`off`, any case).
    text: String,
    /// Absent-value default, used when the text is blank.
    default: bool,
) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::bool(default));
    }
    match super::args::parse_bool_str(&text) {
        Some(b) => Ok(Value::bool(b)),
        None => Err(anyhow::Error::new(std_diag::std_bool_cannot_coerce(
            FUNC_BOOL_OR,
            &text,
        ))),
    }
}

/// Extract capture groups for the first regex match.
///
/// Returns a `LIST<STRING>` where index 0 is the full match and the
/// rest are groups in order; non-participating groups bind empty so
/// indexes stay stable. No match binds an empty list. Matching is
/// case-sensitive unless the pattern carries an inline `(?i)` flag,
/// which makes uppercase and lowercase letters match each other.
///
#[oxdock_func(pure, returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "extract",
    code = r###"
    # Index zero is the full match; groups follow in order.
    IMPORT [STD]
    LET $caps: LIST<STRING> = TEXT_EXTRACT("abc123", "([a-z]+)(\\d+)")
    ASSERT_EQ $caps.0 "abc123"
    ASSERT_EQ $caps.1 "abc"
    ASSERT_EQ $caps.2 "123"
    "###
)]
fn text_extract(
    /// Text to search.
    text: String,
    /// Regex pattern with capture groups.
    pattern: String,
) -> Result<Value> {
    let groups = super::args::text_extract_str(&text, &pattern, "TEXT_EXTRACT")?;
    Ok(super::args::text_list_value(groups))
}

/// Report whether text starts with a literal prefix.
///
/// Returns `BOOL`; the literal check behind tag and target branching
/// such as `matrix.target` suffix and prefix tests. Matching is
/// case-sensitive; there is no case-insensitive mode.
///
#[oxdock_func(pure, returns = TypeTag::Bool)]
#[example(
    name = "starts with",
    code = r###"
    # Present prefixes report true; misses report false.
    IMPORT [STD]
    LET $hit: BOOL = TEXT_STARTS_WITH("linux-musl-x64", "linux")
    ASSERT_EQ $hit true

    # A miss is false, not a failure.
    LET $miss: BOOL = TEXT_STARTS_WITH("macos-arm", "linux")
    ASSERT_EQ $miss false
    "###
)]
fn text_starts_with(
    /// Text to test.
    text: String,
    /// Literal prefix to look for.
    prefix: String,
) -> Result<Value> {
    Ok(Value::bool(text.starts_with(prefix.as_str())))
}

/// Report whether text ends with a literal suffix.
///
/// Returns `BOOL`; the `endsWith(matrix.target, 'linux-musl')`
/// check in CI legs. Matching is case-sensitive; there is no
/// case-insensitive mode.
///
#[oxdock_func(pure, returns = TypeTag::Bool)]
#[example(
    name = "ends with",
    code = r###"
    # Present suffixes report true; misses report false.
    IMPORT [STD]
    LET $hit: BOOL = TEXT_ENDS_WITH("x86_64-linux-musl", "linux-musl")
    ASSERT_EQ $hit true

    # A miss is false, not a failure.
    LET $miss: BOOL = TEXT_ENDS_WITH("x86_64-linux-gnu", "linux-musl")
    ASSERT_EQ $miss false
    "###
)]
fn text_ends_with(
    /// Text to test.
    text: String,
    /// Literal suffix to look for.
    suffix: String,
) -> Result<Value> {
    Ok(Value::bool(text.ends_with(suffix.as_str())))
}

/// Report whether a `STRING` holds a substring or a `LIST` holds a member.
///
/// Dispatches on the haystack like `LEN`: `STRING` does a literal
/// substring search (case-sensitive, no case-insensitive mode);
/// `LIST<ANY>` compares members with strict `values_equal` (numeric
/// `INT` and `FLOAT` compare across types, every other cross-type pair
/// is unequal, matching `SWITCH`). Returns `BOOL`; misses are false,
/// never failures.
///
#[oxdock_func(pure, returns = TypeTag::Bool)]
#[example(
    name = "contains",
    code = r###"
    # Present needles report true; misses report false.
    IMPORT [STD]
    LET $hit: BOOL = CONTAINS("x86_64-linux-musl", "musl")
    ASSERT_EQ $hit true

    # A miss is false, not a failure.
    LET $miss: BOOL = CONTAINS("x86_64-linux-gnu", "musl")
    ASSERT_EQ $miss false
    "###
)]
#[example(
    name = "contains list",
    code = r###"
    # List membership uses strict equality across any element type.
    IMPORT [STD]
    LET $hit: BOOL = CONTAINS(["a", "b", "c"], "b")
    ASSERT_EQ $hit true
    LET $miss: BOOL = CONTAINS(["a", "b", "c"], "z")
    ASSERT_EQ $miss false
    LET $num: BOOL = CONTAINS([1, 2, 3], 2.0)
    ASSERT_EQ $num true

    # Stringified numbers never coerce to integers.
    LET $strnum: BOOL = CONTAINS(["1", "2", "3"], 2)
    ASSERT_EQ $strnum false

    # Boolean lists match true and miss false with no coercion.
    LET $bools: LIST<BOOL> = [true]
    LET $bhit: BOOL = CONTAINS($bools, true)
    ASSERT_EQ $bhit true
    LET $bmiss: BOOL = CONTAINS($bools, false)
    ASSERT_EQ $bmiss false
    LET $nocoerce: BOOL = CONTAINS($bools, 1)
    ASSERT_EQ $nocoerce false
    "###
)]
fn contains(
    /// Text to search or list to probe.
    #[union(String, List)]
    haystack: Value,
    /// Literal substring or member to look for.
    needle: Value,
) -> Result<Value> {
    if let Some(text) = haystack.as_str() {
        let Some(n) = needle.as_str() else {
            return Err(anyhow::Error::new(std_diag::std_contains_needle_type(
                needle.type_name(),
            )));
        };
        return Ok(Value::bool(text.contains(n)));
    }
    if let Some(items) = haystack.as_list() {
        for item in items {
            if super::args::values_equal(item, &needle) {
                return Ok(Value::bool(true));
            }
        }
        return Ok(Value::bool(false));
    }
    Err(anyhow::Error::new(std_diag::std_contains_haystack_type(
        haystack.type_name(),
    )))
}

/// Deduplicate a `LIST`, keeping the first occurrence of each member.
///
/// Membership uses strict `values_equal` (numeric `INT` and `FLOAT`
/// compare across types, every other cross-type pair is unequal,
/// matching `CONTAINS` and `==`), so `[1, 1.0]` collapses to one
/// element while `["1", 1]` keeps both. Order is first occurrence;
/// the empty list binds empty, never failing.
#[oxdock_func(pure, returns = TypeTag::List)]
#[example(
    name = "deduplicate",
    code = r###"
    # First occurrences win, later repeats drop.
    IMPORT [STD]
    LET $seen: LIST<STRING> = UNIQUE(["a", "b", "a", "c", "b"])
    ASSERT_EQ $seen.0 "a"
    ASSERT_EQ $seen.1 "b"
    ASSERT_EQ $seen.2 "c"
    ASSERT_EQ LEN($seen) 3

    # The empty list stays empty.
    LET $empty: LIST<STRING> = UNIQUE([])
    ASSERT_EQ LEN($empty) 0
    "###
)]
#[example(
    name = "strict membership",
    code = r###"
    # Numerics collapse across INT and FLOAT like CONTAINS.
    IMPORT [STD]
    LET $nums: LIST<ANY> = UNIQUE([1, 2.0, 1, 3])
    ASSERT_EQ $nums.0 1
    ASSERT_EQ $nums.1 2.0
    ASSERT_EQ $nums.2 3
    ASSERT_EQ LEN($nums) 3

    # Stringified numbers never coerce to integers.
    LET $mixed: LIST<ANY> = UNIQUE(["1", 1])
    ASSERT_EQ $mixed.0 "1"
    ASSERT_EQ $mixed.1 1
    ASSERT_EQ LEN($mixed) 2
    "###
)]
fn unique(
    /// List to deduplicate.
    items: Vec<Value>,
) -> Result<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(items.len());
    for item in items {
        if !out
            .iter()
            .any(|seen| super::args::values_equal(seen, &item))
        {
            out.push(item);
        }
    }
    Ok(Value::list(out))
}

/// Count the elements of a text, list, or map as an `INT`.
///
/// Strings count Unicode scalars: multibyte characters count once
/// each, so this is not the byte length (`"héllo"` binds 5, not 6).
/// Lists count elements, maps count entries; empty values bind 0.
/// The `${#var}` and `wc -m` counterpart for scripts that size tags,
/// digests, and table rows.
///
#[oxdock_func(pure, returns = TypeTag::Int)]
#[example(
    name = "length",
    code = r###"
    # Plain text counts one per character.
    IMPORT [STD]
    LET $n: INT = LEN("hello")
    ASSERT_EQ $n 5

    # Multibyte characters count once each, not per byte.
    LET $wide: INT = LEN("héllo")
    ASSERT_EQ $wide 5

    # Lists count elements, maps count entries.
    LET $items: LIST<STRING> = ["a", "b", "c"]
    LET $nitems: INT = LEN($items)
    ASSERT_EQ $nitems 3
    LET $row: MAP<name: STRING> = {name: "ada"}
    LET $nkeys: INT = LEN($row)
    ASSERT_EQ $nkeys 1
    LET $empty: INT = LEN([])
    ASSERT_EQ $empty 0
    "###
)]
fn len(
    /// Value to measure.
    #[union(String, List, Map)]
    value: Value,
) -> Result<Value> {
    if let Some(text) = value.as_str() {
        return Ok(Value::int(text.chars().count() as i64));
    }
    if let Some(items) = value.as_list() {
        return Ok(Value::int(items.len() as i64));
    }
    if let Some(entries) = value.as_map() {
        return Ok(Value::int(entries.len() as i64));
    }
    Err(anyhow::Error::new(std_diag::std_len_unexpected_type(
        value.type_name(),
    )))
}

/// Count non-overlapping literal occurrences of a needle.
///
/// Matches never share bytes, so `"aaa"` holds one `"aa"`, matching
/// `grep -o 'aa' | wc -l`. An empty needle fails closed instead of
/// binding every character boundary. Matching is case-sensitive;
/// there is no case-insensitive mode.
///
#[oxdock_func(pure, returns = TypeTag::Int)]
#[example(
    name = "count",
    code = r###"
    # Every non-overlapping occurrence counts once.
    IMPORT [STD]
    LET $n: INT = TEXT_COUNT("hello world", "o")
    ASSERT_EQ $n 2

    # Overlapping candidates never double count.
    LET $once: INT = TEXT_COUNT("aaa", "aa")
    ASSERT_EQ $once 1
    "###
)]
fn text_count(
    /// Text to search.
    text: String,
    /// Literal needle to count; empty fails closed.
    needle: String,
) -> Result<Value> {
    let n = super::args::text_count_str(&text, &needle, "TEXT_COUNT")?;
    Ok(Value::int(n))
}

/// Count lines matching a regex pattern.
///
/// Lines split with universal newline handling, like `TEXT_LINES`,
/// and each line reports independently: this is `grep -c`, so a line
/// with two matches still counts once. Bad patterns fail closed
/// naming the pattern and its position. Matching is case-sensitive
/// unless the pattern carries an inline `(?i)` flag, which makes
/// uppercase and lowercase letters match each other.
///
#[oxdock_func(pure, returns = TypeTag::Int)]
#[example(
    name = "count matches",
    code = r###"
    # One line matches once no matter how many times it hits.
    IMPORT [STD]
    LET $n: INT = TEXT_COUNT_MATCHES("test a: test\ntest b: nope\ntest c: test", ": test")
    ASSERT_EQ $n 2

    # A miss binds zero, never an error.
    LET $none: INT = TEXT_COUNT_MATCHES("test b: nope", ": test$")
    ASSERT_EQ $none 0
    "###
)]
fn text_count_matches(
    /// Text whose lines to test.
    text: String,
    /// Regex pattern tested per line.
    pattern: String,
) -> Result<Value> {
    let n = super::args::text_count_matches_str(&text, &pattern, "TEXT_COUNT_MATCHES")?;
    Ok(Value::int(n))
}

/// Keep the lines matching a regex pattern.
///
/// Splits with universal newline handling, like `TEXT_LINES`, and
/// returns every matching line in order: plain `grep`, so a miss
/// binds an empty list instead of failing. A trailing options MAP
/// carries `invert`: `TEXT_GREP($text, $pattern, {invert: true})`
/// keeps every non-matching line instead, the `grep -v` shape. The
/// MAP omits freely; a missing MAP keeps matches. Matching is
/// case-sensitive unless the pattern carries an inline `(?i)` flag,
/// which makes uppercase and lowercase letters match each other. Bad
/// patterns fail closed naming the pattern and its position.
///
#[oxdock_func(pure, returns = TypeTag::ListOf(std::sync::Arc::new(TypeTag::String)))]
#[example(
    name = "grep",
    code = r###"
    # Matching lines come back as a list, in order.
    IMPORT [STD]
    LET $hits: LIST<STRING> = TEXT_GREP("one\ntwo\none more", "^one")
    ASSERT_EQ $hits.0 "one"
    ASSERT_EQ $hits.1 "one more"

    # A miss binds an empty list, never an error.
    LET $miss: LIST<STRING> = TEXT_GREP("one\ntwo", "^z")
    LET $joined: STRING = TEXT_JOIN($miss, ",")
    ASSERT_EQ $joined ""

    # The (?i) flag ignores case, so uppercase lines match too.
    LET $folded: LIST<STRING> = TEXT_GREP("One\ntwo\nONE more", "(?i)^one")
    ASSERT_EQ $folded.0 "One"
    ASSERT_EQ $folded.1 "ONE more"
    "###
)]
#[example(
    name = "grep invert",
    code = r###"
    # The invert option keeps every non-matching line instead.
    IMPORT [STD]
    LET $kept: LIST<STRING> = TEXT_GREP("one\ntwo\none more", "^one", {invert: true})
    ASSERT_EQ $kept.0 "two"

    # A pattern matching every line binds an empty list, never an error.
    LET $none: LIST<STRING> = TEXT_GREP("one\ntwo", "^", {invert: true})
    LET $joined: STRING = TEXT_JOIN($none, ",")
    ASSERT_EQ $joined ""

    # Options ride into placeholders as MAP literals. Placeholder
    # strings unescape only backslash and quote, so multiline text
    # arrives through a variable.
    LET $two_lines: STRING = "one\ntwo"
    LET $msg: STRING = 'kept:{{ STD::TEXT_GREP($two_lines, "^one", {invert: true}) }}'
    ASSERT_EQ $msg "kept:two"
    "###
)]
fn text_grep(
    /// Text whose lines to filter.
    text: String,
    /// Regex pattern keeping a line.
    pattern: String,
    /// Filter settings: `invert`. Omittable: a missing options MAP fills `{}`.
    #[options("invert?: BOOL")]
    #[optional]
    options: BTreeMap<String, Value>,
) -> Result<Value> {
    // `check_options` in the wrapper verified the shape above (keys
    // and value types); the read below narrows infallibly after.
    let invert = super::args::text_invert_flag(&options);
    Ok(super::args::text_list_value(super::args::text_grep_str(
        &text,
        &pattern,
        invert,
        "TEXT_GREP",
    )?))
}

/// True when a pipe sits at end of stream: closed with nothing buffered,
/// so the next `READ_LINE` would bind `""` via EOF rather than a line.
/// Live writers, pinned keepers, and buffered bytes all answer false.
/// Never blocks: a reader already blocked stays blocked, so branch on
/// `EOF` before reading, not after. OS pairs and unbound handles answer
/// best-effort (kernel bytes are invisible there; see `INSPECT`).
/// Non-pipe arguments bail at the boundary: the parameter declares
/// `PIPE`, so the extractor and the static pass reject them before
/// the body runs.
///
#[oxdock_func(returns = TypeTag::Bool)]
#[example(
    name = "drain to end",
    code = r###"
    # Capture two lines, then drain to end of stream with no sentinel line.
    IMPORT [STD]
    LET $cap: PIPE
    WITH_IO [stdout=$cap] {
        ECHO "one"
        ECHO "two"
    }

    # The loop exits on close, and the count proves both lines drained.
    LET $n: INT = 0
    WHILE !EOF($cap) {
        WITH_IO [stdin=$cap] READ_LINE $line
        $n = $n + 1
    }
    ASSERT_EQ $n 2
    "###
)]
fn eof<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Pipe handle to query for end of stream.
    pipe: PipeHandle,
) -> Result<Value> {
    let info = cx.state.io.inspect_pipe(&pipe);
    Ok(Value::bool(info.closed && info.buffered == 0))
}

/// Field table for the `TASK_POLL` result map. Shared by the declared
/// return tag and the generated schema and reference docs, so the
/// shape agents see is the shape the builtin builds.
fn task_poll_result_fields() -> Arc<[Field]> {
    Arc::from(vec![
        Field {
            name: "state".to_string(),
            ty: TypeTag::String,
            docs: "Task phase: `running` or `finished`.",
            optional: false,
            default: None,

            alias: None,
        },
        Field {
            name: "ok".to_string(),
            ty: TypeTag::Bool,
            docs: "True unless the task failed.",
            optional: false,
            default: None,

            alias: None,
        },
        Field {
            name: "stdout_tail".to_string(),
            ty: TypeTag::String,
            docs: "Bounded stdout tail, live while running.",
            optional: false,
            default: None,

            alias: None,
        },
        Field {
            name: "stderr_tail".to_string(),
            ty: TypeTag::String,
            docs: "Bounded stderr tail, live while running.",
            optional: false,
            default: None,

            alias: None,
        },
        Field {
            name: "truncated_bytes".to_string(),
            ty: TypeTag::Int,
            docs: "Dropped head bytes past the per-stream cap.",
            optional: false,
            default: None,

            alias: None,
        },
        Field {
            name: "exit_code".to_string(),
            ty: TypeTag::Int,
            docs: "Process exit code, present once finished.",
            optional: true,
            default: None,

            alias: None,
        },
        Field {
            name: "result".to_string(),
            ty: TypeTag::Any,
            docs: "Task `RETURN` value, present on clean finish.",
            optional: true,
            default: None,

            alias: None,
        },
        Field {
            name: "error".to_string(),
            ty: TypeTag::String,
            docs: "Failure text, present on failed finish.",
            optional: true,
            default: None,

            alias: None,
        },
    ])
}

/// Poll a background task without consuming it.
///
/// Reports the running state, the output tails accumulated so far,
/// and the final outcome once finished. Repeated polls are safe, and
/// a later `AWAIT` still joins normally. Polling after `AWAIT` fails
/// like double-await: the handle is spent.
///
/// Map keys: `state` always names the phase (`"running"` or
/// `"finished"`); `ok` is true unless the task failed; `stdout_tail`
/// and `stderr_tail` carry bounded output with `truncated_bytes`
/// counting dropped head bytes. Finished tasks add `exit_code`;
/// clean finishes add `result`, failed ones add `error` instead.
#[oxdock_func(returns = TypeTag::Record(task_poll_result_fields()))]
#[example(
    name = "running poll",
    code = r###"
    # A sleeping task is still running on the first poll.
    IMPORT [STD]
    LET $t: HANDLE<STRING> = ASYNC { SLEEP 30s; RETURN "done" }
    LET $m: MAP<state: STRING, ok: BOOL, stdout_tail: STRING, stderr_tail: STRING, truncated_bytes: INT, exit_code?: INT, result?: ANY, error?: STRING> = TASK_POLL($t)
    ASSERT_EQ $m.state "running"
    ASSERT_EQ $m.ok true
    # Clean up the sleeper so the run ends promptly.
    CANCEL $t
    "###
)]
#[example(
    name = "finished poll",
    code = r###"
    # A finished task reports its value and final tails, then still awaits.
    IMPORT [STD]
    LET $t: HANDLE<STRING> = ASYNC { ECHO "hi"; SLEEP 1s; RETURN "done" }
    # Do other work while the task runs; poll once it should be done.
    SLEEP 2s
    LET $m: MAP<state: STRING, ok: BOOL, stdout_tail: STRING, stderr_tail: STRING, truncated_bytes: INT, exit_code?: INT, result?: ANY, error?: STRING> = TASK_POLL($t)
    ASSERT_EQ $m.state "finished"
    ASSERT_EQ $m.result "done"
    LET $o: STRING = AWAIT $t
    ASSERT_EQ $o "done"
    "###
)]
fn task_poll<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Task handle from ASYNC.
    task: HandleValue,
) -> Result<Value> {
    let entry =
        super::handlers::resolve_task_entry_by_id(task.0, &format!("TASK_POLL({})", task.0), cx)?;
    let obs = entry.observe_completion();
    let (stdout_tail, stderr_tail, dropped) = entry.snapshot_tails();
    let mut out = BTreeMap::new();
    out.insert(
        "state".to_string(),
        Value::string(if obs.running { "running" } else { "finished" }.to_string()),
    );
    out.insert("ok".to_string(), Value::bool(obs.ok));
    if !obs.running {
        out.insert("exit_code".to_string(), Value::int(obs.exit_code));
    }
    if let Some(result) = obs.result {
        out.insert("result".to_string(), result);
    }
    if !obs.error.is_empty() {
        out.insert("error".to_string(), Value::string(obs.error));
    }
    out.insert("stdout_tail".to_string(), Value::string(stdout_tail));
    out.insert("stderr_tail".to_string(), Value::string(stderr_tail));
    out.insert(
        "truncated_bytes".to_string(),
        Value::int(dropped.min(i64::MAX as u64) as i64),
    );
    Ok(Value::map(out))
}

fn meta_to_value(meta: &FuncMeta) -> Value {
    let mut map = BTreeMap::new();
    map.insert("name".to_string(), Value::string(meta.name.clone()));
    map.insert("module".to_string(), Value::string(meta.module.clone()));
    map.insert(
        "kind".to_string(),
        Value::string(meta.kind.label().to_string()),
    );
    let params = match &meta.params {
        Some(params) => Value::list(
            params
                .iter()
                .map(|p| {
                    let mut entry = BTreeMap::new();
                    entry.insert("name".to_string(), Value::string(p.name.clone()));
                    entry.insert(
                        "param_type".to_string(),
                        Value::string(
                            p.param_type
                                .as_ref()
                                .map(render_structural)
                                .unwrap_or_default(),
                        ),
                    );
                    entry.insert(
                        "allowed".to_string(),
                        Value::string(
                            p.allowed
                                .map(|values| values.join(", "))
                                .unwrap_or_default(),
                        ),
                    );
                    entry.insert("docs".to_string(), Value::string(String::new()));
                    entry.insert("optional".to_string(), Value::bool(p.optional));
                    entry.insert(
                        "options".to_string(),
                        match p.options {
                            Some(keys) => Value::list(
                                keys.iter()
                                    .map(|key| {
                                        let mut option = BTreeMap::new();
                                        option.insert(
                                            "name".to_string(),
                                            Value::string(key.name.to_string()),
                                        );
                                        option.insert(
                                            "value_type".to_string(),
                                            Value::string(render_structural(&key.value)),
                                        );
                                        option.insert(
                                            "required".to_string(),
                                            Value::bool(key.required),
                                        );
                                        option.insert(
                                            "default".to_string(),
                                            Value::string(
                                                key.default.unwrap_or_default().to_string(),
                                            ),
                                        );
                                        Value::map(option)
                                    })
                                    .collect(),
                            ),
                            None => Value::string(String::new()),
                        },
                    );
                    Value::map(entry)
                })
                .collect(),
        ),
        // Parameterless functions mint an empty LIST, never an empty
        // STRING: the `DESCRIBE` return tag promises `LIST<MAP<ANY>>`
        // for every entry, and the boundary enforces it.
        None => Value::list(Vec::new()),
    };
    map.insert("params".to_string(), params);
    map.insert(
        "returns".to_string(),
        Value::string(
            meta.returns
                .as_ref()
                .map(render_structural)
                .unwrap_or_default(),
        ),
    );
    map.insert("rpn".to_string(), Value::bool(meta.rpn));
    map.insert(
        "summary".to_string(),
        Value::string(meta.summary.to_string()),
    );
    Value::map(map)
}

/// One named record schema: a shape host functions produce and scripts
/// bind by name (`LET $s: SSH_SESSION_INFO`). Travels with its module
/// so schemas register exactly when their producer does.
#[derive(Debug, Clone)]
pub struct RecordSchema {
    pub name: &'static str,
    pub fields: std::sync::Arc<[Field]>,
}

/// One host library: functions and types registered under a single module
/// name. `Engine::register_module` stages these; runs expose them as
/// `MODULE::NAME` calls with `MODULE` provenance on every entry.
#[derive(Clone)]
pub struct HostModule<P: ProcessManager> {
    pub name: String,
    pub funcs: Vec<HostRegistration<P>>,
    pub types: Vec<&'static TypeDescriptor>,
    pub record_schemas: Vec<RecordSchema>,
    /// Shared module state (endpoint registries and the like), stashed
    /// at registration and reachable from bodies through
    /// `StepCtx::module_state`. Lets every entry stay `#[oxdock_func]`:
    /// the macro only generates closers-over-nothing, so state that
    /// cannot ride in a value travels here instead of a hand-built
    /// closure. Prefer `.with_state(...)` over spelling this field.
    pub states: Vec<ModuleState>,
}

/// One shared state object traveling with its module. Keyed by the
/// owning module name plus the value type at registration, so two
/// modules sharing one state type never collide.
#[derive(Clone)]
pub struct ModuleState {
    id: std::any::TypeId,
    state: std::sync::Arc<dyn std::any::Any + Send + Sync>,
}

/// A pre-run steps inspection: pure function of the parsed steps,
///
/// shared so the struct field stays under the complexity lint.
pub type PreRunInspector = dyn Fn(&[Step]) -> Result<()> + Send + Sync;

/// A pre-run check riding in a module's `states`: the runner executes
/// every staged check against the parsed steps before the first step
/// runs, so unmapped services fail fast with zero side effects. Modules
/// attach one with `.with_state(Arc::new(check))`; the runner discovers
/// them by type, so no struct field or plugin knowledge is needed here.
#[derive(Clone)]
pub struct PreRunCheck {
    check: Arc<PreRunInspector>,
}

impl PreRunCheck {
    /// Wrap a steps inspector as a pre-run check.
    pub fn new(check: impl Fn(&[Step]) -> Result<()> + Send + Sync + 'static) -> Self {
        Self {
            check: Arc::new(check),
        }
    }

    /// Run the inspection: `Ok` lets execution start.
    pub fn run(&self, steps: &[Step]) -> Result<()> {
        (self.check)(steps)
    }
}

impl<P: ProcessManager> HostModule<P> {
    /// Attach shared state for bodies to fetch with
    /// `cx.module_state::<T>("MODULE")`. Chainable, like registration.
    pub fn with_state<T: Send + Sync + 'static>(mut self, state: std::sync::Arc<T>) -> Self {
        self.states.push(ModuleState {
            id: std::any::TypeId::of::<T>(),
            state,
        });
        self
    }

    /// Run every staged [`PreRunCheck`] against already-parsed steps.
    /// The runner calls this before the first step executes; the first
    /// failing check aborts the run with no side effects.
    pub fn check_pre_run(&self, steps: &[Step]) -> Result<()> {
        for state in &self.states {
            if let Some(check) = state.state.downcast_ref::<PreRunCheck>() {
                check.run(steps)?;
            }
        }
        Ok(())
    }
}

impl<P: ProcessManager> ExecState<P> {
    /// Register one [`HostModule`]: every function becomes callable as
    /// `MODULE::NAME`, every type joins the run's name directory.
    pub fn register_module(&mut self, module: HostModule<P>) {
        for registration in module.funcs {
            match registration {
                HostRegistration::Stateful { name, meta, func } => {
                    self.functions.register_host(&module.name, name, meta, func);
                }
                HostRegistration::Pure { name, meta, func } => {
                    self.functions
                        .register_pure_host(&module.name, name, meta, func);
                }
                HostRegistration::Both {
                    name,
                    meta,
                    func,
                    template,
                } => {
                    self.functions
                        .register_both_host(&module.name, name, meta, func, template);
                }
            }
        }
        for descriptor in module.types {
            self.register_type(descriptor);
        }
        for schema in module.record_schemas {
            self.register_record_schema(schema.name, schema.fields);
        }
        for state in module.states {
            self.module_states
                .insert((module.name.clone(), state.id), state.state);
        }
    }

    /// All visible functions: natives plus hosts plus current DSL definitions.
    pub fn list_functions(&self) -> Vec<FuncMeta> {
        let mut out: Vec<FuncMeta> = self.functions.entries_metas().into_iter().collect();
        // TODO: Make a "virtual function" and don't hardcode
        if !out.iter().any(|m| m.name == KEYWORD_INSPECT) {
            out.push(FuncMeta {
                name: KEYWORD_INSPECT.to_string(),
                module: STD_MODULE_NAME.to_string(),
                kind: FuncKind::HostCtx,
                params: None,
                returns: Some(TypeTag::Map),
                rpn: false,
                summary: "Inspect a variable binding.",
                docs: "INSPECT($var): dedicated AST node taking a variable, not a value.",
                examples: &[],
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Describe one function by name, or `None` when unknown.
    pub fn describe_function(&self, name: &str) -> Option<Value> {
        if let Some(meta) = self.functions.meta(name) {
            return Some(meta_to_value(&meta));
        }
        // TODO: Make a "virtual function" and don't hardcode
        if name == KEYWORD_INSPECT {
            return Some(meta_to_value(&FuncMeta {
                name: KEYWORD_INSPECT.to_string(),
                module: STD_MODULE_NAME.to_string(),
                kind: FuncKind::HostCtx,
                params: None,
                returns: Some(TypeTag::Map),
                rpn: false,
                summary: "Inspect a variable binding.",
                docs: "INSPECT($var): dedicated AST node taking a variable, not a value.",
                examples: &[],
            }));
        }
        None
    }

    /// Shared snapshot of every registered pure function for
    /// placeholder and host driven expansion. Public so host crates
    /// resolve calls through the live registry instead of hardcoding
    /// module or function names. Cloning the `Arc` never copies the map.
    pub fn pure_function_table(&self) -> Arc<PureTable> {
        self.functions.pure_shared()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_resolver_dispatches_table_entries() {
        // Single dispatch implementation for every expansion path:
        // known names route to the entry, unknown names fail with
        // the sorted known-functions listing.
        let echo: PureFn = Arc::new(|args| {
            args.into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("ECHO needs 1 arg"))
        });
        let mut entries = HashMap::new();
        entries.insert("ECHO".to_string(), echo);
        let mut table = PureTable::new();
        table.insert("T".to_string(), entries);
        let resolve = placeholder_resolver(Arc::new(table));
        let out = resolve("T", "ECHO", vec![Value::string("hi".to_string())]).expect("dispatch");
        assert_eq!(out.as_str().expect("string"), "hi");
        let err = format!(
            "{:#}",
            resolve("T", "NOPE", vec![]).expect_err("unknown must fail")
        );
        assert!(
            err.contains("unknown placeholder function `T::NOPE`"),
            "names the call: {err}"
        );
        assert!(
            err.contains("known functions: T::ECHO"),
            "lists known names: {err}"
        );
    }

    #[test]
    fn describe_entry_renders_union_spellings() {
        // Union params and returns render through the same structural
        // renderer every other tag uses: `DESCRIBE` (and the generated
        // reference built on it) names the admission set, never a
        // bare word or an empty string. Built by macro, so the
        // spelling cannot drift from the members.
        let union = oxdock_parser::union_tag!(String, Pipe);
        let meta = FuncMeta {
            name: "T::F".to_string(),
            module: "T".to_string(),
            kind: FuncKind::HostPure,
            params: Some(vec![FuncParam {
                name: "text".to_string(),
                param_type: Some(union.clone()),
                allowed: None,
                docs: "",
                options: None,
                optional: false,
            }]),
            returns: Some(union),
            rpn: false,
            summary: "test-only",
            docs: "test-only",
            examples: &[],
        };
        let value = meta_to_value(&meta);
        let map = value.as_map().expect("describe value is a map");
        let params = map
            .get("params")
            .expect("params key")
            .as_list()
            .expect("params is a list");
        let first = params[0].as_map().expect("entry is a map");
        assert_eq!(
            first.get("param_type").and_then(Value::as_str),
            Some("STRING|PIPE"),
            "param renders the set"
        );
        assert_eq!(
            map.get("returns").and_then(Value::as_str),
            Some("STRING|PIPE"),
            "returns render the set"
        );
    }

    #[test]
    fn describe_entry_renders_handle_payload() {
        // Shaped handles render structurally (HANDLE<STRING>, not
        // bare HANDLE): script ASYNC FUNC declarations surface their
        // payload through DESCRIBE exactly as declared.
        let meta = FuncMeta {
            name: "SCRIPT::FETCH".to_string(),
            module: "SCRIPT".to_string(),
            kind: FuncKind::Script,
            params: Some(vec![]),
            returns: Some(TypeTag::HandleOf(std::sync::Arc::new(TypeTag::String))),
            rpn: false,
            summary: "test-only",
            docs: "test-only",
            examples: &[],
        };
        let value = meta_to_value(&meta);
        let map = value.as_map().expect("describe value is a map");
        assert_eq!(
            map.get("returns").and_then(Value::as_str),
            Some("HANDLE<STRING>"),
            "returns render the payload"
        );
    }

    #[test]
    fn describe_entry_carries_optional_flag() {
        // The `optional` key on params entries is the machine-readable
        // half of the `?` rendering: pin both values through the same
        // builder the runtime serves, not through prose.
        let meta = FuncMeta {
            name: "T::F".to_string(),
            module: "T".to_string(),
            kind: FuncKind::HostPure,
            params: Some(vec![
                FuncParam {
                    name: "url".to_string(),
                    param_type: Some(TypeTag::String),
                    allowed: None,
                    docs: "",
                    options: None,
                    optional: false,
                },
                FuncParam {
                    name: "options".to_string(),
                    param_type: Some(TypeTag::Map),
                    allowed: None,
                    docs: "",
                    options: None,
                    optional: true,
                },
            ]),
            returns: Some(TypeTag::String),
            rpn: false,
            summary: "test-only",
            docs: "test-only",
            examples: &[],
        };
        let value = meta_to_value(&meta);
        let map = value.as_map().expect("describe value is a map");
        let params = map
            .get("params")
            .expect("params key")
            .as_list()
            .expect("params is a list");
        assert_eq!(params.len(), 2);
        let first = params[0].as_map().expect("entry is a map");
        let second = params[1].as_map().expect("entry is a map");
        assert_eq!(
            first.get("optional").and_then(Value::as_bool),
            Some(false),
            "required param must read false"
        );
        assert_eq!(
            second.get("optional").and_then(Value::as_bool),
            Some(true),
            "omittable param must read true"
        );
    }

    #[test]
    fn template_twins_mirror_string_paths_and_refuse_pipes() {
        // Every template twin answers strings through the shared pump
        // semantics and refuses pipes naming the step IO they need:
        // placeholder evaluation carries no step context to pump
        // through. Table-driven so a twinless mirror op fails here
        // naming it.
        let pipe = Value::pipe_fresh();
        type TwinCase = (&'static str, Box<dyn Fn() -> Result<Value>>);
        let cases: Vec<TwinCase> = vec![
            (
                "TEXT_TRIM",
                Box::new(|| text_trim_string(Value::string("  hello  ".to_string()))),
            ),
            (
                "TEXT_TRIM_PREFIX",
                Box::new(|| {
                    text_trim_prefix_string(Value::string("v1".to_string()), "v".to_string())
                }),
            ),
            (
                "TEXT_TRIM_SUFFIX",
                Box::new(|| {
                    text_trim_suffix_string(Value::string("a.gz".to_string()), "x".to_string())
                }),
            ),
            (
                "TEXT_TO_UPPER",
                Box::new(|| text_to_upper_string(Value::string("hi".to_string()))),
            ),
            (
                "TEXT_TO_LOWER",
                Box::new(|| text_to_lower_string(Value::string("HI".to_string()))),
            ),
            (
                "TEXT_REPLACE",
                Box::new(|| {
                    text_replace_string(
                        Value::string("ab".to_string()),
                        "b".to_string(),
                        "c".to_string(),
                    )
                }),
            ),
            (
                "TEXT_REPLACE_REGEX",
                Box::new(|| {
                    text_replace_regex_string(
                        Value::string("ab".to_string()),
                        "b".to_string(),
                        "c".to_string(),
                    )
                }),
            ),
        ];
        assert_eq!(cases.len(), 7, "every mirror op needs a twin row");
        for (name, run) in &cases {
            let value = run().unwrap_or_else(|e| panic!("{name} twin must answer strings: {e:#}"));
            assert!(
                value.as_str().is_some(),
                "{name} twin must answer STRING, got {}",
                value.type_name()
            );
        }
        assert_eq!(
            text_trim_string(Value::string("  hello  ".to_string()))
                .expect("trim twin")
                .as_str(),
            Some("hello")
        );
        // Pipes refuse through the shared message on every twin.
        let pipe_cases: Vec<(&str, Result<Value>)> = vec![
            ("TEXT_TRIM", text_trim_string(pipe.clone())),
            (
                "TEXT_TRIM_PREFIX",
                text_trim_prefix_string(pipe.clone(), "v".to_string()),
            ),
            (
                "TEXT_TRIM_SUFFIX",
                text_trim_suffix_string(pipe.clone(), ".gz".to_string()),
            ),
            ("TEXT_TO_UPPER", text_to_upper_string(pipe.clone())),
            ("TEXT_TO_LOWER", text_to_lower_string(pipe.clone())),
            (
                "TEXT_REPLACE",
                text_replace_string(pipe.clone(), "a".to_string(), "b".to_string()),
            ),
            (
                "TEXT_REPLACE_REGEX",
                text_replace_regex_string(pipe.clone(), "a".to_string(), "b".to_string()),
            ),
        ];
        for (name, result) in pipe_cases {
            let err = result.expect_err(&format!("{name} twin must refuse PIPE"));
            assert!(
                format!("{err:#}").contains("step IO"),
                "{name} refusal names the missing context: {err:#}"
            );
        }
    }

    #[test]
    fn shell_words_join_split_round_trip() {
        // Join and split are absolute inverses on the word side: the
        // joiner quotes through `shell-words`, the splitter parses the
        // same syntax, so every adversarial word survives the round trip.
        let words = vec![
            "gh".to_string(),
            "issue".to_string(),
            "comment".to_string(),
            "with spaces".to_string(),
            "it's".to_string(),
            "say \"hi\"".to_string(),
            "$HOME".to_string(),
            "$(whoami)".to_string(),
            "C:\\path\\to".to_string(),
            "naïve café".to_string(),
            "".to_string(),
        ];
        let joined = shell_words::join(&words);
        let back = shell_words::split(&joined).expect("join output must parse");
        assert_eq!(back, words);
        assert!(shell_words::split("unbalanced \"quote").is_err());
    }

    /// Fake module graph for the shared forward walk: export lists,
    /// per-file alias targets, and per-file bindings set directly,
    /// so every chase branch pins without files or execution.
    struct FakeView {
        exports: HashMap<String, Vec<String>>,
        targets: HashMap<String, AliasTargets>,
        bindings: HashMap<String, HashMap<String, bool>>,
    }

    impl ModuleGraphView for FakeView {
        fn const_exports(&self, file: &str) -> Option<&[String]> {
            self.exports.get(file).map(Vec::as_slice)
        }

        fn alias_targets(&self, file: &str) -> AliasTargets {
            self.targets.get(file).cloned().unwrap_or_default()
        }

        fn binding(&self, file: &str, name: &str) -> BindingPresence {
            match self.bindings.get(file).and_then(|names| names.get(name)) {
                Some(true) => BindingPresence::Const,
                Some(false) => BindingPresence::Let,
                None => BindingPresence::Absent,
            }
        }
    }

    fn fake_targets(pairs: &[(&str, &str)]) -> AliasTargets {
        let mut targets = AliasTargets::new();
        for (alias, file) in pairs {
            targets
                .entry((*alias).to_string())
                .or_default()
                .push((Some((*file).to_string()), None));
        }
        targets
    }

    fn os_guard(os: &str) -> GuardExpr {
        GuardExpr::Predicate(oxdock_parser::Guard::Attr {
            ns: oxdock_parser::Ns::Os,
            key: None,
            val: Some(os.to_string()),
        })
    }

    #[test]
    fn chase_resolves_bare_and_single_forward() {
        // Bare entries terminate in place; one forward hop lands on
        // the defining file; chains compose hop by hop.
        let mut view = FakeView {
            exports: HashMap::from([
                ("a".to_string(), vec!["N".to_string()]),
                ("b".to_string(), vec!["A::N".to_string()]),
                ("c".to_string(), vec!["B::N".to_string()]),
            ]),
            targets: HashMap::from([
                ("u".to_string(), fake_targets(&[("B", "b")])),
                ("b".to_string(), fake_targets(&[("A", "a")])),
                ("c".to_string(), fake_targets(&[("B", "b")])),
            ]),
            bindings: HashMap::new(),
        };
        assert_eq!(
            chase_const_forward(&view, "a", "N", "U::N").expect("bare"),
            vec!["a".to_string()]
        );
        assert_eq!(
            chase_const_forward(&view, "b", "N", "U::N").expect("forward"),
            vec!["a".to_string()]
        );
        view.targets
            .insert("u".to_string(), fake_targets(&[("C", "c")]));
        assert_eq!(
            chase_const_forward(&view, "c", "N", "U::N").expect("chain"),
            vec!["a".to_string()]
        );
    }

    #[test]
    fn chase_prefers_bare_over_forwards() {
        // A local bare definition shadows forwards of the same base:
        // the file answers for its own binding.
        let view = FakeView {
            exports: HashMap::from([("b".to_string(), vec!["N".to_string(), "A::N".to_string()])]),
            targets: HashMap::from([("b".to_string(), fake_targets(&[("A", "a")]))]),
            bindings: HashMap::new(),
        };
        assert_eq!(
            chase_const_forward(&view, "b", "N", "U::N").expect("bare wins"),
            vec!["b".to_string()]
        );
    }

    #[test]
    fn chase_rejects_split_forwards() {
        // Two forwards of one base in one file fail instead of
        // picking one.
        let view = FakeView {
            exports: HashMap::from([(
                "b".to_string(),
                vec!["A::N".to_string(), "C::N".to_string()],
            )]),
            targets: HashMap::new(),
            bindings: HashMap::new(),
        };
        let err = format!(
            "{}",
            chase_const_forward(&view, "b", "N", "U::N").expect_err("split must fail")
        );
        assert!(err.contains("ambiguous CONST `U::N`"), "{err}");
        assert!(err.contains("A::N") && err.contains("C::N"), "{err}");
    }

    #[test]
    fn chase_names_broken_link_detail() {
        // A hop landing nowhere names the break: missing binding,
        // unexported CONST, or LET terminal each render distinctly.
        let view = FakeView {
            exports: HashMap::from([
                ("b".to_string(), vec!["A::M".to_string()]),
                ("c".to_string(), vec!["A::P".to_string()]),
                ("d".to_string(), vec!["A::Q".to_string()]),
                ("a".to_string(), Vec::new()),
            ]),
            targets: HashMap::from([
                ("b".to_string(), fake_targets(&[("A", "a")])),
                ("c".to_string(), fake_targets(&[("A", "a")])),
                ("d".to_string(), fake_targets(&[("A", "a")])),
            ]),
            bindings: HashMap::from([(
                "a".to_string(),
                HashMap::from([("P".to_string(), true), ("Q".to_string(), false)]),
            )]),
        };
        let err = format!(
            "{}",
            chase_const_forward(&view, "b", "M", "U::M").expect_err("missing must fail")
        );
        assert!(err.contains("does not resolve"), "{err}");
        assert!(err.contains("no import alias `A` exports `M`"), "{err}");
        let err = format!(
            "{}",
            chase_const_forward(&view, "c", "P", "U::P").expect_err("unexported must fail")
        );
        assert!(
            err.contains("holds `P` as CONST but does not export it"),
            "{err}"
        );
        let err = format!(
            "{}",
            chase_const_forward(&view, "d", "Q", "U::Q").expect_err("LET must fail")
        );
        assert!(
            err.contains("is a LET binding and cannot be exported"),
            "{err}"
        );
    }

    #[test]
    fn chase_rejects_cycles() {
        // Mutual forwards never resolve: the walk names the chain
        // instead of recursing forever.
        let view = FakeView {
            exports: HashMap::from([
                ("a".to_string(), vec!["B::N".to_string()]),
                ("b".to_string(), vec!["A::N".to_string()]),
            ]),
            targets: HashMap::from([
                ("a".to_string(), fake_targets(&[("B", "b")])),
                ("b".to_string(), fake_targets(&[("A", "a")])),
            ]),
            bindings: HashMap::new(),
        };
        let err = format!(
            "{}",
            chase_const_forward(&view, "a", "N", "U::N").expect_err("cycle must fail")
        );
        assert!(err.contains("re-export cycle"), "{err}");
    }

    #[test]
    fn chase_unions_disjoint_guard_branches() {
        // One forward over disjoint platform imports chases each live
        // branch to its own terminal instead of failing ambiguous.
        let mut guarded = AliasTargets::new();
        guarded.insert(
            "A".to_string(),
            vec![
                (Some("lin".to_string()), Some(os_guard("linux"))),
                (Some("mac".to_string()), Some(os_guard("macos"))),
            ],
        );
        let view = FakeView {
            exports: HashMap::from([
                ("b".to_string(), vec!["A::N".to_string()]),
                ("lin".to_string(), vec!["N".to_string()]),
                ("mac".to_string(), vec!["N".to_string()]),
            ]),
            targets: HashMap::from([("b".to_string(), guarded)]),
            bindings: HashMap::new(),
        };
        assert_eq!(
            chase_const_forward(&view, "b", "N", "U::N").expect("disjoint branches"),
            vec!["lin".to_string(), "mac".to_string()]
        );
    }

    #[test]
    fn holder_classification_sorts_and_rejects() {
        // Holders come back sorted; unknown aliases and overlapping
        // files report through their own variants.
        let view = FakeView {
            exports: HashMap::from([
                ("b".to_string(), vec!["N".to_string()]),
                ("c".to_string(), vec!["N".to_string()]),
            ]),
            targets: HashMap::new(),
            bindings: HashMap::new(),
        };
        let unit = fake_targets(&[("B", "b"), ("B", "c")]);
        match const_holder_files(&view, &unit, "B", "N") {
            ConstHolders::Ambiguous { .. } => {}
            other => panic!("overlapping holders must be ambiguous, got {other:?}"),
        }
        match const_holder_files(&view, &unit, "Z", "N") {
            ConstHolders::NoAlias => {}
            other => panic!("unknown alias must report NoAlias, got {other:?}"),
        }
        let unit = fake_targets(&[("B", "b")]);
        match const_holder_files(&view, &unit, "B", "N") {
            ConstHolders::Found(files) => assert_eq!(files, vec!["b".to_string()]),
            other => panic!("single holder must be found, got {other:?}"),
        }
    }
}
