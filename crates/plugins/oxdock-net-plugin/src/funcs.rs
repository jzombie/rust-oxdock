//! The `NET` host module: virtual-endpoint listener handles plus
//! explicit-pipe pumps for dial-out and memory sessions.
//!
//! Pipe direction convention (fixed for every func): `out_pipe` carries
//! bytes produced by the wire side (the DSL reads them), `in_pipe`
//! carries bytes consumed by the wire side (the DSL writes them). Pumps
//! are always full-duplex: explicit pipes have no `Null` spelling, so the
//! builtin read-only mode has no equivalent here.
//!
//! `NET_LISTEN`, `NET_CONNECT`, `NET_PORT`, and `NET_ADDR` resolve through
//! the run's [`EndpointRegistry`]: the registry rides a closure-captured
//! `Arc` in hand-built [`HostRegistration::Stateful`] entries (see
//! [`module_with_endpoints`]), because the `#[oxdock_func]` macro
//! generates closers-over-nothing. `NET_ACCEPT` and `NET_CLOSE` reach the
//! same registry through their `NET_LISTENER` handle's state.
//!
//! [`EndpointRegistry`]: crate::endpoints::EndpointRegistry
//! [`HostRegistration::Stateful`]: oxdock_core::HostRegistration::Stateful

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use oxdock_core::{
    FuncKind, FuncMeta, FuncParam, HostModule, HostRegistration, NativeFn, OxDockFn, OxDockType,
    StepCtx, Value,
};
use oxdock_func_macro::oxdock_func;
use oxdock_process::ProcessManager;

use crate::bridge::{pump_memory, pump_stream};
use crate::endpoints::{
    AcquiredListener, EndpointRegistry, MemoryPipePair, SlotKind, acquire_listener,
};
use crate::fetch::{
    DEFAULT_FETCH_TIMEOUT, FetchResult, fetch_stream, parse_sha256_hex, verify_sha256,
};
use crate::state::ListenerState;
use crate::types::NetListenerTag;
use crate::validate::{
    EndpointKey, VirtualEndpoint, parse_connect_endpoint, parse_virtual_endpoint,
};

/// Unique listener ids per process.
static LISTENER_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Mint the next `net-{pid}-{n}` listener id.
fn next_listener_id() -> String {
    let id = LISTENER_IDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("net-{pid}-{id}", pid = std::process::id())
}

/// Default dial timeout for `NET_CONNECT` without a `timeout` option.
const DEFAULT_DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Accept-loop poll interval. Non-blocking accept has no timeout knob in
/// `std`, so the tick is the wakeup source; it only ever delays
/// connection setup (once per accept), never the data path. Ten
/// milliseconds matches the supervisor tick: bound idle wakeups instead
/// of spinning.
#[cfg(windows)]
fn accept_tick() -> Duration {
    Duration::from_millis(16)
}

/// Accept-loop poll interval on Unix schedulers (same rationale).
#[cfg(not(windows))]
fn accept_tick() -> Duration {
    Duration::from_millis(10)
}

/// Read the options MAP for a `NET_*` function. Must be a MAP; unknown
/// keys bail so script typos fail fast instead of silently ignored.
fn net_options<'a>(options: &'a Value, func: &str) -> Result<&'a BTreeMap<String, Value>> {
    options
        .as_map()
        .ok_or_else(|| anyhow::anyhow!("{func} options must be a MAP, got {}", options.type_name()))
}

/// Read an optional BOOL key from the options MAP. Missing binds `None`;
/// present non-BOOLs bail.
fn optional_bool(map: &BTreeMap<String, Value>, func: &str, key: &str) -> Result<Option<bool>> {
    let Some(value) = map.get(key) else {
        return Ok(None);
    };
    let Some(b) = value.as_bool() else {
        bail!(
            "{func} option '{key}' must be a BOOL, got {}",
            value.type_name()
        );
    };
    Ok(Some(b))
}

/// Read an optional DURATION key from the options MAP: a DURATION value
/// binds directly, a STRING parses (`5s`, `100ms`). Missing binds `None`;
/// anything else bails.
fn optional_duration(
    map: &BTreeMap<String, Value>,
    func: &str,
    key: &str,
) -> Result<Option<Duration>> {
    let Some(value) = map.get(key) else {
        return Ok(None);
    };
    if let Some(duration) = value.as_duration() {
        return Ok(Some(duration));
    }
    if let Some(text) = value.as_str() {
        return oxdock_parser::command::parse_duration(text)
            .with_context(|| format!("{func} option '{key}' must be a duration, got {text:?}"))
            .map(Some);
    }
    bail!(
        "{func} option '{key}' must be a DURATION, got {}",
        value.type_name()
    )
}

/// Read an optional INT key from the options MAP. Missing binds `None`;
/// present non-INTs bail.
fn optional_int(map: &BTreeMap<String, Value>, func: &str, key: &str) -> Result<Option<i64>> {
    let Some(value) = map.get(key) else {
        return Ok(None);
    };
    let Some(n) = value.as_i64() else {
        bail!(
            "{func} option '{key}' must be an INT, got {}",
            value.type_name()
        );
    };
    Ok(Some(n))
}

