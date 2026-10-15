//! The `SSH_SERVER` opaque handle type.
//!
//! The value carries shared server lifetime state and nothing secret:
//! credentials live only on the runtime thread. Display and Debug
//! redact everything but the id and address.

use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use oxdock_func_macro::oxdock_type;
use tokio::sync::mpsc;

use crate::state::{DownMsg, ServerState, UpMsg};

/// Handle to one ephemeral SSH server instance.
///
/// Cloning the value shares the server; dropping the last clone
/// signals shutdown.
#[oxdock_type(name = "SSH_SERVER")]
#[derive(Debug, Clone)]
pub struct SshServerTag {
    state: Arc<ServerState>,
}

impl SshServerTag {
    pub fn new(state: Arc<ServerState>) -> Self {
        Self { state }
    }

    pub fn state(&self) -> &Arc<ServerState> {
        &self.state
    }
}

impl PartialEq for SshServerTag {
    fn eq(&self, other: &Self) -> bool {
        self.state.id() == other.state.id()
    }
}

impl fmt::Display for SshServerTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SSH_SERVER({}, {})",
            self.state.id(),
            self.state.addr_text()
        )
    }
}

/// Unique session ids per process.
static SESSION_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Shareable half of one dequeued session: metadata, the session's
/// terminal-size cell, plus take-once pump ends. Cloning shares the
/// session; pumping consumes the ends.
#[derive(Debug)]
struct SshSessionInner {
    id: u64,
    command: Option<String>,
    username: Option<String>,
    peer_addr: Option<SocketAddr>,
    /// This session's size cell, shared with its connection handler
    /// (writer) and its pty pump (reader). Never shared across sessions.
    pty_size: crate::state::SharedPtySize,
    up_rx: Mutex<Option<mpsc::Receiver<UpMsg>>>,
    down_tx: Mutex<Option<mpsc::Sender<DownMsg>>>,
    /// Control sender for the wire `exit-status`, kept across pumps:
    /// a clone of `down_tx` held back at construction, so
    /// `SSH_SEND_EXIT_STATUS` can queue the request even while a byte
    /// pump owns the main ends. Released when the pump finishes (see
    /// `finish_pump`): the wire writer must observe the drain and close
    /// the channel then, not when the last session clone drops (long
    /// after, on an accept loop holding the session). Dies with the
    /// last session clone for sessions that never pump.
    control_tx: Mutex<Option<mpsc::Sender<DownMsg>>>,
}

/// Handle to one dequeued SSH session instance.
///
/// Cloning the value shares the session; metadata reads never consume.
/// Display shows id and peer only: the command string may carry secrets.
#[oxdock_type(name = "SSH_SESSION")]
#[derive(Debug, Clone)]
pub struct SshSessionTag {
    inner: Arc<SshSessionInner>,
}

impl SshSessionTag {
    pub fn new(
        command: Option<String>,
        username: Option<String>,
        peer_addr: Option<SocketAddr>,
        pty_size: crate::state::SharedPtySize,
        up_rx: mpsc::Receiver<UpMsg>,
        down_tx: mpsc::Sender<DownMsg>,
    ) -> Self {
        Self {
            inner: Arc::new(SshSessionInner {
                id: SESSION_IDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                command,
                username,
                peer_addr,
                pty_size,
                up_rx: Mutex::new(Some(up_rx)),
                down_tx: Mutex::new(Some(down_tx.clone())),
                control_tx: Mutex::new(Some(down_tx)),
            }),
        }
    }

    /// Queue an `exit-status` request for the client. Usable before the
    /// pump finishes; after that it bails, so a late send fails loudly
    /// instead of hanging a client that already saw the close.
    pub fn send_exit_status(&self, code: u32) -> Result<()> {
        let slot = self
            .inner
            .control_tx
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let Some(tx) = slot.as_ref() else {
            bail!("SSH session pump already finished");
        };
        tx.blocking_send(crate::state::DownMsg::ExitStatus(code))
            .map_err(|_| anyhow::anyhow!("SSH session wire writer is gone"))?;
        Ok(())
    }

    /// Release the control sender once the byte pump finishes. The wire
    /// writer then observes the drain and closes the channel promptly,
    /// even while the script still holds the session. Idempotent.
    pub fn finish_pump(&self) {
        self.inner
            .control_tx
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
    }

    pub fn command(&self) -> Option<String> {
        self.inner.command.clone()
    }

    pub fn username(&self) -> Option<String> {
        self.inner.username.clone()
    }

    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.inner.peer_addr
    }

    /// Snapshot this session's terminal dimensions.
    pub fn pty_size(&self) -> crate::state::PtySize {
        crate::state::snapshot_pty_size(&self.inner.pty_size).size
    }

    /// Shareable handle to this session's size cell, for its pty pump.
    pub fn pty_size_handle(&self) -> crate::state::SharedPtySize {
        Arc::clone(&self.inner.pty_size)
    }

    /// Take the pump ends for `SSH_PUMP_CHANNEL`. A second pump on the
    /// same session bails instead of splitting bytes across pumps. The
    /// check runs before either take so a failed pump mutates nothing.
    pub fn take_pump_ends(&self) -> Result<(mpsc::Receiver<UpMsg>, mpsc::Sender<DownMsg>)> {
        let mut up = self.inner.up_rx.lock().unwrap_or_else(|p| p.into_inner());
        let mut down = self.inner.down_tx.lock().unwrap_or_else(|p| p.into_inner());
        if up.is_none() || down.is_none() {
            bail!("SSH session already pumped");
        }
        Ok((up.take().unwrap(), down.take().unwrap()))
    }
}

impl PartialEq for SshSessionTag {
    fn eq(&self, other: &Self) -> bool {
        self.inner.id == other.inner.id
    }
}

impl fmt::Display for SshSessionTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let peer = self
            .inner
            .peer_addr
            .map(|addr| addr.to_string())
            .unwrap_or_default();
        match &self.inner.username {
            Some(user) => write!(
                formatter,
                "SSH_SESSION(sess-{} {}@{})",
                self.inner.id, user, peer
            ),
            None => write!(formatter, "SSH_SESSION(sess-{} {})", self.inner.id, peer),
        }
    }
}
