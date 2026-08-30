use anyhow::Result;
use oxdock_core::ExecIo;
use oxdock_macros::oxdock;
use std::path::Path;

use crate::doc_runner::assemble_doc;

pub fn assemble_readme(repo_root: &Path, output: Option<&Path>) -> Result<()> {
    let out_path = output
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| repo_root.join("README.md"));
    let sections_dir = repo_root.join("docs/sections");

    let out_str = out_path.display().to_string();

    let init_steps = oxdock! {
        RAW_WRITE #out_str ""
    };

    assemble_doc(repo_root, init_steps, &sections_dir, &out_path, ExecIo::new())
}
