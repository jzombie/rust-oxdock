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
use oxdock_core::{Field, HostModule, OxDockFn, OxDockType, RecordSchema, StepCtx, TypeTag, Value};
use oxdock_func_macro::oxdock_func;
use oxdock_net_plugin::{
    AcquiredListener, EndpointKey, EndpointRegistry, ServiceTarget, SocketRequirement,
    acquire_listener, no_cli_mapping, parse_virtual_endpoint, resolve_service_target,
};
use oxdock_pipe::PipeHandle;
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
/// EOFs its input, so the reply cannot race teardown.
///
#[oxdock_func(
    returns = TypeTag::Record(dequeue_result_fields()),
    summary = "Dequeue one SSH session with its metadata."
)]
#[example(
    name = "route one session",
    code = r###"
    IMPORT [STD, SSH]
    LET $m: SSH_SERVE_INFO = SSH_SERVE("doc-ssh-route", {username: "u", password: "p"})

    # Serve: dequeue one session and pump a synthetic reply.
    LET $in: PIPE
    LET $out: PIPE
    LET $w: HANDLE = ASYNC {
        LET $sess: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
        ASSERT_EQ $sess.username "u"
        SSH_PUMP_CHANNEL($sess.session, $in, $out)
    }

    # Connect a client to the server.
    LET $cin: PIPE
    LET $cout: PIPE
    LET $c: HANDLE = ASYNC { SSH_CONNECT("doc-ssh-route", "u", "p", $cin, $cout) }

    # Close both inputs: each pump observes EOF, half-closes, and every
    # task returns with no cancellation.
    WITH_IO [stdout=$cin] ECHO "client-hello"
    WITH_IO [stdout=$in] ECHO "server-greeting"
    LET $cDone: MAP<ANY> = AWAIT $c
    ASSERT_EQ $cDone.closed true
    AWAIT $w
    SSH_CLOSE($m.server)
    "###
)]
fn ssh_dequeue<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Server handle from `SSH_SERVE`.
    server: SshServerTag,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!(
            "SSH_DEQUEUE requires ASYNC: wrap it as LET $t: HANDLE = ASYNC {{ SSH_DEQUEUE($server.server) }}"
        );
    }
    let state = Arc::clone(server.state());
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

