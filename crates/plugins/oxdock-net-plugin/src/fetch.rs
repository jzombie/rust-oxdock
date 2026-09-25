//! Pure-Rust HTTPS download for `NET_FETCH` (issue #179).
//!
//! HTTP and TLS live here behind the NET module boundary: callers go
//! through the `NET_FETCH` DSL func, never around it. The stack is
//! `ureq` with `rustls` (no `native-tls`, no `openssl`, no C compiler),
//! so bare Linux hosts fetch with no system openssl, curl, or CA tooling.
//!
//! Bodies stream in chunks: the response reader feeds the caller's sink
//! and a running SHA-256 digest at the same time, so toolchain-sized
//! artifacts never sit fully buffered in memory. The one-shot
//! `read_to_vec` helpers stay unused here on purpose: they cap bodies
//! at 10MB by default.

use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Default fetch timeout when no `timeout` option is given.
pub(crate) const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Chunk size for response streaming.
const CHUNK: usize = 8192;

/// Streamed fetch outcome: HTTP status, total bytes sunk, and the hex
/// SHA-256 digest of every byte sunk.
pub(crate) struct FetchResult {
    pub status: u16,
    pub bytes: u64,
    pub sha256: String,
}

/// Stream `url` into `sink` in chunks, following redirects. Returns the
/// status, byte count, and SHA-256 of the streamed bytes.
/// `cancelled` is polled per chunk so `CANCEL` interrupts promptly.
/// Transport errors retry up to `retries` extra attempts; HTTP 4xx bails
/// immediately, 5xx retries like a transport error.
pub(crate) fn fetch_stream(
    url: &str,
    timeout: Duration,
    retries: u32,
    mut sink: impl FnMut(&[u8]) -> Result<()>,
    cancelled: impl Fn() -> bool,
) -> Result<FetchResult> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        bail!("NET_FETCH url must start with http:// or https://, got {url:?}");
    }
    let mut attempts = 0u32;
    loop {
        match fetch_once(url, timeout, &mut sink, &cancelled) {
            Ok(out) => return Ok(out),
            Err(err) => {
                if attempts >= retries || !is_retryable(&err) {
                    return Err(err);
                }
                attempts += 1;
                std::thread::sleep(Duration::from_millis(100 * u64::from(attempts)));
            }
        }
    }
}

/// Marker for errors worth another attempt: transport failures and HTTP
/// 5xx. Everything else (bad scheme, 4xx, cancellation, sink errors, hash
/// mismatches from callers) fails fast.
fn is_retryable(err: &anyhow::Error) -> bool {
    if err.chain().any(|cause| {
        cause
            .downcast_ref::<ureq::Error>()
            .is_some_and(|e| !matches!(e, ureq::Error::StatusCode(_)))
    }) {
        return true;
    }
    err.to_string().contains("HTTP status 5")
}

/// Single streaming attempt.
fn fetch_once(
    url: &str,
    timeout: Duration,
    sink: &mut impl FnMut(&[u8]) -> Result<()>,
    cancelled: &impl Fn() -> bool,
) -> Result<FetchResult> {
    use sha2::{Digest, Sha256};
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into();
    let response = agent
        .get(url)
        .call()
        .with_context(|| format!("NET_FETCH {url:?} request failed"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        bail!("NET_FETCH {url:?} failed with HTTP status {status}");
    }
    let mut reader = response.into_body().into_reader();
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    let mut chunk = [0u8; CHUNK];
    loop {
        if cancelled() {
            bail!("NET_FETCH interrupted by cancellation");
        }
        use std::io::Read;
        let n = reader
            .read(&mut chunk)
            .with_context(|| format!("NET_FETCH {url:?} failed to read response body"))?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
        sink(&chunk[..n])?;
        total += n as u64;
    }
    Ok(FetchResult {
        status,
        bytes: total,
        sha256: hex::encode(hasher.finalize()),
    })
}

/// Verify a streamed digest against an expected lowercase hex digest.
pub(crate) fn verify_sha256(actual_hex: &str, expected_hex: &str) -> Result<()> {
    if actual_hex != expected_hex.to_lowercase() {
        bail!("NET_FETCH sha256 mismatch: expected {expected_hex}, got {actual_hex}");
    }
    Ok(())
}

/// Parse a 64-character hex SHA-256 digest, normalizing to lowercase.
pub(crate) fn parse_sha256_hex(raw: &str, func: &str) -> Result<String> {
    let trimmed = raw.trim().to_lowercase();
    if trimmed.len() != 64 || !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("{func} option 'sha256' must be a 64-character hex digest, got {raw:?}");
    }
    Ok(trimmed)
}
