//! Thin `--mcp` wrapper: read the script, build the host surface, hand
//! everything to `oxdock-mcp`, and block on the stdio reactor.
//!
//! The CLI owns argument parsing, workspace guards, and endpoint binding;
//! `oxdock-mcp` owns the tool table, the JSON boundary, the JSON-RPC loop,
//! and hot reload (the script file is polled and the session swaps live).

use std::io::IsTerminal;

use anyhow::{Result, bail};

use crate::{Options, ScriptSource, cli_host_modules_with, cli_host_types, read_script};

/// Serve `EXPORT`ed FUNCs as MCP tools over stdio. Reads the script file,
/// binds endpoint sockets up front (fail fast), initializes the session
/// (linear steps run once), then serves until the client disconnects.
/// Edits to the script file hot-swap the session; broken edits keep
/// serving the old tools with a stderr note.
pub fn serve_mcp(opts: Options, workspace_root: oxdock_fs::GuardedPath) -> Result<()> {
    if std::io::stdin().is_terminal() || std::io::stdout().is_terminal() {
        bail!("--mcp requires piped stdio transport");
    }
    let ScriptSource::Path(path) = &opts.script else {
        bail!("--mcp requires a script file (stdin carries the JSON-RPC stream)");
    };
    let script_path = path.clone();
    let script = read_script(&opts.script)?;

    #[cfg(feature = "net")]
    let registry = crate::build_registry(&opts.endpoints)?;
    #[cfg(feature = "net")]
    let modules = cli_host_modules_with(Some(&registry));
    #[cfg(not(feature = "net"))]
    let modules = {
        crate::check_no_net_endpoints(&opts.endpoints)?;
        crate::check_no_net_remotes(&opts.remotes)?;
        cli_host_modules_with(None)
    };
    let types = cli_host_types();

    let handler = oxdock_mcp::init_handler(oxdock_mcp::McpServerConfig {
        workspace_root,
        script_path,
        script,
        modules,
        types,
        watch: true,
    })?;
    oxdock_mcp::serve_stdio_blocking(handler)
}
