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

use anyhow::Result;

/// Parsed script plus the host surface needed to serve it.
pub struct McpServerConfig {
    /// Workspace root for session filesystems.
    pub workspace_root: oxdock_fs::GuardedPath,
    /// Script file, watched for hot reload when enabled.
    pub script_path: oxdock_fs::GuardedPath,
    /// Script text (already read; the CLI owns file IO).
    pub script: String,
    /// extra host modules beyond STD (NET/SSH/MARKDOWN when enabled).
    pub modules: Vec<oxdock_core::HostModule<oxdock_process::DefaultProcessManager>>,
    /// extra host types beyond startup descriptors.
    pub types: Vec<&'static oxdock_core::TypeDescriptor>,
    /// Poll the script file and hot-swap the session on change.
    pub watch: bool,
}

/// Poll interval for hot reload.
pub const WATCH_POLL_MS: u64 = 500;

/// Initialize the session: parse, execute linear steps once, validate
/// `EXPORT`, and build the rmcp handler. Fails fast on opaque types.
/// Assembly rides the same [`OxDockHandler::build_inner`] sequence as
/// hot reload, so startup and reload cannot drift apart.
pub fn init_handler(config: McpServerConfig) -> Result<OxDockHandler> {
    let built = OxDockHandler::build_inner(
        &config.script,
        config.modules.clone(),
        config.types.clone(),
        &config.workspace_root,
    )?;
    let handler = OxDockHandler::from_built(built, &config.script)?;
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
/// hot-reload poll task first when watching is configured, handing it the
/// peer so successful reloads also emit `tools/list_changed`.
pub async fn serve_stdio(handler: OxDockHandler) -> Result<()> {
    use rmcp::ServiceExt;
    let transport = rmcp::transport::io::stdio();
    let reloader = handler.clone();
    let service = handler
        .serve(transport)
        .await
        .map_err(|err| anyhow::anyhow!("MCP stdio serve failed: {err}"))?;
    reloader.spawn_reloader(Some(service.peer().clone()));
    service
        .waiting()
        .await
        .map_err(|err| anyhow::anyhow!("MCP stdio service failed: {err}"))?;
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
