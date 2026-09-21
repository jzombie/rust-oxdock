//! Ephemeral user-space SSH for OxDock scripts.
//!
//! This crate exposes an `SSH` host module through the [`Engine`]
//! facade: `SSH_SERVE` spins up a loopback SSH server with ephemeral
//! credentials held only in memory, while `SSH_DEQUEUE` exposes each
//! session's metadata before `SSH_PUMP_CHANNEL` bridges it through
//! explicit DSL pipes (`SSH_ACCEPT` combines both for simple loops).
//! No OS users, no `sshd_config`, and no Docker are involved; killing
//! the process removes every trace.
//!
//! [`Engine`]: oxdock_core::Engine

mod auth;
mod bridge;
mod funcs;
mod keys;
mod pty;
mod runtime;
mod state;
mod types;
mod validate;

pub use funcs::{module_with, module_with_endpoints};
use oxdock_core::HostModule;
pub use oxdock_process::DefaultProcessManager;

pub use types::{SshServerTag, SshSessionTag};

/// The `SSH` host module with the default process manager.
pub fn module() -> HostModule<DefaultProcessManager> {
    module_with()
}
