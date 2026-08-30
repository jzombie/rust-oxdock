mod command_ref;
mod crate_readme;
mod doc_runner;
mod sections;

use anyhow::Result;
use std::path::PathBuf;

fn main() -> Result<()> {
    let repo_root = find_repo_root()?;

    let output = repo_root.join("docs/sections/06-command-reference.md");
    command_ref::generate(&output)?;
    eprintln!("Command reference written to {}", output.display());

    sections::assemble_readme(&repo_root, None)?;
    crate_readme::generate_all(&repo_root)?;
    eprintln!("All READMEs generated");

    Ok(())
}

fn find_repo_root() -> Result<PathBuf> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));

    let mut current = manifest_dir.canonicalize().unwrap_or(manifest_dir);
    loop {
        if current.join("Cargo.toml").exists() && current.join("docs").exists() {
            return Ok(current);
        }
        if !current.pop() {
            break;
        }
    }

    Ok(PathBuf::from("."))
}
