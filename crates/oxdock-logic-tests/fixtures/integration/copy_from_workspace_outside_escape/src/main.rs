use indoc::indoc;
use oxdock_core::{run_steps_with_context_result_with_io, ExecIo};
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_core::parse_script;
use std::error::Error;
use std::env;
use std::fs;
use std::process;
use std::time::SystemTime;

fn main() -> Result<(), Box<dyn Error>> {
    let tempdir = GuardedPath::tempdir()?;
    let snapshot_root = tempdir.as_guarded_path().clone();
    let workspace_root = tempdir.as_guarded_path().clone();

    let resolver = PathResolver::new_guarded(snapshot_root.clone(), workspace_root.clone())?;

    // Attempt to copy a file that is definitely outside the guarded workspace.
    // Create a unique temp file in the system temp dir and reference it.
    let tmp = env::temp_dir();
    let outside_path = tmp.join(format!(
        "oxdock_outside_{}_{}",
        process::id(),
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_nanos()
    ));
    fs::write(&outside_path, b"outside content")?;
    let outside = outside_path.to_string_lossy().to_string();
    let script = indoc!(r#"
    COPY --from-current-workspace "{outside}" out/target_escape
    "#);
    let script = script.replace("{outside}", &outside);
    let steps = parse_script(&script)?;

    // This should fail
    let res = run_steps_with_context_result_with_io(&snapshot_root, &workspace_root, &steps, ExecIo::new());
    assert!(res.is_err(), "expected COPY from outside workspace to fail");
    Ok(())
}
