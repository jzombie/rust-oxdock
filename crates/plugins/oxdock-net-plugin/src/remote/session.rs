//! Sync transport session behind `REMOTE` blocks: any command yielding a
//! bidirectional pipe to an `oxdock --remote-serve` guest (ssh, a local
//! binary, `docker exec -i`, `kubectl exec -i`, `wsl`). Framing and call
//! correlation ride muxio's sync client (`RpcSyncClient`); OxDock
//! owns only its three-method protocol on top. The child stderr stays
//! inherited so transport diagnostics reach host stderr directly.
//!
//! Lifecycle is one block, one process: lazy spawn on block entry, scoped
//! teardown on exit (streams ended, pumps joined, child killed on drop).
//! No pooling, no shared remote workspace. Host wins every failure: any
//! transport error, EOF, digest mismatch, or cancellation fails the step
//! before anything is applied.
//!
//! Wire layout: handshake is one streaming call carrying the hello JSON
//! with the verdict as its single response; exec is one streaming call
//! whose request sections are `[prefixed descriptor+tar][prefixed
//! script][stdin raw until end]` and whose response items are
//! single-tag chunks (`TAG_STDOUT`, `TAG_STDERR`, `TAG_RESULT` +
//! prefixed result, accumulated until the response stream ends).
//! Declared transfers stream straight between disk and wire with O(chunk)
//! memory through spill staging: no transfer size cap exists anywhere.