/// Read the `NET_LISTENER` payload out of a DSL value.
fn listener_state(value: &Value, func: &str) -> Result<Arc<ListenerState>> {
    let Some(tag) = value.read_heap::<NetListenerTag>(NetListenerTag::descriptor()) else {
        bail!(
            "{func} expects a NET_LISTENER value, got {}",
            value.type_name()
        );
    };
    Ok(Arc::clone(tag.state()))
}

/// Claim a virtual service endpoint and report its address. `bind` is a
/// logical port (`"2251"`) or service name (`"demo-proxy"`): physical
/// binds in-script are rejected, `0` is reserved for the CLI outer
/// mapping. `options` is reserved for future socket settings and must be
/// an empty MAP today. Non-blocking: returns a MAP with `listener`
/// (NET_LISTENER), `addr` (STRING: the physical bind, or the virtual
/// endpoint echo when socketless), and `virtual` (STRING echo).
fn net_listen<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    registry: &Arc<EndpointRegistry>,
    bind: String,
    options: Value,
) -> Result<Value> {
    let _ = cx;
    let map = net_options(&options, "NET_LISTEN")?;
    if let Some(key) = map.keys().next() {
        bail!("NET_LISTEN() unknown option '{key}' (no options exist yet)");
    }
    if bind.contains('/') {
        bail!(
            "NET_LISTEN() invalid endpoint {bind:?}: protocol-qualified endpoints cannot be claimed (UDP listen is not supported); observe -p mappings with NET_PORT/NET_ADDR instead"
        );
    }
    let endpoint = parse_virtual_endpoint(&bind, "NET_LISTEN")?;
    let key = EndpointKey::tcp(endpoint);
    let (acquired, registry) = acquire_listener(registry, &key, "NET_LISTEN")?;
    let id = next_listener_id();
    let state = match acquired {
        AcquiredListener::Tcp { listener, addr } => {
            // The slot keeps the shared backlog socket; the handle pumps
            // on its own clone so close never races an in-flight accept.
            let owned = listener
                .try_clone()
                .context("NET_LISTEN cannot clone its listener")?;
            Arc::new(ListenerState::new_tcp(
                id,
                Arc::clone(&registry),
                key.clone(),
                addr,
                owned,
            ))
        }
        AcquiredListener::Memory => Arc::new(ListenerState::new_memory(
            id,
            Arc::clone(&registry),
            key.clone(),
        )),
        AcquiredListener::Offline => Arc::new(ListenerState::new_offline(
            id,
            Arc::clone(&registry),
            key.clone(),
        )),
    };
    let addr_text = state.addr_text().to_string();
    let mut out = BTreeMap::new();
    out.insert(
        "listener".to_string(),
        Value::mint_heap(NetListenerTag::descriptor(), NetListenerTag::new(state)),
    );
    out.insert("addr".to_string(), Value::string(addr_text));
    out.insert(
        "virtual".to_string(),
        Value::string(key.endpoint.to_string()),
    );
    Ok(Value::map(out))
}

