//! Raw TCP socket bridge behind `LISTEN` / `CONNECT`.
//!
//! The bridge is a payload-agnostic byte pump: it copies `&[u8]` buffers
//! between a TCP socket and the ambient `WITH_IO` pipe bindings without
//! inspecting, framing, or interpreting them. Encryption, framing, and
//! datagram mapping are the caller's job; v1 is TCP-only on loopback binds.
//!
//! Both commands pump on the calling thread, which must be an `ASYNC` task
//! thread: a synchronous pump on the main sequential flow would block on
//! `stdin` while producer steps wait behind it. Each pump direction runs on
//! its own worker thread joined by the calling (supervisor) thread, so no
//! thread ever outlives the task.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use oxdock_process::{CommandStdin, ProcessManager, SharedInput, SharedOutput};

use super::io::StreamHandle;
use super::pipe::PipeInner;
use super::steps::StepCtx;

/// Default dial timeout for `CONNECT` without `--timeout`.
pub(crate) const DEFAULT_DIAL_TIMEOUT: Duration = Duration::from_secs(10);
/// Supervisor tick: completion vs cancellation polling interval.
const SUPERVISOR_TICK: Duration = Duration::from_millis(10);
/// Backstop for timeout-bounded pipe reads: cancellation and teardown
/// resolve within a few ticks with no notify machinery.
const WORKER_BACKSTOP: Duration = Duration::from_millis(10);
/// Pipe-to-socket and socket-to-pipe copy buffer.
const CHUNK: usize = 8192;

/// Accept-loop poll interval. Non-blocking accept has no timeout knob in
/// `std`, so the tick is the wakeup source; it only ever delays connection
/// setup (once per accept-one task), never the data path.
#[cfg(windows)]
fn accept_tick() -> Duration {
    Duration::from_millis(16)
}

/// Accept-loop poll interval on Unix schedulers.
#[cfg(not(windows))]
fn accept_tick() -> Duration {
    Duration::from_millis(1)
}

/// Split `host:port` (or `[v6]:port`), requiring a nonzero numeric port.
/// Pure string validation: no DNS, Miri-safe.
fn split_host_port(cmd: &str, idx: usize, raw: &str, what: &str) -> Result<(String, u16)> {
    let text = raw.trim();
    let prefix = format!("step {}: {cmd} invalid {what} {raw:?}", idx + 1);
    if text.is_empty() {
        bail!("{prefix}: expected host:port");
    }
    let (host, port_text) = if let Some(rest) = text.strip_prefix('[') {
        match rest.split_once("]:") {
            Some((host, port)) => (host.to_string(), port),
            None => bail!("{prefix}: expected [v6]:port"),
        }
    } else {
        match text.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), port),
            None => bail!("{prefix}: expected host:port"),
        }
    };
    if host.is_empty() {
        bail!("{prefix}: expected host:port");
    }
    match port_text.parse::<u16>() {
        Ok(port) => Ok((host, port)),
        _ => bail!("{prefix}: port must be numeric 0-65535"),
    }
}

/// Validate a `CONNECT` endpoint without touching the network: shape and
/// numeric port only. Name resolution happens at dial time.
pub(crate) fn parse_connect_endpoint(idx: usize, raw: &str) -> Result<(String, u16)> {
    let (host, port) = split_host_port("CONNECT", idx, raw, "endpoint")?;
    if port == 0 {
        bail!(
            "step {}: CONNECT invalid endpoint {raw:?}: port must be 1-65535",
            idx + 1
        );
    }
    Ok((host, port))
}

