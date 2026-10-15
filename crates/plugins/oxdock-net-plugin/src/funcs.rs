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
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use oxdock_core::{Field, HostModule, OxDockFn, OxDockType, RecordSchema, StepCtx, TypeTag, Value};
use oxdock_func_macro::oxdock_func;
use oxdock_pipe::PipeHandle;
use oxdock_process::ProcessManager;

use crate::bridge::{pump_memory, pump_stream};
use crate::endpoints::{
    AcquiredListener, EndpointRegistry, MemoryPipePair, ServiceTarget, SocketRequirement,
    acquire_listener, resolve_service_target,
};
use crate::fetch::{FetchOptions, fetch_text_with_options};
use crate::state::ListenerState;
use crate::types::NetListenerTag;
use crate::validate::{EndpointKey, parse_virtual_endpoint};

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

/// Claim a runner-mapped service endpoint and report its address.
/// `bind` is a service name; the runner maps it to a physical bind with
/// `-p`/`--listen` or to memory explicitly. Unmapped names are a hard
/// error before execution. Scripts never bind. `options` is reserved for
/// future socket settings and must be an empty MAP today. Non-blocking:
/// returns a MAP with `listener` (NET_LISTENER), `addr` (STRING: the
/// physical bind, or the virtual endpoint echo when socketless), and
/// `virtual` (STRING echo).
#[oxdock_func(returns = TypeTag::Record(listen_result_fields()), summary = "Claim a runner-mapped service endpoint and report its address.")]
#[example(
    name = "claim and close",
    code = r###"
    IMPORT [STD, NET]
    LET $l: NET_LISTEN_INFO = NET_LISTEN("doc-net-listen", {})
    ASSERT_EQ $l.virtual "doc-net-listen"
    NET_CLOSE($l.listener)
    "###
)]
fn net_listen<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Service name; the runner maps it with -p/--listen or memory.
    bind: String,
    /// Reserved for future socket settings; must be an empty MAP today.
    #[options()]
    options: BTreeMap<String, Value>,
) -> Result<Value> {
    let _ = cx;
    let registry = cx
        .module_state::<EndpointRegistry>("NET")
        .expect("NET module registers its endpoint registry as module state");
    // Unknown keys already failed in the wrapper: `#[options()]`
    // declares a reserved-but-keyless MAP, so `check_options` rejects
    // every passed key before the body runs.
    let _ = options;
    if bind.contains('/') {
        bail!(
            "NET_LISTEN() invalid endpoint {bind:?}: protocol-qualified endpoints cannot be claimed (UDP listen is not supported); observe -p mappings with NET_PORT/NET_ADDR instead"
        );
    }
    let endpoint = parse_virtual_endpoint(&bind, "NET_LISTEN")?;
    let key = EndpointKey::tcp(endpoint);
    let (acquired, registry) = acquire_listener(
        &registry,
        &key,
        "NET_LISTEN",
        SocketRequirement::SocketlessOk,
    )?;
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
/// its input, so the reply cannot race teardown.
///
#[oxdock_func(returns = TypeTag::Record(std::sync::Arc::from([Field {
    name: "closed".to_string(),
    ty: TypeTag::Bool,
    docs: "True when the connection closed cleanly.",
    optional: false,
}])), summary = "Accept one connection into pipes.")]
#[example(
    name = "accept and greet",
    code = r###"
    IMPORT [STD, NET]
    LET $l: NET_LISTEN_INFO = NET_LISTEN("doc-net-demo", {})

    # Accept one connection into fresh pipes.
    LET $in: PIPE
    LET $out: PIPE
    LET $acc: HANDLE = ASYNC { NET_ACCEPT($l.listener, $in, $out, {}) }

    # Connect a client to the listener.
    LET $cin: PIPE
    LET $cout: PIPE
    LET $c: HANDLE = ASYNC { NET_CONNECT("doc-net-demo", $cin, $cout, {}) }

    # Greet through the server pipe and wait for delivery.
    WITH_IO [stdout=$in] ECHO "server-greeting"
    LET $info: MAP<ANY> = INSPECT($cout)
    LET $empty: BOOL = $info.buffer_bytes == 0
    WHILE $empty {
        SLEEP 100ms
        $info = INSPECT($cout)
        $empty = $info.buffer_bytes == 0
    }
    ASSERT_CONTAINS $cout "server-greeting"

    # Shut down; the awaited result carries the closed key.
    CANCEL $c
    LET $done: MAP<ANY> = AWAIT $acc
    ASSERT_EQ $done.closed true
    NET_CLOSE($l.listener)
    "###
)]
fn net_accept<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Listener handle from `NET_LISTEN`.
    listener: NetListenerTag,
    /// Pipe carrying bytes consumed by the wire side.
    in_pipe: PipeHandle,
    /// Pipe carrying bytes produced by the wire side.
    out_pipe: PipeHandle,
    /// Optional `no_half_close` setting.
    #[options("no_half_close?: BOOL")]
    options: BTreeMap<String, Value>,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!(
            "NET_ACCEPT requires ASYNC: wrap it as LET $t: HANDLE = ASYNC {{ NET_ACCEPT($l, $in, $out, {{}}) }}"
        );
    }
    let state = Arc::clone(listener.state());
    let map = &options;
    // Unknown keys already failed in the wrapper against the same
    // `#[options]` static the metadata renders; only read known keys.
    let mut no_half_close = false;
    if let Some(value) = map.get("no_half_close") {
        // `check_options` verified `BOOL`; narrow directly.
        no_half_close = value.as_bool().expect("check_options verified BOOL");
    }
    // Downstream pumps take `&Value`: re-wrap the extracted handles so
    // every helper below keeps its Value-based shape.
    let in_pipe = Value::pipe_handle(in_pipe);
    let out_pipe = Value::pipe_handle(out_pipe);
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
#[oxdock_func(returns = TypeTag::Bool, summary = "Shut down a NET listener.")]
#[example(
    name = "shut down",
    code = r###"
    IMPORT [STD, NET]
    LET $l: NET_LISTEN_INFO = NET_LISTEN("doc-net-close", {})
    LET $ok: BOOL = NET_CLOSE($l.listener)
    ASSERT_EQ $ok true
    "###
)]
fn net_close<P: ProcessManager>(cx: &mut StepCtx<P>, listener: NetListenerTag) -> Result<Value> {
    let _ = cx;
    let state = Arc::clone(listener.state());
    state.request_shutdown();
    Ok(Value::bool(true))
}

