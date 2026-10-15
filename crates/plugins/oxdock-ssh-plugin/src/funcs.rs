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

/// Dequeue one authenticated session for routing before any byte
/// pumping starts. Must run inside `ASYNC`.
///
/// The server sends first: the client side never EOFs its input, so a
/// synthetic reply cannot race teardown.
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

/// Spin up an SSH server on a runner-mapped service. Unmapped and
/// memory services bail: SSH needs a TCP socket.
/// `options` takes required `username`/`password` STRINGs plus the
/// optional `key_path` STRING (workspace-relative Ed25519 file,
/// load-or-create; blank keeps the ephemeral key).
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
            // The runtime owns a clone; the slot keeps the backlog socket.
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
            // Unreachable: `SocketRequired` already bailed; same text.
            drop(shutdown_rx);
            return Err(no_cli_mapping("SSH_SERVE", &key.endpoint));
        }
        AcquiredListener::Offline => {
            // No thread: dropping the receiver makes shutdown a no-op.
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

/// `SSH_DEQUEUE` + `SSH_PUMP_CHANNEL` in one call, for worker loops
/// that need no pre-pump inspection. Must run inside `ASYNC`.
///
/// The server sends first: the client side never EOFs its input, so
/// the reply cannot race teardown.
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
        true,
    )?;
    let mut map = BTreeMap::new();
    map.insert("closed".to_string(), Value::bool(true));
    map.insert(
        "command".to_string(),
        Value::string(session.exec_command.unwrap_or_default()),
    );
    Ok(Value::map(map))
}

/// Pump a dequeued session through explicit pipes until the channel
/// closes. Must run inside `ASYNC`. Pump ends take once: a second
/// pump bails instead of splitting bytes.
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
    let in_pipe = Value::pipe_handle(in_pipe);
    let out_pipe = Value::pipe_handle(out_pipe);
    let (up_rx, down_tx) = session.take_pump_ends()?;
    let cancel = AtomicBool::new(false);
    // Release the control sender when the pump ends (pass or fail):
    // the wire writer must observe the drain and close then, not when
    // the last session clone drops long after.
    let pump = pump_session(cx, &in_pipe, &out_pipe, up_rx, down_tx, &cancel, true);
    session.finish_pump();
    pump?;
    let mut map = BTreeMap::new();
    map.insert("closed".to_string(), Value::bool(true));
    Ok(Value::map(map))
}

/// Report the session's exit status to the client (RFC 4254 6.10),
/// ahead of the channel close. Survives the take-once pump ends, so
/// call it before the pump drains. `SSH_PTY_RUN` reports by itself;
/// this covers other legs (`RUN`, synthetic replies).
#[oxdock_func(returns = TypeTag::Bool, summary = "Send the session exit status to the client.")]
#[example(
    name = "report exit status",
    code = r###"
    IMPORT [STD, SSH]
    LET $m: SSH_SERVE_INFO = SSH_SERVE("doc-ssh-exit-status", {username: "u", password: "p"})
    LET $srv: HANDLE = ASYNC {
        LET $sess: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
        LET $sin: PIPE
        LET $sout: PIPE
        LET $p: HANDLE = ASYNC { SSH_PUMP_CHANNEL($sess.session, $sin, $sout) }
        SSH_SEND_EXIT_STATUS($sess.session, 3)
        WITH_IO [stdout=$sin] ECHO "server-greeting"
        AWAIT $p
    }
    LET $cin: PIPE
    LET $cout: PIPE
    LET $c: HANDLE = ASYNC { SSH_CONNECT("doc-ssh-exit-status", "u", "p", $cin, $cout) }
    # Close the client input: its pump observes EOF and half-closes, so
    # both pumps drain instead of idling on an empty pipe.
    WITH_IO [stdout=$cin] ECHO "client-hello"
    LET $cDone: MAP<ANY> = AWAIT $c
    ASSERT_EQ $cDone.closed true
    ASSERT_EQ $cDone.exit_status 3
    AWAIT $srv
    SSH_CLOSE($m.server)
    "###
)]
fn ssh_send_exit_status<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Session handle from `SSH_DEQUEUE`.
    session: SshSessionTag,
    /// Exit status to report (0 to 4294967295).
    code: i64,
) -> Result<Value> {
    let _ = cx;
    if code < 0 {
        bail!("SSH_SEND_EXIT_STATUS code must not be negative: {code}");
    }
    let code = u32::try_from(code)
        .map_err(|_| anyhow::anyhow!("SSH_SEND_EXIT_STATUS code out of range: {code}"))?;
    session.send_exit_status(code)?;
    Ok(Value::bool(true))
}

