use anyhow::Result;
use oxdock_core::ExecIo;
use std::path::Path;

use crate::doc_runner::{DocSpec, assemble_doc};

pub fn assemble_readme(repo_root: &Path, output: Option<&Path>) -> Result<()> {
    let out_path = output
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| repo_root.join("README.md"));
    let sections_dir = repo_root.join("docs/sections");

    assemble_doc(
        repo_root,
        DocSpec {
            template: None,
            sections_dir: &sections_dir,
            output_path: &out_path,
            inherit_keys: &[],
        },
        ExecIo::new(),
    )
}
