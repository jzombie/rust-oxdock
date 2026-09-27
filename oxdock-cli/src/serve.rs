//! Guest serve loop for sealed remote execution (`--remote-serve`,
//! NET-gated). A muxio sync server drives the guest endpoint over the
//! current process stdio; three stream handlers implement the OxDock
//! protocol. Stdout carries ONLY frames: diagnostics go to stderr. Bails
//! on TTY stdio (serve mode requires piped transport).
//!
//! Guest mode executes the shipped `REMOTE` wrapper locally through the
//! standard scoped machinery: flagged `COPY` declarations acknowledge as
//! no-ops (the session already fulfilled the transfers), and only the
//! executed `--to-host` manifest paths are packed back. Missing push
//! sources fail the exec loudly. Stdin streams live into the running
//! engine; stdout/stderr stream back through the response responder as
//! the engine produces them.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use muxio_core::rpc::rpc_internals::RpcStreamEvent;
use muxio_rpc_service_endpoint::{RpcServiceEndpointInterface, StreamResponder};
use muxio_sync_rpc_server::RpcSyncServer;
use oxdock_fs::{GuardedPath, PathResolver};
use sha2::Digest;

use oxdock_remote_proto::{channel, methods, tar, types};

/// Module-table surface for the handshake: sorted module names each with
/// their sorted function names. Built once at serve startup from the
/// guest engine.
pub fn module_surface() -> Vec<(String, Vec<String>)> {
    let mut engine = crate::Engine::new();
    for module in crate::cli_host_modules_for_serve() {
        engine.register_module(module);
    }
    let table = engine.module_table();
    let mut entries: Vec<(String, Vec<String>)> = table
        .modules
        .into_iter()
        .map(|(name, funcs)| {
            let mut functions: Vec<String> = funcs
                .map(|surface| surface.functions.into_iter().collect())
                .unwrap_or_default();
            functions.sort();
            (name, functions)
        })
        .collect();
    entries.sort();
    entries
}

/// Module names for handshake diagnostics, derived from the same
/// surface the guest executes. No cfg chains: whatever is registered
/// (including downstream libraries) appears by name.
fn guest_modules(surface: &[(String, Vec<String>)]) -> Vec<String> {
    surface.iter().map(|(name, _)| name.clone()).collect()
}

/// One in-flight exec: sections arrive in contract order (fetch
/// descriptor, fetch tar, script, stdin tail) but chunking is never
/// significant. Byte counts drive every transition; request ids from
/// muxio correlate streams, so no arrival-order assumptions exist.
struct GuestExec {
    head: Vec<u8>,
    descriptor: Option<types::TarDescriptor>,
    fetch_remaining: u64,
    fetch_stage: Option<FetchStage>,
    fetch_hash: Option<sha2::Sha256>,
    script_head: Vec<u8>,
    script: Option<Vec<u8>>,
    stdin_backend: Option<Arc<oxdock_pipe::PipeInner>>,
    stdin_pending: Vec<u8>,
    started: bool,
}

impl GuestExec {
    fn new() -> Self {
        Self {
            head: Vec::new(),
            descriptor: None,
            fetch_remaining: 0,
            fetch_stage: None,
            fetch_hash: None,
            script_head: Vec::new(),
            script: None,
            stdin_backend: None,
            stdin_pending: Vec::new(),
            started: false,
        }
    }
}

/// Staging for one inbound tarball: spill file plus incremental hash,
/// rooted in its own tempdir so workspace packing never sees it.
struct FetchStage {
    _tempdir: GuardedPath,
    _dir: oxdock_fs::GuardedTempDir,
    spill: oxdock_core::TransferStage,
}

impl FetchStage {
    fn new() -> Result<Self> {
        let dir = GuardedPath::tempdir().context("serve transfer tempdir failed")?;
        let root = dir.as_guarded_path().clone();
        let resolver = PathResolver::new(root.root(), root.root())?;
        // Memory-backed under Miri: the serve loop never runs there (no
        // transports), so the spill path is unreachable; this exists only
        // so the guest compiles under `--cfg miri`.
        #[cfg(miri)]
        let spill = {
            let _ = (&resolver, &root);
            oxdock_core::TransferStage::new_mem()
        };
        #[cfg(not(miri))]
        let spill = oxdock_core::TransferStage::new_spill(&resolver, &root, "fetch.tar.gz")?;
        Ok(Self {
            _tempdir: root,
            _dir: dir,
            spill,
        })
    }
}

