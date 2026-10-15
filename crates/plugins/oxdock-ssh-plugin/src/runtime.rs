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

/// Inactivity timeout for idle SSH sessions.
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(300);

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
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return,
    };
    runtime.block_on(async move {
        serve_async(listener, key, user, password, queue, shutdown_rx)
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
) -> Result<()> {
    let config = Arc::new(russh::server::Config {
        inactivity_timeout: Some(INACTIVITY_TIMEOUT),
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
        inactivity_timeout: Some(INACTIVITY_TIMEOUT),
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