/// Accept one connection and pump it through explicit pipes until it
/// closes. Must run inside `ASYNC`. `options` holds optional
/// `no_half_close` BOOL. Memory listeners pop a queued pipe pair instead
/// of accepting a socket; offline listeners wait for close/cancel. Returns
/// a MAP with `closed` (BOOL); the example below asserts the key on the
/// awaited result. The server sends first: the client side never EOFs
/// its input, so the reply cannot race teardown. This complete program
/// runs end to end under the docs conformance suite.
///
/// ```oxdock
/// IMPORT [STD, NET]
/// LET $l: MAP = NET_LISTEN("doc-net-demo", {})
///
/// # Accept one connection into fresh pipes.
/// LET $in: PIPE
/// LET $out: PIPE
/// LET $acc: HANDLE = ASYNC { NET_ACCEPT($l.listener, $in, $out, {}) }
///
/// # Connect a client to the listener.
/// LET $cin: PIPE
/// LET $cout: PIPE
/// LET $c: HANDLE = ASYNC { NET_CONNECT("doc-net-demo", $cin, $cout, {}) }
///
/// # Greet through the server pipe and wait for delivery.
/// WITH_IO [stdout=$in] ECHO "server-greeting"
/// LET $info: MAP = INSPECT($cout)
/// LET $empty: BOOL = $info.buffer_bytes == 0
/// WHILE $empty {
///     SLEEP 100ms
///     $info = INSPECT($cout)
///     $empty = $info.buffer_bytes == 0
/// }
/// ASSERT_CONTAINS $cout "server-greeting"
///
/// # Shut down; the awaited result carries the closed key.
/// CANCEL $c
/// LET $done: MAP = AWAIT $acc
/// ASSERT_EQ $done.closed true
/// NET_CLOSE($l.listener)
/// ```
#[oxdock_func(returns = "MAP", summary = "Accept one connection into pipes.")]
fn net_accept<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    listener: Value,
    in_pipe: Value,
    out_pipe: Value,
    options: Value,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!(
            "NET_ACCEPT requires ASYNC: wrap it as LET $t: HANDLE = ASYNC {{ NET_ACCEPT($l, $in, $out, {{}}) }}"
        );
    }
    let state = listener_state(&listener, "NET_ACCEPT")?;
    let map = net_options(&options, "NET_ACCEPT")?;
    let mut no_half_close = false;
    for key in map.keys() {
        if key != "no_half_close" {
            bail!("NET_ACCEPT() unknown option '{key}' (expected: no_half_close)");
        }
    }
    if let Some(flag) = optional_bool(map, "NET_ACCEPT", "no_half_close")? {
        no_half_close = flag;
    }
    if state.is_offline() {
        // Socketless handle: wait for close/cancel like a blocked accept
        // with no client in flight, then report the close.
        loop {
            if cx.is_cancelled() {
                bail!("NET_ACCEPT interrupted by cancellation");
            }
            if state.is_shutdown() {
                bail!("NET_ACCEPT: listener is closed");
            }
            std::thread::sleep(accept_tick());
        }
    }
    if state.is_memory() {
        // In-process rendezvous: pop the oldest CONNECT-side pair, polling
        // like the TCP accept loop so cancel and close win promptly.
        let pair = loop {
            if cx.is_cancelled() {
                bail!("NET_ACCEPT interrupted by cancellation");
            }
            if state.is_shutdown() {
                bail!("NET_ACCEPT: listener is closed");
            }
            if let Some(pair) = state.registry().dequeue_memory_session(state.endpoint()) {
                break pair;
            }
            std::thread::sleep(accept_tick());
        };
        // ACCEPT side: script-in flows to server_to_client, client_to_server
        // flows to script-out.
        pump_memory(
            cx,
            "NET_ACCEPT",
            &in_pipe,
            &out_pipe,
            &pair.client_to_server,
            &pair.server_to_client,
            !no_half_close,
        )?;
        let mut out = BTreeMap::new();
        out.insert("closed".to_string(), Value::bool(true));
        return Ok(Value::map(out));
    }
    let Some(listener) = state.try_clone_listener() else {
        bail!("NET_ACCEPT: listener is closed");
    };
    listener
        .set_nonblocking(true)
        .context("NET_ACCEPT failed to configure listener")?;
    let stream = loop {
        if cx.is_cancelled() {
            bail!("NET_ACCEPT interrupted by cancellation");
        }
        if state.is_shutdown() {
            bail!("NET_ACCEPT: listener is closed");
        }
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(accept_tick());
            }
            Err(err) => {
                bail!("NET_ACCEPT accept failed: {err}");
            }
        }
    };
    stream
        .set_nonblocking(false)
        .context("NET_ACCEPT failed to configure stream")?;
    pump_stream(
        cx,
        "NET_ACCEPT",
        &in_pipe,
        &out_pipe,
        stream,
        !no_half_close,
    )?;
    let mut out = BTreeMap::new();
    out.insert("closed".to_string(), Value::bool(true));
    Ok(Value::map(out))
}

/// Shut a listener down. Idempotent: returns BOOL true. A blocked
/// `NET_ACCEPT` observes the shutdown flag on its next tick and returns
/// instead of hanging.
#[oxdock_func(returns = "BOOL", summary = "Shut down a NET listener.")]
fn net_close<P: ProcessManager>(cx: &mut StepCtx<P>, listener: Value) -> Result<Value> {
    let _ = cx;
    let state = listener_state(&listener, "NET_CLOSE")?;
    state.request_shutdown();
    Ok(Value::bool(true))
}

/// Dial a TCP endpoint and pump it through explicit pipes until it
/// closes. Must run inside `ASYNC`. `options` holds optional `timeout`
/// (DURATION, default 10s) and `no_half_close` BOOL. `target` is a
/// `host:port` dial, a logical port (`"2251"`, loopback or a mapped
/// service), or a service name (memory rendezvous, or a mapped physical
/// address). Under `--offline` every dial bails before any DNS or socket
/// work. Returns a MAP with `closed` (BOOL).
fn net_connect<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    registry: &Arc<EndpointRegistry>,
    target: String,
    in_pipe: Value,
    out_pipe: Value,
    options: Value,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!(
            "NET_CONNECT requires ASYNC: wrap it as LET $t: HANDLE = ASYNC {{ NET_CONNECT($target, $in, $out, {{}}) }}"
        );
    }
    let map = net_options(&options, "NET_CONNECT")?;
    let mut timeout = DEFAULT_DIAL_TIMEOUT;
    let mut no_half_close = false;
    for key in map.keys() {
        if key != "timeout" && key != "no_half_close" {
            bail!("NET_CONNECT() unknown option '{key}' (expected: timeout, no_half_close)");
        }
    }
    if let Some(duration) = optional_duration(map, "NET_CONNECT", "timeout")? {
        timeout = duration;
    }
    if let Some(flag) = optional_bool(map, "NET_CONNECT", "no_half_close")? {
        no_half_close = flag;
    }
    // Sandbox gate first: offline runs open no OS sockets, so even a
    // literal dial must fail before parsing, DNS, or connect.
    if registry.is_offline() {
        bail!("NET_CONNECT failed: engine running in --offline mode");
    }
    if let Ok(endpoint) = parse_virtual_endpoint(&target, "NET_CONNECT") {
        let key = EndpointKey::tcp(endpoint);
        let params = ConnectParams {
            cx,
            registry,
            in_pipe: &in_pipe,
            out_pipe: &out_pipe,
            timeout,
            half_close: !no_half_close,
        };
        return connect_virtual(&params, &key, &target);
    }
    let (host, port) = parse_connect_endpoint(&target)?;
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .with_context(|| format!("NET_CONNECT {target:?} did not resolve"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("NET_CONNECT {target:?} did not resolve"))?;
    dial_tcp(
        cx,
        &target,
        addr,
        &in_pipe,
        &out_pipe,
        timeout,
        !no_half_close,
    )
}

