use anyhow::{Context, Result};
use oxdock_compose::run;
#[allow(clippy::disallowed_types)]
use std::path::PathBuf;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let repo_root = match root_arg(&args) {
        Some(root) => root,
        None => oxdock_fs::discover_workspace_root()
            .context("discover workspace root")?
            .as_path()
            .to_path_buf(),
    };
    if let Some(config) = config_arg(&args) {
        // Edge default lives here so the DSL never names a filename:
        // the flag wins, otherwise run() requires compose.json.
        // SAFETY: process startup, single-threaded; no threads exist
        // yet and nothing reads this var concurrently.
        unsafe {
            std::env::set_var("COMPOSE_CONFIG", config);
        }
    }
    // Dry run: print the composed child instead of piping it through
    // the interpreter. Shows the dynamic composition for debugging.
    // SAFETY: same single-threaded startup as above.
    if args.iter().any(|arg| arg == "--print-child") {
        unsafe {
            std::env::set_var("COMPOSE_PRINT_CHILD", "1");
        }
    }
    run(&repo_root)
}

#[allow(clippy::disallowed_types)]
fn root_arg(args: &[String]) -> Option<PathBuf> {
    args.windows(2)
        .find(|w| w[0] == "--root")
        .map(|w| PathBuf::from(&w[1]))
}

#[allow(clippy::disallowed_types)]
fn config_arg(args: &[String]) -> Option<PathBuf> {
    args.windows(2)
        .find(|w| w[0] == "--config")
        .map(|w| PathBuf::from(&w[1]))
}
