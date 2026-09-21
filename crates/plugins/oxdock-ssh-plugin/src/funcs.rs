//! The `SSH` host module: `SERVE`, `ACCEPT` / `DEQUEUE` + `PUMP_CHANNEL`,
//! `CLOSE`, `CONNECT`, `PUMP`.
//!
//! Pipe direction convention (fixed for every func): `out_pipe` carries
//! bytes produced by the wire side (the DSL reads them), `in_pipe`
//! carries bytes consumed by the wire side (the DSL writes them).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use oxdock_core::{
    FuncKind, FuncMeta, FuncParam, HostModule, HostRegistration, NativeFn, OxDockFn, OxDockType,
    StepCtx, Value,
};
use oxdock_func_macro::oxdock_func;
use oxdock_net_plugin::{AcquiredListener, EndpointRegistry, acquire_listener};
use oxdock_process::ProcessManager;
use russh::keys::{Algorithm, PrivateKey};

use crate::bridge::{pump_pipe_to_pipe, pump_session};
use crate::keys::load_or_create_host_key;
use crate::runtime::{connect_runtime, connect_session};
use crate::state::{
    CLOSE_JOIN_TIMEOUT, Dequeue, PendingSession, ServerState, SessionQueue, ShutdownSignal,
};
use crate::types::{SshServerTag, SshSessionTag};
use crate::validate::parse_serve_endpoint;

/// Unique server ids per process.
static SERVER_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Dequeue tick: shutdown and cancellation surface within a few ticks.
const DEQUEUE_TICK: Duration = Duration::from_millis(10);

/// Read the `SSH_SERVER` payload out of a DSL value.
fn server_state(value: &Value, func: &str) -> Result<Arc<ServerState>> {
    let Some(tag) = value.read_heap::<SshServerTag>(SshServerTag::descriptor()) else {
        bail!(
            "{func} expects an SSH_SERVER value, got {}",
            value.type_name()
        );
    };
    Ok(Arc::clone(tag.state()))
}

/// Read the `SSH_SESSION` payload out of a DSL value.
fn session_tag(value: &Value, func: &str) -> Result<SshSessionTag> {
    let Some(tag) = value.read_heap::<SshSessionTag>(SshSessionTag::descriptor()) else {
        bail!(
            "{func} expects an SSH_SESSION value, got {}",
            value.type_name()
        );
    };
    Ok(tag.clone())
}

/// Block until the queue yields an authenticated session (or teardown).
/// Shared by `SSH_DEQUEUE` and the `SSH_ACCEPT` wrapper.
fn dequeue_session<P: ProcessManager>(
    cx: &StepCtx<P>,
    queue: &Arc<SessionQueue>,
    func: &str,
) -> Result<PendingSession> {
    loop {
        match queue.try_pop() {
            Dequeue::Session(session) => return Ok(session),
            Dequeue::Shutdown => bail!("{func}: server is closed"),
            Dequeue::Empty => {}
        }
        match queue.wait_for_session(DEQUEUE_TICK) {
            Dequeue::Session(session) => return Ok(session),
            Dequeue::Shutdown => bail!("{func}: server is closed"),
            Dequeue::Empty => {}
        }
        if cx.is_cancelled() {
            bail!("{func}: task cancelled");
        }
    }
}