/// Validate a `LISTEN` bind without touching the network. Returns the host
/// (`""` means the loopback default) and an explicit nonzero port.
/// Ephemeral and omitted ports are rejected: without a discovery channel
/// there is no way to learn them.
pub(crate) fn parse_listen_bind(idx: usize, raw: &str) -> Result<(String, u16)> {
    let text = raw.trim();
    if text.is_empty() {
        bail!(
            "step {}: LISTEN invalid bind {raw:?}: expected [host:]port",
            idx + 1
        );
    }
    if text.chars().all(|c| c.is_ascii_digit()) {
        match text.parse::<u16>() {
            Ok(port) if port != 0 => return Ok((String::new(), port)),
            Ok(_) => bail!(
                "step {}: LISTEN ephemeral ports are not supported; bind an explicit loopback port",
                idx + 1
            ),
            _ => bail!(
                "step {}: LISTEN invalid bind {raw:?}: port must be numeric 0-65535",
                idx + 1
            ),
        }
    }
    let (host, port) = split_host_port("LISTEN", idx, raw, "bind address")?;
    if port == 0 {
        bail!(
            "step {}: LISTEN ephemeral ports are not supported; bind an explicit loopback port",
            idx + 1
        );
    }
    // Literal IPs validate here (before the ASYNC gate) so misuse fails
    // deterministically without sockets; names resolve at bind time.
    if !host.is_empty()
        && let Ok(ip) = host.parse::<IpAddr>()
        && !ip.is_loopback()
    {
        bail!("step {}: LISTEN binds loopback-only; got {host}", idx + 1);
    }
    Ok((host, port))
}

/// Resolve a validated `LISTEN` host to a loopback socket address. Literal
/// IPs must be loopback; names resolve and every result must be loopback.
fn resolve_listen_addr(idx: usize, host: &str, port: u16) -> Result<SocketAddr> {
    let host = if host.is_empty() { "127.0.0.1" } else { host };
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !ip.is_loopback() {
            bail!("step {}: LISTEN binds loopback-only; got {host}", idx + 1);
        }
        return Ok(SocketAddr::new(ip, port));
    }
    let mut addrs = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("step {}: LISTEN {host}:{port} did not resolve", idx + 1))?;
    addrs
        .find(|addr| addr.ip().is_loopback())
        .ok_or_else(|| anyhow!("step {}: LISTEN binds loopback-only; got {host}", idx + 1))
}

/// Require ambient `WITH_IO` pipe bindings: a stream stdout, and either a
/// stream stdin or none at all. A missing stdin (`Null`) is not an error:
/// the pump starts half-closed and carries socket bytes to stdout only, so
/// task completion itself reports disconnects with no producer choreography.
/// Anything else (inherit, OS handles, missing stdout) bails with the
/// wrapping pattern spelled out.
fn bridge_streams<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    cmd: &str,
) -> Result<(Option<SharedInput>, SharedOutput)> {
    let reader = match cx.stdin.clone() {
        CommandStdin::Stream(reader) => Some(reader),
        CommandStdin::Null => None,
        _ => bail!(
            "step {}: {cmd} requires WITH_IO [stdin=pipe:...] bindings for input",
            idx + 1
        ),
    };
    let Some(StreamHandle::Stream(writer)) = cx.out.clone() else {
        bail!(
            "step {}: {cmd} requires WITH_IO [..., stdout=pipe:...] bindings",
            idx + 1
        );
    };
    Ok((reader, writer))
}

/// Require execution on an `ASYNC` task thread. A synchronous pump on the
/// main flow blocks on `stdin` while producer steps wait behind it.
fn require_async<P: ProcessManager>(cx: &StepCtx<'_, P>, idx: usize, cmd: &str) -> Result<()> {
    if !cx.state.inside_async {
        bail!(
            "step {}: {cmd} requires ASYNC: wrap it as LET $t: HANDLE = ASYNC {{ WITH_IO [...] {cmd} ... }}",
            idx + 1
        );
    }
    Ok(())
}

/// Whether an I/O error means "peer already gone": routine during teardown,
/// never a task failure.
fn is_peer_gone(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotConnected | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
    )
}

