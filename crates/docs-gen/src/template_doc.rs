use anyhow::Result;
use oxdock_core::ExecIo;
use oxdock_macros::oxdock;
#[allow(clippy::disallowed_types)]
use std::path::{Path, PathBuf};

use crate::runner;

pub enum DocNode {
    #[allow(clippy::disallowed_types)]
    Template(PathBuf),
    Glob(String),
    #[expect(dead_code)]
    Raw(String),
}

#[allow(clippy::disallowed_types)]
pub fn compile(
    repo_root: &Path,
    manifest: &[DocNode],
    output_path: &Path,
    env: ExecIo,
) -> Result<()> {
    let out_str = output_path.display().to_string();
    let mut steps = Vec::new();

    steps.extend(oxdock! {
        RAW_WRITE #out_str ""
    });

    for node in manifest {
        match node {
            DocNode::Template(path) => {
                let path_str = path.display().to_string();
                steps.extend(oxdock! {
                    WITH_IO [stdout=pipe:tmpl] EXPAND #path_str
                    WITH_IO [stdin=pipe:tmpl] APPEND #out_str
                });
            }
            DocNode::Glob(pattern) => {
                let pattern_str = pattern.clone();
                steps.extend(oxdock! {
                    LET $sections = GLOB(#pattern_str)
                    FOR $f IN $sections {
                        WITH_IO [stdout=pipe:sec] READ $f
                        WITH_IO [stdin=pipe:sec] APPEND #out_str
                        APPEND #out_str "\n"
                    }
                });
            }
            DocNode::Raw(text) => {
                steps.extend(oxdock! {
                    APPEND #out_str #text
                });
            }
        }
    }

    runner::run(repo_root, steps, env)
}
