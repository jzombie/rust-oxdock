//! CLI endpoint mapping: virtual service keys to physical binds.
//!
//! Scripts declare logical endpoints (`NET_LISTEN("2251")`,
//! `SSH_SERVE("demo-proxy")`); these flags decide what they bind to:
//!
//! - default: bare ports bind loopback, names open memory rendezvous;
//! - `--listen 0.0.0.0:2251`: expose a logical port on an interface;
//! - `-p 2222:2251`: map loopback outer port 2222 to inner virtual 2251
//!   (`-p 0:2251` takes an ephemeral loopback outer port; prefix an
//!   explicit host for all interfaces, e.g. `-p 0.0.0.0:2222:2251`);
//! - `--offline`: no sockets at all (conflicts with both above).
//!
//! [`build_registry`] turns the flags into a bound [`EndpointRegistry`];
//! binding happens before script parsing so `EADDRINUSE` fails fast.

use std::net::SocketAddr;

use anyhow::Result;

use crate::errors;
#[cfg(feature = "net")]
use oxdock_net_plugin::{BindingSpec, EndpointKey, EndpointRegistry, Protocol, parse_endpoint_ref};
#[cfg(feature = "net")]
use std::sync::Arc;

/// Endpoint exposure flags from CLI args. Built by `Options::parse`,
/// consumed by [`build_registry`].
#[derive(Debug, Clone, Default)]
pub struct EndpointFlags {
    /// `--listen` addresses: each maps its own port as a virtual port.
    pub listens: Vec<SocketAddr>,
    /// `-p` mappings: (outer socket address, inner virtual endpoint as raw
    /// text). A bare outer port binds loopback (use an explicit host such
    /// as `0.0.0.0` for all interfaces); outer port `0` takes an
    /// ephemeral port, resolved at bind time. The inner text is validated by
    /// [`build_registry`] so this container stays free of net types.
    pub publishes: Vec<(SocketAddr, String)>,
    /// `--offline`: socketless run, conflicts with both lists above.
    pub offline: bool,
}

/// Parse one `--listen` value: `[host:]port`. Bare digits bind loopback;
/// otherwise a full socket address (the only place a wildcard or a
/// non-loopback interface may appear). Port `0` is rejected: an exposed
/// ephemeral port has no stable inner key, so it belongs to `-p 0:<inner>`.
pub fn parse_listen_arg(raw: &str) -> Result<SocketAddr> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(errors::listen_requires_address());
    }
    if text.chars().all(|c| c.is_ascii_digit()) {
        let port: u16 = text.parse().map_err(|_| errors::listen_port_numeric(raw))?;
        if port == 0 {
            return Err(errors::listen_port_zero());
        }
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    let addr: SocketAddr = text
        .parse()
        .map_err(|_| errors::listen_address_invalid(raw))?;
    if addr.port() == 0 {
        return Err(errors::listen_port_zero());
    }
    Ok(addr)
}

/// Parse one `-p` value: `[host:]outer:inner` (Docker-style, but safer by
/// default: unlike Docker, a bare outer port binds loopback, never all
/// interfaces). The outer side is a bare port (loopback) or a full socket
/// address (the only place a wildcard or a non-loopback interface may
/// appear, e.g. `-p 0.0.0.0:2222:2251`); the inner side stays raw text
/// (when `net` is enabled the inner is also validated eagerly so parse
/// errors keep their current shape). The inner may carry a protocol
/// qualifier (`-p 5353:dns/udp` maps the UDP slot; unqualified inners map
/// TCP).
/// Supported shapes: `2222:2251`, `127.0.0.1:1234:2251`,
/// `127.0.0.1:1234:ssh-server`, `[::1]:8080:web`, `0:2251` (ephemeral
/// outer port, resolved at bind time).
pub fn parse_publish_arg(raw: &str) -> Result<(SocketAddr, String)> {
    let text = raw.trim();
    let Some((outer_text, inner_text)) = text.rsplit_once(':') else {
        return Err(errors::publish_requires_shape(raw));
    };
    let inner_text = inner_text.trim();
    if inner_text.is_empty() {
        return Err(errors::publish_requires_shape(raw));
    }
    #[cfg(feature = "net")]
    parse_endpoint_ref(inner_text, "-p")
        .map_err(|e| errors::publish_inner_invalid(raw, &anyhow::Error::new(e)))?;
    let outer_text = outer_text.trim();
    let addr = if let Ok(port) = outer_text.parse::<u16>() {
        SocketAddr::from(([127, 0, 0, 1], port))
    } else if let Ok(addr) = outer_text.parse::<SocketAddr>() {
        addr
    } else {
        return Err(errors::publish_outer_invalid(raw));
    };
    Ok((addr, inner_text.to_string()))
}