/// Shared CONNECT plumbing: step context plus the resolved dial and pump
/// settings, so virtual resolution stays under the argument-count lint.
struct ConnectParams<'a, P: ProcessManager> {
    cx: &'a StepCtx<'a, P>,
    registry: &'a Arc<EndpointRegistry>,
    in_pipe: &'a Value,
    out_pipe: &'a Value,
    timeout: Duration,
    half_close: bool,
}

/// Dial one resolved address and pump it. Shared by raw `host:port`
/// dials, loopback port dials, and mapped-service dials.
fn dial_tcp<P: ProcessManager>(
    cx: &StepCtx<P>,
    target: &str,
    addr: SocketAddr,
    in_pipe: &Value,
    out_pipe: &Value,
    timeout: Duration,
    half_close: bool,
) -> Result<Value> {
    let stream = TcpStream::connect_timeout(&addr, timeout)
        .with_context(|| format!("NET_CONNECT {target:?} dial failed"))?;
    pump_stream(cx, "NET_CONNECT", in_pipe, out_pipe, stream, half_close)?;
    let mut out = BTreeMap::new();
    out.insert("closed".to_string(), Value::bool(true));
    Ok(Value::map(out))
}

/// Resolve a logical CONNECT target: memory rendezvous, mapped physical
/// address, or loopback dial. Names auto-create their memory slot so
/// client-before-server order works from either side.
fn connect_virtual<P: ProcessManager>(
    params: &ConnectParams<'_, P>,
    key: &EndpointKey,
    target: &str,
) -> Result<Value> {
    match &key.endpoint {
        VirtualEndpoint::Port(port) => match params.registry.slot_kind(key) {
            SlotKind::Memory => connect_memory(
                params.cx,
                params.registry,
                key,
                params.in_pipe,
                params.out_pipe,
                params.half_close,
            ),
            SlotKind::TcpBound(addr) => dial_tcp(
                params.cx,
                target,
                addr,
                params.in_pipe,
                params.out_pipe,
                params.timeout,
                params.half_close,
            ),
            SlotKind::TcpUnbound => {
                bail!("NET_CONNECT: '{key}' was never bound (the runner must call bind_all)")
            }
            SlotKind::Offline => bail!("NET_CONNECT: '{key}' is offline"),
            SlotKind::Unmapped => dial_tcp(
                params.cx,
                target,
                SocketAddr::from(([127, 0, 0, 1], *port)),
                params.in_pipe,
                params.out_pipe,
                params.timeout,
                params.half_close,
            ),
        },
        VirtualEndpoint::Name(_) => match params.registry.slot_kind(key) {
            SlotKind::Memory | SlotKind::Unmapped => {
                params.registry.ensure_memory_slot(key);
                connect_memory(
                    params.cx,
                    params.registry,
                    key,
                    params.in_pipe,
                    params.out_pipe,
                    params.half_close,
                )
            }
            SlotKind::TcpBound(addr) => dial_tcp(
                params.cx,
                target,
                addr,
                params.in_pipe,
                params.out_pipe,
                params.timeout,
                params.half_close,
            ),
            SlotKind::TcpUnbound => {
                bail!("NET_CONNECT: '{key}' was never bound (the runner must call bind_all)")
            }
            SlotKind::Offline => bail!("NET_CONNECT: '{key}' is offline"),
        },
    }
}

/// Join a memory rendezvous as the client: mint the pipe pair, queue the
/// server half (bounded: a missing consumer fails loudly instead of
/// leaking), and pump the client half. Zero sockets.
fn connect_memory<P: ProcessManager>(
    cx: &StepCtx<P>,
    registry: &Arc<EndpointRegistry>,
    key: &EndpointKey,
    in_pipe: &Value,
    out_pipe: &Value,
    half_close: bool,
) -> Result<Value> {
    let pair = MemoryPipePair::fresh();
    registry.enqueue_memory_session(key, pair.clone())?;
    // CONNECT side: script-in flows to client_to_server, server_to_client
    // flows to script-out.
    pump_memory(
        cx,
        "NET_CONNECT",
        in_pipe,
        out_pipe,
        &pair.server_to_client,
        &pair.client_to_server,
        half_close,
    )?;
    let mut out = BTreeMap::new();
    out.insert("closed".to_string(), Value::bool(true));
    Ok(Value::map(out))
}

