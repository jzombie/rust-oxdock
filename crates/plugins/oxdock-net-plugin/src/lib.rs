//! TCP networking for OxDock scripts.
//!
//! This crate exposes a `NET` host module through the [`Engine`]
//! facade: `NET_LISTEN` claims a virtual service endpoint (mapped to a
//! physical bind by the host runner, loopback by default) and reports its
//! address, `NET_ACCEPT` pumps one connection through explicit DSL pipes,
//! `NET_CLOSE` tears the listener down, and `NET_CONNECT` dials out or
//! joins a memory session. It also owns the endpoint-validation helpers
//! and the virtual endpoint registry shared by host plugins, so shapes
//! stay identical everywhere.
//!
//! [`Engine`]: oxdock_core::Engine

mod bridge;
mod endpoints;
mod funcs;
mod state;
mod types;
pub mod validate;

pub use endpoints::{
    AcquiredListener, BindingSpec, EndpointRegistry, MEMORY_QUEUE_CAP, MemoryPipePair, SlotKind,
    acquire_listener,
};
pub use funcs::{module_with, module_with_endpoints};
use oxdock_core::HostModule;
pub use oxdock_process::DefaultProcessManager;

pub use types::NetListenerTag;
pub use validate::{
    EndpointKey, Protocol, VirtualEndpoint, parse_endpoint_ref, parse_virtual_endpoint,
};

/// The `NET` host module with the default process manager.
pub fn module() -> HostModule<DefaultProcessManager> {
    module_with()
}
