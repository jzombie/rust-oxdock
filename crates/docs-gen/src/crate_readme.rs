use anyhow::Result;
use oxdock_core::ExecIo;
use oxdock_macros::oxdock;
use std::path::{Path, PathBuf};

use crate::doc_runner::assemble_doc;

pub fn generate_all(repo_root: &Path) -> Result<()> {
    let crates_dir = repo_root.join("docs/crates");
    if !crates_dir.exists() {
        return Ok(());
    }

    let template_path = repo_root.join("docs/templates/crate-readme.md");

    for entry in std::fs::read_dir(&crates_dir)?.flatten() {
        if !entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            continue;
        }

        let crate_dir = entry.path();
        let crate_name = match crate_dir.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => continue,
        };

        let cargo_toml_path = find_cargo_toml(repo_root, &crate_dir, &crate_name);
        if !cargo_toml_path.exists() {
            eprintln!("Skipping {crate_name}: no Cargo.toml found");
            continue;
        }

        if let Err(err) = generate_one(&template_path, &crate_dir, &cargo_toml_path, repo_root) {
            eprintln!("  Warning: failed to generate {crate_name}/README.md: {err:#}");
        }
    }

    Ok(())
}

fn find_cargo_toml(repo_root: &Path, crate_doc_dir: &Path, crate_name: &str) -> PathBuf {
    let candidates = [
        repo_root.join("crates").join(crate_name).join("Cargo.toml"),
        repo_root.join("crates/sys").join(crate_name).join("Cargo.toml"),
        repo_root.join(crate_name).join("Cargo.toml"),
        crate_doc_dir.join("Cargo.toml"),
    ];
    for candidate in &candidates {
        if candidate.exists() {
            return candidate.clone();
        }
    }
    crate_doc_dir.join("Cargo.toml")
}

fn generate_one(
    template_path: &Path,
    crate_doc_dir: &Path,
    cargo_toml: &Path,
    repo_root: &Path,
) -> Result<()> {
    let metadata = parse_cargo_metadata(cargo_toml)?;
    let out_path = cargo_toml.parent().unwrap().join("README.md");

    let mut io = ExecIo::new();
    io.insert_inherit_env("CRATE_NAME", &metadata.name);
    io.insert_inherit_env("CRATE_DESCRIPTION", &metadata.description);

    let template_str = template_path.display().to_string();
    let out_str = out_path.display().to_string();

    let init_steps = oxdock! {
        INHERIT_ENV [CRATE_NAME, CRATE_DESCRIPTION]
        WITH_IO [stdout=pipe:base] EXPAND #template_str
        WITH_IO [stdin=pipe:base] WRITE #out_str
    };

    assemble_doc(repo_root, init_steps, crate_doc_dir, &out_path, io)
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
