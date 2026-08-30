use anyhow::Result;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::GuardedPath;
use oxdock_macros::oxdock;
use oxdock_parser::Step;
use std::path::Path;

pub fn assemble_doc(
    repo_root: &Path,
    mut steps: Vec<Step>,
    sections_dir: &Path,
    output_path: &Path,
    env: ExecIo,
) -> Result<()> {
    let out_str = output_path.display().to_string();
    let glob_pattern = format!("{}/*.md", sections_dir.display());

    let core_steps = oxdock! {
        LET $sections = GLOB(#glob_pattern)
        FOR $f IN $sections {
            WITH_IO [stdout=pipe:sec] READ $f
            WITH_IO [stdin=pipe:sec] APPEND #out_str
            APPEND #out_str "\n"
        }
    };

    steps.extend(core_steps);

    let root = GuardedPath::new_root(repo_root)?;
    run_steps_with_context_result_with_io(&root, &root, &steps, env)?;
    Ok(())
}
