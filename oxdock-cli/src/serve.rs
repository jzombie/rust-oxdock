//! Guest serve loop for sealed remote execution (`--remote-serve`,
//! NET-gated). Reads muxio frames on stdin, executes `exec` streams in
//! fresh empty staging workspaces with the guest engine, and writes
//! frames on stdout. Stdout carries ONLY frames: diagnostics go to
//! stderr. Bails on TTY stdio (serve mode requires piped transport).
//!
//! Guest mode executes the shipped `REMOTE` wrapper locally through the
//! standard scoped machinery: flagged `COPY` declarations acknowledge as
//! no-ops (the session already fulfilled the transfers), and only the
//! declared `--to-host` paths are packed back. Missing push sources fail
//! the exec loudly.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, mpsc};

use anyhow::{Context, Result, bail};
use oxdock_fs::{GuardedPath, PathResolver};

use oxdock_remote_proto::bridge::{Bridge, Role, StreamEvent};
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

/// One in-flight exec, accumulating its three host-to-guest channels.
/// Streams key by `(method_id, request_id)` with a sticky tag: only the
/// opening payload carries the tag byte, continuation chunks are pure
/// data (chunk boundaries are never significant). Execution starts when
/// script AND fetch complete; stdin streams live into the running engine
/// and its end closes the feed (EOF). Exactly one exec per transport.
struct GuestExec {
    script: Vec<u8>,
    script_ended: bool,
    fetch: Vec<u8>,
    fetch_ended: bool,
    stdin_bytes: Vec<u8>,
    stdin_ended: bool,
    hello: Vec<u8>,
    started: bool,
    stdin_backend: Option<Arc<oxdock_pipe::PipeInner>>,
}

impl GuestExec {
    fn new() -> Self {
        Self {
            script: Vec::new(),
            script_ended: false,
            fetch: Vec::new(),
            fetch_ended: false,
            stdin_bytes: Vec::new(),
            stdin_ended: false,
            hello: Vec::new(),
            started: false,
            stdin_backend: None,
        }
    }
}

/// One inbound stream: sticky tag plus buffered payload. Exactly mirrors
/// the host's channel buffers.
struct InboundStream {
    tag: Option<u8>,
    bytes: Vec<u8>,
}

/// Inbound stream state plus the exec record it feeds. Keyed by
/// `(method_id, request_id)`; exactly one exec is in flight per transport.
#[allow(dead_code)]
struct ServeState {
    execs: HashMap<(u64, u32), GuestExec>,
    streams: HashMap<(u64, u32), InboundStream>,
}

fn route_chunk(
    execs: &mut HashMap<(u64, u32), GuestExec>,
    streams: &mut HashMap<(u64, u32), InboundStream>,
    method_id: u64,
    request_id: u32,
    bytes: &[u8],
) -> Result<()> {
    // Hello is a single small stream: accumulate raw, strip the tag once
    // at answer time.
    if method_id == methods::HANDSHAKE {
        let entry = execs.entry((method_id, request_id)).or_insert(GuestExec::new());
        entry.hello.extend_from_slice(bytes);
        return Ok(());
    }
    if method_id == methods::CLOSE || method_id != methods::EXEC {
        return Ok(());
    }
    let stream = streams.entry((method_id, request_id)).or_insert(InboundStream {
        tag: None,
        bytes: Vec::new(),
    });
    let mut cursor = bytes;
    if stream.tag.is_none() {
        if cursor.is_empty() {
            return Ok(());
        }
        stream.tag = Some(cursor[0]);
        cursor = &cursor[1..];
    }
    stream.bytes.extend_from_slice(cursor);
    // A started exec feeds stdin live; everything else buffers.
    if stream.tag == Some(channel::TAG_STDIN)
        && let Some(entry) = execs.get_mut(&(methods::EXEC, 0))
        && entry.started
        && let Some(backend) = &entry.stdin_backend
    {
        let writer = backend.writer_handle();
        if let Ok(mut guard) = writer.lock() {
            let _ = guard.write_all(cursor);
            let _ = guard.flush();
        }
        stream.bytes.clear();
        return Ok(());
    }
    let entry = execs.entry((methods::EXEC, 0)).or_insert(GuestExec::new());
    match stream.tag {
        Some(tag) if tag == channel::TAG_SCRIPT => {
            entry.script.extend_from_slice(cursor);
        }
        Some(tag) if tag == channel::TAG_FETCH => {
            entry.fetch.extend_from_slice(cursor);
        }
        Some(tag) if tag == channel::TAG_STDIN => {
            entry.stdin_bytes.extend_from_slice(cursor);
        }
        _ => {}
    }
    stream.bytes.clear();
    Ok(())
}

