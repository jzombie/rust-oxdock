use anyhow::Result;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::GuardedPath;
use oxdock_parser::Step;
#[allow(clippy::disallowed_types)]
use std::path::Path;

#[allow(clippy::disallowed_types)]
pub fn run(repo_root: &Path, steps: Vec<Step>, env: ExecIo) -> Result<()> {
    let root = GuardedPath::new_root(repo_root)?;
    run_steps_with_context_result_with_io(&root, &root, &steps, env)?;
    Ok(())
}
