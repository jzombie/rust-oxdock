//! Tokio runtime hosting: one thread per SSH server, plus the outbound
//! client path shared by `SSH_CONNECT`.
//!
//! The rest of the workspace stays synchronous: this module is the only
//! place a Tokio runtime exists, and the boundary speaks only bounded
//! channels plus explicit pipe handles.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use russh::keys::PrivateKey;
use tokio::sync::mpsc;

use crate::auth::ServerFactory;
use crate::state::{CHANNEL_CAPACITY, DownMsg, SessionQueue, ShutdownSignal, UpMsg};
use russh::server::Server as _;

/// Default idle kill for quiet SSH legs: connections with no traffic
/// this long are reaped, so vanished clients cannot leak tasks, ports,
/// and permits forever. `SSH_SERVE` accepts an `idle_timeout` override
/// per server (zero disables); this default covers `SSH_CONNECT` legs.
pub const DEFAULT_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(300);

/// Default cap per stream for one-shot exec: matches the 8 MiB pipe
/// spill and MCP result caps, so one build log cannot flood the agent.
pub const DEFAULT_EXEC_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Default wall clock for one-shot exec: kill with partials past this.
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(60);

/// Outcome of one one-shot remote exec. `exit` is `None` when the
/// server never sent a status (timeout, early close); callers map that
/// to -1 like `SSH_CONNECT` does.
pub struct ExecOutcome {
    pub exit: Option<i64>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
    pub timed_out: bool,
}

/// Run `command` (one pre-joined shell string) on `target` and collect
/// the result. The join happens once at the call site via
/// `shell_words::join`, never by hand: argv crosses every other
/// boundary as values, and this is the single place it becomes a
/// string. Caps apply while reading, so a flood stops growing the
/// buffers while the drain continues to the exit status. A timeout
/// abandons the drain but keeps whatever arrived: partials are where
/// the diagnosis lives, so they must not be discarded.
pub async fn run_remote_exec(
    target: &SocketAddr,
    username: &str,
    password: &str,
    command: Vec<u8>,
    timeout: Duration,
    max_bytes: usize,
) -> Result<ExecOutcome> {
    #[derive(Default)]
    struct Shared {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        truncated: bool,
        exit: Option<i64>,
    }
    let shared = Arc::new(std::sync::Mutex::new(Shared::default()));
    let worker = Arc::clone(&shared);
    let timed_out = tokio::time::timeout(timeout, async move {
        let config = Arc::new(russh::client::Config {
            inactivity_timeout: Some(DEFAULT_INACTIVITY_TIMEOUT),
            ..Default::default()
        });
        let mut session = russh::client::connect(config, *target, AcceptAnyHostKey)
            .await
            .context("SSH_EXEC dial failed")?;
        let authenticated = session
            .authenticate_password(username, password)
            .await
            .context("SSH_EXEC authentication exchange failed")?;
        if !matches!(authenticated, russh::client::AuthResult::Success) {
            bail!("SSH_EXEC authentication rejected");
        }
        let mut channel = session
            .channel_open_session()
            .await
            .context("SSH_EXEC channel open failed")?;
        channel
            .exec(true, command)
            .await
            .context("SSH_EXEC exec request failed")?;
        let mut eof = false;
        // Messages arrive in channel order, so everything before
        // `ExitStatus` is already collected when it lands. Success acks
        // for the exec request fall through: the request was accepted
        // once the server announces the session.
        loop {
            match channel.wait().await {
                None => break,
                Some(russh::ChannelMsg::Data { data }) => {
                    let mut guard = worker.lock().unwrap_or_else(|poison| poison.into_inner());
                    let room = max_bytes.saturating_sub(guard.stdout.len());
                    if data.len() > room {
                        guard.truncated = true;
                    }
                    guard.stdout.extend_from_slice(&data[..room.min(data.len())]);
                }
                Some(russh::ChannelMsg::ExtendedData { data, ext: 1 }) => {
                    let mut guard = worker.lock().unwrap_or_else(|poison| poison.into_inner());
                    let room = max_bytes.saturating_sub(guard.stderr.len());
                    if data.len() > room {
                        guard.truncated = true;
                    }
                    guard.stderr.extend_from_slice(&data[..room.min(data.len())]);
                }
                Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                    let mut guard = worker.lock().unwrap_or_else(|poison| poison.into_inner());
                    guard.exit.get_or_insert(i64::from(exit_status));
                }
                Some(russh::ChannelMsg::Eof) => {
                    eof = true;
                }
                Some(russh::ChannelMsg::Close) => break,
                Some(_) => {}
            }
            let done = worker
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .exit
                .is_some()
                && eof;
            if done {
                break;
            }
        }
        let _ = session
            .disconnect(russh::Disconnect::ByApplication, "", "")
            .await;
        Ok::<_, anyhow::Error>(())
    })
    .await;
    let mut guard = shared.lock().unwrap_or_else(|poison| poison.into_inner());    match timed_out {
        Ok(Ok(())) => Ok(ExecOutcome {
            exit: guard.exit,
            stdout: std::mem::take(&mut guard.stdout),
            stderr: std::mem::take(&mut guard.stderr),
            truncated: guard.truncated,
            timed_out: false,
        }),
        Ok(Err(err)) => Err(err),
        Err(_) => Ok(ExecOutcome {
            exit: guard.exit,
            stdout: std::mem::take(&mut guard.stdout),
            stderr: std::mem::take(&mut guard.stderr),
            truncated: guard.truncated,
            timed_out: true,
        }),
    }
}

