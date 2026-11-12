use anyhow::{Context, Result};
use oxdock_render::run;
#[allow(clippy::disallowed_types)]
use std::path::PathBuf;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--init") {
        // Scaffold a template render.json in --root (else the cwd)
        // and stop: never overwrite an existing file.
        let dir = match root_arg(&args) {
            Some(root) => root,
            None => std::env::current_dir().context("current directory for --init")?,
        };
        let root = oxdock_fs::GuardedPath::new_root(&dir).context("guard --init directory")?;
        let path = oxdock_render::config::init_config_file(&root)?;
        println!("wrote {}", path.display());
        return Ok(());
    }
    let repo_root = match root_arg(&args) {
        Some(root) => root,
        None => oxdock_fs::discover_workspace_root()
            .context("discover workspace root")?
            .as_path()
            .to_path_buf(),
    };
    if let Some(config) = config_arg(&args) {
        // Edge default lives here so the DSL never names a filename:
        // the flag wins, otherwise run() requires render.json.
        // SAFETY: process startup, single-threaded; no threads exist
        // yet and nothing reads this var concurrently.
        unsafe {
            std::env::set_var("RENDER_CONFIG", config);
        }
    }
    // Dry run: print the rendered child instead of piping it through
    // the interpreter. Shows the dynamically assembled child for debugging.
    // SAFETY: same single-threaded startup as above.
    if args.iter().any(|arg| arg == "--print-child") {
        unsafe {
            std::env::set_var("RENDER_PRINT_CHILD", "1");
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
