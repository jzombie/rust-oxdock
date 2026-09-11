pub mod io;
pub mod manifest;
pub mod oxdock;
pub mod rust;

use anyhow::{Context, Result};
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_macros::oxdock;
#[allow(clippy::disallowed_types)]
use std::path::Path;

use manifest::write_manifests;
use oxdock::command_ref::sync_command_ref;
use rust::cargo::{sync_package_values, workspace_members};
use rust::crate_version;

/// Execute the document pipeline: refresh generated inputs, sync
/// per-member values, assemble one `$files` manifest per target, then
/// render every master template through native OxDock commands.
/// Order lives in the master templates as `{{ $files.group.stem }}`
/// placeholders; the Rust around the pipeline only bridges host data
/// (env, manifests, registry, file contents) that the DSL cannot
/// reach on its own.
#[allow(clippy::disallowed_types)]
pub fn run(repo_root: &Path) -> Result<()> {
    let root = GuardedPath::new_root(repo_root)?;
    let resolver = PathResolver::new(root.as_path(), root.as_path())?;

    let version = crate_version(&root, &resolver)?;
    let mut io = ExecIo::new();
    io.insert_inherit_env("CRATE_VERSION", &version);
    sync_command_ref(&root, &resolver)?;
    for member in workspace_members(&root, &resolver)? {
        sync_package_values(&root, &resolver, &member)?;
    }
    write_manifests(&root, &resolver, &version)?;

    let steps: Vec<oxdock_parser::Step> = oxdock! {
        LET $cfg: MAP = LOAD_JSON("docs-gen.json")
        LET $docs_global: MAP = LOAD_JSON($cfg.global_values)
        FOR $scope: STRING IN $cfg.scopes {
            FOR $tj: STRING IN GLOB("{{ $scope }}/**/target.json") {
                LET $file: MAP = LOAD_JSON($tj)
                FOR $t: MAP IN $file.targets {
                    ECHO "rendering {{ $t.name }} -> {{ $t.out }}"
                    LET $docs_ctx: MAP = LOAD_JSON($t.values)
                    LET $files: MAP = LOAD_JSON("target/oxdock-docs/{{ $t.name }}.json")
                    WRITE $t.out ""
                    WITH_IO [stdout=pipe:render] EXPAND $t.template
                    WITH_IO [stdin=pipe:render] APPEND $t.out
                }
            }
        }
    };
    run_steps_with_context_result_with_io(&root, &root, &steps, io).context("render documents")?;
    eprintln!("docs rendered");
    Ok(())
}