/// Run `op` (a socket shutdown) and swallow only the peer-gone class.
/// Anything else propagates: an unexpectedly broken shutdown is a real
/// task failure, not routine teardown.
fn suppress_peer_gone(op: io::Result<()>) -> io::Result<()> {
    match op {
        Ok(()) => Ok(()),
        Err(err) if is_peer_gone(&err) => Ok(()),
        Err(err) => Err(err),
    }
}

/// Read one chunk from a pipe: timeout-bounded for script pipes (backend
/// found by handle identity), blocking for anything else. Returns `None`
/// on a backstop tick with no data and no close.
fn read_pipe(
    reader: &SharedInput,
    inner: Option<&Arc<PipeInner>>,
    buf: &mut [u8],
) -> io::Result<Option<usize>> {
    match inner {
        Some(inner) => inner.read_into_timeout(buf, WORKER_BACKSTOP),
        None => {
            let mut guard = reader
                .lock()
                .map_err(|_| io::Error::other("stdin lock poisoned"))?;
            guard.read(buf).map(Some)
        }
    }
}

/// `stdin-pipe -> socket` direction. Stdin EOF half-closes the socket and
/// ends the direction; the task continues the socket direction until the
/// socket closes. Cancellation exits at the next tick.
fn pump_in(
    idx: usize,
    cmd: &str,
    reader: SharedInput,
    inner: Option<Arc<PipeInner>>,
    mut stream: TcpStream,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut buf = [0u8; CHUNK];
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        match read_pipe(&reader, inner.as_ref(), &mut buf)
            .with_context(|| format!("step {}: {cmd} stdin pipe read failed", idx + 1))?
        {
            None => continue,
            Some(0) => {
                suppress_peer_gone(stream.shutdown(Shutdown::Write)).map_err(|err| {
                    anyhow!("step {}: {cmd} socket shutdown failed: {err}", idx + 1)
                })?;
                return Ok(());
            }
            Some(n) => stream
                .write_all(&buf[..n])
                .with_context(|| format!("step {}: {cmd} socket write failed", idx + 1))?,
        }
    }
}

/// `socket -> stdout-pipe` direction. Socket EOF ends the direction; the
/// writer drops on thread exit, signalling EOF downstream once the last
/// writer and keeper release. Cancellation exits at the next tick.
fn pump_out(
    idx: usize,
    cmd: &str,
    writer: SharedOutput,
    mut stream: TcpStream,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut buf = [0u8; CHUNK];
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        match stream.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                let mut guard = writer
                    .lock()
                    .map_err(|_| anyhow!("step {}: {cmd} stdout lock poisoned", idx + 1))?;
                guard
                    .write_all(&buf[..n])
                    .with_context(|| format!("step {}: {cmd} stdout pipe write failed", idx + 1))?;
                guard
                    .flush()
                    .with_context(|| format!("step {}: {cmd} stdout pipe flush failed", idx + 1))?;
            }
            Err(err) => {
                return Err(anyhow!("step {}: {cmd} socket read failed: {err}", idx + 1));
            }
        }
    }
}