type ExecMap = Arc<Mutex<HashMap<u32, GuestExec>>>;
type HelloMap = Arc<Mutex<HashMap<u32, Vec<u8>>>>;

/// Serve one transport lifetime: register handlers, then block until the
/// host closes stdin. Exactly one exec per transport; the exec runs on a
/// worker thread while the server read loop stays alive for live stdin.
pub fn serve() -> Result<()> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() || std::io::stdout().is_terminal() {
        bail!("--remote-serve requires piped stdio (no TTY)");
    }
    let setup = RpcSyncServer::stdio();
    let surface = Arc::new(module_surface());
    let module_digest = Arc::new(crate::remote_session_module_hash(&surface));
    let endpoint = setup.endpoint();
    let hellos: HelloMap = Arc::new(Mutex::new(HashMap::new()));

    // Handshake: hello bytes accumulate per request; the verdict is the
    // single finalized response on End.
    {
        let hellos = Arc::clone(&hellos);
        let surface = Arc::clone(&surface);
        let module_digest = Arc::clone(&module_digest);
        futures_executor::block_on(endpoint.register_stream_handler(
            methods::HANDSHAKE,
            move |event, responder, _ctx| {
                answer_handshake_event(&hellos, &surface, &module_digest, event, &responder);
            },
        ))?;
    }

    // Exec: the full section state machine below. One exec per
    // transport shares a single record; request ids correlate.
    {
        let exec_map: ExecMap = Arc::new(Mutex::new(HashMap::new()));
        futures_executor::block_on(endpoint.register_stream_handler(
            methods::EXEC,
            move |event, responder, _ctx| {
                exec_event(&exec_map, &responder, event);
            },
        ))?;
    }

    // Close: acknowledge and let the host drop the transport.
    {
        futures_executor::block_on(endpoint.register_stream_handler(
            methods::CLOSE,
            move |event, responder, _ctx| {
                if matches!(event, RpcStreamEvent::End { .. }) {
                    responder.respond(Vec::new(), true);
                }
            },
        ))?;
    }

    // All handlers are registered before the first byte routes: the
    // server pumps nothing until start().
    let server = setup.start();
    server.join_reader();
    Ok(())
}

fn exec_event(exec_map: &ExecMap, responder: &StreamResponder, event: RpcStreamEvent) {
    let mut execs = exec_map.lock().unwrap_or_else(|poison| poison.into_inner());
    match event {
        RpcStreamEvent::Header { rpc_request_id, .. } => {
            execs.entry(rpc_request_id).or_insert_with(GuestExec::new);
            let _ = responder;
        }
        RpcStreamEvent::PayloadChunk {
            rpc_request_id,
            bytes,
            ..
        } => {
            let entry = execs.entry(rpc_request_id).or_insert_with(GuestExec::new);
            if let Err(err) = feed_exec(entry, &bytes) {
                let message = format!("{err:#}");
                execs.remove(&rpc_request_id);
                send_result(
                    responder,
                    false,
                    &message,
                    String::new(),
                    String::new(),
                    String::new(),
                    &[],
                );
                return;
            }
            if !entry.started && entry.script.is_some() {
                let mut exec = execs.remove(&rpc_request_id).expect("present");
                start_exec(&mut exec, responder.clone());
                // The record stays mapped so later stdin chunks and the
                // stdin End find the live backend.
                let backend = exec.stdin_backend.clone();
                execs.insert(
                    rpc_request_id,
                    GuestExec {
                        stdin_backend: backend,
                        started: true,
                        ..GuestExec::new()
                    },
                );
            }
        }
        RpcStreamEvent::End { rpc_request_id, .. } => {
            if let Some(entry) = execs.get_mut(&rpc_request_id)
                && let Some(backend) = entry.stdin_backend.take()
            {
                backend.force_close();
            }
        }
        RpcStreamEvent::Error { .. } => {}
    }
}

