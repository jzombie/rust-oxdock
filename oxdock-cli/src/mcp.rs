//! Thin `--mcp` wrapper: read the script, build the host surface, hand
//! everything to `oxdock-mcp`, and block on the stdio reactor.
//!
//! The CLI owns argument parsing, workspace guards, and endpoint binding;
//! `oxdock-mcp` owns the tool table, JSON boundary, and JSON-RPC loop.

use std::io::IsTerminal;

use anyhow::{Context, Result, bail};
use oxdock_fs::{GuardedPath, PathResolver, WorkspaceFs};

use crate::{Options, ScriptSource, cli_host_modules_with, cli_host_types, read_script};

/// Serve `EXPORT`ed FUNCs as MCP tools over stdio. Reads the script file,
/// binds endpoint sockets up front (fail fast), initializes the session
/// (linear steps run once), then serves until the client disconnects.
pub fn serve_mcp(opts: Options, workspace_root: GuardedPath) -> Result<()> {
    if std::io::stdin().is_terminal() || std::io::stdout().is_terminal() {
        bail!("--mcp requires piped stdio transport");
    }
    let ScriptSource::Path(_path) = &opts.script else {
        bail!("--mcp requires a script file (stdin carries the JSON-RPC stream)");
    };
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

    // Lazy snapshot backing for session execution: materializes only if
    // the script touches snapshot-targeted paths. Held for the session
    // lifetime so the backing dir outlives the reactor.
    let mut resolver = PathResolver::new_lazy(workspace_root.clone())
        .context("guard workspace root for --mcp")?;
    resolver.set_workspace_root(workspace_root.clone());
    let _snapshot = resolver.snapshot_handle();
    let fs: Box<dyn WorkspaceFs> = Box::new(resolver);

    let handler = oxdock_mcp::init_handler(oxdock_mcp::McpServerConfig {
        fs,
        script,
        modules,
        types,
    })?;
    oxdock_mcp::serve_stdio_blocking(handler)
}
