//! The `NET_LISTENER` opaque handle type.
//!
//! The value carries shared listener lifetime state and nothing secret.
//! Display shows id and address.

use std::fmt;
use std::sync::Arc;

use oxdock_func_macro::oxdock_type;

use crate::state::ListenerState;

/// Handle to one `NET_LISTEN` listener instance.
///
/// Minted by `NET_LISTEN`, consumed by `NET_ACCEPT` and `NET_CLOSE`.
/// Cloning the value shares the listener; dropping the last clone
/// signals shutdown.
#[oxdock_type(name = "NET_LISTENER")]
#[derive(Debug, Clone)]
pub struct NetListenerTag {
    state: Arc<ListenerState>,
}

impl NetListenerTag {
    pub fn new(state: Arc<ListenerState>) -> Self {
        Self { state }
    }

    pub fn state(&self) -> &Arc<ListenerState> {
        &self.state
    }
}

impl PartialEq for NetListenerTag {
    fn eq(&self, other: &Self) -> bool {
        self.state.id() == other.state.id()
    }
}

impl fmt::Display for NetListenerTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "NET_LISTENER({}, {})",
            self.state.id(),
            self.state.addr_text()
        )
    }
}
