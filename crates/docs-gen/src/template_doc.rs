use anyhow::Result;
use oxdock_core::ExecIo;
use oxdock_macros::oxdock;
use std::path::Path;

use crate::runner;

pub fn expand(
    repo_root: &Path,
    template_path: &Path,
    output_path: &Path,
    env: ExecIo,
) -> Result<()> {
    let template_str = template_path.display().to_string();
    let out_str = output_path.display().to_string();

    let steps = oxdock! {
        WITH_IO [stdout=pipe:base] EXPAND #template_str
        WITH_IO [stdin=pipe:base] WRITE #out_str
    };

    runner::run(repo_root, steps, env)
}