/// Feed request bytes through the section state machine: fetch
/// descriptor, fetch tar (spilled + hashed), script, then live stdin.
fn feed_exec(exec: &mut GuestExec, bytes: &[u8]) -> Result<()> {
    let mut cursor = bytes;
    // Fetch descriptor: prefixed JSON, parsed once complete.
    if exec.descriptor.is_none() {
        exec.head.extend_from_slice(cursor);
        let Some((json, rest)) = channel::split_prefixed(&exec.head) else {
            return Ok(());
        };
        let descriptor: types::TarDescriptor =
            serde_json::from_slice(json).context("serve fetch descriptor decode failed")?;
        exec.fetch_remaining = descriptor.tar_len;
        exec.descriptor = Some(descriptor);
        let stage = FetchStage::new()?;
        exec.fetch_stage = Some(stage);
        exec.fetch_hash = Some(sha2::Sha256::new());
        cursor = rest;
    }
    // Fetch tar: spill + hash until tar_len consumed.
    if exec.fetch_remaining > 0 && !cursor.is_empty() {
        let take = (cursor.len() as u64).min(exec.fetch_remaining) as usize;
        let (chunk, rest) = cursor.split_at(take);
        let (Some(stage), Some(hash)) = (exec.fetch_stage.as_mut(), exec.fetch_hash.as_mut())
        else {
            bail!("serve fetch tar arrived with no stage");
        };
        use sha2::Digest;
        stage.spill.write_all(chunk)?;
        hash.update(chunk);
        exec.fetch_remaining -= take as u64;
        cursor = rest;
    }
    // Script: prefixed bytes, parsed once complete.
    if exec.script.is_none() && exec.fetch_remaining == 0 && !cursor.is_empty() {
        exec.script_head.extend_from_slice(cursor);
        if let Some((script, rest)) = channel::split_bytes(&exec.script_head) {
            exec.script = Some(script.to_vec());
            cursor = rest;
        } else {
            return Ok(());
        }
    }
    // Stdin tail: live feed when started, buffer when early.
    if !cursor.is_empty() {
        if let Some(backend) = &exec.stdin_backend {
            let writer = backend.writer_handle();
            if let Ok(mut guard) = writer.lock() {
                let _ = guard.write_all(cursor);
                let _ = guard.flush();
            }
        } else {
            exec.stdin_pending.extend_from_slice(cursor);
        }
    }
    Ok(())
}

/// Start execution once script and fetch sections are complete: verify
/// and unpack the fetch tar into fresh staging, then run the engine on a
/// worker thread with live stdio. Takes the record out of the map; the
/// caller reinserts the live-backend stub.
fn start_exec(exec: &mut GuestExec, responder: StreamResponder) {
    exec.started = true;
    let descriptor = match exec.descriptor.clone() {
        Some(descriptor) => descriptor,
        None => {
            fail_started_exec(&responder, exec, "exec started with no fetch descriptor");
            return;
        }
    };
    let script = match exec.script.clone() {
        Some(script) => script,
        None => {
            fail_started_exec(&responder, exec, "exec started with no script");
            return;
        }
    };
    let Some(mut stage) = exec.fetch_stage.take() else {
        fail_started_exec(&responder, exec, "exec started with no fetch stage");
        return;
    };
    let Some(hash) = exec.fetch_hash.take() else {
        fail_started_exec(&responder, exec, "exec started with no fetch hash");
        return;
    };
    // Verify before touching the workspace.
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(64);
    for byte in hash.finalize() {
        let _ = write!(hex, "{byte:02x}");
    }
    if hex != descriptor.sha256 {
        fail_started_exec(&responder, exec, "fetch sha256 mismatch");
        return;
    }
    let staging = match prepare_staging(&mut stage.spill) {
        Ok(staging) => staging,
        Err(err) => {
            fail_started_exec(&responder, exec, &format!("fetch unpack: {err:#}"));
            return;
        }
    };
    // Live stdin backend: pre-arrived bytes go in now; the feed writer
    // stays alive in the worker for the whole exec.
    let stdin_pipe = oxdock_pipe::ScriptPipe::new();
    let backend = stdin_pipe.pipe_inner();
    {
        let writer = backend.writer_handle();
        if let Ok(mut guard) = writer.lock() {
            let _ = guard.write_all(&exec.stdin_pending);
            let _ = guard.flush();
        }
    }
    exec.stdin_pending.clear();
    exec.stdin_backend = Some(Arc::clone(&backend));
    let feed = backend.writer_handle();
    let script_text = match String::from_utf8(script) {
        Ok(script_text) => script_text,
        Err(_) => {
            fail_started_exec(&responder, exec, "script is not UTF-8");
            return;
        }
    };
    let script_hash = oxdock_remote_proto::sha256_hex(script_text.as_bytes());
    std::thread::Builder::new()
        .name("oxdock-serve-engine".to_string())
        .spawn(move || {
            run_guest_engine(
                responder,
                staging,
                script_text,
                script_hash,
                stdin_pipe,
                feed,
            )
        })
        .expect("serve engine spawn failed");
}

