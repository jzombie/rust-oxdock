//! Ephemeral user-space SSH for OxDock scripts.
//!
//! This crate exposes an `SSH` host module through the [`Engine`]
//! facade: `SSH_SERVE` spins up a loopback SSH server with ephemeral
//! credentials held only in memory, while `SSH_ACCEPT`, `SSH_CONNECT`
//! and `SSH_PUMP` bridge sessions through explicit DSL pipes. No OS
//! users, no `sshd_config`, and no Docker are involved; killing the
//! process removes every trace.
//!
//! [`Engine`]: oxdock_core::Engine

mod auth;
mod bridge;
mod funcs;
mod pty;
mod runtime;
mod state;
mod types;
mod validate;

pub use funcs::module_with;
use oxdock_core::HostModule;
pub use oxdock_process::DefaultProcessManager;

pub use types::SshServerTag;

/// The `SSH` host module with the default process manager.
pub fn module() -> HostModule<DefaultProcessManager> {
    module_with()
}
