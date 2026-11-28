//! Typed EXIT request: `EXIT <code>` aborts the pipeline, and the code
//! must survive every transport it crosses (anyhow chains, `ASYNC` thread
//! boundaries, the `REMOTE` wire protocol) so the CLI can relay it to the
//! process exit status. The display string matches the historic message so
//! existing assertions keep passing; new code should match on the type.

use std::fmt::{Display, Formatter, Result as FmtResult};
use std::sync::Arc;

use oxdock_diagnostics::Diagnostic;
use oxdock_parser::KEYWORD_EXIT;

/// Pipeline exit request carrying the numeric code. Produced by `EXIT`,
/// re-raised by `REMOTE` when the guest exits, and mapped to the process
/// exit status by the CLI.
///
/// Carries an optional chained source: when an exit code attaches to a
/// diagnostic that already has a cause, the prior cause rides behind
/// the request instead of being dropped, so `exit_code_of` finds the
/// code first while causal inspection still reaches the cause.
#[derive(Debug, Clone)]
pub struct ExitRequest {
    code: i64,
    source: Option<Arc<dyn std::error::Error + Send + Sync>>,
}

impl ExitRequest {
    /// Bare exit request with no chained cause.
    pub fn new(code: i64) -> Self {
        Self { code, source: None }
    }

    /// Exit request chained over a prior cause. The code stays the
    /// outermost typed node; the cause remains reachable through
    /// `Error::source`.
    pub fn with_source(code: i64, source: Arc<dyn std::error::Error + Send + Sync>) -> Self {
        Self {
            code,
            source: Some(source),
        }
    }

    /// Numeric exit code.
    pub fn code(&self) -> i64 {
        self.code
    }
}

impl PartialEq for ExitRequest {
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code
    }
}

impl Eq for ExitRequest {}

impl Display for ExitRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{KEYWORD_EXIT} requested with code {}", self.code)
    }
}

impl std::error::Error for ExitRequest {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|cause| cause as &(dyn std::error::Error + 'static))
    }
}

/// Outermost `EXIT` code in the chain, if any. Relays preserve the code
/// unchanged, so first-found and last-found agree; outermost wins because
/// it is the most recent relay decision. Matches both the typed
/// `ExitRequest` carrier and the keyed `dsl.eval.exit_requested`
/// diagnostic (code field, no source so `{err:#}` renders once).
pub fn exit_code_of(err: &anyhow::Error) -> Option<i64> {
    err.chain().find_map(|cause| {
        if let Some(exit) = cause.downcast_ref::<ExitRequest>() {
            return Some(exit.code());
        }
        if let Some(diag) = cause.downcast_ref::<Diagnostic>() {
            return super::diagnostics::eval_exit_requested_code(diag);
        }
        None
    })
}

