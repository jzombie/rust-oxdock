//! Synchronous muxio bridge over stdio byte pipes.
//!
//! One [`Bridge`] wraps an `RpcSession` plus its stream encoders. Emitted
//! frames flow into an `mpsc` queue drained by a transport writer thread;
//! inbound bytes enter through [`read`](Bridge::read), which returns
//! decoded stream events the caller routes per channel. All muxio decode
//! failures convert to `anyhow` errors: malformed wire bytes kill the
//! session (host wins), never panic, never partial-apply.
//!
//! Threading contract: `Bridge` is `Send` but not `Sync`; owners share it
//! as `Arc<Mutex<Bridge>>` across reader, pump, and state-machine threads.
//! Lock discipline is coarse and short: encode or decode, then release.

use std::collections::HashMap;
use std::sync::mpsc;

use anyhow::{Context, Result};
use muxio_core::rpc::rpc_internals::{
    RpcHeader, RpcMessageType, RpcSession, RpcStreamEncoder, RpcStreamEvent,
};
use muxio_core::utils::IdSpace;

use crate::CHUNK_BYTES;

/// Decoded inbound activity on one stream. Streams route by
/// `(method_id, request_id)`: muxio never surfaces raw stream ids on
/// events, and request ids are unique per opened stream on each side.
#[derive(Debug)]
pub enum StreamEvent {
    /// Payload bytes for one stream.
    Chunk {
        method_id: u64,
        request_id: u32,
        bytes: Vec<u8>,
    },
    /// A stream ended normally; buffered channel data is complete.
    End { method_id: u64, request_id: u32 },
    /// Wire-level failure: the session must die.
    Error { detail: String },
}

type EmitFn = Box<dyn FnMut(&[u8]) + Send>;

/// Role selects the muxio id space (client high-bit clear, server set) so
/// host-allocated and guest-allocated stream ids can never collide, plus
/// the message type stamped on opened streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Host,
    Guest,
}

impl Role {
    fn id_space(self) -> IdSpace {
        match self {
            Role::Host => IdSpace::Client,
            Role::Guest => IdSpace::Server,
        }
    }

    fn msg_type(self) -> RpcMessageType {
        match self {
            Role::Host => RpcMessageType::Call,
            Role::Guest => RpcMessageType::Response,
        }
    }
}

/// Framed transport endpoint. Frame bytes leave through `frame_tx`
/// (drained by the owner's writer thread into the pipe); inbound pipe
/// bytes enter through [`read`](Bridge::read).
pub struct Bridge {
    session: RpcSession,
    role: Role,
    frame_tx: mpsc::Sender<Vec<u8>>,
    encoders: HashMap<u32, RpcStreamEncoder<EmitFn>>,
    next_request_id: u32,
}

impl Bridge {
    /// New bridge emitting frames into `frame_tx`. The owner spawns the
    /// writer thread draining that queue into the transport.
    pub fn new(role: Role, frame_tx: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            session: RpcSession::new(role.id_space()),
            role,
            frame_tx,
            encoders: HashMap::new(),
            next_request_id: 1,
        }
    }

    /// Open a stream for `method_id`, writing `tag` plus `first` as the
    /// opening payload. Returns the stream id for [`write`](Bridge::write)
    /// and [`end`](Bridge::end).
    pub fn open(&mut self, method_id: u64, tag: u8, first: &[u8]) -> Result<u32> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        let header = RpcHeader {
            rpc_msg_type: self.role.msg_type(),
            rpc_request_id: request_id,
            rpc_method_id: method_id,
            rpc_metadata_bytes: Vec::new(),
        };
        let tx = self.frame_tx.clone();
        let emit: EmitFn = Box::new(move |bytes: &[u8]| {
            let _ = tx.send(bytes.to_vec());
        });
        let mut encoder = self
            .session
            .init_request(header, CHUNK_BYTES, emit)
            .context("muxio init_request failed")?;
        let stream_id = encoder.stream_id();
        let mut opening = Vec::with_capacity(1 + first.len());
        opening.push(tag);
        opening.extend_from_slice(first);
        encoder
            .write_bytes(&opening)
            .context("muxio opening write failed")?;
        encoder.flush().context("muxio flush failed")?;
        self.encoders.insert(stream_id, encoder);
        Ok(stream_id)
    }

    /// Append raw payload bytes to an open stream.
    pub fn write(&mut self, stream_id: u32, bytes: &[u8]) -> Result<()> {
        let Some(encoder) = self.encoders.get_mut(&stream_id) else {
            anyhow::bail!("write to unknown muxio stream {stream_id}");
        };
        encoder
            .write_bytes(bytes)
            .context("muxio write failed")?;
        encoder.flush().context("muxio flush failed")?;
        Ok(())
    }

    /// End a stream normally and forget its encoder.
    pub fn end(&mut self, stream_id: u32) -> Result<()> {
        let Some(mut encoder) = self.encoders.remove(&stream_id) else {
            anyhow::bail!("end of unknown muxio stream {stream_id}");
        };
        encoder
            .end_stream()
            .context("muxio end_stream failed")?;
        Ok(())
    }

    /// Cancel a stream immediately (cancel/timeout/partition path).
    pub fn cancel(&mut self, stream_id: u32) -> Result<()> {
        let Some(mut encoder) = self.encoders.remove(&stream_id) else {
            return Ok(());
        };
        encoder
            .cancel_stream()
            .context("muxio cancel_stream failed")?;
        Ok(())
    }

    /// Feed inbound transport bytes through the decoder, collecting stream
    /// events. `Err` means the session is dead: the caller tears down the
    /// transport (host wins; guest discards partial state).
    pub fn read(&mut self, input: &[u8]) -> Result<Vec<StreamEvent>> {
        let mut events = Vec::new();
        self.session
            .read_bytes(input, |event| {
                match event {
                    RpcStreamEvent::Header { .. } => {}
                    RpcStreamEvent::PayloadChunk {
                        rpc_method_id,
                        rpc_request_id,
                        bytes,
                        ..
                    } => {
                        events.push(StreamEvent::Chunk {
                            method_id: rpc_method_id,
                            request_id: rpc_request_id,
                            bytes,
                        });
                    }
                    RpcStreamEvent::End {
                        rpc_method_id,
                        rpc_request_id,
                        ..
                    } => {
                        events.push(StreamEvent::End {
                            method_id: rpc_method_id,
                            request_id: rpc_request_id,
                        });
                    }
                    RpcStreamEvent::Error {
                        frame_decode_error,
                        ..
                    } => {
                        events.push(StreamEvent::Error {
                            detail: format!("{frame_decode_error:?}"),
                        });
                    }
                }
                Ok(())
            })
            .context("muxio session decode failed")?;
        Ok(events)
    }
}