/// Dequeue one authenticated session and expose its metadata before any
/// byte pumping starts, so scripts can route on the requested command or
/// client identity. Must run inside `ASYNC`. Returns a MAP with
/// `session` (SSH_SESSION), `command` (STRING, empty for shells),
/// `username` and `addr` (STRINGs, empty when unknown).
///
/// Routing shape: compare the dequeued command against known commands,
/// build a fresh pipe pair per session, and pump a synthetic reply with
/// `SSH_PUMP_CHANNEL`. The server sends first: the client side never
/// EOFs its input, so the reply cannot race teardown. This complete
/// program runs end to end under the docs conformance suite.
///
/// ```oxdock
/// IMPORT [STD, SSH]
/// LET $m: MAP = SSH_SERVE("doc-ssh-demo", {username: "u", password: "p"})
/// LET $in: PIPE
/// LET $out: PIPE
/// LET $w: HANDLE = ASYNC {
///     LET $sess: MAP = SSH_DEQUEUE($m.server)
///     ASSERT_CONTAINS $sess "session"
///     ASSERT_CONTAINS $sess "command"
///     ASSERT_CONTAINS $sess "username"
///     ASSERT_CONTAINS $sess "addr"
///     SSH_PUMP_CHANNEL($sess.session, $in, $out)
/// }
/// LET $cin: PIPE
/// LET $cout: PIPE
/// LET $c: HANDLE = ASYNC { SSH_CONNECT("doc-ssh-demo", "u", "p", $cin, $cout) }
/// WITH_IO [stdout=$in] ECHO "server-greeting"
/// LET $info: MAP = INSPECT($cout)
/// LET $empty: BOOL = $info.buffer_bytes == 0
/// WHILE $empty {
///     SLEEP 100ms
///     $info = INSPECT($cout)
///     $empty = $info.buffer_bytes == 0
/// }
/// ASSERT_CONTAINS $cout "server-greeting"
/// AWAIT $w
/// CANCEL $c
/// SSH_CLOSE($m.server)
/// ```
#[oxdock_func(
    returns = "MAP",
    summary = "Dequeue one SSH session with its metadata."
)]
fn ssh_dequeue<P: ProcessManager>(cx: &mut StepCtx<P>, server: Value) -> Result<Value> {
    if !cx.is_async_task() {
        bail!(
            "SSH_DEQUEUE requires ASYNC: wrap it as LET $t: HANDLE = ASYNC {{ SSH_DEQUEUE($server.server) }}"
        );
    }
    let state = server_state(&server, "SSH_DEQUEUE")?;
    let session = dequeue_session(cx, state.queue(), "SSH_DEQUEUE")?;
    let command = session.exec_command.clone().unwrap_or_default();
    let username = session.username.clone().unwrap_or_default();
    let addr = session
        .peer_addr
        .map(|addr| addr.to_string())
        .unwrap_or_default();
    let tag = SshSessionTag::new(
        session.exec_command,
        session.username,
        session.peer_addr,
        session.pty_size,
        session.up_rx,
        session.down_tx,
    );
    let mut map = BTreeMap::new();
    map.insert(
        "session".to_string(),
        Value::mint_heap(SshSessionTag::descriptor(), tag),
    );
    map.insert("command".to_string(), Value::string(command));
    map.insert("username".to_string(), Value::string(username));
    map.insert("addr".to_string(), Value::string(addr));
    Ok(Value::map(map))
}

/// Read a flat string list (an argv vector) out of a DSL value.
fn argv_list(value: &Value, func: &str) -> Result<Vec<String>> {
    let Some(items) = value.as_list() else {
        bail!("{func} argv must be a LIST of strings");
    };
    if items.is_empty() {
        bail!("{func} argv must not be empty");
    }
    items
        .iter()
        .map(|item| {
            item.as_str().map(str::to_string).ok_or_else(|| {
                anyhow::anyhow!("{func} argv must be strings, got {}", item.type_name())
            })
        })
        .collect()
}

/// Read the options MAP for `SSH_SERVE`. The 2nd argument must be a MAP;
/// unknown keys bail so script typos fail fast instead of silently ignored.
fn serve_options(options: &Value) -> Result<&BTreeMap<String, Value>> {
    options.as_map().ok_or_else(|| {
        anyhow::anyhow!(
            "SSH_SERVE options must be a MAP, got {}",
            options.type_name()
        )
    })
}

/// Read a required STRING key from the options MAP. Missing, non-string,
/// or blank binds bail naming the key: a server without credentials is
/// meaningless, so there is no default.
fn required_string(map: &BTreeMap<String, Value>, func: &str, key: &str) -> Result<String> {
    let Some(value) = map.get(key) else {
        bail!("{func} option '{key}' is required");
    };
    let Some(s) = value.as_str() else {
        bail!(
            "{func} option '{key}' must be a STRING, got {}",
            value.type_name()
        );
    };
    if s.trim().is_empty() {
        bail!("{func} option '{key}' must not be empty");
    }
    Ok(s.to_string())
}

/// Read an optional STRING key from the options MAP. Missing, empty, or
/// whitespace-only binds `None`; present non-strings bail.
fn optional_string(map: &BTreeMap<String, Value>, func: &str, key: &str) -> Result<Option<String>> {
    let Some(value) = map.get(key) else {
        return Ok(None);
    };
    let Some(s) = value.as_str() else {
        bail!(
            "{func} option '{key}' must be a STRING, got {}",
            value.type_name()
        );
    };
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(s.to_string()))
}

