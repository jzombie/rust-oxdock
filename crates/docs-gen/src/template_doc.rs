use anyhow::Result;
use oxdock_core::ExecIo;
use oxdock_fs::GuardedPath;
use oxdock_macros::oxdock;
#[allow(clippy::disallowed_types)]
use std::path::Path;

use crate::runner;

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub fn compile(
    repo_root: &Path,
    manifest_json: &str,
    output_path: &Path,
    env: ExecIo,
) -> Result<()> {
    let root = GuardedPath::new_root(repo_root)?;
    let staging_dir = root.join(".oxdock-staging")?;
    std::fs::create_dir_all(staging_dir.as_path())?;

    let manifest_file = staging_dir.join("docs_manifest.json")?;
    std::fs::write(manifest_file.as_path(), manifest_json)?;

    let manifest_str = manifest_file.as_path().display().to_string();
    let out_str = output_path.display().to_string();

    let steps = oxdock! {
        WRITE #out_str ""
        LET $manifest = LOAD_JSON(#manifest_str)

        FOR $idx, $node IN $manifest {
            ECHO "[{{ $idx}}] {{ $node.kind }}"
            IF $node.kind == "template" {
                WITH_IO [stdout=pipe:tmpl] EXPAND $node.path
                WITH_IO [stdin=pipe:tmpl] APPEND #out_str
            } ELSE IF $node.kind == "glob" {
                LET $sections = GLOB($node.pattern)
                FOR $f IN $sections {
                    WITH_IO [stdout=pipe:sec] READ $f
                    WITH_IO [stdin=pipe:sec] APPEND #out_str
                    APPEND #out_str "\n"
                }
            } ELSE {
                APPEND #out_str $node.text
            }
        }
    };

    let res = runner::run(repo_root, steps, env);
    let _ = std::fs::remove_file(manifest_file.as_path());
    res
}
