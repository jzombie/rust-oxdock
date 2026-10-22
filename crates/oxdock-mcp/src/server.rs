//! rmcp reactor: `ServerHandler` over a persistent [`McpSession`].
//!
//! The handler owns precomputed `Tool` entries plus the session behind a
//! blocking mutex. `list_tools` serves the table; `call_tool` maps JSON
//! arguments to ordered `Value` words, dispatches through the session,
//! and renders the return value plus captured output as content blocks.
//! Tool-level failures return `CallToolResult::error` (caller visible);
//! unroutable requests return protocol `ErrorData`.
//!
//! With watching enabled the server polls the script file and hot-swaps
//! the session plus tool table on change. A broken edit never replaces
//! the live session: the error goes to stderr and the old tools keep
//! serving until a good edit lands.

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result};
use oxdock_core::{FuncMeta, McpSession, StreamChunk, StreamHook, TeeStream, Value, value_to_json};
use oxdock_fs::{GuardedPath, LazyGuardedTempDir, PathResolver, WorkspaceFs};
use oxdock_process::DefaultProcessManager;
/// Logging/message imports live separately: the notification family is
/// deprecated by SEP-2577, but it remains the only content-carrying
/// mid-call channel in this rmcp version (see `stream_note`).
#[allow(deprecated)]
use rmcp::model::{LoggingLevel, LoggingMessageNotification, LoggingMessageNotificationParam};
use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptResponse,
        InitializeRequestParams, InitializeResult, ListPromptsResult, ListResourceTemplatesResult,
        ListResourcesResult, ListToolsResult, PaginatedRequestParams, ProgressNotification,
        ProgressNotificationParam, ReadResourceRequestParams, RequestParamsMeta,
        ServerNotification, Tool, ToolListChangedNotification,
    },
    service::RequestContext,
};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use crate::tools::{HashMapDoc, McpTool, build_tool_table, map_json_args};

/// Live session plus its tool table, swapped atomically on reload.
struct HandlerInner {
    session: McpSession<DefaultProcessManager>,
    tools: Vec<McpTool>,
    /// Snapshot backing-dir holds, one per (re)load. Unmaterialized
    /// handles are path-only, so retained reloads cost nothing on disk.
    _holds: Vec<Arc<LazyGuardedTempDir>>,
    /// Host-side load identity, reported by `MCP_STATUS`.
    status: StatusState,
}

/// What `MCP_STATUS` reports. `generation` starts at 1 and increments on
/// every successful reload (resets on process restart): agents compare it
/// across calls to detect tools changing underneath them. `script_sha256`
/// is the hex of the loaded file bytes: stable across restarts, so the
/// exact file version stays correlatable. `last_reload` records the most
/// recent reload attempt, including failures, so broken edits are
/// client-visible instead of stderr-only.
#[derive(Debug, Clone)]
struct StatusState {
    generation: u64,
    script_sha256: String,
    loaded_at_unix: u64,
    last_reload_ok: bool,
    last_reload_message: String,
}

/// Reserved host tool name. `EXPORT`ing it is rejected: the status tool
/// is synthesized by the host on every load.
pub const STATUS_TOOL_NAME: &str = "MCP_STATUS";

/// File-watch settings for hot reload. Cloned into the watcher task.
#[derive(Clone)]
pub struct WatchConfig {
    /// Script file to poll, read through the workspace guard.
    pub script_path: GuardedPath,
    /// Workspace root for session filesystems.
    pub workspace_root: GuardedPath,
    /// Host modules re-registered on every reload.
    pub modules: Vec<oxdock_core::HostModule<oxdock_process::DefaultProcessManager>>,
    /// Host types re-registered on every reload.
    pub types: Vec<&'static oxdock_core::TypeDescriptor>,
    /// Poll interval in milliseconds.
    pub poll_ms: u64,
}

/// Shared session plus its precomputed tool table.
#[derive(Clone)]
pub struct OxDockHandler {
    inner: Arc<Mutex<HandlerInner>>,
    watch: Option<WatchConfig>,
}

