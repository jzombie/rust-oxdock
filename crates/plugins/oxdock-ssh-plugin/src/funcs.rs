//! The `SSH` host module: `SERVE`, `ACCEPT`, `CLOSE`, `CONNECT`, `PUMP`.
//!
//! Pipe direction convention (fixed for every func): `out_pipe` carries
//! bytes produced by the wire side (the DSL reads them), `in_pipe`
//! carries bytes consumed by the wire side (the DSL writes them).

use std::collections::BTreeMap;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use oxdock_core::{HostModule, OxDockFn, OxDockType, StepCtx, Value};
use oxdock_func_macro::oxdock_func;
use oxdock_process::ProcessManager;
use russh::keys::{Algorithm, PrivateKey};

use crate::bridge::{pump_pipe_to_pipe, pump_session};
use crate::keys::load_or_create_host_key;
use crate::runtime::{connect_runtime, connect_session};
use crate::state::{CLOSE_JOIN_TIMEOUT, Dequeue, ServerState, SessionQueue, ShutdownSignal};
use crate::types::SshServerTag;
use crate::validate::{ephemeral_password, parse_connect_target, parse_serve_bind};

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

/// Read the options MAP for `SSH_SERVE`. The 4th argument must be a MAP;
/// unknown keys bail so script typos fail fast instead of silently ignored.
fn serve_options(options: &Value) -> Result<&BTreeMap<String, Value>> {
    options.as_map().ok_or_else(|| {
        anyhow::anyhow!(
            "SSH_SERVE options must be a MAP, got {}",
            options.type_name()
        )
    })
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

/// Spin up an ephemeral loopback SSH server with the given credentials.
/// An empty password generates a random one. `options` is a MAP with the
/// optional `key_path` STRING (workspace-relative OpenSSH Ed25519 file,
/// load-or-create; absent or blank keeps the ephemeral in-memory key).
/// Non-blocking: returns a MAP with `server` (SSH_SERVER),
/// `addr` (STRING `host:port`), `username` and `password` (STRINGs).
#[oxdock_func(returns = "MAP", summary = "Serve ephemeral SSH on loopback.")]
fn ssh_serve<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    bind: String,
    username: String,
    password: String,
    options: Value,
) -> Result<Value> {
    if username.is_empty() {
        bail!("SSH_SERVE username must not be empty");
    }
    let map = serve_options(&options)?;
    for key in map.keys() {
        if key != "key_path" {
            bail!("SSH_SERVE() unknown option '{key}' (expected: key_path)");
        }
    }
    let key_path = optional_string(map, "SSH_SERVE", "key_path")?;
    let (host, port) = parse_serve_bind(&bind)?;
    let password = if password.is_empty() {
        ephemeral_password()
    } else {
        password
    };
    let host_key = match load_or_create_host_key(cx, "SSH_SERVE", key_path)? {
        Some(key) => key,
        None => PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .context("generate ephemeral Ed25519 host key")?,
    };
    let bind_host = if host.is_empty() {
        "127.0.0.1"
    } else {
        host.as_str()
    };
    let listener = std::net::TcpListener::bind((bind_host, port))
        .with_context(|| format!("SSH_SERVE bind {bind_host}:{port} failed"))?;
    let local_addr = listener
        .local_addr()
        .context("SSH_SERVE cannot read its bound address")?;
    let id = SERVER_IDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let id = format!("ssh-{pid}-{id}", pid = std::process::id());
    let queue = Arc::new(SessionQueue::new());
    let pty_size = crate::state::shared_pty_size();
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<ShutdownSignal>();
    let thread_queue = Arc::clone(&queue);
    let thread_pty_size = Arc::clone(&pty_size);
    let thread_user = username.clone();
    let thread_pass = password.clone();
    let thread_key = host_key;
    let thread = std::thread::Builder::new()
        .name(id.clone())
        .spawn(move || {
            crate::runtime::serve(
                listener,
                thread_key,
                thread_user,
                thread_pass,
                thread_queue,
                thread_pty_size,
                shutdown_rx,
            )
        })
        .context("SSH_SERVE cannot spawn the server thread")?;
    let state = Arc::new(ServerState::new(
        id,
        local_addr,
        queue,
        pty_size,
        shutdown_tx,
        thread,
    ));
    let mut map = BTreeMap::new();
    map.insert(
        "server".to_string(),
        Value::mint_heap(SshServerTag::descriptor(), SshServerTag::new(state)),
    );
    map.insert("addr".to_string(), Value::string(local_addr.to_string()));
    map.insert("username".to_string(), Value::string(username));
    map.insert("password".to_string(), Value::string(password));
    Ok(Value::map(map))
}