/// Dial a runner-mapped service and pump it through explicit pipes
/// until it closes. Must run inside `ASYNC`. `options` holds optional
/// `timeout` (DURATION, default 10s) and `no_half_close` BOOL. `target`
/// is a service name resolved through runner mappings (or an
/// explicitly-mapped memory rendezvous): physical dials come only from
/// mappings, never from script text. Under `--offline` every dial bails
/// before any socket work. Returns a MAP with `closed` (BOOL).
#[oxdock_func(
    returns = TypeTag::Record(std::sync::Arc::from([Field {
        name: "closed".to_string(),
        ty: TypeTag::Bool,
        docs: "True when the connection closed cleanly.",
        optional: false,
    }])),
    summary = "Dial a TCP endpoint into pipes.",
)]
#[example(
    name = "dial and close",
    code = r###"
    IMPORT [STD, NET]
    LET $l: NET_LISTEN_INFO = NET_LISTEN("doc-net-connect", {})
    LET $in: PIPE
    LET $out: PIPE
    LET $acc: HANDLE = ASYNC { NET_ACCEPT($l.listener, $in, $out, {}) }
    LET $cin: PIPE
    LET $cout: PIPE
    LET $c: HANDLE = ASYNC { NET_CONNECT("doc-net-connect", $cin, $cout, {}) }

    # Close both inputs: each pump observes EOF, half-closes, and both
    # tasks return their result maps with no cancellation. Closing only
    # one side would strand the peer with no EOF.
    WITH_IO [stdout=$cin] ECHO "client-hello"
    WITH_IO [stdout=$in] ECHO "server-greeting"
    LET $done: MAP<ANY> = AWAIT $c
    ASSERT_EQ $done.closed true
    LET $adone: MAP<ANY> = AWAIT $acc
    ASSERT_EQ $adone.closed true
    NET_CLOSE($l.listener)
    "###
)]
fn net_connect<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Dial target: a service name resolved through runner mappings.
    target: String,
    /// Pipe carrying bytes consumed by the wire side.
    in_pipe: PipeHandle,
    /// Pipe carrying bytes produced by the wire side.
    out_pipe: PipeHandle,
    /// Optional `timeout` and `no_half_close` settings.
    #[options("timeout?: DURATION|STRING = 10s", "no_half_close?: BOOL")]
    options: BTreeMap<String, Value>,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!(
            "NET_CONNECT requires ASYNC: wrap it as LET $t: HANDLE = ASYNC {{ NET_CONNECT($target, $in, $out, {{}}) }}"
        );
    }
    // Module-shared endpoint registry: stashed at registration, so
    // this entry stays declarative with no captured closure.
    let registry = cx
        .module_state::<EndpointRegistry>("NET")
        .expect("NET module registers its endpoint registry as module state");
    // Pump plumbing takes `&Value` pipes: re-wrap the extracted
    // handles so the bridge keeps its Value-based shape.
    let in_pipe = Value::pipe_handle(in_pipe);
    let out_pipe = Value::pipe_handle(out_pipe);
    let map = &options;
    let mut timeout = DEFAULT_DIAL_TIMEOUT;
    let mut no_half_close = false;
    if let Some(value) = map.get("timeout") {
        // `check_options` verified `DURATION|STRING`; narrow directly.
        if let Some(duration) = value.as_duration() {
            timeout = duration;
        } else {
            let text = value
                .as_str()
                .expect("check_options verified DURATION|STRING");
            timeout = oxdock_parser::command::parse_duration(text).with_context(|| {
                format!("NET_CONNECT option 'timeout' must be a duration, got {text:?}")
            })?;
        }
    }
    if let Some(value) = map.get("no_half_close") {
        no_half_close = value.as_bool().expect("check_options verified BOOL");
    }
    // Sandbox gate first: offline runs open no OS sockets, so even a
    // literal dial must fail before parsing, DNS, or connect.
    if registry.is_offline() {
        bail!("NET_CONNECT failed: engine running in --offline mode");
    }
    // Strictly symbolic: every target is a service name resolved through
    // runner mappings (or memory rendezvous). Physical `host:port` shapes
    // already failed in `parse_virtual_endpoint` below.
    let endpoint = parse_virtual_endpoint(&target, "NET_CONNECT")?;
    let key = EndpointKey::tcp(endpoint);
    let params = ConnectParams {
        cx,
        registry: &registry,
        in_pipe: &in_pipe,
        out_pipe: &out_pipe,
        timeout,
        half_close: !no_half_close,
    };
    connect_virtual(&params, &key, &target)
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

