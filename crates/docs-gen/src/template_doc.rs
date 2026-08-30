use anyhow::{Context, Result};
use oxdock_core::ExecIo;
use oxdock_macros::oxdock;
use std::path::Path;

use crate::runner;

pub fn generate_all(
    repo_root: &Path,
    workspace_toml: &Path,
    template_path: &Path,
    output_name: &str,
) -> Result<()> {
    let members = parse_workspace_members(workspace_toml)
        .context("failed to parse workspace members")?;

    if !template_path.exists() {
        eprintln!(
            "Skipping template docs: template not found at {}",
            template_path.display()
        );
        return Ok(());
    }

    for member in &members {
        let cargo_toml = repo_root.join(member).join("Cargo.toml");
        if !cargo_toml.exists() {
            eprintln!("Skipping {member}: no Cargo.toml found");
            continue;
        }

        let out_path = repo_root.join(member).join(output_name);

        if let Err(err) = generate_one(template_path, &cargo_toml, &out_path, repo_root) {
            eprintln!("  Warning: failed to generate {member}/{output_name}: {err:#}");
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

fn generate_one(
    template_path: &Path,
    cargo_toml: &Path,
    out_path: &Path,
    repo_root: &Path,
) -> Result<()> {
    let metadata = parse_cargo_metadata(cargo_toml)?;

    let mut io = ExecIo::new();
    io.insert_inherit_env("CRATE_NAME", &metadata.name);
    io.insert_inherit_env("CRATE_DESCRIPTION", &metadata.description);

    let template_str = template_path.display().to_string();
    let out_str = out_path.display().to_string();

    let steps = oxdock! {
        INHERIT_ENV [CRATE_NAME, CRATE_DESCRIPTION]
        WITH_IO [stdout=pipe:base] EXPAND #template_str
        WITH_IO [stdin=pipe:base] WRITE #out_str
    };

    runner::run(repo_root, steps, io)
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