/// Serve one transport lifetime on the calling thread: stdin frames in,
/// stdout frames out. Returns when the host closes the session or stdin
/// reaches EOF.
pub fn serve() -> Result<()> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() || std::io::stdout().is_terminal() {
        bail!("--remote-serve requires piped stdio (no TTY)");
    }
    let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>();
    let bridge = Arc::new(Mutex::new(Bridge::new(Role::Guest, frame_tx)));
    let writer = std::thread::Builder::new()
        .name("oxdock-serve-writer".to_string())
        .spawn(move || {
            let stdout = std::io::stdout();
            let mut locked = stdout.lock();
            // Flush every frame: StdoutLock is a LineWriter and muxio
            // frames are binary without newlines, so an unwatched buffer
            // would deadlock the host awaiting the verdict.
            for frame in frame_rx {
                if locked.write_all(&frame).is_err() {
                    break;
                }
                if locked.flush().is_err() {
                    break;
                }
            }
            let _ = locked.flush();
        })
        .context("serve writer spawn failed")?;

    let surface = module_surface();
    let module_digest = crate::remote_session_module_hash(&surface);
    let mut execs: HashMap<(u64, u32), GuestExec> = HashMap::new();
    let mut streams: HashMap<(u64, u32), InboundStream> = HashMap::new();
    let mut exec_thread: Option<std::thread::JoinHandle<()>> = None;
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 8192];
    loop {
        let n = stdin.read(&mut buf).context("serve stdin read failed")?;
        if n == 0 {
            break;
        }
        let events = bridge
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .read(&buf[..n])
            .context("serve muxio decode failed")?;
        for event in events {
            match event {
                StreamEvent::Chunk {
                    method_id,
                    request_id,
                    bytes,
                } => {
                    route_chunk(&mut execs, &mut streams, method_id, request_id, &bytes)?;
                }
                StreamEvent::End {
                    method_id,
                    request_id,
                } => {
                    if let Some(started) = route_end(
                        &bridge,
                        &mut execs,
                        &mut streams,
                        &surface,
                        &module_digest,
                        method_id,
                        request_id,
                    )? {
                        // The exec runs on a worker thread: the read loop
                        // MUST stay alive to feed live stdin and to observe
                        // the stdin End (EOF). Blocking here would deadlock
                        // the engine against its own input.
                        let bridge_thread = Arc::clone(&bridge);
                        let worker = std::thread::Builder::new()
                            .name("oxdock-serve-exec".to_string())
                            .spawn(move || {
                                let StartedExec {
                                    exec,
                                    stdin_reader,
                                    stdin_feed,
                                } = started;
                                let _ = run_exec(
                                    &bridge_thread,
                                    exec,
                                    stdin_reader,
                                    stdin_feed,
                                );
                            })
                            .context("serve exec spawn failed")?;
                        exec_thread = Some(worker);
                    }
                }
                StreamEvent::Error { detail } => {
                    bail!("serve wire error: {detail}");
                }
            }
        }
    }
    drop(bridge);
    let _ = writer.join();
    // One exec per transport: join it before exiting so no output is
    // lost when the host closes the session right after the result.
    if let Some(worker) = exec_thread.take() {
        let _ = worker.join();
    }
    Ok(())
}

