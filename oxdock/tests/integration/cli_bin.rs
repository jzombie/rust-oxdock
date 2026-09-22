use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_process::CommandBuilder;

#[cfg_attr(
    miri,
    ignore = "spawns the CLI binary; Miri does not support process execution"
)]
#[test]
fn cli_binary_runs_script() {
    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let root = tempdir.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    let script = root.join("script.ox").expect("script path");
    resolver
        .write_file(&script, b"WRITE out.txt hi")
        .expect("write script");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_oxdock"));
    cmd.arg("--script").arg("script.ox");
    cmd.env(oxdock_fs::env::WORKSPACE_ROOT, root.display());
    let status = cmd.status().expect("run cli");
    assert!(status.success(), "expected successful CLI exit");
}
