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
mod run;
mod runtime;
mod state;
mod types;
mod validate;

pub use funcs::{module_with, module_with_endpoints};
use oxdock_core::HostModule;
pub use oxdock_process::DefaultProcessManager;

pub use types::{SshServerTag, SshSessionTag};

/// Generate a random password (256 bits, hex) for one server.
/// Committed scripts and tests carry no usable credential: generate
/// at runtime, hand the value to both sides, drop it after.
pub fn generate_password() -> String {
    use aes_gcm::aead::Generate;
    use aes_gcm::{Aes256Gcm, Key};

    let key = Key::<Aes256Gcm>::generate();
    hex::encode(key)
}

/// The `SSH` host module with the default process manager.
pub fn module() -> HostModule<DefaultProcessManager> {
    module_with()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_passwords_are_unique_hex() {
        let a = generate_password();
        let b = generate_password();
        assert_eq!(a.len(), 64, "256 bits render as 64 hex chars");
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit()),
            "hex only, safe in scripts and shells"
        );
        assert_ne!(a, b, "two draws must differ");
    }
}
