//! Blocking HTTPS fetch for `NET_FETCH`: one URL in, one `STRING`
//! out, no filesystem touch.
//!
//! The contract is deliberately narrow: `https` only (cleartext `http`
//! reaches only loopback hosts, so tests can serve fixtures without a
//! TLS stack), at most [`MAX_REDIRECTS`] redirect hops, one
//! [`FETCH_TIMEOUT`] deadline for the whole request, at most
//! [`MAX_BODY_BYTES`] of body, strict UTF-8, and non-2xx statuses bail.
//! Every failure names the URL and the reason; nothing retries.

use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Maximum response body a fetch reads: 10 MiB. Matches ureq's own
/// default cap, stated here so the limit is pinned by name and test.
pub const MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;

/// Redirect hops followed before bailing with `too many redirects`.
const MAX_REDIRECTS: u32 = 5;

/// Whole-request deadline: connect, TLS, headers, and body.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Hosts where cleartext `http` is accepted: loopback only, so the test
/// suite can serve fixtures from `TcpListener` without TLS. Real hosts
/// must use `https`.
fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

/// Split `scheme://rest`. Missing or malformed schemes bail before any
/// socket opens.
fn split_scheme(url: &str) -> Result<(&str, &str)> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| {
        anyhow::anyhow!("NET_FETCH() needs an https URL, got {url:?} (missing '://')")
    })?;
    if scheme.is_empty() || rest.is_empty() {
        bail!("NET_FETCH() needs an https URL, got {url:?}");
    }
    Ok((scheme, rest))
}

/// Host part of `authority` (up to the next `/`): strips optional
/// `user@` and `:port`, unwraps one `[v6]` bracket pair. Query (`?`)
/// and fragment (`#`) delimiters truncate first: without that, a URL
/// like `http://evil.com?@127.0.0.1` would present `127.0.0.1` as the
/// host while the client dials `evil.com`.
fn authority_host(authority: &str) -> &str {
    let authority = authority
        .split(&['/', '?', '#'][..])
        .next()
        .unwrap_or(authority);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    if authority.bytes().filter(|byte| *byte == b':').count() > 1 {
        return authority;
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    }
}

/// Fetch `url` to text under the production limits.
pub fn fetch_text(url: &str) -> Result<String> {
    fetch_text_with_limit(url, MAX_BODY_BYTES)
}

/// Reject non-`https` schemes and cleartext `http` to non-loopback
/// hosts, before any socket opens. Runs per redirect hop, not just on
/// the initial URL, so a `Location` pointing at cleartext never slips
/// through automatic following (which stays disabled).
fn check_url(url: &str) -> Result<()> {
    let (scheme, rest) = split_scheme(url)?;
    if scheme != "https" {
        if scheme == "http" {
            let host = authority_host(rest).to_lowercase();
            if !is_loopback_host(&host) {
                bail!(
                    "NET_FETCH() refuses cleartext http to non-loopback host {host:?}: use https"
                );
            }
        } else {
            bail!(
                "NET_FETCH() supports https URLs (plus loopback http for tests), got scheme {scheme:?}"
            );
        }
    }
    Ok(())
}

