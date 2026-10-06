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

#[cfg_attr(
    miri,
    ignore = "spawns the CLI binary; Miri does not support process execution"
)]
#[test]
fn cli_exit_code_relays_script_exit() {
    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let root = tempdir.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    let script = root.join("script.ox").expect("script path");
    resolver
        .write_file(&script, b"EXIT 3")
        .expect("write script");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_oxdock"));
    cmd.arg("--script").arg("script.ox");
    cmd.env(oxdock_fs::env::WORKSPACE_ROOT, root.display());
    let status = cmd.status().expect("run cli");
    assert_eq!(status.code(), Some(3), "host EXIT must reach the OS");
}

#[cfg_attr(
    miri,
    ignore = "spawns the CLI binary; Miri does not support process execution"
)]
#[test]
fn cli_exit_code_is_one_on_plain_failure() {
    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let root = tempdir.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    let script = root.join("script.ox").expect("script path");
    resolver
        .write_file(&script, b"ECHO $undefined")
        .expect("write script");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_oxdock"));
    cmd.arg("--script").arg("script.ox");
    cmd.env(oxdock_fs::env::WORKSPACE_ROOT, root.display());
    let status = cmd.status().expect("run cli");
    assert_eq!(status.code(), Some(1), "non-exit failures stay at 1");
}

#[cfg_attr(
    miri,
    ignore = "spawns CLI binaries; Miri does not support process execution"
)]
#[test]
fn cli_exit_code_relays_guest_exit_over_loopback() {
    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let root = tempdir.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    let script = root.join("script.ox").expect("script path");
    resolver
        .write_file(&script, b"REMOTE loop {\n    EXIT 3\n}\n")
        .expect("write script");

    // The guest is the same binary in `--remote-serve` mode, so the
    // handshake agrees by construction. The host must exit with the
    // guest's code, proving the relay crossed the real wire protocol.
    let bin = env!("CARGO_BIN_EXE_oxdock");
    let mut cmd = CommandBuilder::new(bin);
    cmd.arg("--script")
        .arg("script.ox")
        .arg("--remote")
        .arg(format!("loop=\"{bin}\""));
    cmd.env(oxdock_fs::env::WORKSPACE_ROOT, root.display());
    let status = cmd.status().expect("run cli");
    assert_eq!(status.code(), Some(3), "guest EXIT must reach the OS");
}
