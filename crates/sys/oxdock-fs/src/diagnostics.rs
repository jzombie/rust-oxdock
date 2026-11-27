//! Filesystem failures behind single constructors with stable keys.
//!
//! This module owns every filesystem template and key (`fs.*`). It
//! builds on the generic [`oxdock_diagnostics::Diagnostic`] transport
//! but owns its wording: changing filesystem text happens here, never
//! at the call sites. Call sites pass structured data only, never
//! `format!` strings. Fixed-shape failures are parameterless;
//! path failures take the concrete path; I/O failures take the
//! concrete `std::io::Error`; process statuses take the concrete
//! `std::process::ExitStatus` (never hardcoded: its `Display` is
//! platform-specific). Same failure mode shares one key with the
//! invoker or detail as a structured field.

use oxdock_diagnostics::Diagnostic;

use crate::workspace_fs::GuardedPath;

/// Structured field keys. Single spelling for the whole module:
/// constructors reference these, never literals, so a key rename is
/// one edit.
const FIELD_MODE: &str = "mode";
const FIELD_CANDIDATE: &str = "candidate";
const FIELD_ROOT: &str = "root";
const FIELD_PATH: &str = "path";
const FIELD_BASE: &str = "base";
const FIELD_REL: &str = "rel";
const FIELD_TEXT: &str = "text";
const FIELD_KEY: &str = "key";
const FIELD_STATUS: &str = "status";
const FIELD_REV: &str = "rev";
const FIELD_ERR: &str = "err";
const FIELD_SRC: &str = "src";
const FIELD_DST: &str = "dst";

// ---------------------------------------------------------------------------
// Guard containment (`fs.guard.*`, `access.rs`)
// ---------------------------------------------------------------------------

/// Access escaping its allowed root. Takes the mode name (the
/// `AccessMode::name()` spelling), the candidate, and the root;
/// rendering lives here. Shared by the canonicalized and lexical
/// paths: one key, one mode.
#[allow(clippy::disallowed_types)]
pub fn guard_access_escapes(
    mode: &str,
    candidate: &std::path::Path,
    root: &std::path::Path,
) -> Diagnostic {
    let message = format!(
        "{mode} access to {} escapes allowed root {}",
        candidate.display(),
        root.display()
    );
    Diagnostic::new(
        "fs.guard.access_escapes",
        message,
        Diagnostic::make_fields(&[
            (FIELD_MODE, mode),
            (FIELD_CANDIDATE, &candidate.display().to_string()),
            (FIELD_ROOT, &root.display().to_string()),
        ]),
    )
}

/// Snapshot-anchored candidate arriving outside the resolve choke
/// points. Takes the candidate; rendering lives here.
#[allow(clippy::disallowed_types)]
pub fn guard_snapshot_not_materialized(candidate: &std::path::Path) -> Diagnostic {
    let message = format!(
        "snapshot root not materialized; resolve through the snapshot choke point first: {}",
        candidate.display()
    );
    Diagnostic::new(
        "fs.guard.snapshot_not_materialized",
        message,
        Diagnostic::make_fields(&[(FIELD_CANDIDATE, &candidate.display().to_string())]),
    )
}

// ---------------------------------------------------------------------------
// Link containment (`fs.link.*`, `io.rs`)
// ---------------------------------------------------------------------------

/// Symlink as the final path component. Takes the path; rendering
/// lives here.
pub fn refuses_symlink_final(path: &GuardedPath) -> Diagnostic {
    let message = format!("refuses symlink final component: {}", path.display());
    Diagnostic::new(
        "fs.link.refuses_symlink_final",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

/// Parent reference above the base. Takes the base; rendering lives
/// here.
pub fn refuses_parent_reference(base: &GuardedPath) -> Diagnostic {
    let message = format!("refuses parent reference above {}", base.display());
    Diagnostic::new(
        "fs.link.refuses_parent_reference",
        message,
        Diagnostic::make_fields(&[(FIELD_BASE, &base.display().to_string())]),
    )
}

/// Link ancestor on the walk. Takes the ancestor; rendering lives
/// here.
#[allow(clippy::disallowed_types)]
pub fn refuses_link_ancestor(current: &std::path::Path) -> Diagnostic {
    let message = format!("refuses link ancestor {}", current.display());
    Diagnostic::new(
        "fs.link.refuses_link_ancestor",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &current.display().to_string())]),
    )
}

// ---------------------------------------------------------------------------
// Link removal (`fs.remove_link.*`, `io.rs`)
// ---------------------------------------------------------------------------

/// Link removal with no file name. Takes the rel text; rendering lives
/// here. Shared by the missing and dot-dot modes: one key, one mode.
pub fn remove_link_no_file_name(rel: &str) -> Diagnostic {
    let message = format!("remove_link denied for {rel}: no file name");
    Diagnostic::new(
        "fs.remove_link.no_file_name",
        message,
        Diagnostic::make_fields(&[(FIELD_REL, rel)]),
    )
}

/// Link removal escaping its root. Takes the rel text; rendering lives
/// here.
pub fn remove_link_escapes_root(rel: &str) -> Diagnostic {
    let message = format!("remove_link denied for {rel}: escapes root");
    Diagnostic::new(
        "fs.remove_link.escapes_root",
        message,
        Diagnostic::make_fields(&[(FIELD_REL, rel)]),
    )
}

// ---------------------------------------------------------------------------
// Miri-only stubs (`fs.miri.*`, `io.rs` / `path.rs`)
// ---------------------------------------------------------------------------

