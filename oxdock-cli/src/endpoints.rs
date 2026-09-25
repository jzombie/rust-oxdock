//! CLI endpoint mapping: virtual service keys to physical binds.
//!
//! Scripts declare logical endpoints (`NET_LISTEN("2251")`,
//! `SSH_SERVE("demo-proxy")`); these flags decide what they bind to:
//!
//! - default: bare ports bind loopback, names open memory rendezvous;
//! - `--listen 0.0.0.0:2251`: expose a logical port on an interface;
//! - `-p 2222:2251`: map outer port 2222 to inner virtual 2251
//!   (`-p 0:2251` takes an ephemeral outer port);
//! - `--offline`: no sockets at all (conflicts with both above).
//!
//! [`build_registry`] turns the flags into a bound [`EndpointRegistry`];
//! binding happens before script parsing so `EADDRINUSE` fails fast.

use std::net::SocketAddr;
#[cfg(feature = "net")]
use std::sync::Arc;

use anyhow::{Context, Result, bail};
#[cfg(feature = "net")]
use oxdock_net_plugin::{BindingSpec, EndpointKey, EndpointRegistry, Protocol, parse_endpoint_ref};

/// Endpoint exposure flags from CLI args. Built by `Options::parse`,
/// consumed by [`build_registry`].
#[derive(Debug, Clone, Default)]
pub struct EndpointFlags {
    /// `--listen` addresses: each maps its own port as a virtual port.
    pub listens: Vec<SocketAddr>,
    /// `-p` mappings: (outer socket address, inner virtual endpoint as raw
    /// text). A bare outer port binds all interfaces; outer port `0` takes an
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
        bail!("--listen requires an address ([host:]port)");
    }
    if text.chars().all(|c| c.is_ascii_digit()) {
        let port: u16 = text
            .parse()
            .map_err(|_| anyhow::anyhow!("--listen port must be numeric 1-65535, got {raw:?}"))?;
        if port == 0 {
            bail!(
                "--listen port 0 has no stable service key (use -p 0:<endpoint> for an ephemeral outer port)"
            );
        }
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    let addr: SocketAddr = text.parse().with_context(|| {
        format!("--listen address must be [host:]port (try 0.0.0.0:2251), got {raw:?}")
    })?;
    if addr.port() == 0 {
        bail!(
            "--listen port 0 has no stable service key (use -p 0:<endpoint> for an ephemeral outer port)"
        );
    }
    Ok(addr)
}

/// Parse one `-p` value: `[host:]outer:inner` (Docker-style). The outer
/// side is a bare port (all interfaces) or a full socket address; the
/// inner side stays raw text here and is validated by [`build_registry`]
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
        bail!(
            "-p requires [host:]outer:inner (try -p 2222:2251 or -p 127.0.0.1:1234:ssh-server), got {raw:?}"
        );
    };
    let inner_text = inner_text.trim();
    if inner_text.is_empty() {
        bail!(
            "-p requires [host:]outer:inner (try -p 2222:2251 or -p 127.0.0.1:1234:ssh-server), got {raw:?}"
        );
    }
    #[cfg(feature = "net")]
    parse_endpoint_ref(inner_text, "-p")
        .with_context(|| format!("-p inner endpoint invalid in {raw:?}"))?;
    let outer_text = outer_text.trim();
    let addr = if let Ok(port) = outer_text.parse::<u16>() {
        SocketAddr::from(([0, 0, 0, 0], port))
    } else if let Ok(addr) = outer_text.parse::<SocketAddr>() {
        addr
    } else {
        bail!(
            "-p outer address invalid in {raw:?} (expected a port 0-65535 or a [host:]port address)"
        );
    };
    Ok((addr, inner_text.to_string()))
}