impl OxDockHandler {
    /// Build the handler: derive tool metadata, validate opaque types
    /// fail fast, and snapshot the table. The session must already be
    /// initialized (linear steps executed).
    pub fn new(
        session: McpSession<DefaultProcessManager>,
        metas: Vec<FuncMeta>,
        docs: &HashMapDoc,
    ) -> Result<Self> {
        reject_reserved_metas(&metas)?;
        let tools = build_tool_table(metas, docs)?;
        Self::from_parts(session, tools, Vec::new(), None, StatusState::initial(""))
    }

    pub(crate) fn from_built(built: BuiltInner, script_text: &str) -> Result<Self> {
        Self::from_parts(
            built.session,
            built.tools,
            vec![built.hold],
            None,
            StatusState::initial(script_text),
        )
    }

    fn from_parts(
        session: McpSession<DefaultProcessManager>,
        tools: Vec<McpTool>,
        holds: Vec<Arc<LazyGuardedTempDir>>,
        watch: Option<WatchConfig>,
        status: StatusState,
    ) -> Result<Self> {
        let mut tools = tools;
        tools.push(status_tool());
        Ok(Self {
            inner: Arc::new(Mutex::new(HandlerInner {
                session,
                tools,
                _holds: holds,
                status,
            })),
            watch,
        })
    }

    /// Attach file watching. The next `serve_stdio` spawns the poll task.
    pub fn with_watch(mut self, watch: WatchConfig) -> Self {
        self.watch = Some(watch);
        self
    }

    /// Bare tool names in source order, for tests and diagnostics.
    pub fn tool_names(&self) -> Vec<String> {
        match self.inner.lock() {
            Ok(inner) => inner.tools.iter().map(|tool| tool.name.clone()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Rebuild the session and tool table from fresh script text and swap
    /// them in atomically. Records the attempt in the status first: a
    /// broken edit keeps serving the old tools but reports the failure
    /// through `MCP_STATUS`.
    pub fn reload(
        &self,
        script_text: &str,
        modules: Vec<oxdock_core::HostModule<oxdock_process::DefaultProcessManager>>,
        types: Vec<&'static oxdock_core::TypeDescriptor>,
        workspace_root: &GuardedPath,
    ) -> Result<()> {
        let outcome = match Self::build_inner(script_text, modules, types, workspace_root) {
            Ok(outcome) => outcome,
            Err(err) => {
                self.note_failed_reload(format!("{err:#}"));
                return Err(err);
            }
        };
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("MCP session lock poisoned during reload"))?;
        inner.status.generation += 1;
        inner.status.script_sha256 = outcome.hash.clone();
        inner.status.loaded_at_unix = unix_now();
        inner.status.last_reload_ok = true;
        inner.status.last_reload_message = format!(
            "reload generation {}: {} tool(s)",
            inner.status.generation,
            outcome.tools.len(),
        );
        inner.session = outcome.session;
        let mut tools = outcome.tools;
        tools.push(status_tool());
        inner.tools = tools;
        inner._holds.push(outcome.hold);
        Ok(())
    }

    /// Record a failed reload without touching the live session, so the
    /// failure is client-visible through `MCP_STATUS`.
    fn note_failed_reload(&self, message: String) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.status.last_reload_ok = false;
            inner.status.last_reload_message = message;
        }
    }

    /// Current load generation. Starts at 1, increments per reload.
    pub fn generation(&self) -> u64 {
        self.inner
            .lock()
            .map(|inner| inner.status.generation)
            .unwrap_or(0)
    }

    /// The `MCP_STATUS` payload: generation, served tools, script hash,
    /// load time, and the last reload outcome.
    pub fn status_payload(&self) -> JsonValue {
        let Some(snapshot) = self.inner.lock().ok().map(|inner| {
            (
                inner.status.clone(),
                inner
                    .tools
                    .iter()
                    .map(|tool| tool.name.clone())
                    .collect::<Vec<_>>(),
            )
        }) else {
            return serde_json::json!({"ok": false, "error": "session lock poisoned"});
        };
        let (status, tools) = snapshot;
        serde_json::json!({
            "generation": status.generation,
            "tools": tools,
            "script_sha256": status.script_sha256,
            "loaded_at_unix": status.loaded_at_unix,
            "last_reload": {
                "ok": status.last_reload_ok,
                "message": status.last_reload_message,
            },
        })
    }