/// Fetch a URL over HTTPS with a pure-Rust TLS stack (`ureq` plus
/// `rustls`, no system openssl or curl). `dest` is either a STRING file
/// path (synchronous download, guarded under the workspace root) or a
/// PIPE value (requires `ASYNC`, bytes stream into the pipe, then EOF).
/// A PIPE dest is always wire-to-script: the fetch writes the body and
/// the script reads, the same direction as `out_pipe` in `NET_CONNECT`.
/// Never write into it; reads observe EOF when the fetch closes.
/// `options` is a MAP with optional `sha256` (STRING hex digest, verified
/// on the wire), `timeout` (DURATION, default 30s), and `retries` (INT,
/// default 2). Under `--offline` every fetch bails before any DNS or
/// socket work. Returns a MAP with `status` (INT), `bytes` (INT),
/// `sha256` (STRING hex), plus `path` (STRING) for file destinations or
/// `closed` (BOOL) for pipe destinations.
fn net_fetch<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    registry: &Arc<EndpointRegistry>,
    url: String,
    dest: Value,
    options: Value,
) -> Result<Value> {
    let map = net_options(&options, "NET_FETCH")?;
    let mut sha256: Option<String> = None;
    let mut timeout = DEFAULT_FETCH_TIMEOUT;
    let mut retries: u32 = 2;
    for key in map.keys() {
        if key != "sha256" && key != "timeout" && key != "retries" {
            bail!("NET_FETCH() unknown option '{key}' (expected: sha256, timeout, retries)");
        }
    }
    if let Some(raw) = map.get("sha256") {
        let Some(text) = raw.as_str() else {
            bail!(
                "NET_FETCH option 'sha256' must be a STRING, got {}",
                raw.type_name()
            );
        };
        sha256 = Some(parse_sha256_hex(text, "NET_FETCH")?);
    }
    if let Some(duration) = optional_duration(map, "NET_FETCH", "timeout")? {
        timeout = duration;
    }
    if let Some(n) = optional_int(map, "NET_FETCH", "retries")? {
        if !(0..=10).contains(&n) {
            bail!("NET_FETCH option 'retries' must be between 0 and 10, got {n}");
        }
        retries = u32::try_from(n).unwrap_or(0);
    }
    if registry.is_offline() {
        bail!("NET_FETCH failed: engine running in --offline mode");
    }
    let is_pipe = dest.as_str().is_none();
    if let Some(path_raw) = dest.as_str() {
        let guarded = anchor_fetch_path(cx.cwd().root(), path_raw)?;
        let result = fetch_file_streamed(&guarded, &url, timeout, retries, || cx.is_cancelled())?;
        if let Some(expected) = sha256 {
            verify_sha256(&result.sha256, &expected)?;
        }
        let mut out = BTreeMap::new();
        out.insert("status".to_string(), Value::int(i64::from(result.status)));
        out.insert(
            "bytes".to_string(),
            Value::int(i64::try_from(result.bytes).unwrap_or(i64::MAX)),
        );
        out.insert("sha256".to_string(), Value::string(result.sha256));
        out.insert("path".to_string(), Value::string(path_raw.to_string()));
        return Ok(Value::map(out));
    }
    let Some(_pipe_handle) = dest.as_pipe_handle() else {
        bail!(
            "NET_FETCH() argument `$dest` must be a STRING file path or a PIPE, got {}",
            dest.type_name()
        );
    };
    if is_pipe && !cx.is_async_task() {
        bail!(
            "NET_FETCH requires ASYNC for PIPE destinations: wrap it as LET $t: HANDLE = ASYNC {{ NET_FETCH($url, $pipe, {{}}) }}"
        );
    }
    let writer = cx.pipe_writer(&dest)?;
    let result = fetch_stream(
        &url,
        timeout,
        retries,
        |chunk| {
            let mut guard = writer.lock().unwrap_or_else(|e| e.into_inner());
            // `Box<dyn Write>` indirection: the guard derefs to the trait
            // object, so no `Write` import is needed for these calls.
            std::io::Write::write_all(&mut *guard, chunk)
                .context("NET_FETCH failed to write response body into pipe")
        },
        || cx.is_cancelled(),
    )?;
    // Flush once at the end: per-chunk flushes would turn streaming into
    // a syscall per 8KB.
    writer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .flush()
        .ok();
    if let Some(expected) = sha256 {
        verify_sha256(&result.sha256, &expected)?;
    }
    cx.close_pipe(&dest)?;
    let mut out = BTreeMap::new();
    out.insert("closed".to_string(), Value::bool(true));
    out.insert("status".to_string(), Value::int(i64::from(result.status)));
    out.insert(
        "bytes".to_string(),
        Value::int(i64::try_from(result.bytes).unwrap_or(i64::MAX)),
    );
    out.insert("sha256".to_string(), Value::string(result.sha256));
    Ok(Value::map(out))
}