/// Build the run's endpoint registry from CLI flags and bind every mapped
/// socket now: callers run this before parsing so bind conflicts fail
/// fast, never parse-then-fail-on-bind.
#[cfg(feature = "net")]
pub fn build_registry(flags: &EndpointFlags) -> Result<Arc<EndpointRegistry>> {
    use oxdock_net_plugin::VirtualEndpoint;
    use std::collections::HashSet;
    if flags.offline && (!flags.listens.is_empty() || !flags.publishes.is_empty()) {
        bail!("--offline conflicts with --listen/-p (offline runs open no sockets)");
    }
    let registry = Arc::new(EndpointRegistry::new(flags.offline));
    let mut inners: HashSet<EndpointKey> = HashSet::new();
    let mut outers: HashSet<SocketAddr> = HashSet::new();
    for addr in &flags.listens {
        let key = EndpointKey::tcp(VirtualEndpoint::Port(addr.port()));
        if !inners.insert(key.clone()) {
            bail!("virtual endpoint '{key}' is mapped twice");
        }
        if addr.port() != 0 && !outers.insert(*addr) {
            bail!("outer socket address {addr} is mapped twice");
        }
        registry.add_mapping(&key, BindingSpec::Exposed { addr: *addr })?;
    }
    for (addr, inner) in &flags.publishes {
        let (protocol, endpoint) = parse_endpoint_ref(inner, "-p")
            .with_context(|| format!("-p inner endpoint invalid in {inner:?}"))?;
        let key = EndpointKey {
            protocol: protocol.unwrap_or(Protocol::Tcp),
            endpoint,
        };
        if !inners.insert(key.clone()) {
            bail!("virtual endpoint '{key}' is mapped twice");
        }
        if addr.port() != 0 && !outers.insert(*addr) {
            bail!("outer socket address {addr} is mapped twice");
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
        assert_eq!(
            parse_listen_arg("2251").unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 2251))
        );
        assert_eq!(
            parse_listen_arg("0.0.0.0:2251").unwrap(),
            SocketAddr::from(([0, 0, 0, 0], 2251))
        );
        for bad in ["", "0", "2251:0", "0.0.0.0:0", "not-an-addr"] {
            parse_listen_arg(bad).expect_err("bad listen must fail");
        }
    }

    #[test]
    fn publish_forms() {
        assert_eq!(
            parse_publish_arg("2222:2251").unwrap(),
            (
                SocketAddr::from(([0, 0, 0, 0], 2222)),
                "2251".to_string()
            )
        );
        assert_eq!(
            parse_publish_arg("0:demo-proxy").unwrap(),
            (
                SocketAddr::from(([0, 0, 0, 0], 0)),
                "demo-proxy".to_string()
            )
        );
        for bad in ["", "2251", "abc:2251"] {
            parse_publish_arg(bad).expect_err("bad publish must fail");
        }
        // Inner-endpoint validation is eager only with `net`; without it the
        // raw text is stored and rejected later with the feature message.
        #[cfg(feature = "net")]
        for bad in ["2222:0", "2222:127.0.0.1:2251"] {
            parse_publish_arg(bad).expect_err("bad inner must fail");
        }
    }

    #[test]
    fn publish_three_part_host_port_forms() {
        assert_eq!(
            parse_publish_arg("127.0.0.1:1234:ssh-server").unwrap(),
            (
                "127.0.0.1:1234".parse().unwrap(),
                "ssh-server".to_string()
            )
        );
        assert_eq!(
            parse_publish_arg("127.0.0.1:1234:2251").unwrap(),
            (
                "127.0.0.1:1234".parse().unwrap(),
                "2251".to_string()
            )
        );
        assert_eq!(
            parse_publish_arg("[::1]:8080:web").unwrap(),
            (
                "[::1]:8080".parse().unwrap(),
                "web".to_string()
            )
        );
    }

    #[cfg(feature = "net")]
    #[test]
    fn registry_rejects_conflicts() {
        let flags = EndpointFlags {
            offline: true,
            listens: vec![SocketAddr::from(([0, 0, 0, 0], 2251))],
            ..EndpointFlags::default()
        };
        build_registry(&flags).expect_err("offline plus listen must fail");
        let flags = EndpointFlags {
            publishes: vec![
                (
                    SocketAddr::from(([0, 0, 0, 0], 2222)),
                    "2251".to_string(),
                ),
                (
                    SocketAddr::from(([0, 0, 0, 0], 2223)),
                    "2251".to_string(),
                ),
            ],
            ..EndpointFlags::default()
        };
        build_registry(&flags).expect_err("duplicate inner must fail");
        let flags = EndpointFlags {
            publishes: vec![
                (
                    SocketAddr::from(([0, 0, 0, 0], 2222)),
                    "2251".to_string(),
                ),
                (
                    SocketAddr::from(([0, 0, 0, 0], 2222)),
                    "2252".to_string(),
                ),
            ],
            ..EndpointFlags::default()
        };
        build_registry(&flags).expect_err("duplicate outer must fail");
    }

    #[cfg(feature = "net")]
    #[test]
    fn registry_rejects_bad_inner() {
        let flags = EndpointFlags {
            publishes: vec![(
                SocketAddr::from(([0, 0, 0, 0], 2222)),
                "0".to_string(),
            )],
            ..EndpointFlags::default()
        };
        build_registry(&flags).expect_err("bad inner must fail");
    }
}