/// Spin up an SSH server on a virtual service endpoint with the given
/// credentials. `bind` is a logical port (`"2251"`) or service name:
/// physical binds in-script are rejected, `0` is reserved for the CLI
/// outer mapping. `options` is a MAP with required `username`/`password`
/// STRINGs (a server without credentials is meaningless, so blanks bail)
/// and the optional `key_path` STRING (workspace-relative OpenSSH Ed25519
/// file, load-or-create; a leading `/` anchors to the workspace root like
/// WRITE, and escapes still bail; absent or blank keeps the ephemeral
/// in-memory key).
/// Non-blocking: returns a MAP with `server` (SSH_SERVER),
/// `addr` (STRING: the physical bind, or the virtual endpoint echo when
/// socketless), `username` and `password` (STRINGs), and `virtual`
/// (STRING echo). Memory services bail: SSH needs a TCP socket, so map
/// the name with `-p`/`--listen`.
fn ssh_serve<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    registry: &Arc<EndpointRegistry>,
    bind: String,
    options: Value,
) -> Result<Value> {
    let map = serve_options(&options)?;
    for key in map.keys() {
        if key != "username" && key != "password" && key != "key_path" {
            bail!("SSH_SERVE() unknown option '{key}' (expected: username, password, key_path)");
        }
    }
    let username = required_string(map, "SSH_SERVE", "username")?;
    let password = required_string(map, "SSH_SERVE", "password")?;
    let key_path = optional_string(map, "SSH_SERVE", "key_path")?;
    let endpoint = parse_serve_endpoint(&bind)?;
    let (acquired, registry) = acquire_listener(registry, &endpoint, "SSH_SERVE")?;
    let host_key = match load_or_create_host_key(cx, "SSH_SERVE", key_path)? {
        Some(key) => key,
        None => PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .context("generate ephemeral Ed25519 host key")?,
    };
    let id = SERVER_IDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let id = format!("ssh-{pid}-{id}", pid = std::process::id());
    let queue = Arc::new(SessionQueue::new());
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<ShutdownSignal>();
    let (local_addr, addr_text, thread) = match acquired {
        AcquiredListener::Tcp { listener, addr } => {
            // The slot keeps the shared backlog socket; the runtime owns
            // its own clone.
            let owned = listener
                .try_clone()
                .context("SSH_SERVE cannot clone its listener")?;
            let thread_queue = Arc::clone(&queue);
            let thread_user = username.clone();
            let thread_pass = password.clone();
            let thread = std::thread::Builder::new()
                .name(id.clone())
                .spawn(move || {
                    crate::runtime::serve(
                        owned,
                        host_key,
                        thread_user,
                        thread_pass,
                        thread_queue,
                        shutdown_rx,
                    )
                })
                .context("SSH_SERVE cannot spawn the server thread")?;
            (addr, addr.to_string(), Some(thread))
        }
        AcquiredListener::Memory => {
            drop(shutdown_rx);
            bail!(
                "SSH_SERVE: '{endpoint}' is a memory service (SSH needs a TCP socket; map it with -p/--listen)"
            )
        }
        AcquiredListener::Offline => {
            // Socketless servers spawn no thread: DEQUEUE waits on the
            // queue until close (test drivers push sessions via the
            // queue). Dropping the receiver makes later shutdown sends a
            // silent no-op.
            drop(shutdown_rx);
            (
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
                endpoint.to_string(),
                None,
            )
        }
    };
    let state = Arc::new(ServerState::new(crate::state::ServerConfig {
        id,
        local_addr,
        addr_text: addr_text.clone(),
        queue,
        shutdown_tx,
        thread,
        registry: Arc::clone(&registry),
        endpoint: endpoint.clone(),
    }));
    let mut map = BTreeMap::new();
    map.insert(
        "server".to_string(),
        Value::mint_heap(SshServerTag::descriptor(), SshServerTag::new(state)),
    );
    map.insert("addr".to_string(), Value::string(addr_text));
    map.insert("username".to_string(), Value::string(username));
    map.insert("password".to_string(), Value::string(password));
    map.insert("virtual".to_string(), Value::string(endpoint.to_string()));
    Ok(Value::map(map))
}

