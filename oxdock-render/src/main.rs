use anyhow::{Context, Result};
use oxdock_render::{cli, run};
#[allow(clippy::disallowed_types)]
use std::path::PathBuf;

#[allow(clippy::disallowed_types)]
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let opts = cli::parse_options(&args)?;
    if opts.help {
        println!("{}", cli::usage());
        return Ok(());
    }
    if opts.version {
        println!("{}", cli::version());
        return Ok(());
    }
    if opts.init {
        // Scaffold a template render.json in --root (else the cwd)
        // and stop: never overwrite an existing file.
        let dir = match opts.root {
            Some(root) => PathBuf::from(root),
            None => std::env::current_dir().context("current directory for --init")?,
        };
        let root = oxdock_fs::GuardedPath::new_root(&dir).context("guard --init directory")?;
        let path = oxdock_render::config::init_config_file(&root)?;
        println!("wrote {}", path.display());
        return Ok(());
    }
    let repo_root = match opts.root {
        Some(root) => PathBuf::from(root),
        None => oxdock_fs::discover_workspace_root()
            .context("discover workspace root")?
            .as_path()
            .to_path_buf(),
    };
    if let Some(config) = opts.config {
        // Edge default lives here so the DSL never names a filename:
        // the flag wins, otherwise run() requires render.json.
        // SAFETY: process startup, single-threaded; no threads exist
        // yet and nothing reads this var concurrently.
        unsafe {
            std::env::set_var(oxdock_fs::env::render::CONFIG, config);
        }
    }
    // Dry run: print the rendered child instead of piping it through
    // the interpreter. Shows the dynamically assembled child for debugging.
    // SAFETY: same single-threaded startup as above.
    if opts.print_child {
        unsafe {
            std::env::set_var(oxdock_fs::env::render::PRINT_CHILD, "1");
        }
    }
    run(&repo_root)
}