/// Unguarded metadata under Miri. Parameterless: the wording is fixed.
pub fn metadata_unguarded_miri() -> Diagnostic {
    Diagnostic::new(
        "fs.miri.metadata_unguarded",
        "metadata_unguarded not supported on Miri".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Unguarded file open under Miri. Parameterless: the wording is
/// fixed.
pub fn open_file_unguarded_miri() -> Diagnostic {
    Diagnostic::new(
        "fs.miri.open_file_unguarded",
        "open_file_unguarded not supported on Miri".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Spill files under Miri. Parameterless: the wording is fixed.
/// Shared by create, open, and truncate: one key, one mode.
pub fn spill_files_miri() -> Diagnostic {
    Diagnostic::new(
        "fs.miri.spill_files",
        "spill files are not supported under Miri (memory-only)".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Private files under Miri. Parameterless: the wording is fixed.
pub fn private_files_miri() -> Diagnostic {
    Diagnostic::new(
        "fs.miri.private_files",
        "private files are not supported under Miri (memory-only)".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Symlinks under Miri. Takes both ends; rendering lives here.
pub fn symlink_unsupported_miri(src: &GuardedPath, dst: &GuardedPath) -> Diagnostic {
    let message = format!(
        "SYMLINK unsupported under Miri ({} -> {})",
        src.display(),
        dst.display()
    );
    Diagnostic::new(
        "fs.miri.symlink_unsupported",
        message,
        Diagnostic::make_fields(&[
            (FIELD_SRC, &src.display().to_string()),
            (FIELD_DST, &dst.display().to_string()),
        ]),
    )
}

/// Temp lock under Miri. Parameterless: the wording is fixed.
pub fn temp_lock_miri() -> Diagnostic {
    Diagnostic::new(
        "fs.miri.temp_lock",
        "temp lock unused under Miri".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

// ---------------------------------------------------------------------------
// Symlinks (`fs.symlink.*`, `io.rs`)
// ---------------------------------------------------------------------------

/// Symlink destination already exists. Takes the destination;
/// rendering lives here. Shared by the host and Miri paths: one key,
/// one mode.
pub fn symlink_dest_exists(path: &GuardedPath) -> Diagnostic {
    let message = format!("SYMLINK destination already exists: {}", path.display());
    Diagnostic::new(
        "fs.symlink.dest_exists",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

/// Symlink source resolving to itself. Takes the path; rendering lives
/// here.
pub fn symlink_self_link(path: &GuardedPath) -> Diagnostic {
    let message = format!(
        "SYMLINK source resolves to the destination itself: {}",
        path.display()
    );
    Diagnostic::new(
        "fs.symlink.self_link",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

/// Symlinks on an unsupported platform. Takes both ends; rendering
/// lives here.
pub fn symlink_unsupported_platform(src: &GuardedPath, dst: &GuardedPath) -> Diagnostic {
    let message = format!(
        "SYMLINK unsupported on this platform ({} -> {})",
        src.display(),
        dst.display()
    );
    Diagnostic::new(
        "fs.symlink.unsupported_platform",
        message,
        Diagnostic::make_fields(&[
            (FIELD_SRC, &src.display().to_string()),
            (FIELD_DST, &dst.display().to_string()),
        ]),
    )
}

// ---------------------------------------------------------------------------
// Missing files (`fs.file.*`, `lib.rs` / `mock.rs`)
// ---------------------------------------------------------------------------

/// Missing file. Takes the path; rendering lives here. Shared by the
/// Miri length probe and the mock reads: one key, one mode.
pub fn missing_file(path: &GuardedPath) -> Diagnostic {
    let message = format!("missing file {}", path.display());
    Diagnostic::new(
        "fs.file.missing",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

// ---------------------------------------------------------------------------
// Copies and sources (`fs.copy.*`, `copy.rs` / `backend.rs` /
// `resolve.rs`)
// ---------------------------------------------------------------------------

/// Unsupported file type. Takes the path; rendering lives here.
/// Shared by the copy classifier and both entry-kind probes: one key,
/// one mode.
#[allow(clippy::disallowed_types)]
pub fn unsupported_file_type(path: &std::path::Path) -> Diagnostic {
    let message = format!("unsupported file type: {}", path.display());
    Diagnostic::new(
        "fs.copy.unsupported_file_type",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

/// Copy source absent from the build context. Takes the guarded
/// source; rendering lives here. Shared by the host and synthetic
/// paths: one key, one mode.
pub fn copy_source_missing(path: &GuardedPath) -> Diagnostic {
    let message = format!("COPY source missing in build context: {}", path.display());
    Diagnostic::new(
        "fs.copy.source_missing",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

/// Copy source of an unsupported kind. Takes the guarded source;
/// rendering lives here.
pub fn copy_source_unsupported_kind(path: &GuardedPath) -> Diagnostic {
    let message = format!("COPY source unsupported kind: {}", path.display());
    Diagnostic::new(
        "fs.copy.source_unsupported_kind",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

/// Workspace-relative copy with no workspace root. Parameterless: the
/// wording is fixed.
pub fn copy_no_workspace_root() -> Diagnostic {
    Diagnostic::new(
        "fs.copy.no_workspace_root",
        "no workspace root set for workspace-relative COPY".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Snapshot copy without a materialized snapshot. Parameterless: the
/// wording is fixed.
pub fn copy_snapshot_not_materialized() -> Diagnostic {
    Diagnostic::new(
        "fs.copy.snapshot_not_materialized",
        "COPY from SNAPSHOT requires a materialized snapshot".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

// ---------------------------------------------------------------------------
// Host backend (`fs.backend.*`, `backend.rs`)
// ---------------------------------------------------------------------------

/// Workdir that is not a directory. Takes the path; rendering lives
/// here.
pub fn workdir_not_dir(path: &GuardedPath) -> Diagnostic {
    let message = format!("WORKDIR path is not a directory: {}", path.display());
    Diagnostic::new(
        "fs.backend.workdir_not_dir",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

/// Open-file path resolution failure. Takes the OS failure; rendering
/// lives here. Shared by the Unix and Windows descriptor paths: one
/// key, one mode.
pub fn fd_path_failed(err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("failed to resolve open file path: {err_text}");
    Diagnostic::new(
        "fs.backend.fd_path_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_ERR, err_text.as_str())]),
    )
}

/// Opened file outside its expected tree. Takes the path, the
/// expected root, and the descriptor-resolved path; rendering lives
/// here.
#[allow(clippy::disallowed_types)]
pub fn opened_outside_tree(
    path: &GuardedPath,
    root: &GuardedPath,
    actual: &std::path::Path,
) -> Diagnostic {
    let message = format!(
        "opened {} outside its expected tree {} (descriptor resolves to {})",
        path.display(),
        root.display(),
        actual.display()
    );
    Diagnostic::new(
        "fs.backend.opened_outside_tree",
        message,
        Diagnostic::make_fields(&[
            (FIELD_PATH, &path.display().to_string()),
            (FIELD_ROOT, &root.display().to_string()),
            (FIELD_CANDIDATE, &actual.display().to_string()),
        ]),
    )
}

// ---------------------------------------------------------------------------
// Synthetic Miri backend (`fs.synthetic.*`, `backend.rs`)
// ---------------------------------------------------------------------------

/// Missing synthetic root state. Parameterless: the wording is fixed.
pub fn synthetic_state_missing_for_root() -> Diagnostic {
    Diagnostic::new(
        "fs.synthetic.state_missing_for_root",
        "synthetic state missing for root".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Missing synthetic build-context state. Parameterless: the wording
/// is fixed.
pub fn synthetic_state_missing_for_build() -> Diagnostic {
    Diagnostic::new(
        "fs.synthetic.state_missing_for_build",
        "synthetic state missing for build context".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Synthetic directory absent. Takes the path; rendering lives here.
pub fn synthetic_dir_missing(path: &GuardedPath) -> Diagnostic {
    let message = format!("directory does not exist: {}", path.display());
    Diagnostic::new(
        "fs.synthetic.dir_missing",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

/// Synthetic stat miss. Takes the path; rendering lives here. Shared
/// by metadata and member-stat lookups: one key, one mode.
pub fn synthetic_stat_failed(path: &GuardedPath) -> Diagnostic {
    let message = format!("failed to stat {}", path.display());
    Diagnostic::new(
        "fs.synthetic.stat_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

/// Synthetic archive member that is a symlink. Takes the path;
/// rendering lives here.
pub fn synthetic_archive_member_symlink(path: &GuardedPath) -> Diagnostic {
    let message = format!("archive member is a symlink: {}", path.display());
    Diagnostic::new(
        "fs.synthetic.archive_member_symlink",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

/// Synthetic entry-kind miss. Takes the path; rendering lives here.
pub fn synthetic_entry_kind_missing(path: &GuardedPath) -> Diagnostic {
    let message = format!("missing entry kind for {}", path.display());
    Diagnostic::new(
        "fs.synthetic.entry_kind_missing",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

/// Synthetic file miss by rel. Takes the rel; rendering lives here.
pub fn synthetic_missing_file(rel: &str) -> Diagnostic {
    let message = format!("missing file {rel}");
    Diagnostic::new(
        "fs.synthetic.missing_file",
        message,
        Diagnostic::make_fields(&[(FIELD_REL, rel)]),
    )
}

/// Synthetic dir removal of a missing dir. Takes the rel; rendering
/// lives here.
pub fn synthetic_remove_dir_missing(rel: &str) -> Diagnostic {
    let message = format!("directory does not exist: {rel}");
    Diagnostic::new(
        "fs.synthetic.remove_dir_missing",
        message,
        Diagnostic::make_fields(&[(FIELD_REL, rel)]),
    )
}

/// Synthetic dir removal of a non-empty dir. Takes the rel; rendering
/// lives here.
pub fn synthetic_remove_dir_not_empty(rel: &str) -> Diagnostic {
    let message = format!("directory not empty: {rel}");
    Diagnostic::new(
        "fs.synthetic.remove_dir_not_empty",
        message,
        Diagnostic::make_fields(&[(FIELD_REL, rel)]),
    )
}

/// Synthetic metadata on an unsupported platform. Parameterless: the
/// wording is fixed.
pub fn synthetic_metadata_unsupported() -> Diagnostic {
    Diagnostic::new(
        "fs.synthetic.metadata_unsupported",
        "synthetic metadata unsupported under Miri on this platform".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

// ---------------------------------------------------------------------------
// Paths (`fs.path.*`, `path.rs`)
// ---------------------------------------------------------------------------

/// Script with a missing parent. Takes the raw text; rendering lives
/// here.
pub fn script_not_found(text: &str) -> Diagnostic {
    let message = format!("script not found: {text}");
    Diagnostic::new(
        "fs.path.script_not_found",
        message,
        Diagnostic::make_fields(&[(FIELD_TEXT, text)]),
    )
}

/// Empty `PATH_BASE` input. Parameterless: the wording is fixed.
/// Shared by the empty and slashes-only modes: one key, one mode.
pub fn path_base_empty() -> Diagnostic {
    Diagnostic::new(
        "fs.path.base_empty",
        "PATH_BASE needs a non-empty path".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Empty `PATH_REL` input. Parameterless: the wording is fixed.
pub fn path_rel_empty() -> Diagnostic {
    Diagnostic::new(
        "fs.path.rel_empty",
        "PATH_REL needs non-empty base and path".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `..` inside `PATH_REL` operands. Takes both sides; `Debug`
/// quoting lives here.
pub fn path_rel_dotdot(base: &str, path: &str) -> Diagnostic {
    let message = format!(
        "PATH_REL base {base:?} is not a prefix of {path:?}: \"..\" segments never relativize"
    );
    Diagnostic::new(
        "fs.path.rel_dotdot",
        message,
        Diagnostic::make_fields(&[(FIELD_BASE, base), (FIELD_PATH, path)]),
    )
}

/// Mixed absolute/relative `PATH_REL` operands. Takes both sides;
/// `Debug` quoting lives here.
pub fn path_rel_mixed(base: &str, path: &str) -> Diagnostic {
    let message = format!(
        "PATH_REL base {base:?} is not a prefix of {path:?}: absolute and relative shapes never mix"
    );
    Diagnostic::new(
        "fs.path.rel_mixed",
        message,
        Diagnostic::make_fields(&[(FIELD_BASE, base), (FIELD_PATH, path)]),
    )
}

/// Non-prefix `PATH_REL` operands. Takes both sides; `Debug` quoting
/// lives here.
pub fn path_rel_not_prefix(base: &str, path: &str) -> Diagnostic {
    let message = format!("PATH_REL base {base:?} is not a prefix of {path:?}");
    Diagnostic::new(
        "fs.path.rel_not_prefix",
        message,
        Diagnostic::make_fields(&[(FIELD_BASE, base), (FIELD_PATH, path)]),
    )
}

/// Temp-scratch collision. Takes the candidate path; rendering lives
/// here.
#[allow(clippy::disallowed_types)]
pub fn scratch_collided(path: &std::path::Path) -> Diagnostic {
    let message = format!(
        "scratch candidate collided (clock reuse?): {}",
        path.display()
    );
    Diagnostic::new(
        "fs.path.scratch_collided",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

// ---------------------------------------------------------------------------
// Cache (`fs.cache.*`, `cache.rs`)
// ---------------------------------------------------------------------------

/// Cache directory creation failure. Takes the directory plus the I/O
/// failure; rendering lives here.
#[allow(clippy::disallowed_types)]
pub fn cache_ensure_failed(dir: &std::path::Path, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("failed to ensure cache dir {}: {err_text}", dir.display());
    Diagnostic::new(
        "fs.cache.ensure_failed",
        message,
        Diagnostic::make_fields(&[
            (FIELD_PATH, &dir.display().to_string()),
            (FIELD_ERR, err_text.as_str()),
        ]),
    )
}

// ---------------------------------------------------------------------------
// Install (`fs.install.*`, `install.rs`)
// ---------------------------------------------------------------------------

/// Atomic-install staging failure. Takes the staging directory plus
/// the I/O failure; rendering lives here (same shape as
/// `cache_ensure_failed`).
#[allow(clippy::disallowed_types)]
pub fn install_stage_failed(dir: &std::path::Path, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!(
        "failed to stage install payload in {}: {err_text}",
        dir.display()
    );
    Diagnostic::new(
        "fs.install.stage_failed",
        message,
        Diagnostic::make_fields(&[
            (FIELD_PATH, &dir.display().to_string()),
            (FIELD_ERR, err_text.as_str()),
        ]),
    )
}

/// Atomic-install mode failure. Takes the staged path plus the I/O
/// failure; rendering lives here.
#[allow(clippy::disallowed_types)]
pub fn install_set_mode_failed(path: &std::path::Path, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("failed to set mode on {}: {err_text}", path.display());
    Diagnostic::new(
        "fs.install.set_mode_failed",
        message,
        Diagnostic::make_fields(&[
            (FIELD_PATH, &path.display().to_string()),
            (FIELD_ERR, err_text.as_str()),
        ]),
    )
}

/// Atomic-install placement failure. Takes both ends plus the I/O
/// failure; rendering lives here.
#[allow(clippy::disallowed_types)]
pub fn install_place_failed(
    staged: &std::path::Path,
    dst: &std::path::Path,
    err: &std::io::Error,
) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!(
        "failed to rename {} to {}: {err_text}",
        staged.display(),
        dst.display()
    );
    Diagnostic::new(
        "fs.install.place_failed",
        message,
        Diagnostic::make_fields(&[
            (FIELD_SRC, &staged.display().to_string()),
            (FIELD_DST, &dst.display().to_string()),
            (FIELD_ERR, err_text.as_str()),
        ]),
    )
}

/// Install destination with no parent. Takes the destination;
/// rendering lives here.
pub fn install_no_parent(dst: &GuardedPath) -> Diagnostic {
    let message = format!("install destination has no parent: {}", dst.display());
    Diagnostic::new(
        "fs.install.no_parent",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &dst.display().to_string())]),
    )
}

/// Generic install failure. Takes the pre-rendered reason verbatim
/// (like `proc.run.synthetic_failed`); kept for install-specific
/// unkeyed fallout now that [`from_anyhow`] falls back to the
/// generic [`operation_failed`].
pub fn install_failed(reason: &str) -> Diagnostic {
    Diagnostic::new(
        "fs.install.failed",
        reason.to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, reason)]),
    )
}

/// Generic filesystem-operation failure. Takes the pre-rendered
/// reason verbatim (like `build.operation.failed`);
/// already-keyed payloads are reclaimed by [`from_anyhow`], so
/// this only names genuinely unkeyed fallout.
pub fn operation_failed(reason: &str) -> Diagnostic {
    Diagnostic::new(
        "fs.operation.failed",
        reason.to_string(),
        Diagnostic::make_fields(&[(FIELD_ERR, reason)]),
    )
}

/// Reclaim an already-keyed diagnostic from an `anyhow` failure,
/// wrapping anything else as a generic operation failure. Single owner
/// for the fs-surface mapping so callers converting file-by-file
/// preserve keys instead of restating the downcast. Outer
/// `.context(...)` frames above the keyed payload replay natively
/// via `with_contexts` (mirroring `lift_dispatch_error`), so no
/// chain text is lost at the flip boundary.
pub fn from_anyhow(err: anyhow::Error) -> Diagnostic {
    if let Some(found) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<Diagnostic>())
        .cloned()
    {
        let mut outer_frames: Vec<String> = Vec::new();
        for cause in err.chain() {
            if cause.downcast_ref::<Diagnostic>().is_some() {
                break;
            }
            outer_frames.push(cause.to_string());
        }
        return found.with_contexts(outer_frames);
    }
    let reason_text = format!("{err:#}");
    operation_failed(&reason_text)
}

// ---------------------------------------------------------------------------
// Snapshot root (`fs.snapshot.*`, `mod.rs`)
// ---------------------------------------------------------------------------

/// Working directory outside the snapshot root. Takes the directory;
/// rendering lives here.
pub fn cwd_outside_snapshot_root(cwd: &GuardedPath) -> Diagnostic {
    let message = format!("cwd outside snapshot root: {}", cwd.display());
    Diagnostic::new(
        "fs.snapshot.cwd_outside_root",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &cwd.display().to_string())]),
    )
}

// ---------------------------------------------------------------------------
// Miri read_dir (`fs.miri.*`, `lib.rs`)
// ---------------------------------------------------------------------------

/// Unguarded `read_dir` under Miri. Parameterless: the wording is
/// fixed.
pub fn unguarded_read_dir_miri() -> Diagnostic {
    Diagnostic::new(
        "fs.miri.unguarded_read_dir",
        "Unguarded read_dir not supported on Miri".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

// ---------------------------------------------------------------------------
// Mock filesystem (`fs.mock.*`, `mock.rs`)
// ---------------------------------------------------------------------------

/// Mock path join failure. Takes the wrapped failure; the chain
/// renders inline.
pub fn mock_join_failed(err: &anyhow::Error) -> Diagnostic {
    let err_text = format!("{err:#}");
    Diagnostic::new(
        "fs.mock.join_failed",
        err_text.clone(),
        Diagnostic::make_fields(&[(FIELD_ERR, err_text.as_str())]),
    )
}

/// Mock metadata probe. Parameterless: the wording is fixed. Shared by
/// the guarded and unguarded probes: one key, one mode.
pub fn mock_metadata_unsupported() -> Diagnostic {
    Diagnostic::new(
        "fs.mock.metadata_unsupported",
        "metadata not supported in mock fs".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Mock unguarded entry point. Parameterless: the wording is fixed.
/// Shared by every unguarded mock method: one key, one mode.
pub fn mock_unguarded_unsupported() -> Diagnostic {
    Diagnostic::new(
        "fs.mock.unguarded_unsupported",
        "unguarded operations not supported in mock fs".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Mock string decode failure. Takes the conversion failure; rendering
/// lives here.
pub fn mock_read_not_utf8(err: &std::string::FromUtf8Error) -> Diagnostic {
    let err_text = err.to_string();
    Diagnostic::new(
        "fs.mock.read_not_utf8",
        err_text.clone(),
        Diagnostic::make_fields(&[(FIELD_ERR, err_text.as_str())]),
    )
}

/// Mock `read_dir`. Parameterless: the wording is fixed.
pub fn mock_read_dir_unsupported() -> Diagnostic {
    Diagnostic::new(
        "fs.mock.read_dir_unsupported",
        "read_dir unsupported in mock fs".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Mock copy. Parameterless: the wording is fixed. Shared by every
/// copy direction: one key, one mode.
pub fn mock_copy_unsupported() -> Diagnostic {
    Diagnostic::new(
        "fs.mock.copy_unsupported",
        "copy unsupported".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Mock symlink. Parameterless: the wording is fixed.
pub fn mock_symlink_unsupported() -> Diagnostic {
    Diagnostic::new(
        "fs.mock.symlink_unsupported",
        "symlink unsupported".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Mock unguarded open. Parameterless: the wording is fixed.
pub fn mock_open_unsupported() -> Diagnostic {
    Diagnostic::new(
        "fs.mock.open_unsupported",
        "open unsupported".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Mock permissions. Parameterless: the wording is fixed.
pub fn mock_perms_unsupported() -> Diagnostic {
    Diagnostic::new(
        "fs.mock.perms_unsupported",
        "perms unsupported".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Mock entry-kind miss. Takes the path; rendering lives here.
pub fn mock_missing_path(path: &GuardedPath) -> Diagnostic {
    let message = format!("missing path {}", path.display());
    Diagnostic::new(
        "fs.mock.missing_path",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

/// Mock file removal of a missing file. Takes the path; rendering
/// lives here.
pub fn mock_remove_no_file(path: &GuardedPath) -> Diagnostic {
    let message = format!("REMOVE {}: no such file", path.display());
    Diagnostic::new(
        "fs.mock.remove_no_file",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

/// Mock directory removal of a missing directory. Takes the path;
/// rendering lives here. Shared by recursive and empty removals: one
/// key, one mode.
pub fn mock_remove_no_dir(path: &GuardedPath) -> Diagnostic {
    let message = format!("REMOVE {}: no such directory", path.display());
    Diagnostic::new(
        "fs.mock.remove_no_dir",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

/// Mock removal of a non-empty directory. Takes the path; rendering
/// lives here.
pub fn mock_remove_not_empty(path: &GuardedPath) -> Diagnostic {
    let message = format!("REMOVE {}: directory not empty", path.display());
    Diagnostic::new(
        "fs.mock.remove_not_empty",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, &path.display().to_string())]),
    )
}

/// Mock git copy. Parameterless: the wording is fixed.
pub fn mock_git_copy_unsupported() -> Diagnostic {
    Diagnostic::new(
        "fs.mock.git_copy_unsupported",
        "git copy unsupported".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

// ---------------------------------------------------------------------------
// Git (`fs.git.*`, `git/`)
// ---------------------------------------------------------------------------

/// Git identity helpers under Miri. Parameterless: the wording is
/// fixed.
pub fn git_identity_miri() -> Diagnostic {
    Diagnostic::new(
        "fs.git.identity_miri",
        "git identity helpers are not supported under Miri".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Failed `git config --get`. Takes the key plus the exit status;
/// rendering lives here (never hardcoded: `ExitStatus` display is
/// platform-specific).
pub fn git_config_get_failed(key: &str, status: std::process::ExitStatus) -> Diagnostic {
    let message = format!("git config --get {key} exited with {status}");
    Diagnostic::new(
        "fs.git.config_get_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_KEY, key), (FIELD_STATUS, &status.to_string())]),
    )
}

/// Failed `git config --local`. Takes the key plus the exit status;
/// rendering lives here.
pub fn git_config_set_failed(key: &str, status: std::process::ExitStatus) -> Diagnostic {
    let message = format!("git config {key} failed with {status}");
    Diagnostic::new(
        "fs.git.config_set_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_KEY, key), (FIELD_STATUS, &status.to_string())]),
    )
}

/// Git copy source outside the build context. Parameterless: the
/// wording is fixed.
pub fn git_copy_outside_build_context() -> Diagnostic {
    Diagnostic::new(
        "fs.git.copy_outside_build_context",
        "COPY_GIT source must resolve within build context".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Failed `git ls-tree`. Takes the rev plus the rel; rendering lives
/// here.
pub fn git_ls_tree_failed(rev: &str, rel: &str) -> Diagnostic {
    let message = format!("git ls-tree failed for {rev}:{rel}");
    Diagnostic::new(
        "fs.git.ls_tree_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REV, rev), (FIELD_REL, rel)]),
    )
}

/// Path absent from the rev. Takes the rel plus the rev; rendering
/// lives here.
pub fn git_path_not_in_rev(rel: &str, rev: &str) -> Diagnostic {
    let message = format!("path {rel} not found in rev {rev}");
    Diagnostic::new(
        "fs.git.path_not_in_rev",
        message,
        Diagnostic::make_fields(&[(FIELD_REL, rel), (FIELD_REV, rev)]),
    )
}

/// Failed `git show`. Takes the rev plus the path; rendering lives
/// here.
pub fn git_show_failed(rev: &str, path: &str) -> Diagnostic {
    let message = format!("git show failed for {rev}:{path}");
    Diagnostic::new(
        "fs.git.show_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REV, rev), (FIELD_PATH, path)]),
    )
}

/// Submodule entries in git copies. Takes the path; rendering lives
/// here.
pub fn git_submodule_unsupported(path: &str) -> Diagnostic {
    let message = format!("submodule entries are not supported: {path}");
    Diagnostic::new(
        "fs.git.submodule_unsupported",
        message,
        Diagnostic::make_fields(&[(FIELD_PATH, path)]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn plain_path() -> &'static std::path::Path {
        std::path::Path::new("a/b.txt")
    }

    fn io_err(msg: &str) -> std::io::Error {
        std::io::Error::other(msg.to_string())
    }

    fn anyhow_err(msg: &str) -> anyhow::Error {
        anyhow::Error::msg(msg.to_string())
    }

    fn utf8_err() -> std::string::FromUtf8Error {
        String::from_utf8(vec![0xff]).unwrap_err()
    }

    fn guarded(rel: &str) -> GuardedPath {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        // The joined member never exists on disk; only its rendering
        // matters, so dropping the empty tempdir is harmless.
        root.join(rel).expect("join")
    }

    #[test]
    fn each_failure_has_a_distinct_key() {
        let err = io_err("boom");
        let chain = anyhow_err("boom");
        let decode_err = utf8_err();
        let status = oxdock_sys_test_utils::exit_status_from_code(1);
        let file = guarded("a.txt");
        let dir = guarded("sub");
        let plain = plain_path();
        let keys = [
            guard_access_escapes("READ", plain, plain).key(),
            guard_snapshot_not_materialized(plain).key(),
            refuses_symlink_final(&file).key(),
            refuses_parent_reference(&dir).key(),
            refuses_link_ancestor(plain).key(),
            remove_link_no_file_name("a").key(),
            remove_link_escapes_root("a").key(),
            metadata_unguarded_miri().key(),
            open_file_unguarded_miri().key(),
            spill_files_miri().key(),
            private_files_miri().key(),
            symlink_unsupported_miri(&file, &dir).key(),
            temp_lock_miri().key(),
            symlink_dest_exists(&file).key(),
            symlink_self_link(&file).key(),
            symlink_unsupported_platform(&file, &dir).key(),
            missing_file(&file).key(),
            unsupported_file_type(plain).key(),
            copy_source_missing(&file).key(),
            copy_source_unsupported_kind(&file).key(),
            copy_no_workspace_root().key(),
            copy_snapshot_not_materialized().key(),
            workdir_not_dir(&dir).key(),
            fd_path_failed(&err).key(),
            opened_outside_tree(&file, &dir, plain).key(),
            synthetic_state_missing_for_root().key(),
            synthetic_state_missing_for_build().key(),
            synthetic_dir_missing(&dir).key(),
            synthetic_stat_failed(&file).key(),
            synthetic_archive_member_symlink(&file).key(),
            synthetic_entry_kind_missing(&file).key(),
            synthetic_missing_file("a.txt").key(),
            synthetic_remove_dir_missing("sub").key(),
            synthetic_remove_dir_not_empty("sub").key(),
            synthetic_metadata_unsupported().key(),
            script_not_found("step.ox").key(),
            path_base_empty().key(),
            path_rel_empty().key(),
            path_rel_dotdot("a", "a/b").key(),
            path_rel_mixed("a", "/b").key(),
            path_rel_not_prefix("a", "b").key(),
            scratch_collided(plain).key(),
            cache_ensure_failed(plain, &err).key(),
            install_stage_failed(plain, &err).key(),
            install_set_mode_failed(plain, &err).key(),
            install_place_failed(plain, plain, &err).key(),
            install_no_parent(&dir).key(),
            install_failed("boom").key(),
            operation_failed("boom").key(),
            cwd_outside_snapshot_root(&dir).key(),
            unguarded_read_dir_miri().key(),
            mock_join_failed(&chain).key(),
            mock_metadata_unsupported().key(),
            mock_unguarded_unsupported().key(),
            mock_read_not_utf8(&decode_err).key(),
            mock_read_dir_unsupported().key(),
            mock_copy_unsupported().key(),
            mock_symlink_unsupported().key(),
            mock_open_unsupported().key(),
            mock_perms_unsupported().key(),
            mock_missing_path(&file).key(),
            mock_remove_no_file(&file).key(),
            mock_remove_no_dir(&dir).key(),
            mock_remove_not_empty(&dir).key(),
            mock_git_copy_unsupported().key(),
            git_identity_miri().key(),
            git_config_get_failed("user.name", status).key(),
            git_config_set_failed("user.name", status).key(),
            git_copy_outside_build_context().key(),
            git_ls_tree_failed("HEAD", "a.txt").key(),
            git_path_not_in_rev("a.txt", "HEAD").key(),
            git_show_failed("HEAD", "a.txt").key(),
            git_submodule_unsupported("sub").key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn shared_modes_keep_one_key() {
        let file = guarded("a.txt");
        let dir = guarded("sub");
        assert_eq!(
            symlink_dest_exists(&file).key(),
            symlink_dest_exists(&dir).key()
        );
        assert_eq!(
            remove_link_no_file_name("a").key(),
            remove_link_no_file_name(".").key()
        );
        assert_eq!(path_base_empty().key(), path_base_empty().key());
        assert_eq!(
            mock_metadata_unsupported().key(),
            mock_metadata_unsupported().key()
        );
        assert_eq!(
            mock_unguarded_unsupported().key(),
            mock_unguarded_unsupported().key()
        );
        assert_eq!(mock_copy_unsupported().key(), mock_copy_unsupported().key());
        assert_eq!(
            mock_remove_no_dir(&file).key(),
            mock_remove_no_dir(&dir).key()
        );
        assert_eq!(spill_files_miri().key(), spill_files_miri().key());
    }

    #[test]
    fn messages_match_canonical_wording() {
        let err = io_err("boom");
        let chain = anyhow_err("boom");
        let decode_err = utf8_err();
        let status = oxdock_sys_test_utils::exit_status_from_code(1);
        let file = guarded("a.txt");
        let dir = guarded("sub");
        let plain = plain_path();
        // Paramless and fixed messages stay byte-identical.
        assert_eq!(
            guard_access_escapes("READ", plain, plain).message(),
            "READ access to a/b.txt escapes allowed root a/b.txt"
        );
        assert_eq!(
            guard_snapshot_not_materialized(plain).message(),
            "snapshot root not materialized; resolve through the snapshot choke point first: a/b.txt"
        );
        assert_eq!(
            refuses_symlink_final(&file).message(),
            format!("refuses symlink final component: {}", file.display())
        );
        assert_eq!(
            refuses_parent_reference(&dir).message(),
            format!("refuses parent reference above {}", dir.display())
        );
        assert_eq!(
            refuses_link_ancestor(plain).message(),
            "refuses link ancestor a/b.txt"
        );
        assert_eq!(
            remove_link_no_file_name("a").message(),
            "remove_link denied for a: no file name"
        );
        assert_eq!(
            remove_link_escapes_root("a").message(),
            "remove_link denied for a: escapes root"
        );
        assert_eq!(
            metadata_unguarded_miri().message(),
            "metadata_unguarded not supported on Miri"
        );
        assert_eq!(
            open_file_unguarded_miri().message(),
            "open_file_unguarded not supported on Miri"
        );
        assert_eq!(
            spill_files_miri().message(),
            "spill files are not supported under Miri (memory-only)"
        );
        assert_eq!(
            private_files_miri().message(),
            "private files are not supported under Miri (memory-only)"
        );
        assert_eq!(
            symlink_unsupported_miri(&file, &dir).message(),
            format!(
                "SYMLINK unsupported under Miri ({} -> {})",
                file.display(),
                dir.display()
            )
        );
        assert_eq!(temp_lock_miri().message(), "temp lock unused under Miri");
        assert_eq!(
            symlink_dest_exists(&file).message(),
            format!("SYMLINK destination already exists: {}", file.display())
        );
        assert_eq!(
            symlink_self_link(&file).message(),
            format!(
                "SYMLINK source resolves to the destination itself: {}",
                file.display()
            )
        );
        assert_eq!(
            symlink_unsupported_platform(&file, &dir).message(),
            format!(
                "SYMLINK unsupported on this platform ({} -> {})",
                file.display(),
                dir.display()
            )
        );
        assert_eq!(
            missing_file(&file).message(),
            format!("missing file {}", file.display())
        );
        assert_eq!(
            unsupported_file_type(plain).message(),
            "unsupported file type: a/b.txt"
        );
        assert_eq!(
            copy_source_missing(&file).message(),
            format!("COPY source missing in build context: {}", file.display())
        );
        assert_eq!(
            copy_source_unsupported_kind(&file).message(),
            format!("COPY source unsupported kind: {}", file.display())
        );
        assert_eq!(
            copy_no_workspace_root().message(),
            "no workspace root set for workspace-relative COPY"
        );
        assert_eq!(
            copy_snapshot_not_materialized().message(),
            "COPY from SNAPSHOT requires a materialized snapshot"
        );
        assert_eq!(
            workdir_not_dir(&dir).message(),
            format!("WORKDIR path is not a directory: {}", dir.display())
        );
        assert_eq!(
            fd_path_failed(&err).message(),
            "failed to resolve open file path: boom"
        );
        assert_eq!(
            opened_outside_tree(&file, &dir, plain).message(),
            format!(
                "opened {} outside its expected tree {} (descriptor resolves to a/b.txt)",
                file.display(),
                dir.display()
            )
        );
        assert_eq!(
            synthetic_state_missing_for_root().message(),
            "synthetic state missing for root"
        );
        assert_eq!(
            synthetic_state_missing_for_build().message(),
            "synthetic state missing for build context"
        );
        assert_eq!(
            synthetic_dir_missing(&dir).message(),
            format!("directory does not exist: {}", dir.display())
        );
        assert_eq!(
            synthetic_stat_failed(&file).message(),
            format!("failed to stat {}", file.display())
        );
        assert_eq!(
            synthetic_archive_member_symlink(&file).message(),
            format!("archive member is a symlink: {}", file.display())
        );
        assert_eq!(
            synthetic_entry_kind_missing(&file).message(),
            format!("missing entry kind for {}", file.display())
        );
        assert_eq!(
            synthetic_missing_file("a.txt").message(),
            "missing file a.txt"
        );
        assert_eq!(
            synthetic_remove_dir_missing("sub").message(),
            "directory does not exist: sub"
        );
        assert_eq!(
            synthetic_remove_dir_not_empty("sub").message(),
            "directory not empty: sub"
        );
        assert_eq!(
            synthetic_metadata_unsupported().message(),
            "synthetic metadata unsupported under Miri on this platform"
        );
        assert_eq!(
            script_not_found("step.ox").message(),
            "script not found: step.ox"
        );
        assert_eq!(
            path_base_empty().message(),
            "PATH_BASE needs a non-empty path"
        );
        assert_eq!(
            path_rel_empty().message(),
            "PATH_REL needs non-empty base and path"
        );
        assert_eq!(
            path_rel_dotdot("a", "a/b").message(),
            "PATH_REL base \"a\" is not a prefix of \"a/b\": \"..\" segments never relativize"
        );
        assert_eq!(
            path_rel_mixed("a", "/b").message(),
            "PATH_REL base \"a\" is not a prefix of \"/b\": absolute and relative shapes never mix"
        );
        assert_eq!(
            path_rel_not_prefix("a", "b").message(),
            "PATH_REL base \"a\" is not a prefix of \"b\""
        );
        assert_eq!(
            scratch_collided(plain).message(),
            "scratch candidate collided (clock reuse?): a/b.txt"
        );
        assert_eq!(
            cache_ensure_failed(plain, &err).message(),
            "failed to ensure cache dir a/b.txt: boom"
        );
        assert_eq!(
            install_stage_failed(plain, &err).message(),
            "failed to stage install payload in a/b.txt: boom"
        );
        assert_eq!(
            install_set_mode_failed(plain, &err).message(),
            "failed to set mode on a/b.txt: boom"
        );
        assert_eq!(
            install_place_failed(plain, plain, &err).message(),
            "failed to rename a/b.txt to a/b.txt: boom"
        );
        assert_eq!(
            install_no_parent(&dir).message(),
            format!("install destination has no parent: {}", dir.display())
        );
        assert_eq!(install_failed("boom").message(), "boom");
        assert_eq!(operation_failed("boom").message(), "boom");
        assert_eq!(
            cwd_outside_snapshot_root(&dir).message(),
            format!("cwd outside snapshot root: {}", dir.display())
        );
        assert_eq!(
            unguarded_read_dir_miri().message(),
            "Unguarded read_dir not supported on Miri"
        );
        assert_eq!(mock_join_failed(&chain).message(), "boom");
        assert_eq!(
            mock_metadata_unsupported().message(),
            "metadata not supported in mock fs"
        );
        assert_eq!(
            mock_unguarded_unsupported().message(),
            "unguarded operations not supported in mock fs"
        );
        assert_eq!(
            mock_read_not_utf8(&decode_err).message(),
            decode_err.to_string()
        );
        assert_eq!(
            mock_read_dir_unsupported().message(),
            "read_dir unsupported in mock fs"
        );
        assert_eq!(mock_copy_unsupported().message(), "copy unsupported");
        assert_eq!(mock_symlink_unsupported().message(), "symlink unsupported");
        assert_eq!(mock_open_unsupported().message(), "open unsupported");
        assert_eq!(mock_perms_unsupported().message(), "perms unsupported");
        assert_eq!(
            mock_missing_path(&file).message(),
            format!("missing path {}", file.display())
        );
        assert_eq!(
            mock_remove_no_file(&file).message(),
            format!("REMOVE {}: no such file", file.display())
        );
        assert_eq!(
            mock_remove_no_dir(&dir).message(),
            format!("REMOVE {}: no such directory", dir.display())
        );
        assert_eq!(
            mock_remove_not_empty(&dir).message(),
            format!("REMOVE {}: directory not empty", dir.display())
        );
        assert_eq!(
            mock_git_copy_unsupported().message(),
            "git copy unsupported"
        );
        assert_eq!(
            git_identity_miri().message(),
            "git identity helpers are not supported under Miri"
        );
        assert_eq!(
            git_config_get_failed("user.name", status).message(),
            format!("git config --get user.name exited with {status}")
        );
        assert_eq!(
            git_config_set_failed("user.name", status).message(),
            format!("git config user.name failed with {status}")
        );
        assert_eq!(
            git_copy_outside_build_context().message(),
            "COPY_GIT source must resolve within build context"
        );
        assert_eq!(
            git_ls_tree_failed("HEAD", "a.txt").message(),
            "git ls-tree failed for HEAD:a.txt"
        );
        assert_eq!(
            git_path_not_in_rev("a.txt", "HEAD").message(),
            "path a.txt not found in rev HEAD"
        );
        assert_eq!(
            git_show_failed("HEAD", "a.txt").message(),
            "git show failed for HEAD:a.txt"
        );
        assert_eq!(
            git_submodule_unsupported("sub").message(),
            "submodule entries are not supported: sub"
        );
    }

    #[test]
    fn from_anyhow_reclaims_keyed_payload_with_frames() {
        let file = guarded("a.txt");
        let keyed = missing_file(&file);
        let key = keyed.key().to_string();
        let framed = anyhow::Error::new(keyed).context("outer frame");
        let reclaimed = from_anyhow(framed);
        assert_eq!(reclaimed.key(), key);
        assert_eq!(reclaimed.contexts(), &["outer frame".to_string()]);
        let bare = from_anyhow(anyhow::Error::msg("boom"));
        assert_eq!(bare.key(), "fs.operation.failed");
        assert_eq!(bare.message(), "boom");
    }
}