/// Pump bytes bidirectionally on the calling (supervisor) thread: spawn one
/// worker per bound direction, then tick supervisor duties (completion vs the
/// task cancellation token) until every worker is reaped. With no stdin
/// reader the socket write-half closes up front and only the socket
/// direction runs. Worker errors cascade through the task token so siblings
/// release promptly; the first error wins for `AWAIT`, external cancellation
/// reports as cancelled.
fn pump(
    idx: usize,
    cmd: &str,
    input: Option<(SharedInput, Option<Arc<PipeInner>>)>,
    writer: SharedOutput,
    stream: TcpStream,
    cancel: &AtomicBool,
) -> Result<()> {
    if input.is_none() {
        // Half-closed from the start: no stdin will ever arrive. A fresh
        // socket cannot be peer-gone yet; anything odd surfaces through the
        // pump threads below.
        let _ = suppress_peer_gone(stream.shutdown(Shutdown::Write));
    }
    let sock_in = stream
        .try_clone()
        .with_context(|| format!("step {}: {cmd} failed to clone socket", idx + 1))?;
    let sock_out = stream
        .try_clone()
        .with_context(|| format!("step {}: {cmd} failed to clone socket", idx + 1))?;
    std::thread::scope(|s| {
        let mut t_in = input.map(|(reader, inner)| {
            s.spawn(move || pump_in(idx, cmd, reader, inner, sock_in, cancel))
        });
        let mut t_out = Some(s.spawn(move || pump_out(idx, cmd, writer, sock_out, cancel)));
        let mut failed: Option<anyhow::Error> = None;
        // Reap finished workers without ever block-joining a live one.
        // External cancellation and observed worker errors both funnel
        // through the task token, which bounds every join below.
        let reap = |slot: &mut Option<std::thread::ScopedJoinHandle<'_, Result<()>>>,
                    failed: &mut Option<anyhow::Error>| {
            if !slot.as_ref().is_some_and(|h| h.is_finished()) {
                return;
            }
            let Some(handle) = slot.take() else {
                return;
            };
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    if failed.is_none() {
                        *failed = Some(err);
                    }
                }
                Err(_) => {
                    if failed.is_none() {
                        *failed = Some(anyhow!("step {}: {cmd} worker thread panicked", idx + 1));
                    }
                }
            }
        };
        loop {
            reap(&mut t_in, &mut failed);
            reap(&mut t_out, &mut failed);
            if t_in.is_none() && t_out.is_none() {
                break;
            }
            if failed.is_some() || cancel.load(Ordering::SeqCst) {
                // Teardown: release every waiter, then join boundedly.
                // Setting our own task token on worker failure cascades to
                // the sibling; the token is task-local (forked per ASYNC),
                // so nothing outside this task observes it.
                cancel.store(true, Ordering::SeqCst);
                let _ = suppress_peer_gone(stream.shutdown(Shutdown::Both));
            }
            std::thread::sleep(SUPERVISOR_TICK);
        }
        if let Some(err) = failed {
            return Err(err);
        }
        if cancel.load(Ordering::SeqCst) {
            bail!("step {}: {cmd} interrupted by cancellation", idx + 1);
        }
        Ok(())
    })
}

/// `CONNECT`: validate, gate, dial, pump. Validation precedes the `ASYNC`
/// gate so malformed endpoints fail deterministically without sockets.
pub(crate) fn connect<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    endpoint: &str,
    timeout: Option<Duration>,
) -> Result<()> {
    let (reader, writer) = bridge_streams(cx, idx, "CONNECT")?;
    let (host, port) = parse_connect_endpoint(idx, endpoint)?;
    require_async(cx, idx, "CONNECT")?;
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .with_context(|| format!("step {}: CONNECT {endpoint:?} did not resolve", idx + 1))?
        .next()
        .ok_or_else(|| anyhow!("step {}: CONNECT {endpoint:?} did not resolve", idx + 1))?;
    let stream = TcpStream::connect_timeout(&addr, timeout.unwrap_or(DEFAULT_DIAL_TIMEOUT))
        .with_context(|| format!("step {}: CONNECT {endpoint:?} dial failed", idx + 1))?;
    let input = match reader {
        Some(reader) => {
            let inner = cx.state.io.stdin_pipe_inner(&reader);
            Some((reader, inner))
        }
        None => None,
    };
    pump(
        idx,
        "CONNECT",
        input,
        writer,
        stream,
        &cx.state.cancel_token,
    )
}

