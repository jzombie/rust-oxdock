//! Pure network endpoint validation shared by host plugins.
//!
//! No I/O happens here: everything is string shapes, so this module is
//! unit-testable without sockets and safe under Miri. Errors are
//! deliberately unprefixed so each caller embeds its own context (`step
//! N: ...` for builtins, `SSH_* ...` for plugins) without reflowing text.

use std::net::IpAddr;

use anyhow::{Result, bail};

/// Parse a numeric port (`0`-`65535`; callers narrow further).
pub fn parse_port(text: &str) -> Result<u16> {
    match text.parse::<u16>() {
        Ok(port) => Ok(port),
        _ => bail!("port must be numeric 0-65535"),
    }
}

/// A logical service endpoint declared by a script: a fixed non-zero port
/// number or a service name. Never a physical bind: scripts carry zero bind
/// authority, so any `host:port` shape (anything containing `:`) is an
/// error. The CLI/host runner maps each endpoint to a physical interface
/// (`--listen`/`-p`) or leaves it socketless (`--offline`, memory slots).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VirtualEndpoint {
    Port(u16),
    Name(String),
}

impl VirtualEndpoint {
    /// Registry key text: `port:2251` or `svc:demo-proxy`.
    pub fn key(&self) -> String {
        match self {
            VirtualEndpoint::Port(port) => format!("port:{port}"),
            VirtualEndpoint::Name(name) => format!("svc:{name}"),
        }
    }
}

impl std::fmt::Display for VirtualEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VirtualEndpoint::Port(port) => write!(formatter, "{port}"),
            VirtualEndpoint::Name(name) => write!(formatter, "{name}"),
        }
    }
}

/// Transport protocol qualifying a registry slot. Sockets are TCP-only
/// today; the compound key exists so a future `5353:dns/udp` mapping can
/// never collide with or silently misroute `5353:dns/tcp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Protocol {
    Tcp,
    Udp,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::Tcp => write!(formatter, "tcp"),
            Protocol::Udp => write!(formatter, "udp"),
        }
    }
}

/// Compound registry key: a protocol plus a logical endpoint. TCP displays
/// bare (`2251`, `demo-proxy`) so every existing TCP message stays
/// byte-identical; UDP displays qualified (`udp/dns`) so it is always
/// explicit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EndpointKey {
    pub protocol: Protocol,
    pub endpoint: VirtualEndpoint,
}

impl EndpointKey {
    /// TCP key for a logical endpoint: the default for `--listen`,
    /// in-script `LISTEN`/`SERVE`, and unqualified lookups.
    pub fn tcp(endpoint: VirtualEndpoint) -> Self {
        Self {
            protocol: Protocol::Tcp,
            endpoint,
        }
    }

    /// UDP key for a logical endpoint: only via explicit qualification.
    pub fn udp(endpoint: VirtualEndpoint) -> Self {
        Self {
            protocol: Protocol::Udp,
            endpoint,
        }
    }
}

impl std::fmt::Display for EndpointKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.protocol {
            Protocol::Tcp => write!(formatter, "{}", self.endpoint),
            Protocol::Udp => write!(formatter, "udp/{}", self.endpoint),
        }
    }
}

/// Split an optional protocol qualifier off endpoint text. Accepts a
/// leading qualifier (`udp/dns`, the DSL lookup form) or a trailing one
/// (`dns/udp`, the `-p` flag form); bare text has no qualifier. Unknown
/// qualifiers bail instead of falling through to name validation.
fn split_protocol_qualifier(text: &str) -> Result<(Option<Protocol>, &str)> {
    let parse_protocol = |segment: &str| match segment {
        "tcp" => Some(Protocol::Tcp),
        "udp" => Some(Protocol::Udp),
        _ => None,
    };
    // Leading form (`udp/dns`, the DSL lookup spelling) wins: a
    // qualified rest is never a valid bare endpoint, so it errors below
    // either way.
    let leading = text
        .split_once('/')
        .filter(|(_, rest)| !rest.is_empty() && !rest.contains('/'))
        .and_then(|(head, rest)| parse_protocol(head).map(|protocol| (Some(protocol), rest)));
    if let Some(qualified) = leading {
        return Ok(qualified);
    }
    let trailing = text
        .rsplit_once('/')
        .filter(|(bare, _)| !bare.is_empty() && !bare.contains('/'))
        .and_then(|(bare, tail)| parse_protocol(tail).map(|protocol| (Some(protocol), bare)));
    if let Some(qualified) = trailing {
        return Ok(qualified);
    }
    if text.contains('/') {
        bail!(
            "unknown protocol qualifier in {text:?} (expected 'tcp/...', 'udp/...', '.../tcp', or '.../udp')"
        );
    }
    Ok((None, text))
}

