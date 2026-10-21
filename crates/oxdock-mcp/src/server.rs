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
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        GetPromptResponse, InitializeRequestParams, InitializeResult, ListPromptsResult,
        ListResourceTemplatesResult, ListResourcesResult, ListToolsResult,
        PaginatedRequestParams, ReadResourceRequestParams, Tool,
    },
    service::RequestContext,
};
use serde_json::Value as JsonValue;

use crate::tools::{HashMapDoc, McpTool, build_tool_table, map_json_args};

/// Live session plus its tool table, swapped atomically on reload.
struct HandlerInner {
    session: McpSession<DefaultProcessManager>,
    tools: Vec<McpTool>,
    /// Snapshot backing-dir holds, one per (re)load. Unmaterialized
    /// handles are path-only, so retained reloads cost nothing on disk.
    _holds: Vec<Arc<LazyGuardedTempDir>>,
}

/// File-watch settings for hot reload. Cloned into the watcher task.
#[derive(Clone)]
pub struct WatchConfig {
    /// Script file to poll, read through the workspace guard.
    pub script_path: GuardedPath,
    /// Workspace root for session filesystems.
    pub workspace_root: GuardedPath,
    /// Host modules re-registered on every reload.
    pub modules:
        Vec<oxdock_core::HostModule<oxdock_process::DefaultProcessManager>>,
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
        let tools = build_tool_table(metas, docs)?;
        Self::from_parts(session, tools, Vec::new(), None)
    }

    pub(crate) fn from_handler_parts(
        session: McpSession<DefaultProcessManager>,
        metas: Vec<FuncMeta>,
        docs: &HashMapDoc,
        holds: Vec<Arc<LazyGuardedTempDir>>,
    ) -> Result<Self> {
        let tools = build_tool_table(metas, docs)?;
        Self::from_parts(session, tools, holds, None)
    }

    fn from_parts(
        session: McpSession<DefaultProcessManager>,
        tools: Vec<McpTool>,
        holds: Vec<Arc<LazyGuardedTempDir>>,
        watch: Option<WatchConfig>,
    ) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Mutex::new(HandlerInner {
                session,
                tools,
                _holds: holds,
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
    /// them in atomically. Fails without touching the live session, so a
    /// broken edit keeps serving the old tools.
    pub fn reload(
        &self,
        script_text: &str,
        modules: Vec<
            oxdock_core::HostModule<oxdock_process::DefaultProcessManager>,
        >,
        types: Vec<&'static oxdock_core::TypeDescriptor>,
        workspace_root: &GuardedPath,
    ) -> Result<()> {
        let mut engine = oxdock_core::Engine::new();
        for module in &modules {
            engine.register_module(module.clone());
        }
        let parsed = oxdock_core::parse_script_with_modules_and_exports(
            script_text,
            engine.module_table(),
        )?;
        if parsed.exports.is_empty() {
            anyhow::bail!("reload: no EXPORT entries, keeping live session");
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
        let mut docs: HashMap<String, (String, String)> = HashMap::new();
        for qualified in metas.iter().map(|meta| meta.name.clone()) {
            if let Some(pair) = session.tool_doc(&qualified) {
                docs.insert(qualified, pair);
            }
        }
        let tools = build_tool_table(metas, &docs)?;
        let mut inner = self.inner.lock().map_err(|_| {
            anyhow::anyhow!("MCP session lock poisoned during reload")
        })?;
        inner.session = session;
        inner.tools = tools;
        inner._holds.push(hold);
        Ok(())
    }

    /// Spawn the content poll task. Broken edits log to stderr and keep
    /// the live session; only a good edit swaps.
    pub fn spawn_reloader(&self) {
        let Some(watch) = self.watch.clone() else {
            return;
        };
        let handler = self.clone();
        tokio::spawn(async move {
            // Seed with the live text so the first tick never rebuilds.
            let mut last: Option<String> =
                read_guarded(&watch.workspace_root, &watch.script_path).ok();
            let mut ticker =
                tokio::time::interval(Duration::from_millis(watch.poll_ms.max(100)));
            loop {
                ticker.tick().await;
                // Debounce editors that write in stages (temp file plus
                // rename): settle before reading so a torn write parses.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let text = match read_guarded(&watch.workspace_root, &watch.script_path) {
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
                match handler.reload(&text, watch.modules.clone(), watch.types.clone(), &watch.workspace_root) {
                    Ok(()) => eprintln!(
                        "mcp reload: serving {} tool(s) from {}",
                        handler.tool_names().len(),
                        watch.script_path.display()
                    ),
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
pub fn session_fs(
    root: &GuardedPath,
) -> Result<(Box<dyn WorkspaceFs>, Arc<LazyGuardedTempDir>)> {
    let mut resolver = PathResolver::new_lazy(root.clone()).context("guard workspace root")?;
    resolver.set_workspace_root(root.clone());
    let hold = resolver.snapshot_handle();
    Ok((Box::new(resolver), hold))
}

/// Read one workspace file through the guard. The watcher stays on
/// guarded reads so edits outside the root never load.
fn read_guarded(root: &GuardedPath, path: &GuardedPath) -> Result<String> {
    let resolver =
        PathResolver::new(root.as_path(), root.as_path()).context("guard workspace root")?;
    resolver.read_to_string(path)
}

impl ServerHandler for OxDockHandler {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        let mut info = rmcp::model::ServerConfig::default();
        info.instructions = Some(
            "OxDock script tools: each EXPORTed FUNC is one MCP tool.".to_string(),
        );
        info.capabilities.tools = Some(Default::default());
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
            let mut guard = inner.lock().map_err(|_| {
                anyhow::anyhow!("MCP session lock poisoned during `{name}`")
            })?;
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
                format!("MCP tool `{}` returned an unserializable value: {err:#}", tool.name),
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

/// Script docs keyed by qualified name, for handler construction.
pub type DocsMap = HashMap<String, (String, String)>;