/// Anchor a `NET_FETCH` file destination under the workspace root,
/// mirroring READ and WRITE: a leading `/` means the workspace root, and
/// anything escaping still bails. Relative paths stay root-anchored.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn anchor_fetch_path(
    root: &std::path::Path,
    raw: &str,
) -> Result<oxdock_fs::GuardedPath> {
    use oxdock_fs::{GuardedPath, PathResolver, to_forward_slashes};
    let normalized = to_forward_slashes(raw);
    let rel_path = std::path::Path::new(&normalized);
    if PathResolver::is_absolute_or_rooted(rel_path) {
        if let Ok(guarded) = GuardedPath::new(root, rel_path) {
            return Ok(guarded);
        }
        let rel = PathResolver::root_relative_path(rel_path);
        let Some(rel_str) = rel.to_str().filter(|s| !s.is_empty()) else {
            bail!("NET_FETCH dest escapes the workspace: {raw}");
        };
        return GuardedPath::new_root(root)
            .with_context(|| "NET_FETCH cannot guard the workspace root".to_string())?
            .join(rel_str)
            .with_context(|| format!("NET_FETCH dest escapes the workspace: {raw}"));
    }
    GuardedPath::new_root(root)
        .with_context(|| "NET_FETCH cannot guard the workspace root".to_string())?
        .join(&normalized)
        .with_context(|| format!("NET_FETCH dest escapes the workspace: {raw}"))
}

/// Stream a URL directly into a workspace-anchored file so all I/O stays
/// inside the guard. Creates parent directories on demand. The file lands
/// atomically in one pass: bytes stream through the open handle while the
/// digest accumulates, so arbitrarily large artifacts never buffer.
fn fetch_file_streamed(
    guarded: &oxdock_fs::GuardedPath,
    url: &str,
    timeout: Duration,
    retries: u32,
    cancelled: impl Fn() -> bool,
) -> Result<FetchResult> {
    use oxdock_fs::PathResolver;
    let root = guarded.root();
    let resolver =
        PathResolver::new(root, root).context("NET_FETCH cannot open the workspace resolver")?;
    resolver.ensure_parent_dir(guarded)?;
    let mut handle = resolver.open_write(guarded)?;
    let result = fetch_stream(
        url,
        timeout,
        retries,
        |chunk| {
            use std::io::Write;
            handle
                .write_all(chunk)
                .context("NET_FETCH failed to write response body to file")
        },
        cancelled,
    );
    // Flush best-effort: a failed fetch leaves a partial file that the
    // next fetch overwrites from scratch.
    use std::io::Write;
    handle.flush().ok();
    result
}

/// The `NET` host module: virtual-endpoint listeners plus dial-out and
/// memory sessions, bridged to DSL pipes. Generic over the process
/// manager like every host module.
pub fn module_with<P: ProcessManager>() -> HostModule<P> {
    module_with_endpoints(Arc::new(EndpointRegistry::new(false)))
}

/// The `NET` host module resolving through `registry`: `NET_LISTEN` and
/// `NET_CONNECT` close over it (the `#[oxdock_func]` macro only generates
/// closers-over-nothing, so these two entries are built by hand with the
/// same arity/type errors the macro would emit); `ACCEPT`/`CLOSE` reach
/// the same registry through their `NET_LISTENER` handle.
pub fn module_with_endpoints<P: ProcessManager>(registry: Arc<EndpointRegistry>) -> HostModule<P> {
    HostModule {
        name: "NET".to_string(),
        funcs: vec![
            net_listen_registration(Arc::clone(&registry)),
            NetAccept::registration(),
            NetClose::registration(),
            net_connect_registration(Arc::clone(&registry)),
            net_port_registration(Arc::clone(&registry)),
            net_addr_registration(Arc::clone(&registry)),
            net_fetch_registration(Arc::clone(&registry)),
        ],
        types: vec![NetListenerTag::descriptor()],
    }
}

/// Hand-built `NET_LISTEN` entry: same shape the macro would emit
/// (arity check, `STRING`/`Value` unpacking, metadata), plus the captured
/// registry threaded into [`net_listen`].
fn net_listen_registration<P: ProcessManager>(
    registry: Arc<EndpointRegistry>,
) -> HostRegistration<P> {
    let func: NativeFn<P> = Arc::new(move |cx, values| {
        if values.len() != 2 {
            bail!("NET_LISTEN() expects 2 argument(s), got {}", values.len());
        }
        let mut values = values.into_iter();
        let bind = match values.next().expect("arity checked above").as_str() {
            Some(s) => s.to_string(),
            None => bail!("NET_LISTEN() argument `$bind` must be a STRING"),
        };
        let options = values.next().expect("arity checked above");
        net_listen(cx, &registry, bind, options)
    });
    HostRegistration::Stateful {
        name: "NET_LISTEN".to_string(),
        meta: FuncMeta {
            name: "NET_LISTEN".to_string(),
            // Assigned at registration (`register_module` fills the
            // module, like the macro's markers): never set here.
            module: String::new(),
            kind: FuncKind::HostCtx,
            params: Some(vec![
                FuncParam {
                    name: "bind".to_string(),
                    param_type: Some("STRING".to_string()),
                },
                FuncParam {
                    name: "options".to_string(),
                    param_type: None,
                },
            ]),
            returns: Some("MAP".to_string()),
            rpn: false,
            summary: "Claim a virtual service endpoint and report its address.",
            docs: "Claim a virtual service endpoint and report its address.",
        },
        func,
    }
}

