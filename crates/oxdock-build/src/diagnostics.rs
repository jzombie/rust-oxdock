//! Build-script asset errors behind single constructors with stable keys.
//!
//! This module owns every build template and key (`build.*`). It builds on
//! the generic [`oxdock_diagnostics::Diagnostic`] transport but owns its
//! wording: changing build text happens here, never at the call sites.
//! Call sites pass structured data only, never `format!` strings.
//! Chain passthroughs render `{reason:#}` inline (never `with_source`,
//! since `Display` does not render `source` and `anyhow` would print a
//! chained source twice).

use oxdock_diagnostics::Diagnostic;
use oxdock_fs::env as oxdock_env;

/// Structured field keys. Single spelling for the whole module:
/// constructors reference these, never literals, so a key rename is
/// one edit.
const FIELD_OUT_DIR: &str = "out_dir";
const FIELD_REL: &str = "rel";
const FIELD_REASON: &str = "reason";
const FIELD_TARGET: &str = "target";
const FIELD_WORKDIR: &str = "workdir";

/// Asset collection from the materialized tree failed. Takes the
/// underlying failure; `{reason:#}` formatting lives here so call
/// sites stay `format!`-free.
pub fn collect_assets_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to collect assets: {reason_text}");
    Diagnostic::new(
        "build.assets.collect_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// `$OUT_DIR` does not name a guarded root. Takes the raw value plus
/// the underlying failure; the `OUT_DIR` spelling derives from the
/// declared env contract, never a literal.
pub fn invalid_out_dir(out: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!(
        "invalid {} {out}: {reason_text}",
        oxdock_env::cargo::build::OUT_DIR,
    );
    Diagnostic::new(
        "build.out_dir.invalid",
        message,
        Diagnostic::make_fields(&[(FIELD_OUT_DIR, out), (FIELD_REASON, &reason_text)]),
    )
}

/// Generated module path rejected by the guard. Takes the underlying
/// failure; `{reason:#}` formatting lives here.
pub fn module_path_rejected(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("generated module path rejected: {reason_text}");
    Diagnostic::new(
        "build.module.path_rejected",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// DSL file path escapes the manifest dir. Takes the relative path
/// plus the underlying failure; quoting lives here.
pub fn dsl_path_escapes_manifest(rel: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("DSL path {rel} escapes manifest dir: {reason_text}");
    Diagnostic::new(
        "build.script.escapes_manifest",
        message,
        Diagnostic::make_fields(&[(FIELD_REL, rel), (FIELD_REASON, &reason_text)]),
    )
}

/// DSL text failed to parse. Takes the underlying failure;
/// `{reason:#}` formatting lives here so call sites stay
/// `format!`-free. Shared by both parse sites (one key, one mode).
pub fn script_parse_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("parse error: {reason_text}");
    Diagnostic::new(
        "build.script.parse_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// DSL execution failed. Takes the underlying failure; the full
/// chain (`{reason:#}`) lives here.
pub fn script_execution_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("execution error: {reason_text}");
    Diagnostic::new(
        "build.script.execution_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// Final workdir missing after the build. Takes the display path plus
/// the underlying failure; rendering lives here.
pub fn final_workdir_missing(workdir: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("final workdir missing after build: {workdir} ({reason_text})");
    Diagnostic::new(
        "build.workdir.missing",
        message,
        Diagnostic::make_fields(&[(FIELD_WORKDIR, workdir), (FIELD_REASON, &reason_text)]),
    )
}

/// Final workdir exists but is not a directory. Takes the display
/// path; rendering lives here.
pub fn final_workdir_not_dir(workdir: &str) -> Diagnostic {
    let message = format!("final workdir is not a directory: {workdir}");
    Diagnostic::new(
        "build.workdir.not_dir",
        message,
        Diagnostic::make_fields(&[(FIELD_WORKDIR, workdir)]),
    )
}

/// `$OUT_DIR` destination exists but is not a directory. Takes the
/// display path; the `OUT_DIR` spelling derives from the declared env
/// contract, never a literal.
pub fn out_dir_target_not_dir(target: &str) -> Diagnostic {
    let message = format!(
        "{} destination exists but is not a directory: {target}",
        oxdock_env::cargo::build::OUT_DIR,
    );
    Diagnostic::new(
        "build.out_dir.not_dir",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target)]),
    )
}

/// Materialize target could not be created. Takes the display path
/// plus the underlying failure; rendering lives here.
pub fn out_dir_create_failed(target: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to create {target}: {reason_text}");
    Diagnostic::new(
        "build.out_dir.create_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target), (FIELD_REASON, &reason_text)]),
    )
}

/// `$OUT_DIR` is unset (not running as a build script). Takes the
/// concrete env failure (chainless, so `{reason:#}` equals the
/// `Display`); the `OUT_DIR` spelling derives from the declared env
/// contract, never a literal.
pub fn out_dir_missing(reason: &std::env::VarError) -> Diagnostic {
    let reason_text = reason.to_string();
    let message = format!(
        "{} missing (not running as a build script?): {reason_text}",
        oxdock_env::cargo::build::OUT_DIR,
    );
    Diagnostic::new(
        "build.out_dir.missing",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// Manifest dir unavailable when resolving a file-based DSL source.
/// Takes the wrapped resolver failure; the chain renders inline.
/// Distinct key from the sibling manifest-dir ctors below: the retired
/// context wordings differ, and bytes stay identical.
pub fn manifest_dir_missing_for_dsl_resolution(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!(
        "{} missing for DSL resolution: {reason_text}",
        oxdock_env::cargo::MANIFEST_DIR,
    );
    Diagnostic::new(
        "build.manifest_dir.missing_for_dsl_resolution",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// Manifest dir unavailable when computing rerun directives. Takes
/// the concrete env failure; rendering lives here. Distinct key:
/// the retired wording carries the build-script hint.
pub fn manifest_dir_missing_for_directives(reason: &std::env::VarError) -> Diagnostic {
    let reason_text = reason.to_string();
    let message = format!(
        "{} missing (not running as a build script?): {reason_text}",
        oxdock_env::cargo::MANIFEST_DIR,
    );
    Diagnostic::new(
        "build.manifest_dir.missing_for_directives",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// Manifest dir unavailable when materializing. Takes the wrapped
/// resolver failure; the chain renders inline. Distinct key: the
/// retired wording is the bare missing text.
pub fn manifest_dir_missing(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("{} missing: {reason_text}", oxdock_env::cargo::MANIFEST_DIR,);
    Diagnostic::new(
        "build.manifest_dir.missing",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// DSL source file unreadable. Takes the relative path plus the
/// wrapped filesystem failure; quoting lives here, the chain renders
/// inline.
pub fn dsl_script_read_failed(rel: &str, reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to read DSL script {rel}: {reason_text}");
    Diagnostic::new(
        "build.script.read_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REL, rel), (FIELD_REASON, &reason_text)]),
    )
}

/// Workspace root discovery failed. Takes the wrapped failure; the
/// chain renders inline.
pub fn workspace_root_discover_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to discover workspace root: {reason_text}");
    Diagnostic::new(
        "build.workspace_root.discover_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

/// Built tree could not be copied into staging. Takes the underlying
/// failure; `{reason:#}` formatting lives here.
pub fn stage_copy_failed(reason: &anyhow::Error) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("failed to copy build output into staging: {reason_text}");
    Diagnostic::new(
        "build.stage.copy_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_REASON, &reason_text)]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var_err() -> std::env::VarError {
        std::env::var("OXDOCK_BUILD_TEST_DEFINITELY_UNSET_VAR").unwrap_err()
    }

    #[test]
    fn each_failure_has_a_distinct_key() {
        let err = anyhow::anyhow!("boom");
        let keys = [
            collect_assets_failed(&err).key(),
            invalid_out_dir("out", &err).key(),
            module_path_rejected(&err).key(),
            dsl_path_escapes_manifest("a.oxdock", &err).key(),
            script_parse_failed(&err).key(),
            script_execution_failed(&err).key(),
            final_workdir_missing("/tmp/w", &err).key(),
            final_workdir_not_dir("/tmp/w").key(),
            out_dir_target_not_dir("/tmp/t").key(),
            out_dir_create_failed("/tmp/t", &err).key(),
            out_dir_missing(&var_err()).key(),
            manifest_dir_missing_for_dsl_resolution(&err).key(),
            manifest_dir_missing_for_directives(&var_err()).key(),
            manifest_dir_missing(&err).key(),
            dsl_script_read_failed("a.oxdock", &err).key(),
            workspace_root_discover_failed(&err).key(),
            stage_copy_failed(&err).key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn messages_match_canonical_wording() {
        let err = anyhow::anyhow!("boom");
        assert_eq!(
            collect_assets_failed(&err).message(),
            "failed to collect assets: boom"
        );
        assert_eq!(
            invalid_out_dir("out", &err).message(),
            "invalid OUT_DIR out: boom"
        );
        assert_eq!(
            module_path_rejected(&err).message(),
            "generated module path rejected: boom"
        );
        assert_eq!(
            dsl_path_escapes_manifest("a.oxdock", &err).message(),
            "DSL path a.oxdock escapes manifest dir: boom"
        );
        assert_eq!(script_parse_failed(&err).message(), "parse error: boom");
        assert_eq!(
            script_execution_failed(&err).message(),
            "execution error: boom"
        );
        assert_eq!(
            final_workdir_missing("/tmp/w", &err).message(),
            "final workdir missing after build: /tmp/w (boom)"
        );
        assert_eq!(
            final_workdir_not_dir("/tmp/w").message(),
            "final workdir is not a directory: /tmp/w"
        );
        assert_eq!(
            out_dir_target_not_dir("/tmp/t").message(),
            "OUT_DIR destination exists but is not a directory: /tmp/t"
        );
        assert_eq!(
            out_dir_create_failed("/tmp/t", &err).message(),
            "failed to create /tmp/t: boom"
        );
        assert_eq!(
            out_dir_missing(&var_err()).message(),
            "OUT_DIR missing (not running as a build script?): environment variable not found"
        );
        assert_eq!(
            manifest_dir_missing_for_dsl_resolution(&err).message(),
            "CARGO_MANIFEST_DIR missing for DSL resolution: boom"
        );
        assert_eq!(
            manifest_dir_missing_for_directives(&var_err()).message(),
            "CARGO_MANIFEST_DIR missing (not running as a build script?): environment variable not found"
        );
        assert_eq!(
            manifest_dir_missing(&err).message(),
            "CARGO_MANIFEST_DIR missing: boom"
        );
        assert_eq!(
            dsl_script_read_failed("a.oxdock", &err).message(),
            "failed to read DSL script a.oxdock: boom"
        );
        assert_eq!(
            workspace_root_discover_failed(&err).message(),
            "failed to discover workspace root: boom"
        );
        assert_eq!(
            stage_copy_failed(&err).message(),
            "failed to copy build output into staging: boom"
        );
    }

    #[test]
    fn out_dir_spelling_follows_env_contract() {
        let err = anyhow::anyhow!("boom");
        assert!(
            invalid_out_dir("out", &err)
                .message()
                .contains(oxdock_env::cargo::build::OUT_DIR)
        );
        assert!(
            out_dir_target_not_dir("/tmp/t")
                .message()
                .contains(oxdock_env::cargo::build::OUT_DIR)
        );
        assert!(
            out_dir_missing(&var_err())
                .message()
                .contains(oxdock_env::cargo::build::OUT_DIR)
        );
        assert!(
            manifest_dir_missing_for_dsl_resolution(&err)
                .message()
                .contains(oxdock_env::cargo::MANIFEST_DIR)
        );
        assert!(
            manifest_dir_missing_for_directives(&var_err())
                .message()
                .contains(oxdock_env::cargo::MANIFEST_DIR)
        );
        assert!(
            manifest_dir_missing(&err)
                .message()
                .contains(oxdock_env::cargo::MANIFEST_DIR)
        );
    }
}
