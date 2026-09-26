//! Sealed remote execution over stdio transports, behind the NET plugin.
//!
//! [`flags`] parses the single repeatable `--remote` flag,
//! [`inventory`] binds targets at startup and validates scripts before any
//! session starts, and [`session`] runs one block over a fresh transport
//! process implementing [`RemoteRunner`](oxdock_core::RemoteRunner). The
//! guest serve loop lives in `oxdock-cli` (`--remote-serve`) and speaks the
//! same [`oxdock_remote_proto`] contract.

pub mod flags;
pub mod inventory;
pub mod session;

pub use flags::parse_remote_arg;
pub use inventory::{RemoteInventory, RemoteTarget};
pub use session::{SessionConfig, StdioSession, module_hash};