/// Accept the next authenticated session and pump it through explicit
/// pipes until the channel closes. Must run inside `ASYNC`. Returns a
/// MAP with `closed` (BOOL) and `command` (STRING, empty for shells).
/// Thin wrapper over `SSH_DEQUEUE` + `SSH_PUMP_CHANNEL` for worker loops
/// that need no pre-pump inspection; use those directly to route on
/// session metadata first. Returns a MAP with `closed` (BOOL) and
/// `command` (STRING, empty for shells); the example below asserts both
/// keys on the awaited result. The server sends first: the client side
/// never EOFs its input, so the reply cannot race teardown. This
/// complete program runs end to end under the docs conformance suite.
///
/// ```oxdock
/// IMPORT [STD, SSH]
/// LET $m: MAP = SSH_SERVE("doc-ssh-demo", {username: "u", password: "p"})
/// LET $in: PIPE
/// LET $out: PIPE
/// LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
/// LET $cin: PIPE
/// LET $cout: PIPE
/// LET $c: HANDLE = ASYNC { SSH_CONNECT("doc-ssh-demo", "u", "p", $cin, $cout) }
/// WITH_IO [stdout=$in] ECHO "server-greeting"
/// LET $info: MAP = INSPECT($cout)
/// LET $empty: BOOL = $info.buffer_bytes == 0
/// WHILE $empty {
///     SLEEP 100ms
///     $info = INSPECT($cout)
///     $empty = $info.buffer_bytes == 0
/// }
/// ASSERT_CONTAINS $cout "server-greeting"
/// LET $done: MAP = AWAIT $acc
/// ASSERT_CONTAINS $done "closed"
/// ASSERT_CONTAINS $done "command"
/// CANCEL $c
/// SSH_CLOSE($m.server)
/// ```
#[oxdock_func(returns = "MAP", summary = "Accept one SSH session into pipes.")]
fn ssh_accept<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    server: Value,
    in_pipe: Value,
    out_pipe: Value,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!(
            "SSH_ACCEPT requires ASYNC: wrap it as LET $t: HANDLE = ASYNC {{ SSH_ACCEPT($server, $in, $out) }}"
        );
    }
    let state = server_state(&server, "SSH_ACCEPT")?;
    let cancel = AtomicBool::new(false);
    let session = dequeue_session(cx, state.queue(), "SSH_ACCEPT")?;
    pump_session(
        cx,
        &in_pipe,
        &out_pipe,
        session.up_rx,
        session.down_tx,
        &cancel,
    )?;
    let mut map = BTreeMap::new();
    map.insert("closed".to_string(), Value::bool(true));
    map.insert(
        "command".to_string(),
        Value::string(session.exec_command.unwrap_or_default()),
    );
    Ok(Value::map(map))
}

/// Pump a dequeued session between explicit DSL pipes until the channel
/// closes. Must run inside `ASYNC`. The session ends are take-once: a
/// second pump on the same session bails instead of splitting bytes.
/// Returns a MAP with `closed` (BOOL).
#[oxdock_func(
    returns = "MAP",
    summary = "Pump a dequeued SSH session through pipes."
)]
fn ssh_pump_channel<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    session: Value,
    in_pipe: Value,
    out_pipe: Value,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!("SSH_PUMP_CHANNEL requires ASYNC: pump it in its own task after SSH_DEQUEUE");
    }
    let tag = session_tag(&session, "SSH_PUMP_CHANNEL")?;
    let (up_rx, down_tx) = tag.take_pump_ends()?;
    let cancel = AtomicBool::new(false);
    pump_session(cx, &in_pipe, &out_pipe, up_rx, down_tx, &cancel)?;
    let mut map = BTreeMap::new();
    map.insert("closed".to_string(), Value::bool(true));
    Ok(Value::map(map))
}

/// Shut a server down and join its runtime thread (bounded). Idempotent:
/// returns BOOL true when no thread remains.
#[oxdock_func(returns = "BOOL", summary = "Shut down an SSH server.")]
fn ssh_close<P: ProcessManager>(cx: &mut StepCtx<P>, server: Value) -> Result<Value> {
    let _ = cx;
    let state = server_state(&server, "SSH_CLOSE")?;
    state.request_shutdown();
    Ok(Value::bool(state.join_thread(CLOSE_JOIN_TIMEOUT)))
}