/// Dial one runner-resolved address and pump it. Only mapped-service
/// dials reach here: resolution already ran through the shared resolver.
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

/// Resolve a CONNECT target through the single shared resolver: a
/// runner-mapped physical address, or an explicitly-mapped memory
/// rendezvous. No script string ever dials a physical address directly.
fn connect_virtual<P: ProcessManager>(
    params: &ConnectParams<'_, P>,
    key: &EndpointKey,
    target: &str,
) -> Result<Value> {
    match resolve_service_target(
        params.registry,
        key,
        "NET_CONNECT",
        SocketRequirement::SocketlessOk,
    )? {
        ServiceTarget::Tcp(addr) => dial_tcp(
            params.cx,
            target,
            addr,
            params.in_pipe,
            params.out_pipe,
            params.timeout,
            params.half_close,
        ),
        ServiceTarget::Memory => connect_memory(
            params.cx,
            params.registry,
            key,
            params.in_pipe,
            params.out_pipe,
            params.half_close,
        ),
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

/// The `NET` host module: virtual-endpoint listeners plus dial-out and
/// memory sessions, bridged to DSL pipes. Generic over the process
/// manager like every host module.
pub fn module_with<P: ProcessManager>() -> HostModule<P> {
    module_with_endpoints(Arc::new(EndpointRegistry::new(false)))
}

/// The `NET` host module resolving through `registry`: the registry
/// travels as module state (`HostModule::with_state`) for bodies to
/// fetch from `cx`, so every entry stays `#[oxdock_func]`;
/// `ACCEPT`/`CLOSE` reach the same registry through their
/// `NET_LISTENER` handle. A pre-run service check rides along: every
/// service the steps name must be fully bound before execution starts.
pub fn module_with_endpoints<P: ProcessManager>(registry: Arc<EndpointRegistry>) -> HostModule<P> {
    let check = registry.pre_run_check("NET");
    HostModule {
        name: "NET".to_string(),
        funcs: vec![
            NetListen::registration(),
            NetAccept::registration(),
            NetClose::registration(),
            NetConnect::registration(),
            NetPort::registration(),
            NetAddr::registration(),
            NetFetch::registration(),
        ],
        types: vec![NetListenerTag::descriptor()],
        record_schemas: vec![RecordSchema {
            name: "NET_LISTEN_INFO",
            fields: listen_result_fields(),
        }],
        states: Vec::new(),
    }
    .with_state(registry)
    .with_state(Arc::new(check))
}

/// Field table for the `NET_LISTEN` result bundle, backing the
/// `NET_LISTEN_INFO` schema and the `NET_LISTEN` return tag: one source
/// for the shape the host mints, so introspection, enforcement, and
/// documentation cannot drift apart.
fn listen_result_fields() -> Arc<[Field]> {
    Arc::from(vec![
        Field {
            name: "listener".to_string(),
            ty: TypeTag::Custom(NetListenerTag::descriptor()),
            docs: "Listener handle for `NET_ACCEPT` and `NET_CLOSE`.",
            optional: false,
        },
        Field {
            name: "addr".to_string(),
            ty: TypeTag::String,
            docs: "Bound socket address (`ip:port`), or the virtual endpoint echo when socketless.",
            optional: false,
        },
        Field {
            name: "virtual".to_string(),
            ty: TypeTag::String,
            docs: "Virtual endpoint echo of the claimed slot.",
            optional: false,
        },
    ])
}

/// Report the bound port of a runner-mapped service without claiming
/// it, so a `-p`-mapped outer port (including ephemeral `-p 0:<inner>`
/// resolutions) can be routed into an inner `RUN` through normal
/// `LET`/`ENV` expansion. `target` is a service name, optionally
/// protocol-qualified (`"tcp/web"`, `"udp/dns"`). Bare text resolves TCP
/// with single-protocol fallback; text bound under both protocols must
/// be qualified. Memory, offline, and unmapped services bail: with no
/// physical bind there is no port to report.
#[oxdock_func(returns = TypeTag::Int, summary = "Report the bound port of a runner-mapped service.")]
#[example(
    name = "observe port",
    code = r###"
    IMPORT [STD, NET]
    INHERIT_ENV [PATH]
    LET $l: NET_LISTEN_INFO = NET_LISTEN("doc-net-observe", {})

    # Observe the bound port without claiming the slot twice, and prove
    # it agrees with the dial string from the same bind.
    LET $port: INT = NET_PORT("doc-net-observe")
    LET $addr: STRING = NET_ADDR("doc-net-observe")
    ASSERT_CONTAINS $addr "{{ $port }}"
    ASSERT_CONTAINS $addr "127.0.0.1:"

    # The shell reads its own environment, with per-platform spelling:
    # quoted "$VAR" passes the parser through untouched on Unix ...
    ENV PROXY_PORT="{{ $port }}"

    [family:unix] LET $o: STRING = RUN echo serving on "$PROXY_PORT"

    # ... while cmd expands %VAR% on Windows.
    [family:windows] LET $o: STRING = RUN echo serving on %PROXY_PORT%

    ASSERT_CONTAINS $o "serving on"
    NET_CLOSE($l.listener)
    "###
)]
fn net_port<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Service name, optionally protocol-qualified; socketless targets bail.
    target: String,
) -> Result<Value> {
    let registry = cx
        .module_state::<EndpointRegistry>("NET")
        .expect("NET module registers its endpoint registry as module state");
    let addr = registry.resolve_ref(&target, "NET_PORT", "port")?;
    Ok(Value::int(i64::from(addr.port())))
}

