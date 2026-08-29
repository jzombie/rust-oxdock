use anyhow::{Context, Result};
use line_ending::LineEnding;
use std::path::{Path, PathBuf};

pub fn assemble_readme(repo_root: &Path, output: Option<&Path>) -> Result<()> {
    let sections_dir = repo_root.join("docs/sections");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&sections_dir)
        .with_context(|| format!("failed to read {}", sections_dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| p.extension().map(|ext| ext == "md").unwrap_or(false))
        .collect();

    entries.sort();

    let lf = LineEnding::LF;
    let mut content = String::new();
    for entry in &entries {
        let section = std::fs::read_to_string(entry)
            .with_context(|| format!("failed to read {}", entry.display()))?;
        let section = LineEnding::normalize(&section);
        // Ensure a blank line separates sections so Markdown headings render correctly.
        if !content.is_empty() && !content.ends_with("\n\n") {
            if content.ends_with('\n') {
                content.push_str(lf.as_str());
            } else {
                content.push_str("\n\n");
            }
        }
        content.push_str(&section);
        if !section.ends_with('\n') {
            content.push_str(lf.as_str());
        }
    }

    let out_path_buf = output.map(|p| p.to_path_buf()).unwrap_or_else(|| repo_root.join("README.md"));
    let out_path = out_path_buf.as_path();
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out_path, &content)?;
    Ok(())
}