/// Unpack a verified fetch spill into fresh empty staging.
fn prepare_staging(
    spill: &mut oxdock_core::TransferStage,
) -> Result<(GuardedPath, oxdock_fs::GuardedTempDir, PathResolver)> {
    spill.rewind()?;
    let dir = GuardedPath::tempdir().context("serve staging tempdir failed")?;
    let staging = dir.as_guarded_path().clone();
    let resolver = PathResolver::new(staging.root(), staging.root())?;
    tar::unpack_from(spill, &mut |entry| match entry {
        tar::StreamedEntry::Dir { path } => {
            if path.is_empty() {
                return Ok(());
            }
            let dest = staging
                .join(&path)
                .map_err(|err| anyhow::anyhow!("fetch dir {path:?} escapes staging: {err:#}"))?;
            resolver.create_dir_all(&dest)?;
            Ok(())
        }
        tar::StreamedEntry::File { path, reader } => {
            let dest = staging
                .join(&path)
                .map_err(|err| anyhow::anyhow!("fetch file {path:?} escapes staging: {err:#}"))?;
            if let Some(idx) = path.rfind('/') {
                resolver.create_dir_all(
                    &staging
                        .join(&path[..idx])
                        .map_err(|err| anyhow::anyhow!("fetch parent escapes staging: {err:#}"))?,
                )?;
            }
            let mut writer = resolver.open_write(&dest)?;
            let mut buf = [0u8; 65536];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                writer.write_all(&buf[..n])?;
            }
            writer.flush()?;
            Ok(())
        }
        tar::StreamedEntry::Symlink { path, target } => {
            // Fetch symlinks resolve to content host-side already; a
            // symlink member here is unexpected. Refuse rather than guess.
            bail!("fetch symlink {path:?} -> {target:?} refused: fetch entries are files")
        }
    })?;
    Ok((staging, dir, resolver))
}