/// Spin up an SSH server on a runner-mapped service with the given
/// credentials. `bind` is a service name; the runner maps it with
/// `-p`/`--listen`, and unmapped services bail with the mapping hint
/// (SSH needs a TCP socket, so there is no memory fallback).
/// `options` is a MAP with required `username`/`password`
/// STRINGs (a server without credentials is meaningless, so blanks bail)
/// and the optional `key_path` STRING (workspace-relative OpenSSH Ed25519
/// file, load-or-create; a leading `/` anchors to the workspace root like
/// WRITE, and escapes still bail; absent or blank keeps the ephemeral
/// in-memory key).
/// Non-blocking: returns a MAP with `server` (SSH_SERVER),
/// `addr` (STRING: the physical bind, or the virtual endpoint echo when
/// socketless), `username` and `password` (STRINGs), and `virtual`
/// (STRING echo). Unmapped and memory services bail with the CLI
/// mapping hint: SSH needs a TCP socket, so map the name with
/// `-p`/`--listen`.
#[oxdock_func(returns = TypeTag::Record(serve_result_fields()), summary = "Serve SSH on a virtual service endpoint.")]
#[example(
    name = "serve and close",
    code = r###"
    IMPORT [STD, SSH]
    LET $m: SSH_SERVE_INFO = SSH_SERVE("doc-ssh-serve", {username: "u", password: "p"})
    ASSERT_EQ $m.virtual "doc-ssh-serve"
    SSH_CLOSE($m.server)
    "###
)]
fn ssh_serve<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Service name to serve; the runner maps it with -p/--listen.
    bind: String,
    /// Server credentials and host key.
    #[options("username: STRING", "password: STRING", "key_path?: STRING")]
    options: BTreeMap<String, Value>,
) -> Result<Value> {
    let map = &options;
    // Compositional reads: presence and blank stay domain errors (a
    // server without credentials is meaningless, so there is no
    // default); shapes already passed `check_options` in the wrapper.
    let username = match map.get("username") {
        Some(value) => {
            let text = value.as_str().expect("check_options verified STRING");
            if text.trim().is_empty() {
                bail!("SSH_SERVE option 'username' must not be empty");
            }
            text.to_string()
        }
        None => bail!("SSH_SERVE option 'username' is required"),
    };
    let password = match map.get("password") {
        Some(value) => {
            let text = value.as_str().expect("check_options verified STRING");
            if text.trim().is_empty() {
                bail!("SSH_SERVE option 'password' must not be empty");
            }
            text.to_string()
        }
        None => bail!("SSH_SERVE option 'password' is required"),
    };
    let key_path = match map.get("key_path") {
        Some(value) => {
            let text = value.as_str().expect("check_options verified STRING");
            if text.trim().is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
        None => None,
    };
    let registry = cx
        .module_state::<EndpointRegistry>("SSH")
        .expect("SSH module registers its endpoint registry as module state");
    let endpoint = parse_serve_endpoint(&bind)?;
    let key = EndpointKey::tcp(endpoint);
    let (acquired, registry) = acquire_listener(
        &registry,
        &key,
        "SSH_SERVE",
        SocketRequirement::SocketRequired,
    )?;
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
            // Defensive: `acquire_listener` with `SocketRequired` already
            // bailed; the text still comes from the one shared helper.
            drop(shutdown_rx);
            return Err(no_cli_mapping("SSH_SERVE", &key.endpoint));
        }
        AcquiredListener::Offline => {
            // Socketless servers spawn no thread: DEQUEUE waits on the
            // queue until close (test drivers push sessions via the
            // queue). Dropping the receiver makes later shutdown sends a
            // silent no-op.
            drop(shutdown_rx);
            (
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
                key.endpoint.to_string(),
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
        endpoint: key.clone(),
    }));
    let mut map = BTreeMap::new();
    map.insert(
        "server".to_string(),
        Value::mint_heap(SshServerTag::descriptor(), SshServerTag::new(state)),
    );
    map.insert("addr".to_string(), Value::string(addr_text));
    map.insert("username".to_string(), Value::string(username));
    map.insert("password".to_string(), Value::string(password));
    map.insert(
        "virtual".to_string(),
        Value::string(key.endpoint.to_string()),
    );
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
/// never EOFs its input, so the reply cannot race teardown.
///
#[oxdock_func(returns = TypeTag::Record(std::sync::Arc::from([Field {
    name: "closed".to_string(),
    ty: TypeTag::Bool,
    docs: "True when the session closed cleanly.",
    optional: false,
}, Field {
    name: "command".to_string(),
    ty: TypeTag::String,
    docs: "Executed command, empty for shells.",
    optional: false,
}])), summary = "Accept one SSH session into pipes.")]
#[example(
    name = "accept and close",
    code = r###"
    IMPORT [STD, SSH]
    LET $m: SSH_SERVE_INFO = SSH_SERVE("doc-ssh-accept", {username: "u", password: "p"})
    LET $in: PIPE
    LET $out: PIPE
    LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
    LET $cin: PIPE
    LET $cout: PIPE
    LET $c: HANDLE = ASYNC { SSH_CONNECT("doc-ssh-accept", "u", "p", $cin, $cout) }

    # Close both inputs: each pump observes EOF, half-closes, and both
    # tasks return their result maps with no cancellation.
    WITH_IO [stdout=$cin] ECHO "client-hello"
    WITH_IO [stdout=$in] ECHO "server-greeting"
    LET $done: MAP<ANY> = AWAIT $acc
    ASSERT_EQ $done.closed true
    ASSERT_CONTAINS $done "command"
    LET $cDone: MAP<ANY> = AWAIT $c
    ASSERT_EQ $cDone.closed true
    SSH_CLOSE($m.server)
    "###
)]
fn ssh_accept<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Server handle from `SSH_SERVE`.
    server: SshServerTag,
    /// Pipe carrying bytes consumed by the wire side.
    in_pipe: PipeHandle,
    /// Pipe carrying bytes produced by the wire side.
    out_pipe: PipeHandle,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!(
            "SSH_ACCEPT requires ASYNC: wrap it as LET $t: HANDLE = ASYNC {{ SSH_ACCEPT($server, $in, $out) }}"
        );
    }
    // Downstream pumps take `&Value`: re-wrap the extracted handles so
    // every helper below keeps its Value-based shape.
    let in_pipe = Value::pipe_handle(in_pipe);
    let out_pipe = Value::pipe_handle(out_pipe);
    let state = Arc::clone(server.state());
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
///
#[oxdock_func(
    returns = TypeTag::Record(std::sync::Arc::from([Field {
        name: "closed".to_string(),
        ty: TypeTag::Bool,
        docs: "True when the session closed cleanly.",
        optional: false,
    }])),
    summary = "Pump a dequeued SSH session through pipes."
)]
#[example(
    name = "pump a session",
    code = r###"
    IMPORT [STD, SSH]
    LET $m: SSH_SERVE_INFO = SSH_SERVE("doc-ssh-pump", {username: "u", password: "p"})
    LET $d: HANDLE = ASYNC { SSH_DEQUEUE($m.server) }
    LET $cin: PIPE
    LET $cout: PIPE
    LET $c: HANDLE = ASYNC { SSH_CONNECT("doc-ssh-pump", "u", "p", $cin, $cout) }
    WITH_IO [stdout=$cin] ECHO "client-hello"
    LET $sess: SSH_DEQUEUE_INFO = AWAIT $d
    LET $sin: PIPE
    LET $sout: PIPE
    LET $p: HANDLE = ASYNC { SSH_PUMP_CHANNEL($sess.session, $sin, $sout) }
    WITH_IO [stdout=$sin] ECHO "server-greeting"
    LET $cDone: MAP<ANY> = AWAIT $c
    ASSERT_EQ $cDone.closed true
    LET $pDone: MAP<ANY> = AWAIT $p
    ASSERT_EQ $pDone.closed true
    SSH_CLOSE($m.server)
    "###
)]
fn ssh_pump_channel<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Session handle from `SSH_DEQUEUE` (pump ends take once).
    session: SshSessionTag,
    /// Pipe carrying bytes consumed by the wire side.
    in_pipe: PipeHandle,
    /// Pipe carrying bytes produced by the wire side.
    out_pipe: PipeHandle,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!("SSH_PUMP_CHANNEL requires ASYNC: pump it in its own task after SSH_DEQUEUE");
    }
    // Downstream pumps take `&Value`: re-wrap the extracted handles so
    // every helper below keeps its Value-based shape.
    let in_pipe = Value::pipe_handle(in_pipe);
    let out_pipe = Value::pipe_handle(out_pipe);
    let (up_rx, down_tx) = session.take_pump_ends()?;
    let cancel = AtomicBool::new(false);
    pump_session(cx, &in_pipe, &out_pipe, up_rx, down_tx, &cancel)?;
    let mut map = BTreeMap::new();
    map.insert("closed".to_string(), Value::bool(true));
    Ok(Value::map(map))
}