fn emit(bridge: &Arc<Mutex<Bridge>>, method_id: u64, tag: u8, first: &[u8]) -> Result<u32> {
    bridge
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .open(method_id, tag, first)
}

fn emit_end(bridge: &Arc<Mutex<Bridge>>, stream: u32) -> Result<()> {
    bridge
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .end(stream)
}


/// A started exec ready for its worker thread: rendered script bytes,
/// fetch bytes, buffered stdin, plus the live stdin reader and feed.
struct StartedExec {
    exec: GuestExec,
    stdin_reader: oxdock_process::SharedInput,
    stdin_feed: oxdock_process::SharedOutput,
}

fn route_end(
    bridge: &Arc<Mutex<Bridge>>,
    execs: &mut HashMap<(u64, u32), GuestExec>,
    streams: &mut HashMap<(u64, u32), InboundStream>,
    surface: &[(String, Vec<String>)],
    module_digest: &str,
    method_id: u64,
    request_id: u32,
) -> Result<Option<StartedExec>> {
    if method_id == methods::HANDSHAKE {
        let Some(exec) = execs.remove(&(method_id, request_id)) else {
            return Ok(None);
        };
        streams.remove(&(method_id, request_id));
        answer_handshake(bridge, surface, module_digest, &exec.hello)?;
        return Ok(None);
    }
    if method_id == methods::CLOSE || method_id != methods::EXEC {
        return Ok(None);
    }
    // Ends identify by sticky stream tag, never by arrival order: the
    // streams map records each stream's tag from its opening payload, so
    // script, fetch, and stdin completion are unambiguous no matter how
    // frames interleave. Script+fetch completion starts the engine;
    // stdin end closes the feed (EOF).
    let tag = streams
        .get(&(method_id, request_id))
        .and_then(|stream| stream.tag);
    streams.remove(&(method_id, request_id));
    let ready = {
        let Some(entry) = execs.get_mut(&(methods::EXEC, 0)) else {
            return Ok(None);
        };
        match tag {
            Some(tag) if tag == channel::TAG_SCRIPT => entry.script_ended = true,
            Some(tag) if tag == channel::TAG_FETCH => entry.fetch_ended = true,
            Some(tag) if tag == channel::TAG_STDIN => {
                entry.stdin_ended = true;
                if let Some(backend) = entry.stdin_backend.take() {
                    backend.force_close();
                }
            }
            _ => {}
        }
        entry.script_ended && entry.fetch_ended && !entry.started
    };
    if !ready {
        return Ok(None);
    }
    let mut exec = execs.remove(&(methods::EXEC, 0)).expect("present");
    exec.started = true;
    // Live stdin backend: pre-arrived bytes go in now; later chunks feed
    // live through routing until stdin End closes it.
    let stdin_pipe = oxdock_pipe::ScriptPipe::new();
    let backend = stdin_pipe.pipe_inner();
    {
        let writer = backend.writer_handle();
        if let Ok(mut guard) = writer.lock() {
            guard.write_all(&exec.stdin_bytes)?;
            guard.flush()?;
        }
    }
    exec.stdin_bytes.clear();
    exec.stdin_backend = Some(Arc::clone(&backend));
    // The record stays mapped so later stdin chunks and the stdin End
    // find the backend. The feed writer MUST stay alive for the whole
    // exec: with zero writers the pipe reads closed and the engine sees
    // instant EOF.
    let feed = backend.writer_handle();
    execs.insert((methods::EXEC, 0), GuestExec {
        stdin_backend: Some(backend),
        started: true,
        ..GuestExec::new()
    });
    Ok(Some(StartedExec {
        exec,
        stdin_reader: stdin_pipe.reader(),
        stdin_feed: feed,
    }))
}