/// Hand-built `NET_CONNECT` entry: same shape the macro would emit, plus
/// the captured registry threaded into [`net_connect`].
fn net_connect_registration<P: ProcessManager>(
    registry: Arc<EndpointRegistry>,
) -> HostRegistration<P> {
    let func: NativeFn<P> = Arc::new(move |cx, values| {
        if values.len() != 4 {
            bail!("NET_CONNECT() expects 4 argument(s), got {}", values.len());
        }
        let mut values = values.into_iter();
        let target = match values.next().expect("arity checked above").as_str() {
            Some(s) => s.to_string(),
            None => bail!("NET_CONNECT() argument `$target` must be a STRING"),
        };
        let in_pipe = values.next().expect("arity checked above");
        let out_pipe = values.next().expect("arity checked above");
        let options = values.next().expect("arity checked above");
        net_connect(cx, &registry, target, in_pipe, out_pipe, options)
    });
    HostRegistration::Stateful {
        name: "NET_CONNECT".to_string(),
        meta: FuncMeta {
            name: "NET_CONNECT".to_string(),
            // Assigned at registration, like the macro's markers.
            module: String::new(),
            kind: FuncKind::HostCtx,
            params: Some(vec![
                FuncParam {
                    name: "target".to_string(),
                    param_type: Some("STRING".to_string()),
                },
                FuncParam {
                    name: "in_pipe".to_string(),
                    param_type: None,
                },
                FuncParam {
                    name: "out_pipe".to_string(),
                    param_type: None,
                },
                FuncParam {
                    name: "options".to_string(),
                    param_type: None,
                },
            ]),
            returns: Some("MAP".to_string()),
            rpn: false,
            summary: "Dial a TCP endpoint into pipes.",
            docs: "Dial a TCP endpoint into pipes.",
        },
        func,
    }
}

/// Hand-built `NET_FETCH` entry: same shape the macro would emit, plus
/// the captured registry threaded into [`net_fetch`].
fn net_fetch_registration<P: ProcessManager>(
    registry: Arc<EndpointRegistry>,
) -> HostRegistration<P> {
    let func: NativeFn<P> = Arc::new(move |cx, values| {
        if values.len() != 3 {
            bail!("NET_FETCH() expects 3 argument(s), got {}", values.len());
        }
        let mut values = values.into_iter();
        let url = match values.next().expect("arity checked above").as_str() {
            Some(s) => s.to_string(),
            None => bail!("NET_FETCH() argument `$url` must be a STRING"),
        };
        let dest = values.next().expect("arity checked above");
        let options = values.next().expect("arity checked above");
        net_fetch(cx, &registry, url, dest, options)
    });
    HostRegistration::Stateful {
        name: "NET_FETCH".to_string(),
        meta: FuncMeta {
            name: "NET_FETCH".to_string(),
            // Assigned at registration, like the macro's markers.
            module: String::new(),
            kind: FuncKind::HostCtx,
            params: Some(vec![
                FuncParam {
                    name: "url".to_string(),
                    param_type: Some("STRING".to_string()),
                },
                FuncParam {
                    name: "dest".to_string(),
                    param_type: None,
                },
                FuncParam {
                    name: "options".to_string(),
                    param_type: None,
                },
            ]),
            returns: Some("MAP".to_string()),
            rpn: false,
            summary: "Fetch a URL body into a file or pipe.",
            docs: "Fetch a URL over HTTPS with a pure-Rust TLS stack into a guarded file path (synchronous) or a PIPE (requires ASYNC). A PIPE dest is always wire-to-script: the fetch writes the body and the script reads, the same direction as out_pipe in NET_CONNECT. Never write into it; reads observe EOF when the fetch closes. Options: sha256 STRING hex digest, timeout DURATION, retries INT. Errors under --offline.",
        },
        func,
    }
}

/// Report the bound port of a virtual service endpoint without claiming
/// it, so a `-p`-mapped outer port (including ephemeral `-p 0:<inner>`
/// resolutions) can be routed into an inner `RUN` through normal
/// `LET`/`ENV` expansion. `target` is a logical port (`"2251"`) or a
/// service name (`"demo-proxy"`), optionally protocol-qualified
/// (`"tcp/web"`, `"udp/dns"`). Bare text resolves TCP with
/// single-protocol fallback; text bound under both protocols must be
/// qualified. Unbound targets bail: no `0` sentinel. The runnable example
/// lives on the function's metadata docs (rendered into the function
/// reference and executed by the docs conformance suite).
fn net_port<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    registry: &Arc<EndpointRegistry>,
    target: String,
) -> Result<Value> {
    let _ = cx;
    let addr = registry.resolve_ref(&target, "NET_PORT")?;
    Ok(Value::int(i64::from(addr.port())))
}