use std::io::{Read, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use muxio_core::rpc::RpcRequest;
use muxio_core::rpc::rpc_internals::{RpcStreamEncoder, rpc_trait::RpcEmit};
use muxio_rpc_service_caller::RpcServiceCallerInterface;
use muxio_rpc_service_caller::dynamic_channel::{DynamicChannelType, DynamicReceiver};
use muxio_sync_rpc_client::RpcSyncClient;
use oxdock_core::{RemoteRequest, RemoteResponse, RemoteRunner};
use oxdock_fs::{GuardedPath, PathResolver, WorkspaceFs};
use oxdock_remote_proto::{channel, methods, tar, types};
use sha2::Digest;

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

type Encoder = RpcStreamEncoder<Box<dyn RpcEmit + Send + Sync>>;

/// One `REMOTE` block execution over a fresh transport process.
pub struct StdioSession {
    config: SessionConfig,
}

impl StdioSession {
    pub fn new(config: SessionConfig) -> Self {
        Self { config }
    }
}

impl RemoteRunner for StdioSession {
    fn run_remote(&self, request: RemoteRequest, fs: &dyn WorkspaceFs) -> Result<RemoteResponse> {
        if self.config.argv.is_empty() {
            bail!("REMOTE transport command is empty");
        }
        let mut argv: Vec<&str> = self.config.argv.iter().map(String::as_str).collect();
        let program = argv.remove(0);
        let mut full_args: Vec<&str> = argv;
        full_args.push("--remote-serve");
        let client =
            RpcSyncClient::spawn(program, &full_args).context("REMOTE transport spawn failed")?;
        let session = MuxioSession::new(client, &request)?;
        session.run(request, fs, &self.config)
    }
}

/// Counter for unique spill names within one session: `create_spill_file`
/// fails when the name exists, so every stage gets its own.
static SPILL_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Live session driver: muxio streaming calls plus a stdin pump thread on
/// the calling thread. Dropping the client kills the transport on every
/// exit path, including early bail.
struct MuxioSession {
    client: Arc<RpcSyncClient>,
    cancelled: Arc<AtomicBool>,
    dead: Arc<AtomicBool>,
    stdin_pump: Option<std::thread::JoinHandle<()>>,
    stdout_backend: Option<Arc<oxdock_pipe::PipeInner>>,
    stderr_sink: Option<oxdock_process::SharedOutput>,
    stdout_accum: Vec<u8>,
    stdout_hash: sha2::Sha256,
    stderr_accum: Vec<u8>,
    /// Staged push tarball: result-section bytes stream here with an
    /// incremental hash, so no result size ever sits fully in memory.
    result_stage: Option<oxdock_core::TransferStage>,
    result_hash: Option<sha2::Sha256>,
    result_json: Option<Vec<u8>>,
    result_head: Vec<u8>,
}

impl MuxioSession {
    fn new(client: Arc<RpcSyncClient>, request: &RemoteRequest) -> Result<Self> {
        Ok(Self {
            client,
            cancelled: Arc::clone(&request.cancelled),
            dead: Arc::new(AtomicBool::new(false)),
            stdin_pump: None,
            stdout_backend: request.stdout.clone(),
            stderr_sink: request.stderr_sink.clone(),
            stdout_accum: Vec::new(),
            stdout_hash: sha2::Sha256::new(),
            stderr_accum: Vec::new(),
            result_stage: None,
            result_hash: None,
            result_json: None,
            result_head: Vec::new(),
        })
    }

    fn check_cancelled(&self) -> Result<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            bail!("REMOTE block cancelled");
        }
        Ok(())
    }

    /// One streaming call: open, write the full payload, end, and collect
    /// every response item until the response stream ends.
    fn roundtrip(&self, method_id: u64, payload: &[u8]) -> Result<Vec<Vec<u8>>> {
        let request = RpcRequest {
            rpc_method_id: method_id,
            rpc_param_bytes: None,
            rpc_prebuffered_payload_bytes: None,
            is_finalized: false,
        };
        let (mut encoder, receiver) = futures_executor::block_on(
            self.client
                .call_rpc_streaming(request, DynamicChannelType::Unbounded),
        )
        .context("REMOTE streaming call failed")?;
        write_all(&mut encoder, payload)?;
        encoder.end_stream().context("REMOTE end_stream failed")?;
        let mut items = Vec::new();
        let mut receiver = receiver;
        while let Some(item) = futures_executor::block_on(receiver.next()) {
            items.push(item.context("REMOTE response item failed")?);
        }
        Ok(items)
    }

    /// Open a streaming call for incremental writes (exec, stdin pump).
    /// The caller owns chunking, ending, and response collection.
    fn open_stream(&self, method_id: u64) -> Result<(Arc<Mutex<Encoder>>, DynamicReceiver)> {
        let request = RpcRequest {
            rpc_method_id: method_id,
            rpc_param_bytes: None,
            rpc_prebuffered_payload_bytes: None,
            is_finalized: false,
        };
        let (encoder, receiver) = futures_executor::block_on(
            self.client
                .call_rpc_streaming(request, DynamicChannelType::Unbounded),
        )
        .context("REMOTE streaming call failed")?;
        Ok((Arc::new(Mutex::new(encoder)), receiver))
    }

    fn route_stdout(&mut self, bytes: &[u8]) -> Result<()> {
        use sha2::Digest;
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
        // Stderr streams live when the step error handle is a shared
        // writer; otherwise it accumulates for the response return.
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

    fn on_response_item(&mut self, item: &[u8]) -> Result<()> {
        if item.is_empty() {
            return Ok(());
        }
        match item[0] {
            tag if tag == channel::TAG_STDOUT => self.route_stdout(&item[1..]),
            tag if tag == channel::TAG_STDERR => self.route_stderr(&item[1..]),
            tag if tag == channel::TAG_RESULT => self.route_result(&item[1..]),
            tag => bail!("REMOTE target sent unknown response tag {tag}"),
        }
    }

    /// Route result-section bytes: the prefixed result JSON parses once
    /// its frame completes; tar bytes stream straight into the staged
    /// spill with an incremental hash. The stage is created up front in
    /// `run_inner`, so absence here is a bug.
    fn route_result(&mut self, bytes: &[u8]) -> Result<()> {
        if self.result_json.is_none() {
            self.result_head.extend_from_slice(bytes);
            if let Some((json, rest)) = channel::split_prefixed(&self.result_head) {
                self.result_json = Some(json.to_vec());
                let rest = rest.to_vec();
                self.result_head.clear();
                if !rest.is_empty() {
                    self.write_result_tar(&rest)?;
                }
            }
            return Ok(());
        }
        self.write_result_tar(bytes)
    }

    fn write_result_tar(&mut self, bytes: &[u8]) -> Result<()> {
        use sha2::Digest;
        let Some(stage) = self.result_stage.as_mut() else {
            bail!("REMOTE result arrived with no staged tarball");
        };
        stage.write_all(bytes)?;
        if let Some(hash) = self.result_hash.as_mut() {
            hash.update(bytes);
        }
        Ok(())
    }

    fn spawn_stdin_pump(
        &mut self,
        backend: Arc<oxdock_pipe::PipeInner>,
        encoder: Arc<Mutex<Encoder>>,
    ) {
        let cancelled = Arc::clone(&self.cancelled);
        let dead = Arc::clone(&self.dead);
        let pump = std::thread::Builder::new()
            .name("oxdock-remote-stdin-pump".to_string())
            .spawn(move || {
                let mut buf = [0u8; 65536];
                loop {
                    if cancelled.load(Ordering::SeqCst) || dead.load(Ordering::SeqCst) {
                        break;
                    }
                    match backend.read_into_timeout(&mut buf, TICK) {
                        Ok(Some(0)) | Ok(None) if dead.load(Ordering::SeqCst) => break,
                        Ok(Some(0)) => {
                            // EOF: writers detached. End the guest stdin.
                            let _ = encoder
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .end_stream();
                            break;
                        }
                        Ok(None) => {}
                        Ok(Some(n)) => {
                            let done = encoder
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .write_bytes(&buf[..n])
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

    /// Stage declared fetch entries to a spill tarball, streaming file
    /// content straight from disk with O(chunk) memory. Returns the spill
    /// (rewound for reading), its sha, byte length, and descriptor.
    fn stage_fetch(
        request: &RemoteRequest,
        fs: &dyn WorkspaceFs,
        scratch: &TransferScratch,
    ) -> Result<(
        oxdock_core::TransferStage,
        String,
        u64,
        types::TarDescriptor,
    )> {
        let mut entries: Vec<(String, u64, Box<dyn Read + Send>)> = Vec::new();
        for (guest_rel, host_path) in &request.fetch_files {
            let len = fs.metadata(host_path).map(|m| m.len()).with_context(|| {
                format!("REMOTE fetch source unreadable: {}", host_path.display())
            })?;
            entries.push((
                guest_rel.clone(),
                len,
                fs.open_read(host_path)? as Box<dyn Read + Send>,
            ));
        }
        let mut stage = scratch.stage("fetch.tar.gz")?;
        let mut hashing = tar::HashingWriter::new(&mut stage);
        let count = tar::pack_to(&mut hashing, &mut entries, &request.fetch_dirs)?;
        hashing.flush()?;
        drop(hashing);
        stage.rewind()?;
        let (sha, len) = scratch.hash_and_len(&mut stage)?;
        stage.rewind()?;
        Ok((
            stage,
            sha,
            len,
            types::TarDescriptor {
                sha256: String::new(),
                entry_count: count,
                tar_len: len,
            },
        ))
    }

    fn run(
        mut self,
        request: RemoteRequest,
        fs: &dyn WorkspaceFs,
        config: &SessionConfig,
    ) -> Result<RemoteResponse> {
        let outcome = self.run_inner(&request, fs, config);
        // Scoped teardown on every path: stop the stdin pump, join it.
        // Dropping the client kills the child.
        self.dead.store(true, Ordering::SeqCst);
        if let Some(pump) = self.stdin_pump.take() {
            let _ = pump.join();
        }
        outcome
    }

    #[allow(clippy::too_many_lines)]
    fn run_inner(
        &mut self,
        request: &RemoteRequest,
        fs: &dyn WorkspaceFs,
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
        let hello_bytes = serde_json::to_vec(&hello).context("REMOTE hello encode failed")?;
        let mut verdict_bytes = Vec::new();
        for item in self.roundtrip(methods::HANDSHAKE, &hello_bytes)? {
            verdict_bytes.extend_from_slice(&item);
        }
        let verdict: types::HandshakeVerdict =
            serde_json::from_slice(&verdict_bytes).context("REMOTE verdict decode failed")?;
        if !verdict.accepted {
            bail!(
                "REMOTE target '{}' rejected the handshake: {}",
                request.target,
                verdict
                    .error
                    .unwrap_or_else(|| "no reason given".to_string())
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

        // Declared fetch in: stage to spill, then stream the staged
        // tarball in chunks inside the exec request sections.
        let scratch = TransferScratch::new()?;
        let (mut fetch_stage, fetch_sha, _fetch_len, mut descriptor) =
            Self::stage_fetch(request, fs, &scratch)?;
        descriptor.sha256.clone_from(&fetch_sha);
        let descriptor_json =
            serde_json::to_vec(&descriptor).context("REMOTE fetch descriptor encode failed")?;

        // Exec call: fetch section, script section, then live stdin.
        let (encoder, receiver) = self.open_stream(methods::EXEC)?;
        {
            let mut guard = encoder.lock().unwrap_or_else(|poison| poison.into_inner());
            // Fetch section: prefixed descriptor, then tar_len raw bytes
            // streamed from spill.
            let mut frame = channel::pack_prefixed(&descriptor_json, &[]);
            // Descriptor carries tar_len; tar bytes follow raw.
            write_all(&mut guard, &frame)?;
            let mut chunk = [0u8; 65536];
            loop {
                let n = fetch_stage.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                write_all(&mut guard, &chunk[..n])?;
            }
            // Script section: prefixed bytes.
            frame = channel::pack_bytes(request.script_text.as_bytes());
            write_all(&mut guard, &frame)?;
        }
        match &request.stdin {
            Some(backend) => self.spawn_stdin_pump(Arc::clone(backend), Arc::clone(&encoder)),
            None => {
                // No stdin: end the request stream now (script section is
                // the last structural part; the guest treats stream end as
                // stdin EOF).
                encoder
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .end_stream()
                    .context("REMOTE stdin end failed")?;
            }
        }

        // Response loop on the calling thread; cancellation is supervised
        // between items by run(). The result stage is created up front so
        // result bytes stream straight to spill from the first item.
        let result_scratch = TransferScratch::new()?;
        let result_stage = result_scratch.stage("push.tar.gz")?;
        self.result_stage = Some(result_stage);
        self.result_hash = Some(sha2::Sha256::new());
        let mut receiver = receiver;
        loop {
            self.check_cancelled()?;
            let next = futures_executor::block_on(receiver.next());
            match next {
                Some(Ok(item)) => self.on_response_item(&item)?,
                Some(Err(err)) => {
                    bail!("REMOTE response failed: {err:#}");
                }
                None => {
                    return self.finish_exec(request, fs, result_scratch);
                }
            }
        }
    }

    /// Assemble the response once the response stream ends: parse the
    /// result JSON, verify echo hashes, then verify the staged push
    /// tarball hash and stream-apply it straight to the host filesystem.
    /// The tarball never sits fully in memory: it rode the spill from
    /// the first result item.
    fn finish_exec(
        &mut self,
        request: &RemoteRequest,
        fs: &dyn WorkspaceFs,
        _result_scratch: TransferScratch,
    ) -> Result<RemoteResponse> {
        let Some(result_json) = self.result_json.take() else {
            bail!("REMOTE target '{}' sent no result frame", request.target);
        };
        let result: types::ExecResult =
            serde_json::from_slice(&result_json).context("REMOTE result decode failed")?;
        if !result.ok {
            bail!(
                "REMOTE target '{}' failed: {}",
                request.target,
                result
                    .error
                    .unwrap_or_else(|| "no reason given".to_string())
            );
        }
        // End-to-end integrity over the bytes each side actually saw.
        let script_hash = oxdock_remote_proto::sha256_hex(request.script_text.as_bytes());
        if result.script_sha256 != script_hash {
            bail!(
                "REMOTE target '{}' executed different script bytes than shipped (transport corruption)",
                request.target
            );
        }
        let stdout_hash = {
            use sha2::Digest;
            let digest = std::mem::replace(&mut self.stdout_hash, sha2::Sha256::new());
            use std::fmt::Write as _;
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
        let Some(mut stage) = self.result_stage.take() else {
            bail!("REMOTE target '{}' sent no result tarball", request.target);
        };
        let result_hash = self.result_hash.take().map(|hasher| {
            use sha2::Digest;
            use std::fmt::Write as _;
            let mut out = String::with_capacity(64);
            for byte in hasher.finalize() {
                let _ = write!(out, "{byte:02x}");
            }
            out
        });
        if result_hash.as_deref() != Some(result.result_tar_sha256.as_str()) {
            bail!(
                "REMOTE target '{}' push tarball sha256 mismatch (transport corruption)",
                request.target
            );
        }
        stage.rewind()?;
        Self::apply_push_tar(request, fs, &mut stage)?;
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
        })
    }

    /// Apply a staged push tarball against the host active root: entries
    /// map through the declared pairs (exact file or directory prefix),
    /// and undeclared guest paths abort the session.
    fn apply_push_tar(
        request: &RemoteRequest,
        fs: &dyn WorkspaceFs,
        stage: &mut oxdock_core::TransferStage,
    ) -> Result<()> {
        let root = fs.root().clone();
        tar::unpack_from(stage, &mut |entry| match entry {
            tar::StreamedEntry::Dir { path } => {
                let host_rel = map_push_path(&request.push_decls, &path).ok_or_else(|| {
                    anyhow::anyhow!(
                        "REMOTE target '{}' returned undeclared path {path:?}",
                        request.target
                    )
                })?;
                oxdock_core::apply_push_dir(fs, &root, &host_rel)
            }
            tar::StreamedEntry::File { path, reader } => {
                let host_rel = map_push_path(&request.push_decls, &path).ok_or_else(|| {
                    anyhow::anyhow!(
                        "REMOTE target '{}' returned undeclared path {path:?}",
                        request.target
                    )
                })?;
                // Entry bytes stream straight into place: apply_push_file
                // pumps O(chunk) itself, so no staging sits between the
                // archive driver and disk.
                oxdock_core::apply_push_file(fs, &root, &host_rel, reader)
            }
            tar::StreamedEntry::Symlink { path, target } => {
                let host_rel = map_push_path(&request.push_decls, &path).ok_or_else(|| {
                    anyhow::anyhow!(
                        "REMOTE target '{}' returned undeclared path {path:?}",
                        request.target
                    )
                })?;
                oxdock_core::apply_push_symlink(fs, &root, &host_rel, &target)
            }
        })
    }
}

/// Scratch space for one transfer tarball: a guarded tempdir plus resolver
/// for spill staging. Dropped (directory removed) when the transfer ends.
struct TransferScratch {
    _temp: GuardedPath,
    _tempdir: oxdock_fs::GuardedTempDir,
    resolver: PathResolver,
}

impl TransferScratch {
    fn new() -> Result<Self> {
        let tempdir = GuardedPath::tempdir().context("REMOTE transfer scratch tempdir failed")?;
        let root = tempdir.as_guarded_path().clone();
        let resolver = PathResolver::new(root.root(), root.root())?;
        Ok(Self {
            _temp: root,
            _tempdir: tempdir,
            resolver,
        })
    }

    fn stage(&self, name: &str) -> Result<oxdock_core::TransferStage> {
        oxdock_core::TransferStage::new_spill(
            &self.resolver,
            &self._temp,
            &format!("{}-{}", next_spill_id(), name),
        )
    }

    fn hash_and_len(&self, stage: &mut oxdock_core::TransferStage) -> Result<(String, u64)> {
        use std::io::Read;
        let mut hasher = sha2::Sha256::new();
        use sha2::Digest;
        let mut len = 0u64;
        let mut buf = [0u8; 65536];
        loop {
            let n = stage.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            len += n as u64;
        }
        use std::fmt::Write as _;
        let mut out = String::with_capacity(64);
        for byte in hasher.finalize() {
            let _ = write!(out, "{byte:02x}");
        }
        Ok((out, len))
    }
}

fn next_spill_id() -> u64 {
    SPILL_COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Write a full payload through a muxio encoder. `write_bytes` buffers
/// internally and reports emitted (not consumed) bytes, so one call
/// takes everything and `flush` pushes the remainder as a final frame.
fn write_all(encoder: &mut Encoder, bytes: &[u8]) -> Result<()> {
    encoder
        .write_bytes(bytes)
        .context("REMOTE stream write failed")?;
    encoder.flush().context("REMOTE stream flush failed")?;
    Ok(())
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