/// Drive one `SSH_SERVE` listener until shutdown. Owns the current-thread
/// runtime; returns when the listener closes (shutdown or fatal accept
/// error). Connected sessions drain independently. The host key arrives
/// fully resolved (ephemeral or stable); this module never touches disk.
pub fn serve(
    listener: std::net::TcpListener,
    key: PrivateKey,
    user: String,
    password: String,
    queue: Arc<SessionQueue>,
    shutdown_rx: std::sync::mpsc::Receiver<ShutdownSignal>,
    inactivity_timeout: Option<Duration>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return,
    };
    runtime.block_on(async move {
        serve_async(
            listener,
            key,
            user,
            password,
            queue,
            shutdown_rx,
            inactivity_timeout,
        )
        .await
        .map(|_| ())
        .unwrap_or(());
    });
}

async fn serve_async(
    listener: std::net::TcpListener,
    key: PrivateKey,
    user: String,
    password: String,
    queue: Arc<SessionQueue>,
    shutdown_rx: std::sync::mpsc::Receiver<ShutdownSignal>,
    inactivity_timeout: Option<Duration>,
) -> Result<()> {
    let config = Arc::new(russh::server::Config {
        inactivity_timeout,
        // Rejection padding stays (user-enumeration resistance), but
        // sized for an ephemeral proxy: every login waits this out
        // (the client's opening `none` probe included), so a full
        // second stalls each password prompt by a noticeable beat.
        auth_rejection_time: Duration::from_millis(100),
        auth_rejection_time_initial: Some(Duration::from_millis(100)),
        keys: vec![key],
        ..Default::default()
    });
    let mut factory = ServerFactory::new(user, password, queue);
    listener
        .set_nonblocking(true)
        .context("prepare listener for the runtime")?;
    let listener =
        tokio::net::TcpListener::from_std(listener).context("hand listener to the runtime")?;
    let running = factory.run_on_socket(config, &listener);
    let handle = running.handle();
    // `spawn_blocking` bridges the sync shutdown signal into the runtime.
    // The closure only blocks on `recv`, never on runtime work.
    let waiter =
        tokio::task::spawn_blocking(move || shutdown_rx.recv().map(|_| ()).map_err(|_| ()));
    tokio::select! {
        result = running => {
            result.context("SSH listener failed")?;
        }
        _ = waiter => {
            handle.shutdown("SSH_CLOSE".to_string());
        }
    }
    Ok(())
}

/// Trust-on-first-use client handler: v1 connects to ephemeral or
/// operator-specified targets, so host key pinning is a follow-up.
pub(crate) struct AcceptAnyHostKey;

impl russh::client::Handler for AcceptAnyHostKey {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool> {
        Ok(true)
    }
}