    /// Spawn the content poll task. On success the watcher also sends
    /// `notifications/tools/list_changed` so clients re-fetch instead of
    /// showing a frozen list. Broken edits log to stderr and keep the
    /// live session; only a good edit swaps.
    pub fn spawn_reloader(&self, peer: Option<Peer<RoleServer>>) {
        let Some(watch) = self.watch.clone() else {
            return;
        };
        let handler = self.clone();
        tokio::spawn(async move {
            // Seed with the live text so the first tick never rebuilds.
            let mut last: Option<String> = read_guarded(&watch.script_path).ok();
            let mut ticker = tokio::time::interval(Duration::from_millis(watch.poll_ms.max(100)));
            loop {
                ticker.tick().await;
                // Debounce editors that write in stages (temp file plus
                // rename): settle before reading so a torn write parses.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let text = match read_guarded(&watch.script_path) {
                    Ok(text) => text,
                    Err(err) => {
                        eprintln!("mcp reload: cannot read script: {err:#}");
                        continue;
                    }
                };
                if last.as_deref() == Some(text.as_str()) {
                    continue;
                }
                last = Some(text.clone());
                match handler.reload(
                    &text,
                    watch.modules.clone(),
                    watch.types.clone(),
                    &watch.workspace_root,
                ) {
                    Ok(()) => {
                        eprintln!(
                            "mcp reload: serving {} tool(s) from {}",
                            handler.tool_names().len(),
                            watch.script_path.display()
                        );
                        if let Some(peer) = &peer {
                            let note = ServerNotification::ToolListChangedNotification(
                                ToolListChangedNotification::default(),
                            );
                            if let Err(err) = peer.send_notification(note).await {
                                eprintln!("mcp reload: list_changed notify failed: {err:#}");
                            }
                        }
                    }
                    // reload() already recorded the failure for MCP_STATUS.
                    Err(err) => eprintln!("mcp reload: keeping live session: {err:#}"),
                }
            }
        });
    }

    fn find(&self, name: &str) -> Option<McpTool> {
        match self.inner.lock() {
            Ok(inner) => inner.tools.iter().find(|tool| tool.name == name).cloned(),
            Err(_) => None,
        }
    }
}

/// Build the lazy session filesystem plus its backing-dir hold. The hold
/// must outlive the session, so it rides in `HandlerInner`.
pub fn session_fs(root: &GuardedPath) -> Result<(Box<dyn WorkspaceFs>, Arc<LazyGuardedTempDir>)> {
    let mut resolver = PathResolver::new_lazy(root.clone()).context("guard workspace root")?;
    resolver.set_workspace_root(root.clone());
    let hold = resolver.snapshot_handle();
    Ok((Box::new(resolver), hold))
}

/// Read one script file through its own guard. Absolute script paths
/// carry their parent as guard root (`join_script`), which may differ
/// from the workspace root when the server was launched elsewhere;
/// rooting the read at the path itself keeps reload working there.
fn read_guarded(path: &GuardedPath) -> Result<String> {
    let anchor = path.root();
    let resolver = PathResolver::new(anchor, anchor).context("guard script root")?;
    resolver.read_to_string(path)
}

