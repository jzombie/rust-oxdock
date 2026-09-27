use anyhow::{Context, Result};
use oxdock_fs::PathResolver;
use oxdock_cli::{EndpointFlags, execute_with_result, Options, ScriptSource};

fn main() {
    if let Err(err) = run() {
        eprintln!("fixture failed: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let resolver = PathResolver::from_manifest_env().context("resolve fixture manifest dir")?;
    let script = resolver.root().join("script.oxfile")?;
    let opts = Options { script: ScriptSource::Path(script), shell: false, endpoints: EndpointFlags::default(), remote_serve: false, remotes: Vec::new() };
    execute_with_result(opts, resolver.root().clone())?;
    Ok(())
}
