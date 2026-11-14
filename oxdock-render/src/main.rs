use anyhow::{Context, Result};
use oxdock_render::{RunOptions, cli, run_with_options};
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
        // `--config`/`--print-child` with `--init` already failed in
        // `parse_options`, so there is nothing to silently ignore.
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
    // Edge default lives in the options layer so the DSL never names
    // a filename: the flag wins, otherwise `run` requires render.json.
    // No process-global mutation: options travel by value.
    run_with_options(
        &repo_root,
        RunOptions {
            config: opts.config,
            print_child: opts.print_child,
            child_bin: None,
        },
    )
}
