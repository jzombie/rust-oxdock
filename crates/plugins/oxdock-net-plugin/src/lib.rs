//! TCP networking for OxDock scripts.
//!
//! This crate exposes a `NET` host module through the [`Engine`]
//! facade: `NET_LISTEN` claims a runner-mapped service endpoint and
//! reports its address, `NET_ACCEPT` pumps one connection through
//! explicit DSL pipes, `NET_CLOSE` tears the listener down, and
//! `NET_CONNECT` dials out or joins a memory session. Unmapped services
//! rendezvous in memory; scripts never bind. It also owns the
//! endpoint-validation helpers and the virtual endpoint registry shared
//! by host plugins, so resolution stays literally one code path
//! everywhere.
//!
//! [`Engine`]: oxdock_core::Engine

mod bridge;
mod endpoints;
mod fetch;
mod funcs;
pub mod remote;
mod state;
mod types;
pub mod validate;

pub use endpoints::{
    AcquiredListener, BindingSpec, EndpointRegistry, MEMORY_QUEUE_CAP, MemoryPipePair,
    ServiceTarget, SlotKind, SocketRequirement, acquire_listener, classify_service_call,
    no_cli_mapping, resolve_service_target,
};
pub use funcs::{module_with, module_with_endpoints};
use oxdock_core::HostModule;
pub use oxdock_process::DefaultProcessManager;
pub use remote::{
    RemoteInventory, RemoteTarget, SessionConfig, StdioSession, module_hash, parse_remote_arg,
};

pub use types::NetListenerTag;
pub use validate::{
    EndpointKey, Protocol, VirtualEndpoint, parse_endpoint_ref, parse_virtual_endpoint,
};

/// The `NET` host module with the default process manager.
pub fn module() -> HostModule<DefaultProcessManager> {
    module_with()
}
