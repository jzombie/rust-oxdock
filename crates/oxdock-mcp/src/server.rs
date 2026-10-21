//! rmcp reactor: `ServerHandler` over a persistent [`McpSession`].
//!
//! The handler owns precomputed `Tool` entries plus the session behind a
//! blocking mutex. `list_tools` serves the table; `call_tool` maps JSON
//! arguments to ordered `Value` words, dispatches through the session,
//! and renders the return value plus captured output as content blocks.
//! Tool-level failures return `CallToolResult::error` (caller visible);
//! unroutable requests return protocol `ErrorData`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use oxdock_core::{FuncMeta, McpSession, Value, value_to_json};
use oxdock_process::DefaultProcessManager;
use rmcp::{
    ErrorData as McpError,
    RoleServer,
    ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        GetPromptResponse, InitializeRequestParams, InitializeResult, ListPromptsResult,
        ListResourceTemplatesResult, ListResourcesResult, ListToolsResult,
        PaginatedRequestParams, ReadResourceResponse, Tool,
    },
    service::RequestContext,
};
use serde_json::Value as JsonValue;

use crate::tools::{HashMapDoc, McpTool, build_tool_table, map_json_args};

/// Shared session plus its precomputed tool table.
#[derive(Clone)]
pub struct OxDockHandler {
    session: Arc<Mutex<McpSession<DefaultProcessManager>>>,
    tools: Vec<McpTool>,
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
        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            tools,
        })
    }

    fn find(&self, name: &str) -> Option<&McpTool> {
        self.tools.iter().find(|tool| tool.name == name)
    }
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
        let tools: Vec<Tool> = self.tools.iter().map(rmcp_tool_for).collect();
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
        let session = Arc::clone(&self.session);
        let output = tokio::task::spawn_blocking(move || {
            let mut guard = session.lock().map_err(|_| {
                anyhow::anyhow!("MCP session lock poisoned during `{name}`")
            })?;
            guard.call(&name, values)
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
        _request: rmcp::model::ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
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
