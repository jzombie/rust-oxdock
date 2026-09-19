//! Pure validation helpers for SSH endpoint shapes and ephemeral credentials.
//!
//! No I/O happens here: everything is string validation plus random
//! generation, so this module is unit-testable without a network.

use anyhow::{Result, bail};

/// Length of auto-generated ephemeral passwords.
pub const GENERATED_PASSWORD_LEN: usize = 24;

/// Split `host:port` (or `[v6]:port`), requiring a numeric port.
/// Pure string validation: no DNS, Miri-safe.
fn split_host_port(raw: &str, what: &str) -> Result<(String, u16)> {
    let text = raw.trim();
    if text.is_empty() {
        bail!("{what} {raw:?}: expected host:port");
    }
    let (host, port_text) = if let Some(rest) = text.strip_prefix('[') {
        match rest.split_once("]:") {
            Some((host, port)) => (host.to_string(), port),
            None => bail!("{what} {raw:?}: expected [v6]:port"),
        }
    } else {
        match text.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), port),
            None => bail!("{what} {raw:?}: expected host:port"),
        }
    };
    if host.is_empty() {
        bail!("{what} {raw:?}: expected host:port");
    }
    match port_text.parse::<u16>() {
        Ok(port) => Ok((host, port)),
        _ => bail!("{what} {raw:?}: port must be numeric 0-65535"),
    }
}

/// Whether a bind host is a loopback address. Only these are permitted for
/// `SSH_SERVE`: the server must never listen on a reachable interface.
fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Validate an `SSH_SERVE` bind address without touching the network.
/// Returns the host to bind (`""` means the IPv4 loopback default) and the
/// port, where `0` requests an ephemeral port (reported back via the result
/// MAP, so unlike `LISTEN` no discovery channel is needed).
pub fn parse_serve_bind(raw: &str) -> Result<(String, u16)> {
    let text = raw.trim();
    if text.is_empty() {
        bail!("SSH_SERVE invalid bind {raw:?}: expected [host:]port");
    }
    if text.chars().all(|c| c.is_ascii_digit()) {
        match text.parse::<u16>() {
            Ok(port) => return Ok((String::new(), port)),
            _ => bail!("SSH_SERVE invalid bind {raw:?}: port must be numeric 0-65535"),
        }
    }
    let (host, port) = split_host_port(raw, "SSH_SERVE invalid bind")?;
    if !is_loopback_host(&host) {
        bail!(
            "SSH_SERVE invalid bind {raw:?}: only loopback binds are allowed (127.0.0.1, localhost, ::1)"
        );
    }
    Ok((host, port))
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

/// Generate an ephemeral alphanumeric password of
/// [`GENERATED_PASSWORD_LEN`] characters.
pub fn ephemeral_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    use rand::RngExt as _;
    let mut rng = rand::rng();
    (0..GENERATED_PASSWORD_LEN)
        .map(|_| {
            let idx = rng.random_range(0..ALPHABET.len());
            ALPHABET[idx] as char
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_port_binds_loopback_default() {
        assert_eq!(parse_serve_bind("2222").unwrap(), (String::new(), 2222));
    }

    #[test]
    fn ephemeral_port_allowed() {
        assert_eq!(parse_serve_bind("0").unwrap(), (String::new(), 0));
        assert_eq!(
            parse_serve_bind("127.0.0.1:0").unwrap(),
            ("127.0.0.1".to_string(), 0)
        );
    }

    #[test]
    fn loopback_forms_accepted() {
        assert_eq!(
            parse_serve_bind("127.0.0.1:2222").unwrap(),
            ("127.0.0.1".to_string(), 2222)
        );
        assert_eq!(
            parse_serve_bind("localhost:2222").unwrap(),
            ("localhost".to_string(), 2222)
        );
        assert_eq!(
            parse_serve_bind("[::1]:2222").unwrap(),
            ("::1".to_string(), 2222)
        );
    }

    #[test]
    fn non_loopback_rejected() {
        for raw in ["0.0.0.0:2222", "192.168.1.10:22", "example.com:22", ":2222"] {
            parse_serve_bind(raw).expect_err("non-loopback must fail");
        }
    }

    #[test]
    fn malformed_binds_rejected() {
        for raw in ["", "abc", "99999", "127.0.0.1", "127.0.0.1:abc"] {
            parse_serve_bind(raw).expect_err("malformed bind must fail");
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

    #[test]
    fn generated_password_shape() {
        let first = ephemeral_password();
        let second = ephemeral_password();
        assert_eq!(first.len(), GENERATED_PASSWORD_LEN);
        assert!(
            first.chars().all(|c| c.is_ascii_alphanumeric()),
            "{first:?}"
        );
        assert_ne!(first, second, "passwords must differ");
    }
}