/// Build the run's endpoint registry from CLI flags and bind every mapped
/// socket now: callers run this before parsing so bind conflicts fail
/// fast, never parse-then-fail-on-bind.
#[cfg(feature = "net")]
pub fn build_registry(flags: &EndpointFlags) -> Result<Arc<EndpointRegistry>> {
    use crate::errors;
    use oxdock_net_plugin::VirtualEndpoint;
    use std::collections::HashSet;
    if flags.offline && (!flags.listens.is_empty() || !flags.publishes.is_empty()) {
        return Err(errors::offline_conflict());
    }
    let registry = Arc::new(EndpointRegistry::new(flags.offline));
    let mut inners: HashSet<EndpointKey> = HashSet::new();
    let mut outers: HashSet<SocketAddr> = HashSet::new();
    for addr in &flags.listens {
        let key = EndpointKey::tcp(VirtualEndpoint::Port(addr.port()));
        if !inners.insert(key.clone()) {
            return Err(errors::endpoint_duplicate_inner(&key.to_string()));
        }
        if addr.port() != 0 && !outers.insert(*addr) {
            return Err(errors::endpoint_duplicate_outer(&addr.to_string()));
        }
        registry.add_mapping(&key, BindingSpec::Exposed { addr: *addr })?;
    }
    for (addr, inner) in &flags.publishes {
        let (protocol, endpoint) = parse_endpoint_ref(inner, "-p")
            .map_err(|e| errors::publish_inner_invalid(inner, &anyhow::Error::new(e)))?;
        let key = EndpointKey {
            protocol: protocol.unwrap_or(Protocol::Tcp),
            endpoint,
        };
        if !inners.insert(key.clone()) {
            return Err(errors::endpoint_duplicate_inner(&key.to_string()));
        }
        if addr.port() != 0 && !outers.insert(*addr) {
            return Err(errors::endpoint_duplicate_outer(&addr.to_string()));
        }
        registry.add_mapping(&key, BindingSpec::Exposed { addr: *addr })?;
    }
    registry.bind_all()?;
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_forms() {
        use crate::errors::Diagnostic as Diag;
        assert_eq!(
            parse_listen_arg("2251").unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 2251))
        );
        assert_eq!(
            parse_listen_arg("0.0.0.0:2251").unwrap(),
            SocketAddr::from(([0, 0, 0, 0], 2251))
        );
        for (bad, key) in [
            ("", "cli.listen_requires_address"),
            ("0", "cli.listen_port_zero"),
            ("0.0.0.0:0", "cli.listen_port_zero"),
            ("not-an-addr", "cli.listen_address_invalid"),
            ("2251:0", "cli.listen_address_invalid"),
        ] {
            let err = parse_listen_arg(bad).expect_err("bad listen must fail");
            let diag = err
                .downcast_ref::<Diag>()
                .expect("listen errors stay typed");
            assert_eq!(diag.key(), key, "{bad}: {diag}");
        }
    }

    #[test]
    fn publish_forms() {
        use crate::errors::Diagnostic as Diag;
        assert_eq!(
            parse_publish_arg("2222:2251").unwrap(),
            (SocketAddr::from(([127, 0, 0, 1], 2222)), "2251".to_string())
        );
        assert_eq!(
            parse_publish_arg("0:demo-proxy").unwrap(),
            (
                SocketAddr::from(([127, 0, 0, 1], 0)),
                "demo-proxy".to_string()
            )
        );
        for (bad, key) in [
            ("", "cli.publish_requires_shape"),
            ("2251", "cli.publish_requires_shape"),
            ("abc:2251", "cli.publish_outer_invalid"),
            ("2222:127.0.0.1:2251", "cli.publish_outer_invalid"),
        ] {
            let err = parse_publish_arg(bad).expect_err("bad publish must fail");
            let diag = err
                .downcast_ref::<Diag>()
                .expect("publish errors stay typed");
            assert_eq!(diag.key(), key, "{bad}: {diag}");
        }
        // Inner-endpoint validation is eager only with `net`; without it the
        // raw text is stored and rejected later with the feature message.
        #[cfg(feature = "net")]
        {
            let err = parse_publish_arg("2222:0").expect_err("bad inner must fail");
            let diag = err
                .downcast_ref::<crate::errors::Diagnostic>()
                .expect("inner errors stay typed");
            assert_eq!(diag.key(), "cli.publish_inner_invalid");
        }
    }

    #[test]
    fn publish_three_part_host_port_forms() {
        assert_eq!(
            parse_publish_arg("127.0.0.1:1234:ssh-server").unwrap(),
            ("127.0.0.1:1234".parse().unwrap(), "ssh-server".to_string())
        );
        assert_eq!(
            parse_publish_arg("127.0.0.1:1234:2251").unwrap(),
            ("127.0.0.1:1234".parse().unwrap(), "2251".to_string())
        );
        assert_eq!(
            parse_publish_arg("[::1]:8080:web").unwrap(),
            ("[::1]:8080".parse().unwrap(), "web".to_string())
        );
        // All interfaces is an explicit opt-in, never the bare default.
        assert_eq!(
            parse_publish_arg("0.0.0.0:2222:2251").unwrap(),
            ("0.0.0.0:2222".parse().unwrap(), "2251".to_string())
        );
    }

    #[cfg(feature = "net")]
    #[test]
    fn registry_rejects_conflicts() {
        use crate::errors::Diagnostic as Diag;
        let flags = EndpointFlags {
            offline: true,
            listens: vec![SocketAddr::from(([0, 0, 0, 0], 2251))],
            ..EndpointFlags::default()
        };
        let err = build_registry(&flags).expect_err("offline plus listen must fail");
        assert_eq!(
            err.downcast_ref::<Diag>().expect("typed").key(),
            "cli.offline_conflict"
        );
        let flags = EndpointFlags {
            publishes: vec![
                (SocketAddr::from(([0, 0, 0, 0], 2222)), "2251".to_string()),
                (SocketAddr::from(([0, 0, 0, 0], 2223)), "2251".to_string()),
            ],
            ..EndpointFlags::default()
        };
        let err = build_registry(&flags).expect_err("duplicate inner must fail");
        assert_eq!(
            err.downcast_ref::<Diag>().expect("typed").key(),
            "cli.endpoint_duplicate_inner"
        );
        let flags = EndpointFlags {
            publishes: vec![
                (SocketAddr::from(([0, 0, 0, 0], 2222)), "2251".to_string()),
                (SocketAddr::from(([0, 0, 0, 0], 2222)), "2252".to_string()),
            ],
            ..EndpointFlags::default()
        };
        let err = build_registry(&flags).expect_err("duplicate outer must fail");
        assert_eq!(
            err.downcast_ref::<Diag>().expect("typed").key(),
            "cli.endpoint_duplicate_outer"
        );
    }

    #[cfg(feature = "net")]
    #[test]
    fn registry_rejects_bad_inner() {
        let flags = EndpointFlags {
            publishes: vec![(SocketAddr::from(([0, 0, 0, 0], 2222)), "0".to_string())],
            ..EndpointFlags::default()
        };
        let err = build_registry(&flags).expect_err("bad inner must fail");
        assert_eq!(
            err.downcast_ref::<crate::errors::Diagnostic>()
                .expect("typed")
                .key(),
            "cli.publish_inner_invalid"
        );
    }
}