impl ServerHandler for OxDockHandler {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        let mut info = rmcp::model::ServerConfig::default();
        info.instructions =
            Some("OxDock script tools: each EXPORTed FUNC is one MCP tool.".to_string());
        let mut tools_cap = rmcp::model::ToolsCapability::default();
        tools_cap.list_changed = Some(true);
        info.capabilities.tools = Some(tools_cap);
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = match self.inner.lock() {
            Ok(inner) => inner.tools.iter().map(rmcp_tool_for).collect(),
            Err(_) => {
                return Err(McpError::new(
                    rmcp::model::ErrorCode::INTERNAL_ERROR,
                    "MCP session lock poisoned",
                    None,
                ));
            }
        };
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        // Host-synthesized status probe: answered from the handler state,
        // never dispatched into the script session.
        if request.name.as_ref() == STATUS_TOOL_NAME {
            let payload = self.status_payload();
            let mut content = vec![ContentBlock::json(&payload).map_err(|err| {
                McpError::new(
                    rmcp::model::ErrorCode::INTERNAL_ERROR,
                    format!("MCP content encode failed: {err}"),
                    None,
                )
            })?];
            content.push(ContentBlock::text(format!(
                "generation {}: {} tool(s), last reload ok={}",
                payload["generation"],
                payload["tools"]
                    .as_array()
                    .map(|tools| tools.len())
                    .unwrap_or(0),
                payload["last_reload"]["ok"],
            )));
            let mut result = CallToolResult::success(content);
            result.structured_content = Some(crate::tools::wrap_structured(payload));
            return Ok(result.into());
        }
        let tool = self.find(request.name.as_ref()).ok_or_else(|| {
            McpError::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                format!("unknown MCP tool `{}`", request.name),
                None,
            )
        })?;
        // Extracted before `arguments` moves below: the token borrow
        // must precede the partial move.
        let progress_token = request.progress_token();
        let args_json = request
            .arguments
            .map(JsonValue::Object)
            .unwrap_or(JsonValue::Null);
        let values: Vec<Value> = map_json_args(&tool.params, &args_json).map_err(|err| {
            McpError::new(
                rmcp::model::ErrorCode::INVALID_PARAMS,
                format!("{err:#}"),
                None,
            )
        })?;
        let name = tool.name.clone();
        let inner = Arc::clone(&self.inner);
        // Live stream: default-routed step output forwards as
        // notifications/message while the blocking call runs. Bounded
        // channel with try_send: a slow client never blocks script
        // execution, overflow drops and counts. Explicit WITH_IO
        // redirections bypass the tee, so redirected bytes stay silent
        // here exactly as they stay off the terminal outside MCP.
        let (stream_tx, stream_rx) = std::sync::mpsc::sync_channel::<StreamChunk>(512);
        let stream_dropped = Arc::new(AtomicUsize::new(0));
        let hook = StreamHook {
            tx: stream_tx,
            dropped: Arc::clone(&stream_dropped),
        };
        let peer = context.peer.clone();
        let rt = tokio::runtime::Handle::current();
        let stream_tool = tool.name.clone();
        // Standards-first streaming: a client-supplied progress token
        // addresses progress notifications (not deprecated, content in
        // `message`, byte count as progress). Without a token there is
        // nothing to address progress to, so chunks fall back to
        // notifications/message. Each chunk goes out exactly once, on
        // the best available channel; when rmcp removes the deprecated
        // family, only the fallback branch goes with it. No fork.
        let forwarder = tokio::task::spawn_blocking(move || {
            // Frame coalescing: stage bytes per stream and flush one
            // notification per batch. Same bytes, far fewer envelopes.
            const COALESCE_CAP: usize = 4096;
            const COALESCE_IDLE: Duration = Duration::from_millis(50);
            let mut bytes_so_far: u64 = 0;
            let mut coal = Coalescer::new(COALESCE_CAP);
            let send_batch = |tool: &str,
                              token: Option<&rmcp::model::ProgressToken>,
                              total: u64,
                              batch: StreamChunk| {
                let note = stream_notification(tool, token, total, &batch);
                // Best effort: a gone client fails sends until the call
                // ends and the channel closes; the thread then exits.
                let _ = rt.block_on(peer.send_notification(note));
            };
            loop {
                match stream_rx.recv_timeout(COALESCE_IDLE) {
                    Ok(chunk) => {
                        bytes_so_far += chunk.bytes.len() as u64;
                        for batch in coal.push(chunk.kind, &chunk.bytes) {
                            send_batch(&stream_tool, progress_token.as_ref(), bytes_so_far, batch);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        for batch in coal.flush() {
                            send_batch(&stream_tool, progress_token.as_ref(), bytes_so_far, batch);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            for batch in coal.flush() {
                send_batch(&stream_tool, progress_token.as_ref(), bytes_so_far, batch);
            }
        });
        let output = tokio::task::spawn_blocking(move || {
            let mut guard = inner
                .lock()
                .map_err(|_| anyhow::anyhow!("MCP session lock poisoned during `{name}`"))?;
            guard.session.set_stream_hook(Some(hook));
            let result = guard.session.call(&name, values);
            guard.session.set_stream_hook(None);
            result
        })
        .await
        .map_err(|err| {
            McpError::new(
                rmcp::model::ErrorCode::INTERNAL_ERROR,
                format!("MCP tool task failed: {err}"),
                None,
            )
        })?
        .map_err(|err| {
            McpError::new(
                rmcp::model::ErrorCode::INTERNAL_ERROR,
                format!("{err:#}"),
                None,
            )
        })?;
        // Forwarder drains on tx drop (the session closure returned, on
        // either path). Join so every streamed byte precedes the final
        // result; then report transit drops, if any.
        let _ = forwarder.await;
        let dropped = stream_dropped.load(Ordering::Relaxed);
        let json = value_to_json(&output.value).map_err(|err| {
            McpError::new(
                rmcp::model::ErrorCode::INTERNAL_ERROR,
                format!(
                    "MCP tool `{}` returned an unserializable value: {err:#}",
                    tool.name
                ),
                None,
            )
        })?;
        let mut content = vec![ContentBlock::json(&json).map_err(|err| {
            McpError::new(
                rmcp::model::ErrorCode::INTERNAL_ERROR,
                format!("MCP content encode failed: {err}"),
                None,
            )
        })?];
        if dropped > 0 {
            content.push(ContentBlock::text(format!(
                "[live stream dropped {dropped} chunk(s) under backpressure]"
            )));
        }
        let mut result = CallToolResult::success(content);
        result.structured_content = Some(crate::tools::wrap_structured(json));
        Ok(result.into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult {
            ..Default::default()
        })
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult {
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        _request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, McpError> {
        Err(McpError::new(
            rmcp::model::ErrorCode::METHOD_NOT_FOUND,
            "no resources",
            None,
        ))
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        Ok(ListPromptsResult {
            ..Default::default()
        })
    }

    async fn get_prompt(
        &self,
        _request: rmcp::model::GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
        Err(McpError::new(
            rmcp::model::ErrorCode::METHOD_NOT_FOUND,
            "no prompts",
            None,
        ))
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }
}

/// One live output fragment as a client notification. Progress first
/// when the request carried a token (standards-clean, content in
/// `message`, byte count as progress, no total since the length is
/// unknowable mid-run); notifications/message fallback otherwise.
/// `notifications/message` is deprecated by SEP-2577 but remains the
/// only content-carrying mid-call channel for token-less clients in
/// this rmcp version; only the fallback branch depends on it.
#[allow(deprecated)]
fn stream_notification(
    tool: &str,
    token: Option<&rmcp::model::ProgressToken>,
    bytes_so_far: u64,
    chunk: &StreamChunk,
) -> ServerNotification {
    let text = String::from_utf8_lossy(&chunk.bytes).into_owned();
    if let Some(token) = token {
        let param =
            ProgressNotificationParam::new(token.clone(), bytes_so_far as f64).with_message(text);
        return ServerNotification::ProgressNotification(ProgressNotification::new(param));
    }
    let (level, text) = match chunk.kind {
        TeeStream::Stdout => (
            LoggingLevel::Info,
            String::from_utf8_lossy(&chunk.bytes).into_owned(),
        ),
        TeeStream::Stderr => (
            LoggingLevel::Warning,
            String::from_utf8_lossy(&chunk.bytes).into_owned(),
        ),
    };
    let param = LoggingMessageNotificationParam::new(level, serde_json::Value::String(text))
        .with_logger(tool.to_string());
    ServerNotification::LoggingMessageNotification(LoggingMessageNotification::new(param))
}

/// Frame coalescer: stages bytes per stream, emits ready batches.
/// Same bytes, fewer envelopes. Bounded and constant: two `cap`-byte
/// stagers, flushed on fill, idle timeout, stream switch, and drain.
/// Drop accounting lives upstream at ingress, untouched by batching.
struct Coalescer {
    out: Vec<u8>,
    err: Vec<u8>,
    cap: usize,
}

impl Coalescer {
    fn new(cap: usize) -> Self {
        Self {
            out: Vec::new(),
            err: Vec::new(),
            cap,
        }
    }

    /// Stage one write; returns batches ready to send now, in order.
    /// A stream switch flushes the other side first so interleaving
    /// stays faithful; a fill flushes the writer's own stage first.
    fn push(&mut self, kind: TeeStream, bytes: &[u8]) -> Vec<StreamChunk> {
        let mut ready = Vec::new();
        let (own, other_kind, other) = match kind {
            TeeStream::Stdout => (&mut self.out, TeeStream::Stderr, &mut self.err),
            TeeStream::Stderr => (&mut self.err, TeeStream::Stdout, &mut self.out),
        };
        if !other.is_empty() {
            ready.push(StreamChunk {
                kind: other_kind,
                bytes: std::mem::take(other),
            });
        }
        if own.len() + bytes.len() > self.cap && !own.is_empty() {
            ready.push(StreamChunk {
                kind,
                bytes: std::mem::take(own),
            });
        }
        own.extend_from_slice(bytes);
        ready
    }

    /// Drain both stagers, stdout then stderr. Empty stages emit nothing.
    fn flush(&mut self) -> Vec<StreamChunk> {
        let mut ready = Vec::new();
        if !self.out.is_empty() {
            ready.push(StreamChunk {
                kind: TeeStream::Stdout,
                bytes: std::mem::take(&mut self.out),
            });
        }
        if !self.err.is_empty() {
            ready.push(StreamChunk {
                kind: TeeStream::Stderr,
                bytes: std::mem::take(&mut self.err),
            });
        }
        ready
    }
}

fn rmcp_tool_for(tool: &McpTool) -> Tool {
    let schema_map = match &tool.input_schema {
        JsonValue::Object(map) => map.clone(),
        _ => Default::default(),
    };
    let output_map = match &tool.output_schema {
        JsonValue::Object(map) => map.clone(),
        _ => Default::default(),
    };
    Tool::new(tool.name.clone(), tool.description.clone(), schema_map)
        .with_raw_output_schema(Arc::new(output_map))
}

/// Reject an `EXPORT` of the synthesized host tool name.
fn reject_reserved_metas(metas: &[FuncMeta]) -> Result<()> {
    for meta in metas {
        if oxdock_parser::base_name(&meta.name) == STATUS_TOOL_NAME {
            anyhow::bail!("{STATUS_TOOL_NAME} is reserved for the host status tool");
        }
    }
    Ok(())
}

/// The synthesized host status tool, appended after every export table.
fn status_tool() -> McpTool {
    use crate::tools::output_schema_for;
    let mut input = serde_json::Map::new();
    input.insert("type".to_string(), JsonValue::String("object".to_string()));
    let input_schema = JsonValue::Object(input);
    McpTool {
        name: STATUS_TOOL_NAME.to_string(),
        qualified: "MCP::STATUS".to_string(),
        description: "Host status: load generation, served tools, script hash, and the last reload outcome. Generation starts at 1 and increments per successful reload; compare it across calls to detect tools changing underneath you.".to_string(),
        input_schema,
        output_schema: output_schema_for(&status_meta()).unwrap_or(JsonValue::Object(serde_json::Map::new())),
        params: Vec::new(),
    }
}

fn status_meta() -> FuncMeta {
    FuncMeta {
        name: "MCP::STATUS".to_string(),
        module: "MCP".to_string(),
        kind: oxdock_core::FuncKind::Script,
        params: Some(Vec::new()),
        returns: None,
        rpn: false,
        summary: "Host status probe.",
        docs: "Reports the live session identity.",
        examples: &[],
    }
}

impl StatusState {
    fn initial(script_text: &str) -> Self {
        Self {
            generation: 1,
            script_sha256: script_hash(script_text),
            loaded_at_unix: unix_now(),
            last_reload_ok: true,
            last_reload_message: "initial load".to_string(),
        }
    }
}

fn script_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// One validated rebuild: parse, init the session, derive the table.
/// The single assembly sequence shared by initial load and hot reload,
/// so startup and reload cannot drift apart.
pub(crate) struct BuiltInner {
    pub(crate) session: McpSession<DefaultProcessManager>,
    pub(crate) tools: Vec<McpTool>,
    pub(crate) hash: String,
    pub(crate) hold: Arc<LazyGuardedTempDir>,
}

impl OxDockHandler {
    pub(crate) fn build_inner(
        script_text: &str,
        modules: Vec<oxdock_core::HostModule<oxdock_process::DefaultProcessManager>>,
        types: Vec<&'static oxdock_core::TypeDescriptor>,
        workspace_root: &GuardedPath,
    ) -> Result<BuiltInner> {
        let mut engine = oxdock_core::Engine::new();
        for module in &modules {
            engine.register_module(module.clone());
        }
        let parsed =
            oxdock_core::parse_script_with_modules_and_exports(script_text, engine.module_table())?;
        if parsed.exports.is_empty() {
            anyhow::bail!("no EXPORT entries: add `EXPORT [NAME, ...]` to expose MCP tools");
        }
        if parsed.exports.iter().any(|name| name == STATUS_TOOL_NAME) {
            anyhow::bail!("{STATUS_TOOL_NAME} is reserved for the host status tool");
        }
        let (fs, hold) = session_fs(workspace_root)?;
        let session = McpSession::init(
            fs,
            oxdock_process::default_process_manager(),
            modules,
            types,
            &parsed,
        )?;
        let metas = session.tool_metas()?;
        reject_reserved_metas(&metas)?;
        let mut docs: HashMap<String, (String, String)> = HashMap::new();
        for qualified in metas.iter().map(|meta| meta.name.clone()) {
            if let Some(pair) = session.tool_doc(&qualified) {
                docs.insert(qualified, pair);
            }
        }
        let tools = build_tool_table(metas, &docs)?;
        Ok(BuiltInner {
            session,
            tools,
            hash: script_hash(script_text),
            hold,
        })
    }
}

/// Script docs keyed by qualified name, for handler construction.
pub type DocsMap = HashMap<String, (String, String)>;

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{NumberOrString, ProgressToken};

    fn chunk(kind: TeeStream, bytes: &[u8]) -> StreamChunk {
        StreamChunk {
            kind,
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn token_present_prefers_progress_with_content() {
        let token = ProgressToken(NumberOrString::Number(7));
        let note = stream_notification("TOOL", Some(&token), 42, &chunk(TeeStream::Stdout, b"hi"));
        let ServerNotification::ProgressNotification(progress) = note else {
            panic!("expected progress notification, got something else");
        };
        assert_eq!(progress.params.progress, 42.0);
        assert_eq!(progress.params.message.as_deref(), Some("hi"));
    }

    #[test]
    #[allow(deprecated)]
    fn token_absent_falls_back_to_logging_message() {
        let note = stream_notification("TOOL", None, 42, &chunk(TeeStream::Stderr, b"oops"));
        let ServerNotification::LoggingMessageNotification(log) = note else {
            panic!("expected logging notification, got something else");
        };
        assert_eq!(log.params.level, LoggingLevel::Warning);
        assert_eq!(log.params.logger.as_deref(), Some("TOOL"));
        assert_eq!(
            log.params.data,
            serde_json::Value::String("oops".to_string())
        );
    }

    #[test]
    fn coalescer_batches_small_writes_and_flushes_on_drain() {
        let mut coal = Coalescer::new(4096);
        assert!(coal.push(TeeStream::Stdout, b"tick ").is_empty());
        assert!(coal.push(TeeStream::Stdout, b"one").is_empty());
        let ready = coal.push(TeeStream::Stdout, b"\n");
        assert!(ready.is_empty(), "under cap: nothing emitted yet");
        let drained = coal.flush();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].kind, TeeStream::Stdout);
        assert_eq!(drained[0].bytes, b"tick one\n");
        assert!(coal.flush().is_empty(), "drained stays drained");
    }

    #[test]
    fn coalescer_splits_at_cap_and_keeps_stream_attribution() {
        let mut coal = Coalescer::new(8);
        assert!(coal.push(TeeStream::Stdout, b"12345678").is_empty());
        let ready = coal.push(TeeStream::Stdout, b"more");
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].bytes, b"12345678");
        let drained = coal.flush();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].bytes, b"more");
    }

    #[test]
    fn coalescer_flushes_other_stream_first_on_switch() {
        let mut coal = Coalescer::new(4096);
        assert!(coal.push(TeeStream::Stdout, b"out").is_empty());
        let ready = coal.push(TeeStream::Stderr, b"err");
        assert_eq!(ready.len(), 1, "switch flushes the waiting side first");
        assert_eq!(ready[0].kind, TeeStream::Stdout);
        assert_eq!(ready[0].bytes, b"out");
        let drained = coal.flush();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].kind, TeeStream::Stderr);
        assert_eq!(drained[0].bytes, b"err");
    }
}