/// `LISTEN`: validate, gate, bind, accept-one, pump. Like `CONNECT`,
/// validation precedes the `ASYNC` gate; the bind itself fails fast.
pub(crate) fn listen<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    idx: usize,
    bind: &str,
) -> Result<()> {
    let (reader, writer) = bridge_streams(cx, idx, "LISTEN")?;
    let (host, port) = parse_listen_bind(idx, bind)?;
    require_async(cx, idx, "LISTEN")?;
    let addr = resolve_listen_addr(idx, &host, port)?;
    let listener = TcpListener::bind(addr)
        .with_context(|| format!("step {}: LISTEN {bind:?} bind failed", idx + 1))?;
    listener
        .set_nonblocking(true)
        .with_context(|| format!("step {}: LISTEN failed to configure listener", idx + 1))?;
    let cancel = &cx.state.cancel_token;
    let stream = loop {
        if cancel.load(Ordering::SeqCst) {
            bail!("step {}: LISTEN interrupted by cancellation", idx + 1);
        }
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(accept_tick());
            }
            Err(err) => {
                return Err(anyhow!("step {}: LISTEN accept failed: {err}", idx + 1));
            }
        }
    };
    stream
        .set_nonblocking(false)
        .with_context(|| format!("step {}: LISTEN failed to configure stream", idx + 1))?;
    let input = match reader {
        Some(reader) => {
            let inner = cx.state.io.stdin_pipe_inner(&reader);
            Some((reader, inner))
        }
        None => None,
    };
    pump(idx, "LISTEN", input, writer, stream, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_endpoint_accepts_host_port() {
        let (host, port) = parse_connect_endpoint(0, "127.0.0.1:8080").expect("valid");
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 8080);
    }

    #[test]
    fn connect_endpoint_trims_and_rejects_shapes() {
        let (host, port) = parse_connect_endpoint(0, "  example.com:80  ").expect("valid");
        assert_eq!(host, "example.com");
        assert_eq!(port, 80);
        for bad in [
            "",
            "not-an-endpoint",
            "127.0.0.1",
            "127.0.0.1:",
            ":8080",
            "a:0",
            "a:notaport",
        ] {
            let err = format!("{:#}", parse_connect_endpoint(4, bad).unwrap_err());
            assert!(
                err.contains("step 5: CONNECT invalid endpoint"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn listen_bind_accepts_loopback_forms() {
        let (host, port) = parse_listen_bind(0, "127.0.0.1:8080").expect("valid");
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 8080);
        let (host, port) = parse_listen_bind(0, "8080").expect("bare port defaults host");
        assert_eq!(host, "");
        assert_eq!(port, 8080);
        let (host, port) = parse_listen_bind(0, "  127.0.0.1:2222  ").expect("valid");
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 2222);
    }

    #[test]
    fn listen_bind_rejects_ephemeral() {
        for bad in ["0", "127.0.0.1:0"] {
            let err = format!("{:#}", parse_listen_bind(0, bad).unwrap_err());
            assert!(err.contains("ephemeral"), "{bad}: {err}");
        }
    }

    #[test]
    fn teardown_suppresses_peer_gone_only() {
        assert!(suppress_peer_gone(Ok(())).is_ok());
        for kind in [
            io::ErrorKind::NotConnected,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::BrokenPipe,
        ] {
            assert!(
                suppress_peer_gone(Err(io::Error::new(kind, "peer gone"))).is_ok(),
                "{kind:?} must suppress"
            );
        }
        assert!(
            suppress_peer_gone(Err(io::Error::new(io::ErrorKind::InvalidInput, "bad"))).is_err()
        );
    }

    #[test]
    fn listen_resolve_rejects_non_loopback() {
        let err = format!("{:#}", resolve_listen_addr(0, "0.0.0.0", 8080).unwrap_err());
        assert!(err.contains("loopback"), "{err}");
        let err = format!(
            "{:#}",
            resolve_listen_addr(0, "93.184.216.34", 8080).unwrap_err()
        );
        assert!(err.contains("loopback"), "{err}");
        let addr = resolve_listen_addr(0, "", 8080).expect("default host");
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        let addr = resolve_listen_addr(0, "127.0.0.2", 8080).expect("loopback range");
        assert_eq!(addr.ip().to_string(), "127.0.0.2");
    }
}