/// Accept the next authenticated session and pump it through explicit
/// pipes until the channel closes. Must run inside `ASYNC`. Returns a
/// MAP with `closed` (BOOL) and `command` (STRING, empty for shells).
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
    let session = loop {
        match state.queue().try_pop() {
            Dequeue::Session(session) => break session,
            Dequeue::Shutdown => bail!("SSH_ACCEPT: server is closed"),
            Dequeue::Empty => {}
        }
        match state.queue().wait_for_session(DEQUEUE_TICK) {
            Dequeue::Session(session) => break session,
            Dequeue::Shutdown => bail!("SSH_ACCEPT: server is closed"),
            Dequeue::Empty => {}
        }
        if cx.is_cancelled() {
            bail!("SSH_ACCEPT: task cancelled");
        }
    };
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

/// Shut a server down and join its runtime thread (bounded). Idempotent:
/// returns BOOL true when no thread remains.
#[oxdock_func(pure, returns = "BOOL", summary = "Shut down an SSH server.")]
fn ssh_close(server: Value) -> Result<Value> {
    let state = server_state(&server, "SSH_CLOSE")?;
    state.request_shutdown();
    Ok(Value::bool(state.join_thread(CLOSE_JOIN_TIMEOUT)))
}

/// Connect to an SSH server with distinct inner credentials and pump the
/// shell channel through explicit pipes until it closes. Must run inside
/// `ASYNC`, concurrently with the `SSH_PUMP` tasks (never before them).
/// Returns a MAP with `closed` (BOOL).
#[oxdock_func(returns = "MAP", summary = "Open an SSH client session into pipes.")]
fn ssh_connect<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    target: String,
    username: String,
    password: String,
    in_pipe: Value,
    out_pipe: Value,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!("SSH_CONNECT requires ASYNC: run it in its own task beside the SSH_PUMP tasks");
    }
    if username.is_empty() {
        bail!("SSH_CONNECT username must not be empty");
    }
    let (host, port) = parse_connect_target(&target)?;
    let addr = format!("{host}:{port}")
        .to_socket_addrs()
        .with_context(|| format!("SSH_CONNECT cannot resolve {host}:{port}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("SSH_CONNECT cannot resolve {host}:{port}"))?;
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

/// Run `argv` under a local pseudo-terminal sized from the outer session
/// and pump it through explicit pipes until the child exits. `rows`/`cols`
/// seed the initial size when positive; non-positive falls back to the
/// latest size any outer session requested (24x80 default). Outer
/// window-change requests resize the terminal live. Must run inside
/// `ASYNC`. Returns the INT exit code. Environment is inherited from the
/// host process and the working directory comes from the script; script
/// `ENV` overrides do not apply (unlike `RUN`).
#[oxdock_func(
    returns = "INT",
    summary = "Run a command under a sized local terminal into pipes."
)]
fn ssh_pty_run<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    server: Value,
    argv: Value,
    rows: i64,
    cols: i64,
    in_pipe: Value,
    out_pipe: Value,
) -> Result<Value> {
    if !cx.is_async_task() {
        bail!("SSH_PTY_RUN requires ASYNC: run it in its own task beside the SSH_ACCEPT task");
    }
    let state = server_state(&server, "SSH_PTY_RUN")?;
    let argv = argv_list(&argv, "SSH_PTY_RUN")?;
    let initial = if rows > 0 && cols > 0 {
        crate::state::PtySize::new(rows as u32, cols as u32)
    } else {
        state.pty_size()
    };
    let cancel = AtomicBool::new(false);
    let code = crate::pty::pump_pty_session(
        cx,
        &argv,
        initial,
        &state.pty_size_handle(),
        &in_pipe,
        &out_pipe,
        &cancel,
    )?;
    Ok(Value::int(code))
}

/// The `SSH` host module: ephemeral server plus client, bridged to DSL
/// pipes. Generic over the process manager like every host module.
pub fn module_with<P: ProcessManager>() -> HostModule<P> {
    HostModule {
        name: "SSH".to_string(),
        funcs: vec![
            SshServe::registration(),
            SshAccept::registration(),
            SshClose::registration(),
            SshConnect::registration(),
            SshPump::registration(),
            SshPtyRun::registration(),
        ],
        types: vec![SshServerTag::descriptor()],
    }
}
