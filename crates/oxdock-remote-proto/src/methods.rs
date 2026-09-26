//! Stable muxio method names for sealed remote execution.
//!
//! Names carry no version suffix: compatibility is decided by
//! [`PROTO_DIGEST`](crate::PROTO_DIGEST) equality, never by comparing a
//! human-maintained number. Any edit here changes the digest on the next
//! build automatically. The table stays at three entries by design:
//! block-granular execution needs no per-command methods, and adding one
//! would violate the protocol-shape lock.

use muxio_rpc_service::rpc_method_id;

/// Unary handshake: client sends its digest tuple, server accepts or
/// rejects before any script byte ships.
pub const HANDSHAKE: u64 = rpc_method_id!("oxdock.remote.handshake");

/// Bidirectional streaming exec: header plus script and snapshot bytes in,
/// stdin sub-stream host to guest, stdout/stderr chunks plus terminal
/// result and result bytes out.
pub const EXEC: u64 = rpc_method_id!("oxdock.remote.exec");

/// Unary idempotent session teardown from either side.
pub const CLOSE: u64 = rpc_method_id!("oxdock.remote.close");
