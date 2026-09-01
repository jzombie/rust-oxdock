use indoc::indoc;
use oxdock_core::{run_steps_with_context_result_with_io, ExecIo};
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_core::parse_script;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let tempdir = GuardedPath::tempdir()?;
    let snapshot_root = tempdir.as_guarded_path().clone();
    let workspace_root = tempdir.as_guarded_path().clone();

    let resolver = PathResolver::new_guarded(snapshot_root.clone(), workspace_root.clone())?;

    // Create a file in the workdir (build_context) and one in workspace root
    let build_file = workspace_root.join("build_source.txt")?;
    resolver.write_file(&build_file, b"from build")?;

    let workspace_dir = workspace_root.join("ws_dir")?;
    resolver.create_dir_all(&workspace_dir)?;
    let workspace_file = workspace_dir.join("ws_source.txt")?;
    resolver.write_file(&workspace_file, b"from workspace")?;

    // Script: copy from workspace-relative path into snapshot output
    // Use the new flag to indicate copy from current workspace
    let ws_src = workspace_file
        .as_path()
        .to_string_lossy()
        .replace('\\', "\\\\");
    let out_ws_path = "out/target_ws/ws_source.txt";
    let out_build_path = "out/target_build/build_source.txt";
    let script = indoc!(r#"
    COPY --from-current-workspace "{ws}" "{out_ws}"
    COPY "{build}" "{out_build}"
    "#);
    let script = script
        .replace("{ws}", &ws_src)
        .replace("{out_ws}", out_ws_path)
        .replace("{build}", "./build_source.txt")
        .replace("{out_build}", out_build_path);

    let steps = parse_script(&script)?;

    // Run with build context = workspace_root so resolver can resolve './build_source.txt'
    run_steps_with_context_result_with_io(&snapshot_root, &workspace_root, &steps, ExecIo::new())?;

    // Verify outputs exist
    let out_ws = snapshot_root.join("out/target_ws/ws_source.txt")?;
    let out_build = snapshot_root.join("out/target_build/build_source.txt")?;

    let ws_contents = resolver.read_to_string(&out_ws)?;
    let build_contents = resolver.read_to_string(&out_build)?;
    assert!(ws_contents.contains("from workspace"));
    assert!(build_contents.contains("from build"));

    Ok(())
}