/// Connect to an SSH server with distinct inner credentials and pump the
/// shell channel through explicit pipes until it closes. Must run inside
/// `ASYNC`, concurrently with the `SSH_PUMP` tasks (never before them).
/// `target` is a logical port (`"2251"`: CLI-mapped address or loopback
/// default), a service name (CLI-mapped address only; unmapped names are
/// memory services and SSH needs TCP), a served address (`$m.addr`), or
/// a `host:port` dial. Under `--offline` the dial bails before any DNS
/// or socket work. Returns a MAP with `closed` (BOOL).
fn ssh_connect<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    registry: &Arc<EndpointRegistry>,
    target: String,
    username: String,
    password: String,
    in_pipe: Value,
    out_pipe: Value,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!("SSH_CONNECT requires ASYNC: run it in its own task beside the SSH_PUMP tasks");
    }
    // Sandbox gate first: offline runs open no OS sockets.
    if registry.is_offline() {
        bail!("SSH_CONNECT failed: engine running in --offline mode");
    }
    if username.is_empty() {
        bail!("SSH_CONNECT username must not be empty");
    }
    let addr = crate::validate::resolve_connect_addr(registry, &target)?;
    let runtime = connect_runtime()?;
    let session = runtime
        .block_on(connect_session(&addr, &username, &password))
        .context("SSH_CONNECT failed")?;
    let crate::runtime::OutboundSession {
        up_rx,
        down_tx,
        handle,
        ..
    } = session;
    let cancel = AtomicBool::new(false);
    let pump = pump_session(cx, &in_pipe, &out_pipe, up_rx, down_tx, &cancel);
    let _ = runtime.block_on(handle.disconnect(russh::Disconnect::ByApplication, "", ""));
    pump?;
    let mut map = BTreeMap::new();
    map.insert("closed".to_string(), Value::bool(true));
    Ok(Value::map(map))
}

/// Copy one pipe into another until EOF, then close the target.
/// Returns the INT byte count. Either task placement works, as long as
/// the other end is live (usually an `ASYNC` task).
#[oxdock_func(returns = "INT", summary = "Copy one pipe into another until EOF.")]
fn ssh_pump<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    from_pipe: Value,
    to_pipe: Value,
) -> Result<Value> {
    let cancel = AtomicBool::new(false);
    let total = pump_pipe_to_pipe(cx, &from_pipe, &to_pipe, &cancel)?;
    Ok(Value::int(total))
}

/// Run `argv` under a local pseudo-terminal sized from the dequeued
/// session and pump it through explicit pipes until the child exits.
/// `rows`/`cols` seed the initial size when positive; non-positive falls
/// back to the session's requested size (the outer pty request, 24x80
/// default). Outer window-change requests resize this session's terminal
/// live; every session owns its size cell, so concurrent guests never
/// observe each other. Must run inside `ASYNC`. Returns the INT exit
/// code. Environment is inherited from the host process and layered with
/// the script environment like `RUN`: block-scoped `ENV` (such as the
/// session's `SSH_USER` / `SSH_CLIENT` / `SSH_SERVER` / `SSH_COMMAND`
/// relay) reaches the child; the working directory comes from the script.
#[oxdock_func(
    returns = "INT",
    summary = "Run a command under a sized local terminal into pipes."
)]
fn ssh_pty_run<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    session: Value,
    argv: Value,
    rows: i64,
    cols: i64,
    in_pipe: Value,
    out_pipe: Value,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!("SSH_PTY_RUN requires ASYNC: run it in its own task beside the session pump task");
    }
    let tag = session_tag(&session, "SSH_PTY_RUN")?;
    let argv = argv_list(&argv, "SSH_PTY_RUN")?;
    let initial = if rows > 0 && cols > 0 {
        crate::state::PtySize::new(rows as u32, cols as u32)
    } else {
        tag.pty_size()
    };
    let cancel = AtomicBool::new(false);
    let code = crate::pty::pump_pty_session(
        cx,
        &argv,
        initial,
        &tag.pty_size_handle(),
        &in_pipe,
        &out_pipe,
        &cancel,
    )?;
    Ok(Value::int(code))
}

/// The `SSH` host module: virtual-endpoint server plus client, bridged
/// to DSL pipes. Generic over the process manager like every host module.
pub fn module_with<P: ProcessManager>() -> HostModule<P> {
    module_with_endpoints(Arc::new(EndpointRegistry::new(false)))
}

