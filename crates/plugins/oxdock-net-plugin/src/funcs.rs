//! The `NET` host module: virtual-endpoint listener handles plus
//! explicit-pipe pumps for dial-out and memory sessions.
//!
//! Pipe direction convention (fixed for every func): `out_pipe` carries
//! bytes produced by the wire side (the DSL reads them), `in_pipe`
//! carries bytes consumed by the wire side (the DSL writes them). Pumps
//! are always full-duplex: explicit pipes have no `Null` spelling, so the
//! builtin read-only mode has no equivalent here.
//!
//! `NET_LISTEN` and `NET_CONNECT` resolve through the run's
//! [`EndpointRegistry`]: the registry rides a closure-captured `Arc` in
//! hand-built [`HostRegistration::Stateful`] entries (see
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
use crate::state::ListenerState;
use crate::types::NetListenerTag;
use crate::validate::{VirtualEndpoint, parse_connect_endpoint, parse_virtual_endpoint};

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
    let endpoint = parse_virtual_endpoint(&bind, "NET_LISTEN")?;
    let (acquired, registry) = acquire_listener(registry, &endpoint, "NET_LISTEN")?;
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
                endpoint.clone(),
                addr,
                owned,
            ))
        }
        AcquiredListener::Memory => Arc::new(ListenerState::new_memory(
            id,
            Arc::clone(&registry),
            endpoint.clone(),
        )),
        AcquiredListener::Offline => Arc::new(ListenerState::new_offline(
            id,
            Arc::clone(&registry),
            endpoint.clone(),
        )),
    };
    let addr_text = state.addr_text().to_string();
    let mut out = BTreeMap::new();
    out.insert(
        "listener".to_string(),
        Value::mint_heap(NetListenerTag::descriptor(), NetListenerTag::new(state)),
    );
    out.insert("addr".to_string(), Value::string(addr_text));
    out.insert("virtual".to_string(), Value::string(endpoint.to_string()));
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
        let params = ConnectParams {
            cx,
            registry,
            in_pipe: &in_pipe,
            out_pipe: &out_pipe,
            timeout,
            half_close: !no_half_close,
        };
        return connect_virtual(&params, &endpoint, &target);
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
    endpoint: &VirtualEndpoint,
    target: &str,
) -> Result<Value> {
    match endpoint {
        VirtualEndpoint::Port(port) => match params.registry.slot_kind(endpoint) {
            SlotKind::Memory => connect_memory(
                params.cx,
                params.registry,
                endpoint,
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
                bail!("NET_CONNECT: '{endpoint}' was never bound (the runner must call bind_all)")
            }
            SlotKind::Offline => bail!("NET_CONNECT: '{endpoint}' is offline"),
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
        VirtualEndpoint::Name(_) => match params.registry.slot_kind(endpoint) {
            SlotKind::Memory | SlotKind::Unmapped => {
                params.registry.ensure_memory_slot(endpoint);
                connect_memory(
                    params.cx,
                    params.registry,
                    endpoint,
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
                bail!("NET_CONNECT: '{endpoint}' was never bound (the runner must call bind_all)")
            }
            SlotKind::Offline => bail!("NET_CONNECT: '{endpoint}' is offline"),
        },
    }
}

/// Join a memory rendezvous as the client: mint the pipe pair, queue the
/// server half (bounded: a missing consumer fails loudly instead of
/// leaking), and pump the client half. Zero sockets.
fn connect_memory<P: ProcessManager>(
    cx: &StepCtx<P>,
    registry: &Arc<EndpointRegistry>,
    endpoint: &VirtualEndpoint,
    in_pipe: &Value,
    out_pipe: &Value,
    half_close: bool,
) -> Result<Value> {
    let pair = MemoryPipePair::fresh();
    registry.enqueue_memory_session(endpoint, pair.clone())?;
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
            net_connect_registration(registry),
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