/// Worker body: run the engine with live stdio, then pack and send the
/// result. Stdout/stderr emitter drains sinks into tagged responder
/// chunks as the engine produces them; the push tar streams from spill.
#[allow(clippy::too_many_lines)]
fn run_guest_engine(
    responder: StreamResponder,
    staging: (GuardedPath, oxdock_fs::GuardedTempDir, PathResolver),
    script_text: String,
    script_hash: String,
    stdin_pipe: oxdock_pipe::ScriptPipe,
    feed: oxdock_process::SharedOutput,
) {
    let (staging_root, _staging_dir, staging_resolver) = staging;
    let staging_root_for_engine = staging_root.clone();
    let stdout_sink = Arc::new(Mutex::new(Vec::new()));
    let stderr_sink = Arc::new(Mutex::new(Vec::new()));
    let mut io = crate::ExecIo::new();
    io.set_stdin(Some(stdin_pipe.reader()));
    io.set_stdout(Some(stdout_sink.clone()));
    io.set_stderr(Some(stderr_sink.clone()));
    io.set_remote_guest(true);
    let manifest_sink: crate::PushManifestSink = Arc::new(Mutex::new(Vec::new()));
    io.set_push_manifest_sink(Arc::clone(&manifest_sink));
    let mut engine = crate::Engine::new().with_io(io);
    for module in crate::cli_host_modules_for_serve() {
        engine.register_module(module);
    }
    let engine_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let engine_done_emit = Arc::clone(&engine_done);
    let engine_thread = std::thread::Builder::new()
        .name("oxdock-serve-engine-inner".to_string())
        .spawn(move || engine.run_script(&staging_root_for_engine, &script_text))
        .expect("serve engine inner spawn failed");
    // Emitter pump: position-tracked drains keep memory bounded no matter
    // how much the guest produces.
    let mut out_pos = 0usize;
    let mut err_pos = 0usize;
    loop {
        {
            let out_guard = stdout_sink
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if out_guard.len() > out_pos {
                let mut chunk = vec![channel::TAG_STDOUT];
                chunk.extend_from_slice(&out_guard[out_pos..]);
                responder.respond(chunk, false);
                out_pos = out_guard.len();
            }
        }
        {
            let err_guard = stderr_sink
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if err_guard.len() > err_pos {
                let mut chunk = vec![channel::TAG_STDERR];
                chunk.extend_from_slice(&err_guard[err_pos..]);
                responder.respond(chunk, false);
                err_pos = err_guard.len();
            }
        }
        if engine_done_emit.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        if engine_thread.is_finished() {
            engine_done_emit.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let outcome = engine_thread
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("serve engine thread panicked")));
    drop(feed);
    let stdout_bytes = stdout_sink.lock().unwrap().clone();
    let stdout_hash = oxdock_remote_proto::sha256_hex(&stdout_bytes);
    // Pack the executed manifest to spill, then stream it as result-tar
    // chunks behind the prefixed result JSON.
    let manifest = manifest_sink
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    let (push_tar_len, push_sha, push_spill, push_dir) =
        match pack_manifest(&staging_resolver, &staging_root, &manifest) {
            Ok(staged) => staged,
            Err(err) => {
                send_result(
                    &responder,
                    false,
                    &format!("{err:#}"),
                    String::new(),
                    script_hash,
                    stdout_hash,
                    &[],
                );
                return;
            }
        };
    let (ok, error) = match outcome {
        Ok(_) => (true, None),
        Err(err) => (false, Some(format!("{err:#}"))),
    };
    let result = types::ExecResult {
        ok,
        error,
        result_tar_sha256: push_sha.clone(),
        script_sha256: script_hash,
        stdout_sha256: stdout_hash,
    };
    let result_json = match serde_json::to_vec(&result) {
        Ok(result_json) => result_json,
        Err(_) => return,
    };
    let frame = channel::pack_prefixed(&result_json, &[]);
    respond_tagged(&responder, channel::TAG_RESULT, &frame);
    // Stream the staged push tar in chunks, then the terminal frame.
    // The host runs the tar section to response end. Dropping the spill
    // dir here deletes the archive: it must outlive streaming (hence the
    // binding), and nothing after this point touches the spill.
    let _ = push_tar_len;
    stream_spill(&responder, push_spill);
    responder.respond(Vec::new(), true);
    drop(push_dir);
}

