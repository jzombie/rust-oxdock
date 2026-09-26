//! Stdio transport session behind `REMOTE` blocks: any command yielding a
//! bidirectional pipe to an `oxdock --remote-serve` guest (ssh, a local
//! binary, `docker exec -i`, `kubectl exec -i`, `wsl`). Muxio frames the
//! multiplexed channels over the child stdio; the child stderr stays
//! inherited so transport diagnostics reach host stderr directly.
//!
//! Lifecycle is one block, one process: lazy spawn on block entry, scoped
//! teardown on exit (streams ended, pumps joined, child killed on drop).
//! No pooling, no shared remote workspace. Host wins every failure: any
//! transport error, EOF, digest mismatch, or cancellation fails the step
//! before anything is applied.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use oxdock_core::{RemoteRequest, RemoteResponse, RemoteRunner};
use oxdock_process::{ChildHandle, CommandBuilder, PipedChild};
use oxdock_remote_proto::bridge::{Bridge, Role, StreamEvent};
use oxdock_remote_proto::{channel, methods, tar, types};
use sha2::{Digest, Sha256};

/// Pump tick for cancellation checks and stdin drain backstops.
const TICK: Duration = Duration::from_millis(10);

/// Host identity bundle for the handshake. Built once at CLI startup from
/// the compiled binary plus the engine module table. Module names are
/// data from the registered modules, never a cfg chain, so downstream
/// libraries surface automatically.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Tokenized transport command; `--remote-serve` is appended at spawn.
    pub argv: Vec<String>,
    /// `CARGO_PKG_VERSION` of the host binary, diagnostics only.
    pub oxdock_version: String,
    /// Sorted registered module names, diagnostics only.
    pub modules: Vec<String>,
    /// sha256 over the sorted module table (modules plus function sets):
    /// language-surface identity. Agreement by construction.
    pub module_hash: String,
}

