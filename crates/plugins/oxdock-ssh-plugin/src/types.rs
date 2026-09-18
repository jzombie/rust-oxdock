//! The `SSH_SERVER` opaque handle type.
//!
//! The value carries shared server lifetime state and nothing secret:
//! credentials live only on the runtime thread. Display and Debug
//! redact everything but the id and address.

use std::fmt;
use std::sync::Arc;

use oxdock_func_macro::oxdock_type;

use crate::state::ServerState;

/// Handle to one ephemeral SSH server instance.
///
/// Minted by `SSH_SERVE`, consumed by `SSH_ACCEPT` and `SSH_CLOSE`.
/// Cloning the value shares the server; dropping the last clone
/// signals shutdown.
#[oxdock_type(name = "SSH_SERVER")]
#[derive(Debug, Clone)]
pub struct SshServerTag {
    state: Arc<ServerState>,
}

impl SshServerTag {
    pub fn new(state: Arc<ServerState>) -> Self {
        Self { state }
    }

    pub fn state(&self) -> &Arc<ServerState> {
        &self.state
    }
}

impl PartialEq for SshServerTag {
    fn eq(&self, other: &Self) -> bool {
        self.state.id() == other.state.id()
    }
}

impl fmt::Display for SshServerTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SSH_SERVER({}, {})",
            self.state.id(),
            self.state.local_addr()
        )
    }
}
