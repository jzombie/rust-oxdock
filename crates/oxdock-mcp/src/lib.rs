//! MCP tool hosting for exported OxDock script functions.
//!
//! The reactor keeps one [`McpSession`] for the process lifetime: linear
//! init once, then `tools/list` serves the precomputed table and each
//! `tools/call` dispatches through the live state. Capture sinks drain
//! into the response and truncate to zero after every call. With watching
//! enabled the server polls the script file and hot-swaps the session on
//! change; broken edits keep serving the old tools.

pub mod schema;
pub mod server;
pub mod tools;

pub use server::{DocsMap, OxDockHandler, WatchConfig, session_fs};
pub use tools::{
    HashMapDoc, McpTool, build_tool_table, map_json_args, output_schema_for, value_to_json_result,
    wrap_structured,
};

use std::collections::HashMap;

use anyhow::{Result, bail};
use oxdock_core::{Engine, McpSession};

/// Parsed script plus the host surface needed to serve it.
pub struct McpServerConfig {
    /// Workspace root for session filesystems.
    pub workspace_root: oxdock_fs::GuardedPath,
    /// Script file, watched for hot reload when enabled.
    pub script_path: oxdock_fs::GuardedPath,
    /// Script text (already read; the CLI owns file IO).
    pub script: String,
    /// extra host modules beyond STD (NET/SSH/MARKDOWN when enabled).
    pub modules: Vec<
        oxdock_core::HostModule<oxdock_process::DefaultProcessManager>,
    >,
    /// extra host types beyond startup descriptors.
    pub types: Vec<&'static oxdock_core::TypeDescriptor>,
    /// Poll the script file and hot-swap the session on change.
    pub watch: bool,
}

/// Poll interval for hot reload.
pub const WATCH_POLL_MS: u64 = 500;

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
    if parsed
        .exports
        .iter()
        .any(|name| name == server::STATUS_TOOL_NAME)
    {
        bail!(
            "{} is reserved for the host status tool",
            server::STATUS_TOOL_NAME
        );
    }
    let (fs, hold) = session_fs(&config.workspace_root)?;
    let session = McpSession::init(
        fs,
        oxdock_process::default_process_manager(),
        config.modules.clone(),
        config.types.clone(),
        &parsed,
    )?;
    let metas = session.tool_metas()?;
    let mut docs: HashMap<String, (String, String)> = HashMap::new();
    for qualified in metas.iter().map(|meta| meta.name.clone()) {
        if let Some(pair) = session.tool_doc(&qualified) {
            docs.insert(qualified, pair);
        }
    }
    let handler = OxDockHandler::from_handler_parts_with_status(
        session,
        metas,
        &docs,
        vec![hold],
        &config.script,
    )?;
    if config.watch {
        Ok(handler.with_watch(WatchConfig {
            script_path: config.script_path,
            workspace_root: config.workspace_root,
            modules: config.modules,
            types: config.types,
            poll_ms: WATCH_POLL_MS,
        }))
    } else {
        Ok(handler)
    }
}

/// Serve the handler over stdio until the client disconnects. Spawns the
/// hot-reload poll task first when watching is configured.
pub async fn serve_stdio(handler: OxDockHandler) -> Result<()> {
    handler.spawn_reloader();
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