/// Shut a server down and join its runtime thread (bounded). Idempotent:
/// returns BOOL true when no thread remains.
///
#[oxdock_func(returns = TypeTag::Bool, summary = "Shut down an SSH server.")]
#[example(
    name = "shut down",
    code = r###"
    IMPORT [STD, SSH]
    LET $m: SSH_SERVE_INFO = SSH_SERVE("doc-ssh-close", {username: "u", password: "p"})
    LET $ok: BOOL = SSH_CLOSE($m.server)
    ASSERT_EQ $ok true
    "###
)]
fn ssh_close<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Server handle from `SSH_SERVE`.
    server: SshServerTag,
) -> Result<Value> {
    let _ = cx;
    let state = Arc::clone(server.state());
    state.request_shutdown();
    Ok(Value::bool(state.join_thread(CLOSE_JOIN_TIMEOUT)))
}

/// Connect to an SSH server with distinct inner credentials and pump the
/// shell channel through explicit pipes until it closes. Must run inside
/// `ASYNC`, concurrently with the `SSH_PUMP` tasks (never before them).
/// `target` is a service name resolved through runner mappings: SSH
/// needs a TCP socket, so unmapped services bail with the CLI mapping
/// hint. Under `--offline` the dial bails before any socket work.
/// Returns a MAP with `closed` (BOOL).
#[oxdock_func(returns = TypeTag::Map, summary = "Open an SSH client session into pipes.")]
#[example(
    name = "connect and close",
    code = r###"
    IMPORT [STD, SSH]
    LET $m: SSH_SERVE_INFO = SSH_SERVE("doc-ssh-connect", {username: "u", password: "p"})
    LET $in: PIPE
    LET $out: PIPE
    LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
    LET $cin: PIPE
    LET $cout: PIPE
    LET $c: HANDLE = ASYNC { SSH_CONNECT("doc-ssh-connect", "u", "p", $cin, $cout) }

    # Close both inputs: each pump observes EOF, half-closes, and both
    # tasks return their result maps with no cancellation.
    WITH_IO [stdout=$cin] ECHO "client-hello"
    WITH_IO [stdout=$in] ECHO "server-greeting"
    LET $done: MAP<ANY> = AWAIT $c
    ASSERT_EQ $done.closed true
    LET $adone: MAP<ANY> = AWAIT $acc
    ASSERT_EQ $adone.closed true
    SSH_CLOSE($m.server)
    "###
)]
fn ssh_connect<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Dial target: a service name resolved through runner mappings.
    target: String,
    /// Inner credentials for the SSH session.
    username: String,
    /// Inner credentials for the SSH session.
    password: String,
    /// Pipe carrying bytes consumed by the wire side.
    in_pipe: PipeHandle,
    /// Pipe carrying bytes produced by the wire side.
    out_pipe: PipeHandle,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!("SSH_CONNECT requires ASYNC: run it in its own task beside the SSH_PUMP tasks");
    }
    // Module-shared endpoint registry: stashed at registration, so
    // this entry stays declarative with no captured closure.
    let registry = cx
        .module_state::<EndpointRegistry>("SSH")
        .expect("SSH module registers its endpoint registry as module state");
    // The session pump takes `&Value` pipes: re-wrap the extracted
    // handles so the bridge keeps its Value-based shape.
    let in_pipe = Value::pipe_handle(in_pipe);
    let out_pipe = Value::pipe_handle(out_pipe);
    // Sandbox gate first: offline runs open no OS sockets.
    if registry.is_offline() {
        bail!("SSH_CONNECT failed: engine running in --offline mode");
    }
    if username.is_empty() {
        bail!("SSH_CONNECT username must not be empty");
    }
    // Strictly symbolic, resolved through the single shared resolver:
    // served addresses and `host:port` shapes already failed in the
    // parser below, so only runner-mapped services reach the dial.
    let endpoint = parse_virtual_endpoint(&target, "SSH_CONNECT")?;
    let key = EndpointKey::tcp(endpoint);
    let addr = match resolve_service_target(
        &registry,
        &key,
        "SSH_CONNECT",
        SocketRequirement::SocketRequired,
    )? {
        ServiceTarget::Tcp(addr) => addr,
        // Defensive: the shared resolver already bailed; same text.
        ServiceTarget::Memory => {
            return Err(no_cli_mapping("SSH_CONNECT", &key.endpoint));
        }
    };
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
///
#[oxdock_func(returns = TypeTag::Int, summary = "Copy one pipe into another until EOF.")]
#[example(
    name = "copy",
    code = r###"
    # Closing the source ends the copy: the count proves the bytes moved.
    IMPORT [STD, SSH]
    LET $in: PIPE
    WITH_IO [stdout=$in] ECHO "pump-me"
    LET $out: PIPE
    LET $n: INT = SSH_PUMP($in, $out)
    ASSERT_EQ $n 8
    WITH_IO [stdin=$out] READ_LINE $got
    ASSERT_EQ $got "pump-me"
    "###
)]
fn ssh_pump<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Source pipe to drain.
    from_pipe: PipeHandle,
    /// Target pipe receiving the bytes (closed at EOF).
    to_pipe: PipeHandle,
) -> Result<Value> {
    // `pump_pipe_to_pipe` takes `&Value`: re-wrap the extracted
    // handles so the bridge keeps its Value-based shape.
    let from_pipe = Value::pipe_handle(from_pipe);
    let to_pipe = Value::pipe_handle(to_pipe);
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
/// code. Environment is exactly the script environment like `RUN`:
/// block-scoped `ENV` (such as the session's `SSH_USER` / `SSH_CLIENT` /
/// `SSH_SERVER` / `SSH_COMMAND` relay) reaches the child; host names
/// arrive only through `INHERIT_ENV`, including `TERM` for terminal
/// definitions. The working directory comes from the script.
///
#[oxdock_func(
    returns = TypeTag::Int,
    summary = "Run a command under a sized local terminal into pipes."
)]
#[example(
    name = "run under a terminal",
    code = r###"
    IMPORT [STD, SSH]
    INHERIT_ENV [PATH]
    LET $m: SSH_SERVE_INFO = SSH_SERVE("doc-ssh-pty", {username: "u", password: "p"})
    LET $d: HANDLE = ASYNC { SSH_DEQUEUE($m.server) }
    LET $cin: PIPE
    LET $cout: PIPE
    LET $c: HANDLE = ASYNC { SSH_CONNECT("doc-ssh-pty", "u", "p", $cin, $cout) }
    WITH_IO [stdout=$cin] ECHO "client-hello"
    LET $sess: SSH_DEQUEUE_INFO = AWAIT $d

    # The client task only proved the session live: cancel it before the
    # terminal run, so no two pumps share the session.
    CANCEL $c
    LET $pin: PIPE
    LET $pout: PIPE
    # Assert on the whole pipe: terminal streams carry setup noise ahead
    # of child output, so line reads are version-dependent.
    [family:unix] {
        LET $t: HANDLE = ASYNC { SSH_PTY_RUN($sess.session, ["sh", "-c", "echo pty-hi"], 0, 0, $pin, $pout) }
        LET $code: INT = AWAIT $t
        ASSERT_EQ $code 0
        ASSERT_CONTAINS $pout "pty-hi"
    }
    [family:windows] {
        LET $t: HANDLE = ASYNC { SSH_PTY_RUN($sess.session, ["cmd", "/c", "echo pty-hi"], 0, 0, $pin, $pout) }
        LET $code: INT = AWAIT $t
        ASSERT_EQ $code 0
        ASSERT_CONTAINS $pout "pty-hi"
    }
    SSH_CLOSE($m.server)
    "###
)]
fn ssh_pty_run<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Session handle from `SSH_DEQUEUE`.
    session: SshSessionTag,
    /// Program and arguments to run.
    argv: Vec<String>,
    /// Terminal rows when positive, session size otherwise.
    rows: i64,
    /// Terminal columns when positive, session size otherwise.
    cols: i64,
    /// Pipe carrying bytes consumed by the child.
    in_pipe: PipeHandle,
    /// Pipe carrying bytes produced by the child.
    out_pipe: PipeHandle,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!("SSH_PTY_RUN requires ASYNC: run it in its own task beside the session pump task");
    }
    // `pump_pty_session` takes `&Value` pipes: re-wrap the extracted
    // handles so the bridge keeps its Value-based shape.
    let in_pipe = Value::pipe_handle(in_pipe);
    let out_pipe = Value::pipe_handle(out_pipe);
    let initial = if rows > 0 && cols > 0 {
        crate::state::PtySize::new(rows as u32, cols as u32)
    } else {
        session.pty_size()
    };
    let cancel = AtomicBool::new(false);
    let code = crate::pty::pump_pty_session(
        cx,
        &argv,
        initial,
        &session.pty_size_handle(),
        &in_pipe,
        &out_pipe,
        &cancel,
    )?;
    Ok(Value::int(code))
}

