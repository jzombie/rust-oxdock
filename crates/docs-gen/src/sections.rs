use anyhow::Result;
use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::GuardedPath;
use oxdock_parser::parse_script;
use std::path::Path;

pub fn assemble_readme(repo_root: &Path, output: Option<&Path>) -> Result<()> {
    let out_path = output
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| repo_root.join("README.md"));

    let script = indoc!(
        r#"
        RAW_WRITE "README.md" ""
        LET $sections = GLOB("docs/sections/*.md")
        FOR $f IN $sections {
            // Read file to `pipe:sec` pipe
            WITH_IO [stdout=pipe:sec] READ $f

            // Append `pipe:sec` pipe to README.md
            WITH_IO [stdin=pipe:sec] APPEND README.md

            // Add trailing line break
            APPEND README.md "\n"
        }
        "#
    );

    let root = GuardedPath::new_root(repo_root)?;
    let steps = parse_script(script)?;
    run_steps_with_context_result_with_io(&root, &root, &steps, ExecIo::new())?;

    if out_path != repo_root.join("README.md") {
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(repo_root.join("README.md"), &out_path)?;
    }

    Ok(())
}
