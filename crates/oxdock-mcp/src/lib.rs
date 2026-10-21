//! MCP tool hosting for exported OxDock script functions.
//!
//! The reactor keeps one [`McpSession`] for the process lifetime: linear
//! init once, then `tools/list` serves the precomputed table and each
//! `tools/call` dispatches through the live state. Capture sinks drain
//! into the response and truncate to zero after every call.

pub mod schema;
pub mod server;
pub mod tools;

pub use server::{DocsMap, OxDockHandler};
pub use tools::{
    HashMapDoc, McpTool, build_tool_table, map_json_args, output_schema_for, value_to_json_result,
    wrap_structured,
};

use std::collections::HashMap;

use anyhow::{Result, bail};
use oxdock_core::{Engine, McpSession};
use oxdock_process::default_process_manager;

/// Parsed script plus the host surface needed to serve it.
pub struct McpServerConfig {
    /// Execution filesystem (the CLI hands its lazy snapshot resolver).
    pub fs: Box<dyn oxdock_fs::WorkspaceFs>,
    /// Script text (already read; the CLI owns file IO).
    pub script: String,
    /// extra host modules beyond STD (NET/SSH/MARKDOWN when enabled).
    pub modules: Vec<
        oxdock_core::HostModule<oxdock_process::DefaultProcessManager>,
    >,
    /// extra host types beyond startup descriptors.
    pub types: Vec<&'static oxdock_core::TypeDescriptor>,
}

/// Initialize the session: parse, execute linear steps once, validate
/// `EXPORT`, and build the rmcp handler. Fails fast on opaque types.
pub fn init_handler(config: McpServerConfig) -> Result<OxDockHandler> {
    let mut engine = Engine::new();
    for module in &config.modules {
        engine.register_module(module.clone());
    }
    let parsed =
        oxdock_core::parse_script_with_modules_and_exports(&config.script, engine.module_table())?;
    if parsed.exports.is_empty() {
        bail!("no EXPORT entries: add `EXPORT [NAME, ...]` to expose MCP tools");
    }
    let session = McpSession::init(
        config.fs,
        default_process_manager(),
        config.modules,
        config.types,
        &parsed,
    )?;
    let metas = session.tool_metas()?;
    let mut docs: HashMap<String, (String, String)> = HashMap::new();
    for qualified in metas.iter().map(|meta| meta.name.clone()) {
        if let Some(pair) = session.tool_doc(&qualified) {
            docs.insert(qualified, pair);
        }
    }
    OxDockHandler::new(session, metas, &docs)
}

/// Serve the handler over stdio until the client disconnects.
pub async fn serve_stdio(handler: OxDockHandler) -> Result<()> {
    use rmcp::ServiceExt;
    let transport = rmcp::transport::io::stdio();
    let service = handler.serve(transport).await.map_err(|err| {
        anyhow::anyhow!("MCP stdio serve failed: {err}")
    })?;
    service.waiting().await.map_err(|err| {
        anyhow::anyhow!("MCP stdio service failed: {err}")
    })?;
    Ok(())
}

/// Blocking entry for hosts without a tokio runtime (the CLI): builds a
/// current-thread runtime and serves until disconnect.
pub fn serve_stdio_blocking(handler: OxDockHandler) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| anyhow::anyhow!("build MCP tokio runtime: {err}"))?;
    runtime.block_on(serve_stdio(handler))
}