fn answer_handshake(
    bridge: &Arc<Mutex<Bridge>>,
    surface: &[(String, Vec<String>)],
    module_digest: &str,
    hello_bytes: &[u8],
) -> Result<()> {
    // The hello stream carries its tag byte first, like every channel.
    let payload = hello_bytes.strip_prefix(&[channel::TAG_HELLO]).unwrap_or(hello_bytes);
    let hello: types::HandshakeHello =
        serde_json::from_slice(payload).context("serve hello decode failed")?;
    let accepted = hello.proto_digest == *oxdock_remote_proto::PROTO_DIGEST;
    let verdict = types::HandshakeVerdict {
        accepted,
        proto_digest: oxdock_remote_proto::PROTO_DIGEST.clone(),
        oxdock_version: env!("CARGO_PKG_VERSION").to_string(),
        modules: guest_modules(surface),
        module_hash: module_digest.to_string(),
        error: if accepted {
            None
        } else {
            Some(format!(
                "protocol digest mismatch (host {} guest {})",
                hello.proto_digest, *oxdock_remote_proto::PROTO_DIGEST
            ))
        },
    };
    let bytes = serde_json::to_vec(&verdict).context("serve verdict encode failed")?;
    let stream = emit(bridge, methods::HANDSHAKE, channel::TAG_VERDICT, &bytes)?;
    emit_end(bridge, stream)?;
    Ok(())
}

/// Module names for handshake diagnostics, derived from the same
/// surface the guest executes. No cfg chains: whatever is registered
/// (including downstream libraries) appears by name.
fn guest_modules(surface: &[(String, Vec<String>)]) -> Vec<String> {
    surface.iter().map(|(name, _)| name.clone()).collect()
}