/// Pack executed manifest entries to a spill tarball. Returns tar length,
/// sha, the rewound spill, and the spill's tempdir: the caller holds the
/// dir until streaming finishes (dropping it deletes the spill file), so
/// no `mem::forget` leaks a guest tempdir per exec. The spill stays in
/// its own directory, never inside staging, so packing can never observe
/// it (same discipline as [`FetchStage`); a guest file literally named
/// like the spill can neither collide with nor be clobbered by it.
fn pack_manifest(
    resolver: &PathResolver,
    staging: &GuardedPath,
    manifest: &[(String, String)],
) -> Result<(
    u64,
    String,
    oxdock_core::TransferStage,
    oxdock_fs::GuardedTempDir,
)> {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    let mut ordered: Vec<String> = Vec::new();
    for (guest_src, _) in manifest.iter().rev() {
        if seen.insert(guest_src.clone()) {
            ordered.push(guest_src.clone());
        }
    }
    ordered.reverse();
    // Expand directories into file lists plus dir lists (names only).
    let mut files: Vec<(String, u64, Box<dyn Read + Send>)> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    for guest_src in &ordered {
        let src = staging.join(guest_src).map_err(|err| {
            anyhow::anyhow!("guest push source {guest_src:?} escapes staging: {err:#}")
        })?;
        match resolver.entry_kind(&src) {
            Ok(oxdock_fs::EntryKind::Dir) => {
                dirs.push(guest_src.clone());
                let mut stack = vec![(src, guest_src.clone())];
                while let Some((dir, rel)) = stack.pop() {
                    let mut entries = resolver.read_dir_entries(&dir)?;
                    entries.sort_by_key(|entry| entry.file_name());
                    for entry in entries {
                        let name = entry.file_name().to_string_lossy().to_string();
                        let child = dir.join(&name)?;
                        let child_rel = format!("{rel}/{name}");
                        match resolver.entry_kind(&child)? {
                            oxdock_fs::EntryKind::Dir => {
                                dirs.push(child_rel.clone());
                                stack.push((child, child_rel));
                            }
                            _ => {
                                let len = resolver.metadata(&child).map(|m| m.len()).with_context(
                                    || format!("guest push source {child_rel:?} unreadable"),
                                )?;
                                files.push((
                                    child_rel,
                                    len,
                                    resolver.open_read(&child)? as Box<dyn Read + Send>,
                                ));
                            }
                        }
                    }
                }
            }
            Ok(_) => {
                let len = resolver.metadata(&src).map(|m| m.len()).with_context(|| {
                    anyhow::anyhow!("guest push source {guest_src:?} unreadable")
                })?;
                files.push((
                    guest_src.clone(),
                    len,
                    resolver.open_read(&src)? as Box<dyn Read + Send>,
                ));
            }
            Err(err) => {
                bail!("guest push source {guest_src:?} missing: {err:#}");
            }
        }
    }
    let push_dir = GuardedPath::tempdir().context("serve push tempdir failed")?;
    let push_root = push_dir.as_guarded_path().clone();
    let push_resolver = PathResolver::new(push_root.root(), push_root.root())?;
    // Memory-backed under Miri: same unreachable-spill rationale as
    // `FetchStage::new` above.
    #[cfg(miri)]
    let mut spill = {
        let _ = (&push_resolver, &push_root);
        oxdock_core::TransferStage::new_mem()
    };
    #[cfg(not(miri))]
    let mut spill =
        oxdock_core::TransferStage::new_spill(&push_resolver, &push_root, "push.tar.gz")?;
    // NOTE: spill tempdir must outlive streaming below; moved out via the
    // returned stage is insufficient (directory deleted on drop). The
    // caller streams immediately, then drops: keep the dir alive here by
    // leaking scope discipline — see below.
    let mut hashing = tar::HashingWriter::new(&mut spill);
    tar::pack_to(&mut hashing, &mut files, &dirs)?;
    hashing.flush()?;
    drop(hashing);
    spill.rewind()?;
    // Hash by re-reading (one extra disk pass, O(1) memory).
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest;
    let mut len = 0u64;
    let mut buf = [0u8; 65536];
    loop {
        let n = spill.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        len += n as u64;
    }
    use std::fmt::Write as _;
    let mut sha = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(sha, "{byte:02x}");
    }
    spill.rewind()?;
    Ok((len, sha, spill, push_dir))
}

/// Send one tagged responder chunk.
fn respond_tagged(responder: &StreamResponder, tag: u8, bytes: &[u8]) {
    let mut chunk = Vec::with_capacity(1 + bytes.len());
    chunk.push(tag);
    chunk.extend_from_slice(bytes);
    responder.respond(chunk, false);
}