/// Validate a possibly protocol-qualified endpoint without touching the
/// network. Returns the qualifier (`None` for bare text, which callers
/// resolve as TCP with single-protocol fallback) plus the validated bare
/// endpoint. The `func` prefix names the caller for error context.
pub fn parse_endpoint_ref(raw: &str, func: &str) -> Result<(Option<Protocol>, VirtualEndpoint)> {
    let text = raw.trim();
    let (protocol, bare) = split_protocol_qualifier(text)
        .map_err(|err| anyhow::anyhow!("{func} invalid endpoint {raw:?}: {err:#}"))?;
    let endpoint = parse_virtual_endpoint(bare, func)?;
    Ok((protocol, endpoint))
}

/// Reserved service names: resolving them as hosts would be ambiguous, so
/// scripts must not claim them.
fn is_reserved_name(name: &str) -> bool {
    matches!(name, "localhost" | "0.0.0.0" | "::" | "::1") || name.parse::<IpAddr>().is_ok()
}

/// Validate a `SERVE`/`LISTEN`/`CONNECT` endpoint key without touching the
/// network. Bare digits `1`-`65535` parse as port keys (claimable only
/// when the runner mapped them with `--listen`/`-p`; never bound or
/// dialed from a script); anything else must be a service name (`1`-`64`
/// chars, `[A-Za-z0-9_-]`, containing at least one letter, `-`, or `_`,
/// so numeric strings never parse as names). `0` is banned in-script:
/// ephemeral ports belong exclusively to the CLI outer mapping
/// (`-p 0:<name>`). The `func` prefix names the caller (`NET_LISTEN`,
/// `SSH_SERVE`) for error context.
pub fn parse_virtual_endpoint(raw: &str, func: &str) -> Result<VirtualEndpoint> {
    let prefix = format!("{func} invalid endpoint {raw:?}");
    let text = raw.trim();
    if text.is_empty() {
        bail!("{prefix}: expected a port 1-65535 or a service name");
    }
    if text.contains(':') {
        bail!(
            "{prefix}: scripts declare logical endpoints, not host:port binds (the host runner maps them with --listen/-p)"
        );
    }
    if text.contains('/') || text.chars().any(char::is_whitespace) {
        bail!("{prefix}: service names use [A-Za-z0-9_-] only");
    }
    if text.chars().all(|c| c.is_ascii_digit()) {
        let port = parse_port(text).map_err(|err| anyhow::anyhow!("{prefix}: {err}"))?;
        if port == 0 {
            bail!(
                "{prefix}: port 0 is reserved for the CLI outer mapping (use -p 0:<endpoint>); scripts declare fixed ports 1-65535 or service names"
            );
        }
        return Ok(VirtualEndpoint::Port(port));
    }
    if text.len() > 64
        || !text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || !text
            .chars()
            .any(|c| c.is_ascii_alphabetic() || c == '-' || c == '_')
    {
        bail!(
            "{prefix}: service names are 1-64 chars of [A-Za-z0-9_-] with at least one letter, '-' or '_'"
        );
    }
    if is_reserved_name(text) {
        bail!("{prefix}: {text:?} is a reserved host name, not a service name");
    }
    Ok(VirtualEndpoint::Name(text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_bounds() {
        assert_eq!(parse_port("0").unwrap(), 0);
        assert_eq!(parse_port("65535").unwrap(), 65535);
        for bad in ["", "abc", "99999", "-1"] {
            parse_port(bad).expect_err("bad port must fail");
        }
    }

    #[test]
    fn virtual_endpoint_ports() {
        assert_eq!(
            parse_virtual_endpoint("2251", "NET_LISTEN").unwrap(),
            VirtualEndpoint::Port(2251)
        );
        assert_eq!(
            parse_virtual_endpoint("  80  ", "NET_LISTEN").unwrap(),
            VirtualEndpoint::Port(80)
        );
        assert_eq!(
            parse_virtual_endpoint("65535", "SSH_SERVE").unwrap(),
            VirtualEndpoint::Port(65535)
        );
        for bad in ["", "0", "00", "99999", "abc:123"] {
            parse_virtual_endpoint(bad, "NET_LISTEN").expect_err("bad port must fail");
        }
        // `0` is reserved for the CLI outer mapping, never for scripts.
        let err = format!(
            "{:#}",
            parse_virtual_endpoint("0", "NET_LISTEN").unwrap_err()
        );
        assert!(err.contains("-p 0:"), "{err}");
    }

    #[test]
    fn virtual_endpoint_names() {
        assert_eq!(
            parse_virtual_endpoint("demo-proxy", "NET_LISTEN").unwrap(),
            VirtualEndpoint::Name("demo-proxy".to_string())
        );
        assert_eq!(
            parse_virtual_endpoint("worker_task_7", "SSH_SERVE").unwrap(),
            VirtualEndpoint::Name("worker_task_7".to_string())
        );
        // Any `:` is a physical bind shape, rejected in-script.
        for bad in [
            "127.0.0.1:2251",
            "localhost:22",
            "[::1]:2222",
            "0.0.0.0:8080",
            "example.com:22",
        ] {
            let err = format!(
                "{:#}",
                parse_virtual_endpoint(bad, "NET_LISTEN").unwrap_err()
            );
            assert!(err.contains("logical endpoints"), "{bad}: {err}");
        }
        // Reserved host names and IP literals are not service names.
        for bad in ["localhost", "0.0.0.0", "::1", "127.0.0.1", "10.0.0.5"] {
            parse_virtual_endpoint(bad, "NET_LISTEN").expect_err("reserved must fail");
        }
        // Charset and slash/whitespace rules.
        for bad in ["has space", "a/b", "bad!", ""] {
            parse_virtual_endpoint(bad, "NET_LISTEN").expect_err("bad name must fail");
        }
        parse_virtual_endpoint(&"x".repeat(65), "NET_LISTEN").expect_err("overlong name must fail");
    }

    #[test]
    fn virtual_endpoint_keys() {
        assert_eq!(VirtualEndpoint::Port(2251).key(), "port:2251".to_string());
        assert_eq!(
            VirtualEndpoint::Name("demo-proxy".to_string()).key(),
            "svc:demo-proxy".to_string()
        );
    }

    #[test]
    fn endpoint_key_display_stays_bare_for_tcp() {
        let tcp = EndpointKey::tcp(VirtualEndpoint::Name("dns".to_string()));
        assert_eq!(tcp.to_string(), "dns");
        let udp = EndpointKey::udp(VirtualEndpoint::Name("dns".to_string()));
        assert_eq!(udp.to_string(), "udp/dns");
        let port = EndpointKey::tcp(VirtualEndpoint::Port(2251));
        assert_eq!(port.to_string(), "2251");
    }

    #[test]
    fn endpoint_ref_accepts_qualifiers() {
        assert_eq!(
            parse_endpoint_ref("web", "NET_PORT").unwrap(),
            (None, VirtualEndpoint::Name("web".to_string()))
        );
        assert_eq!(
            parse_endpoint_ref("tcp/web", "NET_PORT").unwrap(),
            (
                Some(Protocol::Tcp),
                VirtualEndpoint::Name("web".to_string())
            )
        );
        assert_eq!(
            parse_endpoint_ref("udp/dns", "NET_PORT").unwrap(),
            (
                Some(Protocol::Udp),
                VirtualEndpoint::Name("dns".to_string())
            )
        );
        assert_eq!(
            parse_endpoint_ref("dns/udp", "-p").unwrap(),
            (
                Some(Protocol::Udp),
                VirtualEndpoint::Name("dns".to_string())
            )
        );
        assert_eq!(
            parse_endpoint_ref("2251/tcp", "-p").unwrap(),
            (Some(Protocol::Tcp), VirtualEndpoint::Port(2251))
        );
        for bad in ["sctp/dns", "dns/sctp", "a/b", "udp/", "/dns", ""] {
            parse_endpoint_ref(bad, "NET_PORT").expect_err("bad qualifier must fail");
        }
        parse_endpoint_ref("udp/0", "NET_PORT").expect_err("qualified zero must fail");
    }

    /// The mandate, pinned at parser level: physical `host:port` shapes
    /// never parse as dial targets anymore. Every CONNECT target goes
    /// through `parse_virtual_endpoint` (service key or nothing).
    #[test]
    fn physical_shapes_never_parse_as_targets() {
        for bad in [
            "10.0.0.5:2222",
            "127.0.0.1:9",
            "example.com:22",
            "localhost:22",
            "[::1]:2222",
        ] {
            let err = format!(
                "{:#}",
                parse_virtual_endpoint(bad, "NET_CONNECT").unwrap_err()
            );
            assert!(err.contains("logical endpoints"), "{bad}: {err}");
        }
    }
}