/// Shut a server down and join its runtime thread (bounded).
/// Idempotent.
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

/// Open an SSH client session and pump its shell channel through
/// explicit pipes until it closes. Must run inside `ASYNC`, beside
/// the pump tasks. Under `--offline` the dial bails before any
/// socket work. The result MAP carries `exit_status`, or -1 when the
/// server sent none.
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
    // Registry travels as module state.
    let registry = cx
        .module_state::<EndpointRegistry>("SSH")
        .expect("SSH module registers its endpoint registry as module state");
    let in_pipe = Value::pipe_handle(in_pipe);
    let out_pipe = Value::pipe_handle(out_pipe);
    if registry.is_offline() {
        bail!("SSH_CONNECT failed: engine running in --offline mode");
    }
    if username.is_empty() {
        bail!("SSH_CONNECT username must not be empty");
    }
    // Symbolic only: physical shapes already failed in the parser.
    let endpoint = parse_virtual_endpoint(&target, "SSH_CONNECT")?;
    let key = EndpointKey::tcp(endpoint);
    let addr = match resolve_service_target(
        &registry,
        &key,
        "SSH_CONNECT",
        SocketRequirement::SocketRequired,
    )? {
        ServiceTarget::Tcp(addr) => addr,
        // Unreachable: the shared resolver already bailed; same text.
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
        exit_status,
        wire_done,
        handle,
        ..
    } = session;
    let cancel = AtomicBool::new(false);
    // Client linger: the reply and the exit-status arrive after the
    // client's own stdin EOF, so never cut the output wait short.
    let pump = pump_session(cx, &in_pipe, &out_pipe, up_rx, down_tx, &cancel, false);
    let _ = runtime.block_on(handle.disconnect(russh::Disconnect::ByApplication, "", ""));
    pump?;
    // Joined after disconnect: a post-EOF status records before this read.
    let _ = runtime.block_on(wire_done);
    let mut map = BTreeMap::new();
    map.insert("closed".to_string(), Value::bool(true));
    let status = exit_status
        .lock()
        .map(|slot| slot.map(i64::from))
        .unwrap_or(None);
    map.insert("exit_status".to_string(), Value::int(status.unwrap_or(-1)));
    Ok(Value::map(map))
}

/// Copy one pipe into another until EOF, then close the target.
/// Either task placement works, as long as the other end is live.
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
    let from_pipe = Value::pipe_handle(from_pipe);
    let to_pipe = Value::pipe_handle(to_pipe);
    let cancel = AtomicBool::new(false);
    let total = pump_pipe_to_pipe(cx, &from_pipe, &to_pipe, &cancel)?;
    Ok(Value::int(total))
}

/// Run `argv` under a local pseudo-terminal and pump it through
/// explicit pipes until the child exits. Must run inside `ASYNC`.
/// Returns the INT exit code.
///
/// `rows`/`cols` seed the initial size when positive, else the
/// session's requested size (24x80 default). Outer window-changes
/// resize live; each session owns its cell.
///
/// The code also reports as the wire `exit-status` at child exit, so
/// stock clients see the real code instead of 255.
///
/// Every session opens cleared and homed, like Win32-OpenSSH, so the
/// child renders onto a known blank screen.
///
/// Windows joining follows MSVCRT rules, so CRT programs parse back
/// exactly `argv`. `cmd /c` is the exception (its tail is a command,
/// not `argv`): route command text via `ENV SSH_COMMAND="..."` with
/// `["cmd", "/c", "%SSH_COMMAND%"]` instead.
///
/// Stdin arrives untranslated. Consoles submit on CR, so a DSL line
/// bound for a console child ends with `\r`. Real terminals already
/// send CR for Enter.
///
/// Environment is exactly the script environment like `RUN`:
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
    let in_pipe = Value::pipe_handle(in_pipe);
    let out_pipe = Value::pipe_handle(out_pipe);
    let initial = if rows > 0 && cols > 0 {
        crate::state::PtySize::new(rows as u32, cols as u32)
    } else {
        session.pty_size()
    };
    let cancel = AtomicBool::new(false);
    // Best effort: an unpumped session has no reader.
    let report = |code: u32| {
        let _ = session.send_exit_status(code);
    };
    let code = crate::pty::pump_pty_session(
        cx,
        &argv,
        initial,
        &session.pty_size_handle(),
        &in_pipe,
        &out_pipe,
        &cancel,
        Some(&report),
    )?;
    Ok(Value::int(code))
}

