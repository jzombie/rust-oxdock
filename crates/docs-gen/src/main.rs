mod command_ref;
mod crate_readme;
mod sections;

use anyhow::Result;
use std::path::PathBuf;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let subcommand = args.get(1).map(|s| s.as_str()).unwrap_or("assemble-readme");

    let repo_root = find_repo_root()?;

    match subcommand {
        "generate-command-ref" => {
            let output = args
                .get(3)
                .map(|s| PathBuf::from(s))
                .unwrap_or_else(|| repo_root.join("docs/sections/06-command-reference.md"));
            command_ref::generate(&output)?;
            eprintln!("Command reference written to {}", output.display());
        }
        "generate-crate-readmes" => {
            sections::assemble_readme(&repo_root, None)?;
            crate_readme::generate_all(&repo_root)?;
            eprintln!("All READMEs generated");
        }
        "all" => {
            let output = repo_root.join("docs/sections/06-command-reference.md");
            command_ref::generate(&output)?;
            eprintln!("Command reference written to {}", output.display());
            sections::assemble_readme(&repo_root, None)?;
            crate_readme::generate_all(&repo_root)?;
            eprintln!("All READMEs generated");
        }
        other => {
            eprintln!("Unknown subcommand: {other}");
            eprintln!("Usage: docs-gen <generate-command-ref|generate-crate-readmes|all>");
            std::process::exit(1);
        }
    }

    Ok(())
}

fn find_repo_root() -> Result<PathBuf> {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()));
    let root = manifest_dir
        .to_str()
        .and_then(|s| {
            s.strip_suffix("/crates/docs-gen")
                .or_else(|| s.strip_suffix("\\crates/docs-gen"))
        })
        .map(PathBuf::from)
        .unwrap_or(manifest_dir);
    Ok(root)
}
