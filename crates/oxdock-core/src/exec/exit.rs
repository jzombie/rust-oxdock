//! Typed EXIT request: `EXIT <code>` aborts the pipeline, and the code
//! must survive every transport it crosses (anyhow chains, `ASYNC` thread
//! boundaries, the `REMOTE` wire protocol) so the CLI can relay it to the
//! process exit status. The display string matches the historic message so
//! existing assertions keep passing; new code should match on the type.

use std::fmt::{Display, Formatter, Result as FmtResult};

/// Pipeline exit request carrying the numeric code. Produced by `EXIT`,
/// re-raised by `REMOTE` when the guest exits, and mapped to the process
/// exit status by the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitRequest(pub i64);

impl Display for ExitRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "EXIT requested with code {}", self.0)
    }
}

impl std::error::Error for ExitRequest {}

/// Outermost `EXIT` code in the chain, if any. Relays preserve the code
/// unchanged, so first-found and last-found agree; outermost wins because
/// it is the most recent relay decision.
pub fn exit_code_of(err: &anyhow::Error) -> Option<i64> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<ExitRequest>().map(|exit| exit.0))
}

#[cfg(test)]
mod tests {
    use super::{ExitRequest, exit_code_of};

    #[test]
    fn display_matches_historic_message() {
        assert_eq!(ExitRequest(3).to_string(), "EXIT requested with code 3");
    }

    #[test]
    fn finds_code_through_context_wrappers() {
        let err = anyhow::Error::new(ExitRequest(3)).context("REMOTE 'prod' failed");
        assert_eq!(exit_code_of(&err), Some(3));
    }

    #[test]
    fn none_without_exit() {
        let err = anyhow::anyhow!("boom").context("wrapped");
        assert_eq!(exit_code_of(&err), None);
    }
}
