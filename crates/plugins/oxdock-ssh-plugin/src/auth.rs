//! Ephemeral russh server: factory plus per-connection password handler.
//!
//! Authentication checks caller-supplied ephemeral credentials held in
//! memory. No OS users, no PAM, no `sshd_config` are ever consulted.
//! Only password auth is accepted; every other method uses the trait
//! defaults, which reject.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Result, bail};
use bytes::Bytes;
use russh::server::{Auth, Handler, Msg, Session};
use russh::{Channel, ChannelId};
use tokio::sync::mpsc;

use crate::state::{CHANNEL_CAPACITY, DownMsg, PendingSession, SessionQueue, UpMsg};

/// Per-channel wire state on one connection.
struct ChannelState {
    /// Wire bytes toward the pump. Dropped on channel close so the pump
    /// observes EOF by drain.
    up_tx: mpsc::Sender<UpMsg>,
    /// Moved into the queued session once the channel becomes a byte
    /// stream (shell/exec request).
    up_rx: Option<mpsc::Receiver<UpMsg>>,
    /// Pump-to-wire receiver, moved into the wire-writer task at announce.
    down_rx: Option<mpsc::Receiver<DownMsg>>,
    /// Pump-to-wire sender, moved into the queued session at announce.
    down_tx: Option<mpsc::Sender<DownMsg>>,
    /// Whether this channel was already announced to the session queue.
    announced: bool,
}

/// Compare passwords without early exit on content. Lengths still leak
/// through the fast path; russh pads rejections to `auth_rejection_time`
/// regardless.
fn passwords_equal(expected: &str, presented: &str) -> bool {
    let (expected, presented) = (expected.as_bytes(), presented.as_bytes());
    if expected.len() != presented.len() {
        return false;
    }
    expected
        .iter()
        .zip(presented.iter())
        .fold(0u8, |acc, (left, right)| acc | (left ^ right))
        == 0
}

/// Forwards pump bytes to the wire until the pump goes away, then closes
/// the channel. Runs on the runtime thread; never touches sync pipes.
async fn drive_wire(
    mut down_rx: mpsc::Receiver<DownMsg>,
    handle: russh::server::Handle,
    id: ChannelId,
) {
    while let Some(message) = down_rx.recv().await {
        let result = match message {
            DownMsg::Data(bytes) => handle.data(id, bytes).await.map(|_| ()).map_err(|_| ()),
            DownMsg::Eof => handle.eof(id).await,
        };
        if result.is_err() {
            break;
        }
    }
    let _ = handle.close(id).await;
}

/// One handler per client connection, minted by [`ServerFactory`].
pub struct EphemeralHandler {
    expected_user: Arc<String>,
    expected_pass: Arc<String>,
    queue: Arc<SessionQueue>,
    channels: HashMap<ChannelId, ChannelState>,
}

impl Handler for EphemeralHandler {
    type Error = anyhow::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth> {
        if user == self.expected_user.as_str() && passwords_equal(&self.expected_pass, password) {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<()> {
        let (up_tx, up_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (down_tx, down_rx) = mpsc::channel(CHANNEL_CAPACITY);
        self.channels.insert(
            channel.id(),
            ChannelState {
                up_tx,
                up_rx: Some(up_rx),
                down_rx: Some(down_rx),
                down_tx: Some(down_tx),
                announced: false,
            },
        );
        reply.accept().await;
        Ok(())
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<()> {
        self.announce(channel, None, session).await
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        let command = String::from_utf8_lossy(data).into_owned();
        self.announce(channel, Some(command), session).await
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<()> {
        let Some(state) = self.channels.get(&channel) else {
            bail!("data for unknown channel");
        };
        state
            .up_tx
            .send(UpMsg::Data(Bytes::copy_from_slice(data)))
            .await
            .map_err(|_| anyhow::anyhow!("pump went away"))?;
        Ok(())
    }

    /// A byte-pump channel has no process that could produce output
    /// after stdin EOF (unlike a real shell), so the EOF half-closes the
    /// pump instead: signal [`UpMsg::Eof`] and let the cascade drain. The
    /// bounded queue is FIFO, so bytes sent before the EOF still flush
    /// first, and closing eagerly here would win a race against them
    /// (instant-EOF clients like `printf ... | ssh` would lose their
    /// reply). The channel itself closes when the pump finishes and its
    /// wire-writer task ends. If the pump is already gone, close now.
    async fn channel_eof(&mut self, channel: ChannelId, session: &mut Session) -> Result<()> {
        let eof_delivered = match self.channels.get(&channel) {
            Some(state) => state.up_tx.send(UpMsg::Eof).await.is_ok(),
            None => false,
        };
        if !eof_delivered {
            session
                .handle()
                .close(channel)
                .await
                .map_err(|_| anyhow::anyhow!("wire gone during EOF close"))?;
            self.channels.remove(&channel);
        }
        Ok(())
    }

    async fn channel_close(&mut self, channel: ChannelId, _session: &mut Session) -> Result<()> {
        self.channels.remove(&channel);
        Ok(())
    }
}

impl EphemeralHandler {
    /// Publish a channel to the session queue once it becomes a byte
    /// stream, and spawn its wire-writer task. Idempotent per channel:
    /// only the first of shell/exec wins.
    async fn announce(
        &mut self,
        channel: ChannelId,
        exec_command: Option<String>,
        session: &mut Session,
    ) -> Result<()> {
        let Some(state) = self.channels.get_mut(&channel) else {
            bail!("request for unknown channel");
        };
        if state.announced {
            let _ = session.channel_success(channel);
            return Ok(());
        }
        state.announced = true;
        let (Some(up_rx), Some(down_rx), Some(down_tx)) = (
            state.up_rx.take(),
            state.down_rx.take(),
            state.down_tx.take(),
        ) else {
            bail!("channel already announced");
        };
        let handle = session.handle();
        tokio::spawn(drive_wire(down_rx, handle, channel));
        self.queue.push(PendingSession {
            exec_command,
            up_rx,
            down_tx,
        });
        let _ = session.channel_success(channel);
        Ok(())
    }
}

/// Mint [`EphemeralHandler`]s sharing one credential pair and queue.
pub struct ServerFactory {
    expected_user: Arc<String>,
    expected_pass: Arc<String>,
    queue: Arc<SessionQueue>,
}

impl ServerFactory {
    pub fn new(user: String, password: String, queue: Arc<SessionQueue>) -> Self {
        Self {
            expected_user: Arc::new(user),
            expected_pass: Arc::new(password),
            queue,
        }
    }
}

impl russh::server::Server for ServerFactory {
    type Handler = EphemeralHandler;

    fn new_client(&mut self, _peer_addr: Option<SocketAddr>) -> EphemeralHandler {
        EphemeralHandler {
            expected_user: Arc::clone(&self.expected_user),
            expected_pass: Arc::clone(&self.expected_pass),
            queue: Arc::clone(&self.queue),
            channels: HashMap::new(),
        }
    }
}