/// Hand-built `NET_PORT` entry: same shape the macro would emit, plus the
/// captured registry threaded into [`net_port`].
fn net_port_registration<P: ProcessManager>(
    registry: Arc<EndpointRegistry>,
) -> HostRegistration<P> {
    let func: NativeFn<P> = Arc::new(move |cx, values| {
        if values.len() != 1 {
            bail!("NET_PORT() expects 1 argument(s), got {}", values.len());
        }
        let mut values = values.into_iter();
        let target = match values.next().expect("arity checked above").as_str() {
            Some(s) => s.to_string(),
            None => bail!("NET_PORT() argument `$target` must be a STRING"),
        };
        net_port(cx, &registry, target)
    });
    HostRegistration::Stateful {
        name: "NET_PORT".to_string(),
        meta: FuncMeta {
            name: "NET_PORT".to_string(),
            // Assigned at registration, like the macro's markers.
            module: String::new(),
            kind: FuncKind::HostCtx,
            params: Some(vec![FuncParam {
                name: "target".to_string(),
                param_type: Some("STRING".to_string()),
            }]),
            returns: Some("INT".to_string()),
            rpn: false,
            summary: "Report the bound port of a virtual service endpoint.",
            docs: indoc::indoc! {r#"
                Report the bound port of a virtual service endpoint without claiming it, so a `-p`-mapped outer port (including ephemeral `-p 0:<inner>` resolutions) can be routed into an inner `RUN` through normal `LET`/`ENV` expansion. `target` is a logical port (`"2251"`) or a service name (`"demo-proxy"`), optionally protocol-qualified (`"tcp/web"`, `"udp/dns"`). Bare text resolves TCP with single-protocol fallback; text bound under both protocols must be qualified. Unbound targets bail: no `0` sentinel.

                ```oxdock
                IMPORT [STD, NET]
                LET $l: MAP = NET_LISTEN("23791", {})

                # Observe the bound port without claiming the slot twice.
                LET $port: INT = NET_PORT("23791")
                ASSERT_EQ $port 23791

                # The shell reads its own environment, with per-platform spelling:
                # quoted "$VAR" passes the parser through untouched on unix ...
                ENV PROXY_PORT="{{ $port }}"

                [unix] LET $o: STRING = RUN echo serving on "$PROXY_PORT"

                # ... while cmd expands %VAR% on Windows.
                [windows] LET $o: STRING = RUN echo serving on %PROXY_PORT%

                ASSERT_CONTAINS $o "23791"
                NET_CLOSE($l.listener)
                ```"#},
        },
        func,
    }
}

/// Report the full bound socket address (`ip:port`) of a virtual service
/// endpoint without claiming it: the dial-string companion to [`net_port`].
/// Same qualifier and fallback rules; unbound targets bail, never an
/// empty string. The runnable example lives on the function's metadata
/// docs (rendered into the function reference and executed by the docs
/// conformance suite).
fn net_addr<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    registry: &Arc<EndpointRegistry>,
    target: String,
) -> Result<Value> {
    let _ = cx;
    let addr = registry.resolve_ref(&target, "NET_ADDR")?;
    Ok(Value::string(addr.to_string()))
}

/// Hand-built `NET_ADDR` entry: same shape the macro would emit, plus the
/// captured registry threaded into [`net_addr`].
fn net_addr_registration<P: ProcessManager>(
    registry: Arc<EndpointRegistry>,
) -> HostRegistration<P> {
    let func: NativeFn<P> = Arc::new(move |cx, values| {
        if values.len() != 1 {
            bail!("NET_ADDR() expects 1 argument(s), got {}", values.len());
        }
        let mut values = values.into_iter();
        let target = match values.next().expect("arity checked above").as_str() {
            Some(s) => s.to_string(),
            None => bail!("NET_ADDR() argument `$target` must be a STRING"),
        };
        net_addr(cx, &registry, target)
    });
    HostRegistration::Stateful {
        name: "NET_ADDR".to_string(),
        meta: FuncMeta {
            name: "NET_ADDR".to_string(),
            // Assigned at registration, like the macro's markers.
            module: String::new(),
            kind: FuncKind::HostCtx,
            params: Some(vec![FuncParam {
                name: "target".to_string(),
                param_type: Some("STRING".to_string()),
            }]),
            returns: Some("STRING".to_string()),
            rpn: false,
            summary: "Report the bound socket address of a virtual service endpoint.",
            docs: indoc::indoc! {r#"
                Report the full bound socket address (`ip:port`) of a virtual service endpoint without claiming it: the dial-string companion to `NET_PORT`. Same qualifier and fallback rules; unbound targets bail, never an empty string.

                ```oxdock
                IMPORT [STD, NET]
                LET $l: MAP = NET_LISTEN("23792", {})

                # Observe the dial string without claiming the slot twice.
                LET $addr: STRING = NET_ADDR("23792")
                ASSERT_EQ $addr "127.0.0.1:23792"
                NET_CLOSE($l.listener)
                ```"#},
        },
        func,
    }
}