/// Fetch `url` to text with an explicit body cap. The cap exists so
/// tests can pin the too-large failure without serving 10 MiB.
///
/// Redirects follow by hand, at most [`MAX_REDIRECTS`] hops: the
/// client never follows automatically, so every `Location` target
/// passes [`check_url`] before the next request. Absolute URLs only:
/// a relative `Location` fails closed instead of widening silently,
/// as does a redirect without a `Location` header.
pub fn fetch_text_with_limit(initial_url: &str, max_bytes: u64) -> Result<String> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .max_redirects(0)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut current_url = initial_url.to_string();
    let mut redirects = 0u32;
    loop {
        check_url(&current_url)?;
        let response = match agent.get(&current_url).call() {
            Ok(response) => response,
            Err(err) => {
                return Err(anyhow::Error::from(err))
                    .with_context(|| format!("NET_FETCH({current_url:?}) failed"));
            }
        };
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            if redirects >= MAX_REDIRECTS {
                bail!("NET_FETCH({initial_url:?}) failed: too many redirects");
            }
            let location = response.headers().get("location").ok_or_else(|| {
                anyhow::anyhow!("NET_FETCH({current_url:?}) redirect missing Location header")
            })?;
            let location = location.to_str().with_context(|| {
                format!("NET_FETCH({current_url:?}) redirect Location is not visible ASCII")
            })?;
            current_url = location.to_string();
            redirects += 1;
            continue;
        }
        if !(200..300).contains(&status) {
            bail!("NET_FETCH({current_url:?}) failed: http status: {status}");
        }
        let mut response = response;
        let body = response
            .body_mut()
            .with_config()
            .limit(max_bytes)
            .lossy_utf8(false)
            .read_to_string()
            .with_context(|| format!("NET_FETCH({current_url:?}) failed"))?;
        return Ok(body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serve one canned HTTP/1.1 response, then exit. Returns the bound
    /// loopback address.
    fn serve_bytes(body: Vec<u8>, status: &'static str) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            drain_request(&mut stream);
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).expect("head");
            stream.write_all(&body).expect("body");
        });
        addr
    }

    /// Drain the request head so the server never races the client close.
    fn drain_request(stream: &mut std::net::TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while buf.len() < 65536 {
            match stream.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    buf.push(byte[0]);
                    if buf.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
    }

    /// Serve `rounds` absolute `302` redirects back to this server,
    /// then exit. The client must stop following and bail first.
    /// Absolute targets pin the manual-follow path: every hop passes
    /// the scheme gate again.
    fn serve_redirect_loop(rounds: usize) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let location = format!("http://{addr}/loop");
        std::thread::spawn(move || {
            for _ in 0..rounds {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                drain_request(&mut stream);
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .expect("redirect");
            }
        });
        addr
    }

    /// Serve one `302` to an arbitrary `Location`, then exit.
    fn serve_redirect_once(location: String) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            drain_request(&mut stream);
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .expect("redirect");
        });
        addr
    }

    #[test]
    fn scheme_shapes_reject_before_any_socket() {
        for url in [
            "example.com/x",
            "://example.com/x",
            "https://",
            "ftp://example.com/x",
            "file:///etc/passwd",
        ] {
            fetch_text(url).expect_err("bad scheme must fail offline: {url}");
        }
    }

    #[test]
    fn cleartext_to_real_hosts_refuses_without_dialing() {
        let err = fetch_text("http://example.com/x").expect_err("cleartext must fail");
        assert!(
            err.to_string().contains("cleartext"),
            "refusal must name the reason: {err}"
        );
    }

    #[test]
    fn userinfo_spoof_in_query_or_fragment_stays_remote() {
        // `http://evil.com?@127.0.0.1` must not read as loopback: the
        // client would dial evil.com. Truncation at `?`/`#` keeps the
        // real host. Refusal happens pre-dial, so no sockets open.
        for url in [
            "http://evil.com?@127.0.0.1",
            "http://evil.com#@127.0.0.1",
            "http://evil.com/path?x=@localhost",
        ] {
            let err = fetch_text(url).expect_err("spoofed host must fail: {url}");
            assert!(
                format!("{err:#}").contains("cleartext"),
                "spoof must hit the cleartext refusal: {err:#}"
            );
        }
    }

    #[test]
    fn loopback_hosts_pass_the_gate() {
        for host in ["127.0.0.1", "127.0.0.1:9", "::1", "[::1]:9", "localhost"] {
            assert!(
                is_loopback_host(authority_host(host)),
                "{host} must count as loopback"
            );
        }
        assert!(!is_loopback_host(authority_host("example.com")));
        assert!(!is_loopback_host(authority_host("127.0.0.1.evil.com")));
    }

    #[test]
    #[cfg_attr(miri, ignore = "needs loopback TCP")]
    fn loopback_body_round_trips_exact() {
        let addr = serve_bytes(b"{\"a\": 1}".to_vec(), "200 OK");
        let body = fetch_text(&format!("http://{addr}/doc.json")).expect("loopback fetch");
        assert_eq!(body, "{\"a\": 1}");
    }

    #[test]
    #[cfg_attr(miri, ignore = "needs loopback TCP")]
    fn non_2xx_bails_with_status() {
        let addr = serve_bytes(b"nope".to_vec(), "500 Internal Server Error");
        let err = fetch_text(&format!("http://{addr}/x")).expect_err("500 must fail");
        assert!(
            format!("{err:#}").contains("500"),
            "status must surface: {err:#}"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "needs loopback TCP")]
    fn redirect_to_cleartext_remote_refuses_before_dialing() {
        // A loopback hop redirecting at cleartext off loopback must
        // fail the per-hop gate: no socket to the target ever opens.
        // `127.0.0.2` is loopback-range but outside the literal
        // allowlist, so refusal is deterministic with no dial.
        let addr = serve_redirect_once("http://127.0.0.2:9/x".to_string());
        let err = fetch_text(&format!("http://{addr}/start")).expect_err("cleartext hop must fail");
        assert!(
            format!("{err:#}").contains("cleartext"),
            "hop must hit the cleartext refusal: {err:#}"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "needs loopback TCP")]
    fn relative_redirect_fails_closed() {
        // No silent same-origin widening: a relative `Location` has no
        // scheme to validate, so the hop bails.
        let addr = serve_redirect_once("/relative".to_string());
        let err = fetch_text(&format!("http://{addr}/start")).expect_err("relative hop must fail");
        assert!(
            format!("{err:#}").contains("missing '://'"),
            "hop must fail closed on scheme: {err:#}"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "needs loopback TCP")]
    fn redirect_loop_bails() {
        let addr = serve_redirect_loop(8);
        let err = fetch_text(&format!("http://{addr}/loop")).expect_err("loop must fail");
        assert!(
            format!("{err:#}").contains("too many redirects"),
            "redirect cap must surface: {err:#}"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "needs loopback TCP")]
    fn body_over_cap_bails() {
        let addr = serve_bytes(vec![b'x'; 64], "200 OK");
        let err = fetch_text_with_limit(&format!("http://{addr}/big"), 16)
            .expect_err("over-cap body must fail");
        assert!(
            format!("{err:#}").contains("larger than request limit"),
            "cap must surface: {err:#}"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "needs loopback TCP")]
    fn invalid_utf8_bails() {
        let addr = serve_bytes(b"\xff\xfe invalid".to_vec(), "200 OK");
        let err = fetch_text(&format!("http://{addr}/bin")).expect_err("bad UTF-8 must fail");
        assert!(
            format!("{err:#}").contains("UTF-8"),
            "encoding must surface: {err:#}"
        );
    }
}
