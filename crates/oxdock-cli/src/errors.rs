//! CLI user-facing errors behind single constructors with stable keys.
//!
//! This module owns every CLI template and key (`cli.*`). It builds on
//! the generic [`oxdock_diagnostics::Diagnostic`] transport but owns its
//! wording: changing CLI text happens here, never in the leaf crate.
//! DSL diagnostics stay owned by the DSL crates.

pub use oxdock_diagnostics::Diagnostic;

/// `--listen`/`-p` without the `net` feature.
pub fn feature_required(flag: &str) -> anyhow::Error {
    let message = format!("{flag} requires the `net` feature (rebuild with --features net)");
    let diag = Diagnostic::new(
        "cli.feature_required",
        message,
        Diagnostic::make_fields(&[("flag", flag)]),
    );
    anyhow::Error::new(diag)
}

/// `--offline` combined with socket flags.
pub fn offline_conflict() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.offline_conflict",
        "--offline conflicts with --listen/-p (offline runs open no sockets)".to_string(),
        Diagnostic::make_fields(&[]),
    );
    anyhow::Error::new(diag)
}

/// Same virtual endpoint mapped twice.
pub fn endpoint_duplicate_inner(endpoint: &str) -> anyhow::Error {
    let message = format!("virtual endpoint '{endpoint}' is mapped twice");
    let diag = Diagnostic::new(
        "cli.endpoint_duplicate_inner",
        message,
        Diagnostic::make_fields(&[("endpoint", endpoint)]),
    );
    anyhow::Error::new(diag)
}

/// Same outer socket address mapped twice.
pub fn endpoint_duplicate_outer(addr: &str) -> anyhow::Error {
    let message = format!("outer socket address {addr} is mapped twice");
    let diag = Diagnostic::new(
        "cli.endpoint_duplicate_outer",
        message,
        Diagnostic::make_fields(&[("address", addr)]),
    );
    anyhow::Error::new(diag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_message_shared_across_three_sites() {
        for flag in ["--listen", "-p", "script module"] {
            let err = feature_required(flag);
            let diag = err
                .downcast_ref::<Diagnostic>()
                .expect("CLI errors stay typed");
            assert_eq!(diag.key(), "cli.feature_required");
            assert!(diag.message().contains(flag), "{diag}");
        }
    }

    #[test]
    fn endpoint_duplicates_name_the_key() {
        let inner = endpoint_duplicate_inner("2251");
        let diag = inner
            .downcast_ref::<Diagnostic>()
            .expect("typed CLI error");
        assert_eq!(diag.key(), "cli.endpoint_duplicate_inner");

        let outer = endpoint_duplicate_outer("127.0.0.1:2222");
        let diag = outer
            .downcast_ref::<Diagnostic>()
            .expect("typed CLI error");
        assert_eq!(diag.key(), "cli.endpoint_duplicate_outer");
    }

    #[test]
    fn untrusted_input_cannot_crash_cli_errors() {
        let evil = "bad\u{1f}flag".to_string();
        let err = feature_required(&evil);
        let diag = err.downcast_ref::<Diagnostic>().expect("typed");
        assert!(!diag.message().contains('\u{1f}'));
    }
}