/// Stream a rewound spill over the responder in chunks.
fn stream_spill(responder: &StreamResponder, mut spill: oxdock_core::TransferStage) {
    let mut buf = [0u8; 65536];
    while let Ok(n) = spill.read(&mut buf) {
        if n == 0 {
            break;
        }
        respond_tagged(responder, channel::TAG_RESULT, &buf[..n]);
    }
}

/// Send the terminal result frame plus an empty-tar result section.
/// Used for failures before any push tarball exists.
fn send_result(
    responder: &StreamResponder,
    ok: bool,
    error: &str,
    result_tar_sha256: String,
    script_hash: String,
    stdout_hash: String,
    _tar: &[u8],
) {
    let result = types::ExecResult {
        ok,
        error: if ok { None } else { Some(error.to_string()) },
        result_tar_sha256,
        script_sha256: script_hash,
        stdout_sha256: stdout_hash,
    };
    if let Ok(result_json) = serde_json::to_vec(&result) {
        let frame = channel::pack_prefixed(&result_json, &[]);
        respond_tagged(responder, channel::TAG_RESULT, &frame);
    }
    // Terminal: the host ends the EXEC stream on this final frame.
    responder.respond(Vec::new(), true);
}

/// Fail an exec that never started: error result plus empty tar section.
fn fail_started_exec(responder: &StreamResponder, _exec: &mut GuestExec, error: &str) {
    send_result(
        responder,
        false,
        error,
        String::new(),
        String::new(),
        String::new(),
        &[],
    );
}

fn answer_handshake_event(
    hellos: &HelloMap,
    surface: &[(String, Vec<String>)],
    module_digest: &str,
    event: RpcStreamEvent,
    responder: &StreamResponder,
) {
    match event {
        RpcStreamEvent::PayloadChunk {
            rpc_request_id,
            bytes,
            ..
        } => {
            hellos
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .entry(rpc_request_id)
                .or_default()
                .extend_from_slice(&bytes);
        }
        RpcStreamEvent::End { rpc_request_id, .. } => {
            let mut hellos = hellos.lock().unwrap_or_else(|poison| poison.into_inner());
            let Some(hello_bytes) = hellos.remove(&rpc_request_id) else {
                return;
            };
            drop(hellos);
            answer_handshake(surface, module_digest, &hello_bytes, responder);
        }
        _ => {}
    }
}

fn answer_handshake(
    surface: &[(String, Vec<String>)],
    module_digest: &str,
    hello_bytes: &[u8],
    responder: &StreamResponder,
) {
    let verdict = match serde_json::from_slice::<types::HandshakeHello>(hello_bytes) {
        Ok(hello) if hello.proto_digest == *oxdock_remote_proto::PROTO_DIGEST => {
            types::HandshakeVerdict {
                accepted: true,
                proto_digest: oxdock_remote_proto::PROTO_DIGEST.clone(),
                oxdock_version: env!("CARGO_PKG_VERSION").to_string(),
                modules: guest_modules(surface),
                module_hash: module_digest.to_string(),
                error: None,
            }
        }
        Ok(hello) => types::HandshakeVerdict {
            accepted: false,
            proto_digest: oxdock_remote_proto::PROTO_DIGEST.clone(),
            oxdock_version: env!("CARGO_PKG_VERSION").to_string(),
            modules: guest_modules(surface),
            module_hash: module_digest.to_string(),
            error: Some(format!(
                "protocol digest mismatch (host {} guest {})",
                hello.proto_digest,
                *oxdock_remote_proto::PROTO_DIGEST
            )),
        },
        Err(err) => types::HandshakeVerdict {
            accepted: false,
            proto_digest: oxdock_remote_proto::PROTO_DIGEST.clone(),
            oxdock_version: env!("CARGO_PKG_VERSION").to_string(),
            modules: guest_modules(surface),
            module_hash: module_digest.to_string(),
            error: Some(format!("hello decode failed: {err:#}")),
        },
    };
    if let Ok(bytes) = serde_json::to_vec(&verdict) {
        responder.respond(bytes, true);
    }
}