/// Field table for the `SSH_SERVE` result bundle, backing the
/// `SSH_SERVE_INFO` schema and the `SSH_SERVE` return tag: one source
/// for the shape the host mints, so introspection, enforcement, and
/// documentation cannot drift apart.
fn serve_result_fields() -> Arc<[Field]> {
    Arc::from(vec![
        Field {
            name: "server".to_string(),
            ty: TypeTag::Custom(SshServerTag::descriptor()),
            docs: "Server handle for `SSH_ACCEPT`, `SSH_DEQUEUE`, and `SSH_CLOSE`.",
            optional: false,
        },
        Field {
            name: "addr".to_string(),
            ty: TypeTag::String,
            docs: "Bound socket address (`ip:port`) of the server.",
            optional: false,
        },
        Field {
            name: "username".to_string(),
            ty: TypeTag::String,
            docs: "Configured username.",
            optional: false,
        },
        Field {
            name: "password".to_string(),
            ty: TypeTag::String,
            docs: "Configured password.",
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

/// Field table for the `SSH_DEQUEUE` result bundle, backing the
/// `SSH_DEQUEUE_INFO` schema and the `SSH_DEQUEUE` return tag.
fn dequeue_result_fields() -> Arc<[Field]> {
    Arc::from(vec![
        Field {
            name: "session".to_string(),
            ty: TypeTag::Custom(SshSessionTag::descriptor()),
            docs: "Session handle for `SSH_PUMP_CHANNEL` (pump ends take once).",
            optional: false,
        },
        Field {
            name: "command".to_string(),
            ty: TypeTag::String,
            docs: "Executed command, empty for shells.",
            optional: false,
        },
        Field {
            name: "username".to_string(),
            ty: TypeTag::String,
            docs: "Authenticated username, empty when unset.",
            optional: false,
        },
        Field {
            name: "addr".to_string(),
            ty: TypeTag::String,
            docs: "Peer address, empty when unset.",
            optional: false,
        },
    ])
}

/// The `SSH` host module: virtual-endpoint server plus client, bridged
/// to DSL pipes. Generic over the process manager like every host module.
pub fn module_with<P: ProcessManager>() -> HostModule<P> {
    module_with_endpoints(Arc::new(EndpointRegistry::new(false)))
}

/// The `SSH` host module resolving through `registry`: the registry
/// travels as module state (`HostModule::with_state`) for bodies to
/// fetch from `cx`, so every entry stays `#[oxdock_func]`. Everything
/// else reaches the same registry through its `SSH_SERVER` handle. A
/// pre-run service check rides along: every service the steps name must
/// be fully bound before execution starts.
pub fn module_with_endpoints<P: ProcessManager>(registry: Arc<EndpointRegistry>) -> HostModule<P> {
    let check = registry.pre_run_check("SSH");
    HostModule {
        name: "SSH".to_string(),
        funcs: vec![
            SshServe::registration(),
            SshAccept::registration(),
            SshDequeue::registration(),
            SshPumpChannel::registration(),
            SshClose::registration(),
            SshConnect::registration(),
            SshPump::registration(),
            SshPtyRun::registration(),
        ],
        types: vec![SshServerTag::descriptor(), SshSessionTag::descriptor()],
        record_schemas: vec![
            RecordSchema {
                name: "SSH_SERVE_INFO",
                fields: serve_result_fields(),
            },
            RecordSchema {
                name: "SSH_DEQUEUE_INFO",
                fields: dequeue_result_fields(),
            },
        ],
        states: Vec::new(),
    }
    .with_state(registry)
    .with_state(Arc::new(check))
}