/// Canonical module-table hash: sorted `module::function` entries over
/// sha256. Both ends compute the identical string from their own engine,
/// so any language-surface drift fails the handshake.
pub fn module_hash(entries: &[(String, Vec<String>)]) -> String {
    use sha2::{Digest, Sha256};
    let mut sorted: Vec<(String, Vec<String>)> = entries.to_vec();
    sorted.sort();
    let mut hasher = Sha256::new();
    for (module, mut functions) in sorted {
        functions.sort();
        for function in functions {
            hasher.update(module.as_bytes());
            hasher.update(b"::");
            hasher.update(function.as_bytes());
            hasher.update(b"\n");
        }
    }
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// One `REMOTE` block execution over a fresh transport process.
pub struct StdioSession {
    config: SessionConfig,
}

impl StdioSession {
    pub fn new(config: SessionConfig) -> Self {
        Self { config }
    }

    fn spawn(&self) -> Result<PipedChild> {
        if self.config.argv.is_empty() {
            bail!("REMOTE transport command is empty");
        }
        let mut builder = CommandBuilder::new(&self.config.argv[0]);
        if self.config.argv.len() > 1 {
            builder.args(&self.config.argv[1..]);
        }
        builder.arg("--remote-serve");
        builder
            .spawn_piped()
            .context("REMOTE transport spawn failed")
    }
}

impl RemoteRunner for StdioSession {
    fn run_remote(&self, request: RemoteRequest) -> Result<RemoteResponse> {
        let child = self.spawn()?;
        let session = ActiveSession::new(child, &request)?;
        session.run(request, &self.config)
    }
}

/// Live session state machine: pumps plus protocol progress on the calling
/// thread. The child handle rides along so every exit path, including
/// early bail, kills the transport through [`ChildHandle`] drop glue.
struct ActiveSession {
    // Scoped teardown glue: never read, dropped last so the transport dies
    // on every exit path including early bail.
    #[allow(dead_code)]
    handle: ChildHandle,
    bridge: Arc<Mutex<Bridge>>,
    incoming: mpsc::Receiver<PipeRead>,
    cancelled: Arc<AtomicBool>,
    dead: Arc<AtomicBool>,
    stdin_pump: Option<std::thread::JoinHandle<()>>,
    stdout_backend: Option<Arc<oxdock_pipe::PipeInner>>,
    stderr_sink: Option<oxdock_process::SharedOutput>,
    stdout_accum: Vec<u8>,
    stdout_hash: Sha256,
    stderr_accum: Vec<u8>,
    channels: HashMap<(u64, u32), ChannelBuffer>,
}

enum PipeRead {
    Bytes(Vec<u8>),
    Eof,
    Error(String),
}

struct ChannelBuffer {
    tag: Option<u8>,
    bytes: Vec<u8>,
    ended: bool,
}

impl Drop for ActiveSession {
    fn drop(&mut self) {
        self.dead.store(true, Ordering::SeqCst);
    }
}

impl ActiveSession {
    fn new(child: PipedChild, request: &RemoteRequest) -> Result<Self> {
        let PipedChild {
            handle,
            stdin: child_stdin,
            stdout: child_stdout,
        } = child;
        let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>();
        let bridge = Arc::new(Mutex::new(Bridge::new(Role::Host, frame_tx)));
        let (incoming_tx, incoming) = mpsc::channel::<PipeRead>();

        let mut stdin = child_stdin;
        std::thread::Builder::new()
            .name("oxdock-remote-frame-writer".to_string())
            .spawn(move || {
                for frame in frame_rx {
                    if stdin.write_all(&frame).is_err() {
                        break;
                    }
                }
                let _ = stdin.flush();
            })
            .context("REMOTE writer thread spawn failed")?;

        let mut stdout = child_stdout;
        std::thread::Builder::new()
            .name("oxdock-remote-frame-reader".to_string())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) => {
                            let _ = incoming_tx.send(PipeRead::Eof);
                            break;
                        }
                        Ok(n) => {
                            if incoming_tx.send(PipeRead::Bytes(buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            let _ = incoming_tx.send(PipeRead::Error(err.to_string()));
                            break;
                        }
                    }
                }
            })
            .context("REMOTE reader thread spawn failed")?;

        Ok(Self {
            handle,
            bridge,
            incoming,
            cancelled: Arc::clone(&request.cancelled),
            dead: Arc::new(AtomicBool::new(false)),
            stdin_pump: None,
            stdout_backend: request.stdout.clone(),
            stderr_sink: request.stderr_sink.clone(),
            stdout_accum: Vec::new(),
            stdout_hash: Sha256::new(),
            stderr_accum: Vec::new(),
            channels: HashMap::new(),
        })
    }

    fn check_cancelled(&self) -> Result<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            bail!("REMOTE block cancelled");
        }
        Ok(())
    }

    fn bridge_open(&self, method_id: u64, tag: u8, first: &[u8]) -> Result<u32> {
        self.bridge
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .open(method_id, tag, first)
    }

    fn bridge_end(&self, stream: u32) -> Result<()> {
        self.bridge
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .end(stream)
    }

    fn route_stdout(&mut self, bytes: &[u8]) -> Result<()> {
        self.stdout_hash.update(bytes);
        if let Some(backend) = &self.stdout_backend {
            let writer = backend.writer_handle();
            if let Ok(mut guard) = writer.lock() {
                guard.write_all(bytes)?;
                guard.flush()?;
            }
        } else {
            self.stdout_accum.extend_from_slice(bytes);
        }
        Ok(())
    }

    fn route_stderr(&mut self, bytes: &[u8]) -> Result<()> {
        if let Some(sink) = &self.stderr_sink {
            if let Ok(mut guard) = sink.lock() {
                guard.write_all(bytes)?;
                guard.flush()?;
            }
        } else {
            self.stderr_accum.extend_from_slice(bytes);
        }
        Ok(())
    }

    fn feed(&mut self, raw: &[u8]) -> Result<()> {
        let events = self
            .bridge
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .read(raw)?;
        for event in events {
            match event {
                StreamEvent::Chunk {
                    method_id,
                    request_id,
                    bytes,
                } => self.on_chunk(method_id, request_id, &bytes)?,
                StreamEvent::End {
                    method_id,
                    request_id,
                } => self.on_end(method_id, request_id)?,
                StreamEvent::Error { detail } => {
                    bail!("REMOTE wire error: {detail}");
                }
            }
        }
        Ok(())
    }

    fn on_chunk(&mut self, method_id: u64, request_id: u32, bytes: &[u8]) -> Result<()> {
        let key = (method_id, request_id);
        let buffer = self.channels.entry(key).or_insert(ChannelBuffer {
            tag: None,
            bytes: Vec::new(),
            ended: false,
        });
        let mut cursor = bytes;
        if buffer.tag.is_none() {
            if cursor.is_empty() {
                return Ok(());
            }
            buffer.tag = Some(cursor[0]);
            cursor = &cursor[1..];
        }
        // Stdio channels stream straight through; control channels buffer
        // until End (chunk boundaries are never significant). Stdout and
        // stderr both flow live when sinks exist, so large outputs never
        // accumulate unbounded on either end.
        if method_id == methods::EXEC && buffer.tag == Some(channel::TAG_STDOUT) {
            self.route_stdout(cursor)?;
        } else if method_id == methods::EXEC && buffer.tag == Some(channel::TAG_STDERR) {
            self.route_stderr(cursor)?;
        } else {
            buffer.bytes.extend_from_slice(cursor);
        }
        Ok(())
    }

    fn on_end(&mut self, method_id: u64, request_id: u32) -> Result<()> {
        if let Some(buffer) = self.channels.get_mut(&(method_id, request_id)) {
            buffer.ended = true;
        }
        Ok(())
    }

    fn take_channel(&mut self, method_id: u64, tag: u8) -> Option<Vec<u8>> {
        let key = self
            .channels
            .iter()
            .find(|((method, _), buffer)| {
                *method == method_id && buffer.tag == Some(tag) && buffer.ended
            })
            .map(|(key, _)| *key)?;
        self.channels.remove(&key).map(|buffer| buffer.bytes)
    }

    /// Block until a control channel completes, pumping transport bytes.
    /// EOF or transport errors fail fast; cancellation aborts promptly.
    fn await_channel(&mut self, method_id: u64, tag: u8, what: &str) -> Result<Vec<u8>> {
        loop {
            if let Some(bytes) = self.take_channel(method_id, tag) {
                return Ok(bytes);
            }
            self.check_cancelled()?;
            match self.incoming.recv_timeout(TICK) {
                Ok(PipeRead::Bytes(raw)) => self.feed(&raw)?,
                Ok(PipeRead::Eof) => {
                    // Drain any frames already decoded before dying.
                    if let Some(bytes) = self.take_channel(method_id, tag) {
                        return Ok(bytes);
                    }
                    bail!("REMOTE transport closed before {what} completed");
                }
                Ok(PipeRead::Error(detail)) => {
                    bail!("REMOTE transport read failed before {what} completed: {detail}");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("REMOTE transport reader died before {what} completed");
                }
            }
        }
    }

    fn spawn_stdin_pump(&mut self, backend: Arc<oxdock_pipe::PipeInner>, stream: u32) {
        let bridge = Arc::clone(&self.bridge);
        let cancelled = Arc::clone(&self.cancelled);
        let dead = Arc::clone(&self.dead);
        let pump = std::thread::Builder::new()
            .name("oxdock-remote-stdin-pump".to_string())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    if cancelled.load(Ordering::SeqCst) || dead.load(Ordering::SeqCst) {
                        break;
                    }
                    match backend.read_into_timeout(&mut buf, TICK) {
                        Ok(Some(0)) | Ok(None) if dead.load(Ordering::SeqCst) => break,
                        Ok(Some(0)) => {
                            // EOF: writers detached. End the guest stdin.
                            let _ = bridge
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .end(stream);
                            break;
                        }
                        Ok(None) => {}
                        Ok(Some(n)) => {
                            let done = bridge
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .write(stream, &buf[..n])
                                .is_err();
                            if done {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        if let Ok(pump) = pump {
            self.stdin_pump = Some(pump);
        }
    }

    fn run(mut self, request: RemoteRequest, config: &SessionConfig) -> Result<RemoteResponse> {
        let outcome = self.run_inner(&request, config);
        // Scoped teardown on every path: stop the stdin pump, close the
        // session, join the pump. The child dies with `self` through
        // `ChildHandle` drop glue.
        self.dead.store(true, Ordering::SeqCst);
        let _ = self
            .bridge
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .open(methods::CLOSE, channel::TAG_CLOSE, &[]);
        if let Some(pump) = self.stdin_pump.take() {
            let _ = pump.join();
        }
        outcome
    }

    fn run_inner(
        &mut self,
        request: &RemoteRequest,
        config: &SessionConfig,
    ) -> Result<RemoteResponse> {
        // Handshake first: hello out, verdict back, verified before any
        // script byte ships.
        let hello = types::HandshakeHello {
            proto_digest: oxdock_remote_proto::PROTO_DIGEST.to_string(),
            oxdock_version: config.oxdock_version.clone(),
            modules: config.modules.clone(),
            module_hash: config.module_hash.clone(),
        };
        let hello_bytes =
            serde_json::to_vec(&hello).context("REMOTE hello encode failed")?;
        let hello_stream = self.bridge_open(methods::HANDSHAKE, channel::TAG_HELLO, &hello_bytes)?;
        self.bridge_end(hello_stream)?;
        let verdict_bytes =
            self.await_channel(methods::HANDSHAKE, channel::TAG_VERDICT, "handshake")?;
        let verdict: types::HandshakeVerdict =
            serde_json::from_slice(&verdict_bytes).context("REMOTE verdict decode failed")?;
        if !verdict.accepted {
            bail!(
                "REMOTE target '{}' rejected the handshake: {}",
                request.target,
                verdict.error.unwrap_or_else(|| "no reason given".to_string())
            );
        }
        if verdict.proto_digest != *oxdock_remote_proto::PROTO_DIGEST {
            bail!(
                "REMOTE target '{}' protocol digest mismatch (host {} guest {}): rebuild and reinstall matching oxdock on the guest",
                request.target,
                oxdock_remote_proto::PROTO_DIGEST.as_str(),
                verdict.proto_digest
            );
        }
        if verdict.module_hash != config.module_hash {
            bail!(
                "REMOTE target '{}' language surface mismatch (host {} guest {}): rebuild and reinstall matching oxdock on the guest",
                request.target,
                config.module_hash,
                verdict.module_hash
            );
        }

        // Declared fetch in: pack the host-resolved entries (named by
        // guest-dest paths) and stream descriptor plus tarball. Always
        // opened, even empty, so the guest never waits on a missing
        // channel.
        let sync_files: Vec<tar::SyncFile> = request
            .fetch_files
            .iter()
            .map(|(path, bytes)| tar::SyncFile {
                path: path.clone(),
                bytes: bytes.clone(),
            })
            .collect();
        let (fetch_bytes, descriptor) = tar::pack(&sync_files, &request.fetch_dirs)?;
        let descriptor_json =
            serde_json::to_vec(&descriptor).context("REMOTE fetch descriptor encode failed")?;
        let fetch_frame = channel::pack_prefixed(&descriptor_json, &fetch_bytes);
        let fetch_stream = self.bridge_open(methods::EXEC, channel::TAG_FETCH, &fetch_frame)?;
        self.bridge_end(fetch_stream)?;

        // Script text, then stdin (pumped live when bound, EOF now when not).
        let script_stream =
            self.bridge_open(methods::EXEC, channel::TAG_SCRIPT, request.script_text.as_bytes())?;
        self.bridge_end(script_stream)?;
        let stdin_stream = self.bridge_open(methods::EXEC, channel::TAG_STDIN, &[])?;
        match &request.stdin {
            Some(backend) => self.spawn_stdin_pump(Arc::clone(backend), stdin_stream),
            None => self.bridge_end(stdin_stream)?,
        }

        // Result: prefixed result JSON plus push tar bytes. The tar
        // entries are guest-source paths; each must match a declared
        // `--to-host` entry (exact file or directory prefix), else the
        // guest over-shared and the session aborts.
        let result_frame =
            self.await_channel(methods::EXEC, channel::TAG_RESULT, "remote execution")?;
        let Some((result_json, push_tar)) = channel::split_prefixed(&result_frame) else {
            bail!("REMOTE target '{}' sent a malformed result frame", request.target);
        };
        let result: types::ExecResult =
            serde_json::from_slice(result_json).context("REMOTE result decode failed")?;
        if !result.ok {
            bail!(
                "REMOTE target '{}' failed: {}",
                request.target,
                result.error.unwrap_or_else(|| "no reason given".to_string())
            );
        }
        // End-to-end integrity over the bytes each side actually saw.
        let script_hash = sha256_hex(request.script_text.as_bytes());
        if result.script_sha256 != script_hash {
            bail!(
                "REMOTE target '{}' executed different script bytes than shipped (transport corruption)",
                request.target
            );
        }
        let stdout_hash = {
            use std::fmt::Write as _;
            let digest = std::mem::replace(&mut self.stdout_hash, Sha256::new());
            let mut out = String::with_capacity(64);
            for byte in digest.finalize() {
                let _ = write!(out, "{byte:02x}");
            }
            out
        };
        if result.stdout_sha256 != stdout_hash {
            bail!(
                "REMOTE target '{}' stdout hash mismatch (transport corruption)",
                request.target
            );
        }
        tar::verify_sha256(push_tar, &result.result_tar_sha256)?;
        let unpacked = tar::unpack(push_tar)?;
        let mut push_files = Vec::new();
        let mut push_dirs = Vec::new();
        let mut push_symlinks = Vec::new();
        for dir in unpacked.dirs {
            push_dirs.push(map_push_path(&request.push_decls, &dir).ok_or_else(|| {
                anyhow::anyhow!(
                    "REMOTE target '{}' returned undeclared path {dir:?}",
                    request.target
                )
            })?);
        }
        for entry in unpacked.files {
            let host_rel = map_push_path(&request.push_decls, &entry.path).ok_or_else(|| {
                anyhow::anyhow!(
                    "REMOTE target '{}' returned undeclared path {:?}",
                    request.target,
                    entry.path
                )
            })?;
            push_files.push((host_rel, entry.bytes));
        }
        for link in unpacked.symlinks {
            let host_rel = map_push_path(&request.push_decls, &link.path).ok_or_else(|| {
                anyhow::anyhow!(
                    "REMOTE target '{}' returned undeclared path {:?}",
                    request.target,
                    link.path
                )
            })?;
            push_symlinks.push((host_rel, link.target));
        }
        // Bytes returned when no backend streams them; empty otherwise
        // (core appends these to whatever the backend already received).
        let stdout_bytes = if self.stdout_backend.is_some() {
            Vec::new()
        } else {
            std::mem::take(&mut self.stdout_accum)
        };
        Ok(RemoteResponse {
            stdout_bytes,
            stderr_bytes: std::mem::take(&mut self.stderr_accum),
            push_files,
            push_dirs,
            push_symlinks,
        })
    }
}

/// Map one guest-source path from a push tarball to its declared host
/// destination: exact file match, or directory-prefix fan-out. `None`
/// means the guest sent an undeclared path. Both sides normalize leading
/// `./` first: declarations may spell `./out.txt` while tar entries
/// arrive canonicalized as `out.txt`.
fn map_push_path(decls: &[(String, String)], guest_path: &str) -> Option<String> {
    let guest_path = normalize_push_path(guest_path);
    for (guest_src, host_dst) in decls {
        let guest_src = normalize_push_path(guest_src);
        if guest_path == guest_src {
            return Some(host_dst.clone());
        }
        if let Some(rest) = guest_path.strip_prefix(guest_src.as_str())
            && rest.starts_with('/')
        {
            return Some(format!("{host_dst}{rest}"));
        }
    }
    None
}

fn normalize_push_path(path: &str) -> String {
    let mut rest = path;
    while let Some(stripped) = rest.strip_prefix("./") {
        rest = stripped;
    }
    rest.to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    oxdock_remote_proto::sha256_hex(bytes)
}