/// The `SSH` host module resolving through `registry`: `SSH_SERVE` and
/// `SSH_CONNECT` close over it (hand-built entries; the `#[oxdock_func]`
/// macro only generates closers-over-nothing). Everything else reaches
/// the same registry through its `SSH_SERVER` handle.
pub fn module_with_endpoints<P: ProcessManager>(registry: Arc<EndpointRegistry>) -> HostModule<P> {
    HostModule {
        name: "SSH".to_string(),
        funcs: vec![
            ssh_serve_registration(Arc::clone(&registry)),
            SshAccept::registration(),
            SshDequeue::registration(),
            SshPumpChannel::registration(),
            SshClose::registration(),
            ssh_connect_registration(registry),
            SshPump::registration(),
            SshPtyRun::registration(),
        ],
        types: vec![SshServerTag::descriptor(), SshSessionTag::descriptor()],
    }
}

/// Hand-built `SSH_SERVE` entry: same shape the macro would emit (arity
/// check, `STRING`/`Value` unpacking, metadata), plus the captured
/// registry threaded into [`ssh_serve`].
fn ssh_serve_registration<P: ProcessManager>(
    registry: Arc<EndpointRegistry>,
) -> HostRegistration<P> {
    let func: NativeFn<P> = Arc::new(move |cx, values| {
        if values.len() != 2 {
            bail!("SSH_SERVE() expects 2 argument(s), got {}", values.len());
        }
        let mut values = values.into_iter();
        let bind = match values.next().expect("arity checked above").as_str() {
            Some(s) => s.to_string(),
            None => bail!("SSH_SERVE() argument `$bind` must be a STRING"),
        };
        let options = values.next().expect("arity checked above");
        ssh_serve(cx, &registry, bind, options)
    });
    HostRegistration::Stateful {
        name: "SSH_SERVE".to_string(),
        meta: FuncMeta {
            name: "SSH_SERVE".to_string(),
            // Assigned at registration, like the macro's markers.
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
            summary: "Serve SSH on a virtual service endpoint.",
            docs: "Serve SSH on a virtual service endpoint.",
        },
        func,
    }
}

/// Hand-built `SSH_CONNECT` entry: same shape the macro would emit, plus
/// the captured registry threaded into [`ssh_connect`].
fn ssh_connect_registration<P: ProcessManager>(
    registry: Arc<EndpointRegistry>,
) -> HostRegistration<P> {
    let func: NativeFn<P> = Arc::new(move |cx, values| {
        if values.len() != 5 {
            bail!("SSH_CONNECT() expects 5 argument(s), got {}", values.len());
        }
        let mut values = values.into_iter();
        let target = match values.next().expect("arity checked above").as_str() {
            Some(s) => s.to_string(),
            None => bail!("SSH_CONNECT() argument `$target` must be a STRING"),
        };
        let username = match values.next().expect("arity checked above").as_str() {
            Some(s) => s.to_string(),
            None => bail!("SSH_CONNECT() argument `$username` must be a STRING"),
        };
        let password = match values.next().expect("arity checked above").as_str() {
            Some(s) => s.to_string(),
            None => bail!("SSH_CONNECT() argument `$password` must be a STRING"),
        };
        let in_pipe = values.next().expect("arity checked above");
        let out_pipe = values.next().expect("arity checked above");
        ssh_connect(cx, &registry, target, username, password, in_pipe, out_pipe)
    });
    HostRegistration::Stateful {
        name: "SSH_CONNECT".to_string(),
        meta: FuncMeta {
            name: "SSH_CONNECT".to_string(),
            // Assigned at registration, like the macro's markers.
            module: String::new(),
            kind: FuncKind::HostCtx,
            params: Some(vec![
                FuncParam {
                    name: "target".to_string(),
                    param_type: Some("STRING".to_string()),
                },
                FuncParam {
                    name: "username".to_string(),
                    param_type: Some("STRING".to_string()),
                },
                FuncParam {
                    name: "password".to_string(),
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
            ]),
            returns: Some("MAP".to_string()),
            rpn: false,
            summary: "Open an SSH client session into pipes.",
            docs: "Open an SSH client session into pipes. Target shapes: a logical port (CLI-mapped address or loopback default), a service name (CLI-mapped address only), a served address, or a host:port dial.",
        },
        func,
    }
}
