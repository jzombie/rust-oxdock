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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use oxdock_core::{FuncMeta, McpSession, Value, value_to_json};
use oxdock_fs::{GuardedPath, LazyGuardedTempDir, PathResolver, WorkspaceFs};
use oxdock_process::DefaultProcessManager;
use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptResponse,
        InitializeRequestParams, InitializeResult, ListPromptsResult, ListResourceTemplatesResult,
        ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams,
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
        _context: RequestContext<RoleServer>,
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
        let output = tokio::task::spawn_blocking(move || {
            let mut guard = inner
                .lock()
                .map_err(|_| anyhow::anyhow!("MCP session lock poisoned during `{name}`"))?;
            guard.session.call(&name, values)
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
        if !output.stdout.is_empty() {
            content.push(ContentBlock::text(output.stdout));
        }
        if !output.stderr.is_empty() {
            content.push(ContentBlock::text(output.stderr));
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