#[allow(clippy::too_many_arguments)]
fn run_exec(
    bridge: &Arc<Mutex<Bridge>>,
    exec: GuestExec,
    stdin: oxdock_process::SharedInput,
    _feed: oxdock_process::SharedOutput,
) -> Result<()> {
    // Staging: fresh empty temp dir per exec (nothing ships by default).
    let temp = GuardedPath::tempdir().context("serve staging tempdir failed")?;
    let staging = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(staging.root(), staging.root())?;

    // Fetch tarball: length-prefixed descriptor plus gzip bytes.
    let Some((descriptor_json, fetch_tar)) = channel::split_prefixed(&exec.fetch) else {
        return fail_exec(
            bridge,
            &exec.script,
            &[],
            "malformed fetch frame from host",
        );
    };
    let descriptor: types::TarDescriptor =
        serde_json::from_slice(descriptor_json).context("serve fetch descriptor decode failed")?;
    if let Err(err) = tar::verify_sha256(fetch_tar, &descriptor.sha256) {
        return fail_exec(bridge, &exec.script, &[], &format!("fetch sha256: {err:#}"));
    }
    let unpacked = match tar::unpack(fetch_tar) {
        Ok(unpacked) => unpacked,
        Err(err) => return fail_exec(bridge, &exec.script, &[], &format!("fetch unpack: {err:#}")),
    };
    for dir in &unpacked.dirs {
        if dir.is_empty() {
            continue;
        }
        let path = staging.join(dir).map_err(|err| {
            anyhow::anyhow!("fetch dir {dir:?} escapes staging: {err:#}")
        })?;
        resolver.create_dir_all(&path)?;
    }
    for entry in &unpacked.files {
        let path = staging.join(&entry.path).map_err(|err| {
            anyhow::anyhow!("fetch file {:?} escapes staging: {err:#}", entry.path)
        })?;
        if let Some(idx) = entry.path.rfind('/') {
            resolver.create_dir_all(&staging.join(&entry.path[..idx]).map_err(|err| {
                anyhow::anyhow!("fetch parent escapes staging: {err:#}")
            })?)?;
        }
        let mut writer = resolver.open_write(&path)?;
        writer.write_all(&entry.bytes)?;
        writer.flush()?;
    }

    // Execute the shipped script in guest mode against staging, with
    // the engine on a worker thread and stdio streaming live around it.
    // Stdin arrives through the caller-built live pipe (pre-arrived bytes
    // already fed, later chunks appended by routing, EOF at stdin End):
    // the engine reads while the host streams. Stdout/stderr drain
    // through an emitter pump into their channels as the engine produces
    // them. Nothing here waits for exec completion except the join below,
    // so interactive guests stream instead of buffering to the end.
    let script_text = String::from_utf8(exec.script.clone()).context("serve script not UTF-8")?;
    let script_hash = sha256_hex(exec.script.as_slice());
    let stdout_sink = Arc::new(Mutex::new(Vec::new()));
    let stderr_sink = Arc::new(Mutex::new(Vec::new()));
    let mut io = crate::ExecIo::new();
    io.set_stdin(Some(stdin));
    io.set_stdout(Some(stdout_sink.clone()));
    io.set_stderr(Some(stderr_sink.clone()));
    io.set_remote_guest(true);
    let mut engine = crate::Engine::new().with_io(io);
    for module in crate::cli_host_modules_for_serve() {
        engine.register_module(module);
    }
    let staging_run = staging.clone();
    let script_run = script_text.clone();
    let engine_thread = std::thread::Builder::new()
        .name("oxdock-serve-engine".to_string())
        .spawn(move || engine.run_script(&staging_run, &script_run))
        .context("serve engine spawn failed")?;
    // Emitter pump: drain new sink bytes into stdout/stderr streams.
    // Positions track consumption, so memory stays bounded no matter how
    // much the guest produces. Completion polls the thread handle: the
    // loop only exits when the engine finished, so no output is lost.
    let mut out_pos = 0usize;
    let mut err_pos = 0usize;
    let mut out_stream: Option<u32> = None;
    let mut err_stream: Option<u32> = None;
    loop {
        {
            let out_guard = stdout_sink.lock().unwrap_or_else(|poison| poison.into_inner());
            if out_guard.len() > out_pos {
                let stream = match out_stream {
                    Some(stream) => stream,
                    None => {
                        let stream = emit(bridge, methods::EXEC, channel::TAG_STDOUT, &[])?;
                        out_stream = Some(stream);
                        stream
                    }
                };
                bridge
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .write(stream, &out_guard[out_pos..])?;
                out_pos = out_guard.len();
            }
        }
        {
            let err_guard = stderr_sink.lock().unwrap_or_else(|poison| poison.into_inner());
            if err_guard.len() > err_pos {
                let stream = match err_stream {
                    Some(stream) => stream,
                    None => {
                        let stream = emit(bridge, methods::EXEC, channel::TAG_STDERR, &[])?;
                        err_stream = Some(stream);
                        stream
                    }
                };
                bridge
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .write(stream, &err_guard[err_pos..])?;
                err_pos = err_guard.len();
            }
        }
        if engine_thread.is_finished() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let outcome = engine_thread.join().unwrap_or_else(|_| {
        Err(anyhow::anyhow!("serve engine thread panicked"))
    });
    let stdout_bytes = stdout_sink.lock().unwrap().clone();
    let stdout_hash = sha256_hex(&stdout_bytes);
    if let Some(stream) = out_stream {
        emit_end(bridge, stream)?;
    }
    if let Some(stream) = err_stream {
        emit_end(bridge, stream)?;
    }

    // Re-derive push declarations from the shipped script AST (single
    // source: the same header the host used) and pack declared paths.
    let (push_files, push_dirs) = match &outcome {
        Ok(_) => collect_push_entries(&staging, &script_text)?,
        Err(_) => (Vec::new(), Vec::new()),
    };
    let sync_files: Vec<tar::SyncFile> = push_files
        .into_iter()
        .map(|(path, bytes)| tar::SyncFile { path, bytes })
        .collect();
    let (push_tar, push_descriptor) = tar::pack(&sync_files, &push_dirs)?;

    let (ok, error) = match outcome {
        Ok(_) => (true, None),
        Err(err) => (false, Some(format!("{err:#}"))),
    };
    let result = types::ExecResult {
        ok,
        error,
        result_tar_sha256: push_descriptor.sha256.clone(),
        script_sha256: script_hash,
        stdout_sha256: stdout_hash,
    };
    let result_json = serde_json::to_vec(&result).context("serve result encode failed")?;
    let frame = channel::pack_prefixed(&result_json, &push_tar);
    // Result carrier last: the host already streamed stdout/stderr live.
    let result_stream = emit(bridge, methods::EXEC, channel::TAG_RESULT, &frame)?;
    emit_end(bridge, result_stream)?;
    Ok(())
}

fn fail_exec(
    bridge: &Arc<Mutex<Bridge>>,
    script: &[u8],
    stdout: &[u8],
    error: &str,
) -> Result<()> {
    let result = types::ExecResult {
        ok: false,
        error: Some(error.to_string()),
        result_tar_sha256: String::new(),
        script_sha256: sha256_hex(script),
        stdout_sha256: sha256_hex(stdout),
    };
    let result_json = serde_json::to_vec(&result).context("serve result encode failed")?;
    let frame = channel::pack_prefixed(&result_json, &[]);
    let stream = emit(bridge, methods::EXEC, channel::TAG_RESULT, &frame)?;
    emit_end(bridge, stream)?;
    Ok(())
}

/// Collected push entries: guest-relative file bytes plus guest-relative
/// directories, both declared `--to-host` paths packed after execution.
type PushEntries = (Vec<(String, Vec<u8>)>, Vec<String>);

/// Re-derive `--to-host` declarations from shipped script text and pack
/// the declared guest paths. The guest is authoritative for nothing here:
/// undeclared writes never enter the tarball, and missing sources fail
/// the exec loudly.
fn collect_push_entries(
    staging: &GuardedPath,
    script_text: &str,
) -> Result<PushEntries> {
    let steps: Vec<oxdock_parser::Step> =
        crate::parse_script(script_text).context("serve re-parse failed")?;
    let resolver = PathResolver::new(staging.root(), staging.root())?;
    let mut decls = Vec::new();
    fn visit(kind: &oxdock_parser::StepKind, out: &mut Vec<(String, String)>) {
        if let oxdock_parser::StepKind::Copy {
            to_host: true,
            from,
            to,
            ..
        } = kind
        {
            out.push((from.to_string(), to.to_string()));
        }
        kind.walk_child_kinds(&mut |child| visit(child, out));
    }
    for step in &steps {
        visit(&step.kind, &mut decls);
    }
    // Declarations resolve guest-locally (templates, no host scope): keep
    // to literal relative paths; dynamic forms bail loudly.
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for (guest_src, _host_dst) in &decls {
        if guest_src.contains("{{") || guest_src.contains('$') {
            bail!("serve cannot push dynamic path {guest_src:?}");
        }
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
                                let mut reader = resolver.open_read(&child)?;
                                let mut bytes = Vec::new();
                                reader.read_to_end(&mut bytes)?;
                                files.push((child_rel, bytes));
                            }
                        }
                    }
                }
            }
            Ok(_) => {
                let mut reader = resolver.open_read(&src).map_err(|err| {
                    anyhow::anyhow!("guest push source {guest_src:?} unreadable: {err:#}")
                })?;
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes)?;
                files.push((guest_src.clone(), bytes));
            }
            Err(err) => {
                bail!("guest push source {guest_src:?} missing: {err:#}");
            }
        }
    }
    Ok((files, dirs))
}

fn sha256_hex(bytes: &[u8]) -> String {
    oxdock_remote_proto::sha256_hex(bytes)
}
