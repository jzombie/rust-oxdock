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
    run(&repo_root)
}

#[allow(clippy::disallowed_types)]
fn root_arg(args: &[String]) -> Option<PathBuf> {
    args.windows(2)
        .find(|w| w[0] == "--root")
        .map(|w| PathBuf::from(&w[1]))
}