/// Outermost `EXIT` code in a `Diagnostic` source chain, if any.
/// Mirrors [`exit_code_of`] for the Diagnostic-native path: walks
/// `std::error::Error::source` links looking for `ExitRequest` or the
/// keyed exit-requested diagnostic. Snapshot and step wrappers attach
/// the request via `with_source`, so the code survives without
/// flattening the message chain.
pub fn exit_code_of_diag(err: &Diagnostic) -> Option<i64> {
    let mut current: Option<&(dyn std::error::Error + 'static)> =
        Some(err as &(dyn std::error::Error + 'static));
    while let Some(node) = current {
        if let Some(exit) = node.downcast_ref::<ExitRequest>() {
            return Some(exit.code());
        }
        if let Some(diag) = node.downcast_ref::<Diagnostic>()
            && let Some(code) = super::diagnostics::eval_exit_requested_code(diag)
        {
            return Some(code);
        }
        current = node.source();
    }
    None
}

/// Numeric code for a platform wait status, when the platform reports
/// one. Signal terminations yield `None`; callers fall back to the
/// platform `Display` for that case only, never hardcoding either
/// rendering in templates or tests.
pub fn exit_status_code(status: &std::process::ExitStatus) -> Option<i32> {
    status.code()
}

/// Single owner for wait-status rendering in diagnostics. Prefers the
/// numeric code when the platform reports one, otherwise uses the
/// platform `Display` (signal termination). Templates store the
/// numeric code as a field; tests pin the numeric prefix only.
pub fn describe_exit_status(status: &std::process::ExitStatus) -> String {
    match exit_status_code(status) {
        Some(code) => code.to_string(),
        None => status.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{ExitRequest, exit_code_of, exit_code_of_diag};
    use oxdock_diagnostics::Diagnostic;
    use std::sync::Arc;

    #[test]
    fn display_matches_historic_message() {
        assert_eq!(
            ExitRequest::new(3).to_string(),
            "EXIT requested with code 3"
        );
    }

    #[test]
    fn finds_code_through_context_wrappers() {
        let err = anyhow::Error::new(ExitRequest::new(3)).context("REMOTE 'prod' failed");
        assert_eq!(exit_code_of(&err), Some(3));
    }

    #[test]
    fn none_without_exit() {
        let err = anyhow::anyhow!("boom").context("wrapped");
        assert_eq!(exit_code_of(&err), None);
    }

    #[test]
    fn diag_finds_code_through_source_chain() {
        let inner = Diagnostic::new(
            "dsl.eval.async_cancelled",
            "ASYNC task cancelled".to_string(),
            Diagnostic::make_fields(&[]),
        )
        .with_source(Arc::new(ExitRequest::new(7)));
        assert_eq!(exit_code_of_diag(&inner), Some(7));
    }

    #[test]
    fn chained_exit_keeps_code_and_prior_cause() {
        use std::error::Error as _;
        let cause = Arc::new(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        let exit = ExitRequest::with_source(7, cause);
        assert_eq!(exit.code(), 7);
        let prior = exit
            .source()
            .expect("prior cause rides behind the request")
            .downcast_ref::<std::io::Error>()
            .expect("prior cause keeps its type");
        assert_eq!(prior.kind(), std::io::ErrorKind::NotFound);
        let diag = Diagnostic::new(
            "dsl.eval.async_cancelled",
            "ASYNC task cancelled".to_string(),
            Diagnostic::make_fields(&[]),
        )
        .with_source(Arc::new(exit));
        assert_eq!(exit_code_of_diag(&diag), Some(7));
        assert!(
            diag.source()
                .and_then(|cause| cause.downcast_ref::<ExitRequest>())
                .is_some()
        );
        let carried = exit_code_of(&anyhow::Error::new(diag));
        assert_eq!(carried, Some(7));
        assert_eq!(
            ExitRequest::new(7),
            ExitRequest::with_source(
                7,
                Arc::new(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"))
            )
        );
    }

    #[test]
    fn diag_none_without_exit() {
        let diag = Diagnostic::new(
            "dsl.eval.async_cancelled",
            "ASYNC task cancelled".to_string(),
            Diagnostic::make_fields(&[]),
        );
        assert_eq!(exit_code_of_diag(&diag), None);
    }

    #[test]
    fn keyed_exit_requested_carries_code_without_source() {
        use std::error::Error as _;
        let diag = crate::exec::diagnostics::eval_exit_requested(7);
        assert_eq!(diag.key(), "dsl.eval.exit_requested");
        assert_eq!(diag.message(), "EXIT requested with code 7");
        assert_eq!(diag.fields().get("code"), Some(&"7".to_string()));
        assert!(
            diag.source().is_none(),
            "no identical source to duplicate rendering"
        );
        assert_eq!(exit_code_of_diag(&diag), Some(7));
        let carried = exit_code_of(&anyhow::Error::new(diag).context("step 2"));
        assert_eq!(carried, Some(7));
    }

    #[test]
    fn describe_prefers_numeric_code() {
        let status = oxdock_sys_test_utils::exit_status_from_code(3);
        assert_eq!(super::describe_exit_status(&status), "3");
        assert_eq!(super::exit_status_code(&status), Some(3));
    }
}