/// Report the full bound socket address (`ip:port`) of a runner-mapped
/// service without claiming it: the dial-string companion to
/// [`net_port`]. Same qualifier and fallback rules; socketless targets
/// bail, never an empty string.
#[oxdock_func(returns = TypeTag::String, summary = "Report the bound socket address of a runner-mapped service.")]
#[example(
    name = "observe dial string",
    code = r###"
    IMPORT [STD, NET]
    LET $l: NET_LISTEN_INFO = NET_LISTEN("doc-net-dial", {})

    # Observe the dial string without claiming the slot twice: a real
    # loopback bind, so the shape is always ip:port.
    LET $addr: STRING = NET_ADDR("doc-net-dial")
    ASSERT_CONTAINS $addr "127.0.0.1:"
    NET_CLOSE($l.listener)
    "###
)]
fn net_addr<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Service name, optionally protocol-qualified; socketless targets bail.
    target: String,
) -> Result<Value> {
    let registry = cx
        .module_state::<EndpointRegistry>("NET")
        .expect("NET module registers its endpoint registry as module state");
    let addr = registry.resolve_ref(&target, "NET_ADDR", "address")?;
    Ok(Value::string(addr.to_string()))
}

/// Fetch an `https` URL to text: the network source for `PARSE_JSON`
/// and `PARSE_TOML`. `HostCtx` (never pure) so the entry never reaches
/// the pure table that backs `{{ }}` placeholders: fetching touches
/// the network.
///
/// `options` carries `headers` (a MAP of STRING to STRING, sent on
/// every request including redirect hops), `timeout` (a DURATION
/// whole-request deadline, default 30s), `max_body_bytes` (an INT body
/// cap, default 10 MiB), and `max_redirects` (an INT hop budget,
/// default 5, `0` disables following). Streaming is out of scope:
/// a streamed body would arrive as a `PIPE`, not text, so it needs
/// its own function rather than an option on this one.
///
/// No runnable example lives on this metadata: a fetch needs a live
/// server, which docs conformance cannot provide. The contract, in
/// full: `https` only (cleartext `http` reaches loopback hosts only, so
/// tests serve fixtures without TLS), redirects follow by hand (at most
/// 5 hops, absolute URLs only, every hop re-validated against the
/// scheme gate, so a `Location` can never smuggle cleartext), at most
/// 10 MiB of body, strict UTF-8, non-2xx statuses bail. Failures name
/// the URL and the reason; nothing retries, nothing touches the
/// filesystem.
#[oxdock_func(returns = TypeTag::String, summary = "Fetch an https URL to text.")]
#[example(
    name = "loopback fetch",
    code = r###"
    # Serve one canned HTTP reply on the harness-mapped service, then
    # fetch it through the observed dial string.
    IMPORT [STD, NET]
    LET $l: NET_LISTEN_INFO = NET_LISTEN("doc-net-fetch", {})
    LET $in: PIPE
    LET $out: PIPE
    LET $acc: HANDLE = ASYNC { NET_ACCEPT($l.listener, $in, $out, {}) }
    WITH_IO [stdout=$in] ECHO "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"
    LET $base: STRING = NET_ADDR("doc-net-fetch")
    LET $body: STRING = NET_FETCH("http://{{ $base }}/doc.json", {})
    ASSERT_EQ $body "{}"
    CANCEL $acc
    NET_CLOSE($l.listener)
    "###
)]
fn net_fetch<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// `https` URL to fetch; cleartext `http` reaches loopback hosts only.
    url: String,
    /// Fetch settings: `headers`, `timeout`, `max_body_bytes`, and `max_redirects`. Omittable: a missing options MAP fills `{}`.
    #[options(
        "headers?: MAP<STRING>",
        "timeout?: DURATION|STRING",
        "max_body_bytes?: INT",
        "max_redirects?: INT"
    )]
    #[optional]
    options: BTreeMap<String, Value>,
) -> Result<Value> {
    let _ = cx;
    let options_map = &options;
    let mut headers = Vec::new();
    if let Some(value) = options_map.get("headers") {
        // `check_options` verified `MAP<STRING>`; narrow directly.
        for (entry_key, entry_value) in value.as_map().expect("check_options verified MAP<STRING>")
        {
            headers.push((
                entry_key.clone(),
                entry_value
                    .as_str()
                    .expect("check_options verified STRING elements")
                    .to_string(),
            ));
        }
    }
    let mut timeout = crate::fetch::DEFAULT_FETCH_TIMEOUT;
    if let Some(value) = options_map.get("timeout") {
        // `check_options` verified `DURATION|STRING`; narrow directly.
        if let Some(duration) = value.as_duration() {
            timeout = duration;
        } else {
            let text = value
                .as_str()
                .expect("check_options verified DURATION|STRING");
            timeout = oxdock_parser::command::parse_duration(text).with_context(|| {
                format!("NET_FETCH option 'timeout' must be a duration, got {text:?}")
            })?;
        }
    }
    let mut max_body_bytes = crate::fetch::DEFAULT_MAX_BODY_BYTES;
    if let Some(value) = options_map.get("max_body_bytes") {
        let n = value.as_i64().expect("check_options verified INT");
        if n < 0 {
            bail!("NET_FETCH option 'max_body_bytes' must be non-negative, got {n}");
        }
        max_body_bytes = n as u64;
    }
    let mut max_redirects = crate::fetch::DEFAULT_MAX_REDIRECTS as u64;
    if let Some(value) = options_map.get("max_redirects") {
        let n = value.as_i64().expect("check_options verified INT");
        if n < 0 {
            bail!("NET_FETCH option 'max_redirects' must be non-negative, got {n}");
        }
        max_redirects = n as u64;
    }
    let max_redirects = u32::try_from(max_redirects).map_err(|_| {
        anyhow::anyhow!("NET_FETCH() option 'max_redirects' exceeds u32: {max_redirects}")
    })?;
    let body = fetch_text_with_options(
        &url,
        &FetchOptions {
            headers,
            timeout,
            max_body_bytes,
            max_redirects,
        },
    )?;
    Ok(Value::string(body))
}
