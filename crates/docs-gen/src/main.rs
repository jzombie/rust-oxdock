mod command_ref;
mod runner;
mod template_doc;

use anyhow::{Context, Result};
use oxdock_core::ExecIo;
use std::path::{Path, PathBuf};
use template_doc::DocNode;

fn main() -> Result<()> {
    let repo_root = find_repo_root()?;

    // Phase 1: command reference
    let cmd_ref_out = repo_root.join("docs/sections/06-command-reference.md");
    command_ref::generate(&cmd_ref_out)?;
    eprintln!("Command reference written to {}", cmd_ref_out.display());

    // Phase 2: per-crate docs from template
    let template_dir = repo_root.join("docs/templates");
    generate_crate_docs(&repo_root, &template_dir)?;
    eprintln!("All crate READMEs generated");

    Ok(())
}

fn generate_crate_docs(repo_root: &Path, template_dir: &Path) -> Result<()> {
    let workspace_toml = repo_root.join("Cargo.toml");
    let members =
        parse_workspace_members(&workspace_toml).context("failed to parse workspace members")?;

    let header = template_dir.join("crate-header.md");
    let footer = template_dir.join("crate-footer.md");

    if !header.exists() || !footer.exists() {
        eprintln!(
            "Skipping crate docs: templates not found at {}",
            template_dir.display()
        );
        return Ok(());
    }

    for member in &members {
        let member_cargo_toml = repo_root.join(member).join("Cargo.toml");
        if !member_cargo_toml.exists() {
            eprintln!("Skipping {member}: no Cargo.toml found");
            continue;
        }

        let meta = parse_cargo_metadata(&member_cargo_toml)?;
        let mut env = ExecIo::new();
        env.insert_inherit_env("CRATE_NAME", &meta.name);
        env.insert_inherit_env("CRATE_DESCRIPTION", &meta.description);

        let sections_dir = repo_root.join("docs/crates").join(&meta.name);
        let glob = format!("{}/*.md", sections_dir.display());

        let manifest = vec![
            DocNode::Template(header.clone()),
            DocNode::Glob(glob),
            DocNode::Template(footer.clone()),
        ];

        let out_path = repo_root.join(member).join("README.md");
        if let Err(err) = template_doc::compile(repo_root, &manifest, &out_path, env) {
            eprintln!("  Warning: failed to generate {member}/README.md: {err:#}");
        }
    }

    Ok(())
}

fn parse_workspace_members(workspace_toml: &Path) -> Result<Vec<String>> {
    let contents = std::fs::read_to_string(workspace_toml)?;
    let doc: toml_edit::DocumentMut = contents.parse()?;

    let members = doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .context("workspace.members not found or not an array")?;

    Ok(members
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect())
}

struct CargoMetadata {
    name: String,
    description: String,
}

fn parse_cargo_metadata(cargo_toml: &Path) -> Result<CargoMetadata> {
    let contents = std::fs::read_to_string(cargo_toml)?;
    let doc: toml_edit::DocumentMut = contents.parse()?;

    let name = doc
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let description = doc
        .get("package")
        .and_then(|p| p.get("description"))
        .and_then(|v| v.as_str())
        .unwrap_or("No description provided.")
        .to_string();

    Ok(CargoMetadata { name, description })
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
