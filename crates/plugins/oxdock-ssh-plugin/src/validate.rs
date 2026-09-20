//! Pure validation helpers for SSH endpoint shapes.
//!
//! No I/O happens here: everything is string validation, so this module is
//! unit-testable without a network. Host/port splitting and the loopback
//! gate live in `oxdock-net-plugin` (shared, no duplication); only the
//! serve/connect wrappers with their SSH-specific messages stay here.

use anyhow::{Result, bail};
use oxdock_net_plugin::validate as net_validate;

/// Split `host:port` (or `[v6]:port`) with an SSH-context prefix.
/// Pure string validation: no DNS, Miri-safe. Shape and port rules come
/// from the shared NET validators; only the message context is SSH's.
fn split_host_port(raw: &str, what: &str) -> Result<(String, u16)> {
    let prefix = format!("{what} {raw:?}");
    let (host, port_text) =
        net_validate::split_host_port(raw).map_err(|err| anyhow::anyhow!("{prefix}: {err}"))?;
    let port =
        net_validate::parse_port(&port_text).map_err(|err| anyhow::anyhow!("{prefix}: {err}"))?;
    Ok((host, port))
}

/// Validate an `SSH_SERVE` endpoint without touching the network. Scripts
/// declare a logical port or service name; physical mapping belongs to the
/// host runner. Delegates to the shared NET parser so shapes stay identical
/// everywhere; only the caller label is SSH's.
pub fn parse_serve_endpoint(raw: &str) -> Result<net_validate::VirtualEndpoint> {
    net_validate::parse_virtual_endpoint(raw, "SSH_SERVE")
}

/// Validate an `SSH_CONNECT` target without touching the network.
/// Any host is allowed; a bare host defaults to port 22.
pub fn parse_connect_target(raw: &str) -> Result<(String, u16)> {
    let text = raw.trim();
    if text.is_empty() {
        bail!("SSH_CONNECT invalid target {raw:?}: expected host[:port]");
    }
    if !text.contains(':') {
        return Ok((text.to_string(), 22));
    }
    let (host, port) = split_host_port(raw, "SSH_CONNECT invalid target")?;
    if port == 0 {
        bail!("SSH_CONNECT invalid target {raw:?}: port must be 1-65535");
    }
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_endpoint_delegates_to_shared_parser() {
        assert_eq!(
            parse_serve_endpoint("2222").unwrap(),
            net_validate::VirtualEndpoint::Port(2222)
        );
        assert_eq!(
            parse_serve_endpoint("demo-proxy").unwrap(),
            net_validate::VirtualEndpoint::Name("demo-proxy".to_string())
        );
        // Physical binds and ephemeral 0 are rejected in-script.
        for raw in ["127.0.0.1:2222", "0.0.0.0:2222", "0", ""] {
            parse_serve_endpoint(raw).expect_err("non-logical endpoint must fail");
        }
    }

    #[test]
    fn connect_target_shapes() {
        assert_eq!(
            parse_connect_target("10.0.0.5").unwrap(),
            ("10.0.0.5".to_string(), 22)
        );
        assert_eq!(
            parse_connect_target("10.0.0.5:2222").unwrap(),
            ("10.0.0.5".to_string(), 2222)
        );
        assert_eq!(
            parse_connect_target("127.0.0.1:0").unwrap_err().to_string(),
            "SSH_CONNECT invalid target \"127.0.0.1:0\": port must be 1-65535"
        );
        parse_connect_target("").expect_err("empty target must fail");
    }
}