/// Run `argv` directly (no terminal) and pump it through explicit pipes
/// until the child exits. Must run inside `ASYNC`. Returns the INT exit
/// code (-1 when the child died by signal).
///
/// Bytes travel untouched: no init sequence, no newline translation, no
/// setup handshake. This is the binary-safe leg `scp`-style protocols
/// need; `SSH_PTY_RUN` owns the interactive side instead.
///
/// The code reports as the wire `exit-status` at child exit, so stock
/// clients see the real code instead of 255. Signaled children report
/// nothing: clients observe the close without a status.
///
/// Stdin EOF closes the child's stdin, so stdin-driven children like
/// `cat` exit on their own. Standard output and error ride separate
/// pipes: diagnostics must never pollute the byte stream.
///
/// Environment is exactly the script environment like `RUN`:
/// block-scoped `ENV` reaches the child; host names arrive only through
/// `INHERIT_ENV`. The working directory comes from the script.
///
/// Windows joining follows MSVCRT rules, so CRT programs parse back
/// exactly `argv`. `cmd /c` is the exception (its tail is a command,
/// not `argv`): route command text via `ENV SSH_COMMAND="..."` with
/// `["cmd", "/c", "%SSH_COMMAND%"]` instead.
#[oxdock_func(
    returns = TypeTag::Int,
    summary = "Run a command with piped stdio into explicit pipes."
)]
#[example(
    name = "run with piped stdio",
    code = r###"
    IMPORT [STD, SSH]
    INHERIT_ENV [PATH]
    LET $m: SSH_SERVE_INFO = SSH_SERVE("doc-ssh-run", {username: "u", password: "p"})
    LET $d: HANDLE = ASYNC { SSH_DEQUEUE($m.server) }
    LET $cin: PIPE
    LET $cout: PIPE
    LET $c: HANDLE = ASYNC { SSH_CONNECT("doc-ssh-run", "u", "p", $cin, $cout) }
    WITH_IO [stdout=$cin] ECHO "client-hello"
    LET $sess: SSH_DEQUEUE_INFO = AWAIT $d

    # The client task only proved the session live: cancel it before the
    # run, so no two pumps share the session.
    CANCEL $c
    LET $pin: PIPE
    LET $pout: PIPE
    LET $perr: PIPE
    [family:unix] {
        LET $t: HANDLE = ASYNC { SSH_RUN($sess.session, ["sh", "-c", "echo run-hi"], $pin, $pout, $perr) }
        LET $code: INT = AWAIT $t
        ASSERT_EQ $code 0
        ASSERT_CONTAINS $pout "run-hi"
    }
    [family:windows] {
        LET $t: HANDLE = ASYNC { SSH_RUN($sess.session, ["cmd", "/c", "echo run-hi"], $pin, $pout, $perr) }
        LET $code: INT = AWAIT $t
        ASSERT_EQ $code 0
        ASSERT_CONTAINS $pout "run-hi"
    }
    SSH_CLOSE($m.server)
    "###
)]
fn ssh_run<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Session handle from `SSH_DEQUEUE`.
    session: SshSessionTag,
    /// Program and arguments to run.
    argv: Vec<String>,
    /// Pipe carrying bytes consumed by the child on stdin.
    in_pipe: PipeHandle,
    /// Pipe carrying bytes produced by the child on stdout.
    out_pipe: PipeHandle,
    /// Pipe carrying bytes produced by the child on stderr.
    err_pipe: PipeHandle,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!("SSH_RUN requires ASYNC: run it in its own task beside the session pump task");
    }
    let in_pipe = Value::pipe_handle(in_pipe);
    let out_pipe = Value::pipe_handle(out_pipe);
    let err_pipe = Value::pipe_handle(err_pipe);
    let cancel = AtomicBool::new(false);
    // Best effort: an unpumped session has no reader.
    let report = |code: u32| {
        let _ = session.send_exit_status(code);
    };
    let code = crate::run::pump_run_session(
        cx,
        &argv,
        &in_pipe,
        &out_pipe,
        &err_pipe,
        &cancel,
        Some(&report),
    )?;
    Ok(Value::int(code))
}

/// Field table backing the `SSH_SERVE_INFO` schema and return tag.
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

/// Field table backing the `SSH_DEQUEUE_INFO` schema and return tag.
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
            SshSendExitStatus::registration(),
            SshClose::registration(),
            SshConnect::registration(),
            SshPump::registration(),
            SshPtyRun::registration(),
            SshRun::registration(),
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
