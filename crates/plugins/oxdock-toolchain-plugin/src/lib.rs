//! Portable OxDock builder for OxDock scripts.
//!
//! This crate exposes a `TOOLCHAIN` host module through the [`Engine`]
//! facade: `TOOLCHAIN_ENSURE` provisions the pinned Rust toolchain for
//! a target triple under the dedicated cache group, and
//! `TOOLCHAIN_BUILD` compiles staged source with the cached toolchain
//! only (auto-provisioning when the binaries are missing). Nothing
//! reads the local repo as toolchain source and nothing writes into
//! it; every artifact lives under `<cache-root>/toolchain`.
//!
//! [`Engine`]: oxdock_core::Engine

mod build;
mod fingerprint;
mod funcs;
mod provision;
mod targets;

pub use funcs::module_with;
use oxdock_core::HostModule;
pub use oxdock_process::DefaultProcessManager;
pub use targets::{host_exe_name, host_triple, target_exe_name};

/// The `TOOLCHAIN` host module with the default process manager.
pub fn module() -> HostModule<DefaultProcessManager> {
    module_with()
}
