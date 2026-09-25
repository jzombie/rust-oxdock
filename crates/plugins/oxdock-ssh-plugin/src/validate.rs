//! Pure validation helpers for SSH endpoint shapes.
//!
//! No I/O happens here: everything is string validation, so this module is
//! unit-testable without a network. Host/port splitting and the loopback
//! gate live in `oxdock-net-plugin` (shared, no duplication); only the
//! serve/connect wrappers with their SSH-specific messages stay here.

use std::net::ToSocketAddrs;

use anyhow::{Context, Result, bail};
use oxdock_net_plugin::validate as net_validate;
use oxdock_net_plugin::{EndpointKey, EndpointRegistry, SlotKind, VirtualEndpoint};

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

/// Resolve an `SSH_CONNECT` target to a dial address. Logical endpoints
/// go through the registry so CLI mappings (`-p`/`--listen`) behave
/// like the serve side; anything else dials directly. Mirrors
/// `NET_CONNECT` resolution, except SSH cannot ride memory queues, so
/// those arms bail with the map hint instead of rendezvousing. Reads
/// registry slots only, never binds or dials.
pub fn resolve_connect_addr(
    registry: &EndpointRegistry,
    target: &str,
) -> Result<std::net::SocketAddr> {
    if let Ok(endpoint) = net_validate::parse_virtual_endpoint(target, "SSH_CONNECT") {
        let key = EndpointKey::tcp(endpoint);
        return match &key.endpoint {
            VirtualEndpoint::Port(port) => match registry.slot_kind(&key) {
                SlotKind::Memory => {
                    bail!(
                        "SSH_CONNECT: '{key}' is a memory service (SSH needs a TCP socket; map it with -p/--listen)"
                    )
                }
                SlotKind::TcpBound(addr) => Ok(addr),
                SlotKind::TcpUnbound => {
                    bail!("SSH_CONNECT: '{key}' was never bound (the runner must call bind_all)")
                }
                SlotKind::Offline => bail!("SSH_CONNECT: '{key}' is offline"),
                SlotKind::Unmapped => Ok(std::net::SocketAddr::from(([127, 0, 0, 1], *port))),
            },
            VirtualEndpoint::Name(_) => match registry.slot_kind(&key) {
                SlotKind::TcpBound(addr) => Ok(addr),
                SlotKind::TcpUnbound => {
                    bail!("SSH_CONNECT: '{key}' was never bound (the runner must call bind_all)")
                }
                SlotKind::Memory | SlotKind::Unmapped => {
                    bail!(
                        "SSH_CONNECT: '{key}' is a memory service (SSH needs a TCP socket; map it with -p/--listen)"
                    )
                }
                SlotKind::Offline => bail!("SSH_CONNECT: '{key}' is offline"),
            },
        };
    }
    let (host, port) = parse_connect_target(target)?;
    format!("{host}:{port}")
        .to_socket_addrs()
        .with_context(|| format!("SSH_CONNECT cannot resolve {host}:{port}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("SSH_CONNECT cannot resolve {host}:{port}"))
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

    fn fresh_registry() -> EndpointRegistry {
        EndpointRegistry::new(false)
    }

    #[test]
    fn resolve_unmapped_port_dials_loopback() {
        let addr = resolve_connect_addr(&fresh_registry(), "23471").expect("loopback default");
        assert_eq!(addr, std::net::SocketAddr::from(([127, 0, 0, 1], 23471)));
    }

    #[test]
    fn resolve_unmapped_name_is_memory() {
        // Virtual-first precedence: a bare single-label name classifies
        // as a service, never as a port-22 host. SSH cannot ride memory
        // queues, so it bails with the map hint; write `name:22` for a
        // literal single-label host dial.
        let err = resolve_connect_addr(&fresh_registry(), "demo-proxy").expect_err("memory bails");
        assert!(err.to_string().contains("memory service"), "{err:#}");
        let err = resolve_connect_addr(&fresh_registry(), "myhost").expect_err("memory bails");
        assert!(err.to_string().contains("memory service"), "{err:#}");
    }

    #[test]
    fn resolve_memory_mapped_port_bails() {
        use oxdock_net_plugin::BindingSpec;
        let registry = fresh_registry();
        registry
            .add_mapping(
                &EndpointKey::tcp(net_validate::VirtualEndpoint::Port(23472)),
                BindingSpec::Memory,
            )
            .expect("mapping");
        let err = resolve_connect_addr(&registry, "23472").expect_err("memory bails");
        assert!(err.to_string().contains("memory service"), "{err:#}");
    }

    #[test]
    fn resolve_offline_mapped_port_bails() {
        use oxdock_net_plugin::BindingSpec;
        let registry = fresh_registry();
        registry
            .add_mapping(
                &EndpointKey::tcp(net_validate::VirtualEndpoint::Port(23473)),
                BindingSpec::Offline,
            )
            .expect("mapping");
        let err = resolve_connect_addr(&registry, "23473").expect_err("offline bails");
        assert!(err.to_string().contains("offline"), "{err:#}");
    }

    #[test]
    fn resolve_physical_target_still_dials() {
        let addr = resolve_connect_addr(&fresh_registry(), "127.0.0.1:2222").expect("resolves");
        assert_eq!(addr, std::net::SocketAddr::from(([127, 0, 0, 1], 2222)));
    }
}
