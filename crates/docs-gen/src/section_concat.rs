use anyhow::Result;
use oxdock_core::ExecIo;
use oxdock_macros::oxdock;
use std::path::Path;

use crate::runner;

pub fn assemble(
    repo_root: &Path,
    glob_pattern: &str,
    output_path: &Path,
    env: ExecIo,
) -> Result<()> {
    let out_str = output_path.display().to_string();
    let pattern_str = glob_pattern.to_string();

    let steps = oxdock! {
        RAW_WRITE #out_str ""
        LET $sections = GLOB(#pattern_str)
        FOR $f IN $sections {
            WITH_IO [stdout=pipe:sec] READ $f
            WITH_IO [stdin=pipe:sec] APPEND #out_str
            APPEND #out_str "\n"
        }
    };

    runner::run(repo_root, steps, env)
}
