mod command_ref;
mod runner;
mod section_concat;
mod template_doc;

use anyhow::Result;
use oxdock_core::ExecIo;
use std::path::PathBuf;

fn main() -> Result<()> {
    let repo_root = find_repo_root()?;

    // Phase 1: command reference
    let cmd_ref_out = repo_root.join("docs/sections/06-command-reference.md");
    command_ref::generate(&cmd_ref_out)?;
    eprintln!("Command reference written to {}", cmd_ref_out.display());

    // Phase 2: root README from section files
    let sections_dir = repo_root.join("docs/sections");
    let sections_glob = format!("{}/*.md", sections_dir.display());
    let root_readme = repo_root.join("README.md");
    section_concat::assemble(&repo_root, &sections_glob, &root_readme, ExecIo::new())?;
    eprintln!("Root README written to {}", root_readme.display());

    // Phase 3: per-crate docs from template
    let template = repo_root.join("docs/templates/crate-readme.md");
    let workspace_toml = repo_root.join("Cargo.toml");
    template_doc::generate_all(&repo_root, &workspace_toml, &template, "README.md")?;
    eprintln!("All crate READMEs generated");

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