/// Outcome of one outbound session: handoff queues the pump threads
/// attach to, plus the client handle for teardown. The tasks driving the
/// queues live on the caller's runtime, which must outlive the pump.
pub struct OutboundSession {
    pub up_rx: mpsc::Receiver<UpMsg>,
    pub down_tx: mpsc::Sender<DownMsg>,
    /// First `exit-status` the server sent, if any. Shared with the wire
    /// task; read after the pump drains.
    pub exit_status: Arc<std::sync::Mutex<Option<u32>>>,
    /// Wire task join handle: awaited after disconnect so a status that
    /// arrives after EOF is recorded before the result map is read.
    pub wire_done: tokio::task::JoinHandle<()>,
    pub(crate) handle: russh::client::Handle<AcceptAnyHostKey>,
}

/// Build a runtime for one `SSH_CONNECT` leg and open the session on it.
/// The returned session borrows the runtime's task set: the caller runs
/// the synchronous pump first, then disconnects, then drops the runtime.
///
/// A single-worker multi-thread runtime (not current-thread) is required:
/// `block_on` runs setup from the calling thread while spawned wire tasks
/// keep driving the queues during the synchronous pump.
pub fn connect_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .context("build SSH client runtime")
}

/// Connect to `target` with `username`/`password` and open a shell
/// channel on `runtime`. Wire tasks are spawned on the same runtime.
pub async fn connect_session(
    target: &SocketAddr,
    username: &str,
    password: &str,
) -> Result<OutboundSession> {
    let config = Arc::new(russh::client::Config {
        inactivity_timeout: Some(DEFAULT_INACTIVITY_TIMEOUT),
        ..Default::default()
    });
    let mut session = russh::client::connect(config, *target, AcceptAnyHostKey)
        .await
        .context("SSH_CONNECT dial failed")?;
    let authenticated = session
        .authenticate_password(username, password)
        .await
        .context("SSH_CONNECT authentication exchange failed")?;
    if !matches!(authenticated, russh::client::AuthResult::Success) {
        bail!("SSH_CONNECT authentication rejected");
    }
    let channel = session
        .channel_open_session()
        .await
        .context("SSH_CONNECT channel open failed")?;
    channel
        .request_shell(true)
        .await
        .context("SSH_CONNECT shell request failed")?;
    let (mut reader, writer) = channel.split();

    let (up_tx, up_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (down_tx, mut down_rx) = mpsc::channel::<DownMsg>(CHANNEL_CAPACITY);

    // Wire bytes toward the pump. A channel EOF half-closes the pump
    // (it may still flush its own output); a close ends the queue by
    // dropping the sender. The first exit-status is recorded for the
    // result map; later ones cannot happen on one channel. EOF never
    // ends this task: a status learned at child teardown can arrive
    // after it (stock clients behave the same), and only Close or
    // disconnect means no more messages will ever come. Sends after
    // the pump is gone fail silently and the drain continues.
    let exit_status = Arc::new(std::sync::Mutex::new(None));
    let exit_status_writer = Arc::clone(&exit_status);
    let wire_done = tokio::spawn(async move {
        while let Some(message) = reader.wait().await {
            match message {
                russh::ChannelMsg::Data { data } => {
                    let _ = up_tx.send(UpMsg::Data(Bytes::copy_from_slice(&data))).await;
                }
                russh::ChannelMsg::ExitStatus { exit_status: code } => {
                    if let Ok(mut slot) = exit_status_writer.lock() {
                        slot.get_or_insert(code);
                    }
                }
                russh::ChannelMsg::Eof => {
                    let _ = up_tx.send(UpMsg::Eof).await;
                }
                russh::ChannelMsg::Close => break,
                _ => {}
            }
        }
    });

    // Pump bytes toward the wire. EOF closes the channel; pump loss ends
    // the task.
    tokio::spawn(async move {
        while let Some(message) = down_rx.recv().await {
            let result = match message {
                DownMsg::Data(bytes) => writer.data_bytes(bytes).await.map(|_| ()),
                DownMsg::Eof => writer.eof().await.map(|_| ()),
                // Server-to-client only: never queued on this leg.
                DownMsg::ExitStatus(_) => Ok(()),
            };
            if result.is_err() {
                break;
            }
        }
        let _ = writer.close().await;
    });

    Ok(OutboundSession {
        up_rx,
        down_tx,
        exit_status,
        wire_done,
        handle: session,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_inactivity_timeout_is_five_minutes() {
        // The idle kill default is a policy choice, not an accident:
        // pin it so a quiet edit cannot reintroduce the beat every
        // login used to wait out.
        assert_eq!(DEFAULT_INACTIVITY_TIMEOUT, Duration::from_secs(300));
    }
}
