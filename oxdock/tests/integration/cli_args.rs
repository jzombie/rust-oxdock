use oxdock::{GuardedPath, PathResolver};
use oxdock_process::CommandBuilder;

#[cfg_attr(
    miri,
    ignore = "spawns the CLI binary; Miri does not support process execution"
)]
#[test]
fn help_flag_prints_usage_and_succeeds() {
    for flag in ["--help", "-h"] {
        let output = CommandBuilder::new(env!("CARGO_BIN_EXE_oxdock"))
            .arg(flag)
            .output()
            .expect("run cli with help flag");
        assert!(output.success(), "expected successful CLI exit for {flag}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("Usage: oxdock"),
            "help output missing usage for {flag}: {stdout}"
        );
    }
}

#[cfg_attr(
    miri,
    ignore = "spawns the CLI binary; Miri does not support process execution"
)]
#[test]
fn positional_script_path_runs_like_script_flag() {
    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let root = tempdir.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    let script = root.join("script.ox").expect("script path");
    resolver
        .write_file(&script, b"ECHO hello-positional")
        .expect("write script");

    let output = CommandBuilder::new(env!("CARGO_BIN_EXE_oxdock"))
        .arg("script.ox")
        .env(oxdock_fs::env::oxdock::WORKSPACE_ROOT, root.display())
        .output()
        .expect("run cli with positional script");
    assert!(
        output.success(),
        "expected successful CLI exit for positional script"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello-positional"),
        "script output missing from: {stdout}"
    );
}

#[cfg(all(feature = "net", feature = "mcp"))]
#[cfg_attr(
    miri,
    ignore = "spawns the CLI binary; Miri does not support process execution"
)]
#[test]
fn exclusive_modes_reject_staged_script_args() {
    // #226: `--remote-serve` and `--mcp` run alone; anything staged
    // after the script position fails instead of serving.
    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let root = tempdir.as_guarded_path().clone();
    for args in [
        vec!["--remote-serve", "script.ox", "extra"],
        vec!["--mcp", "script.ox", "--", "extra"],
    ] {
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_oxdock"));
        for arg in args {
            cmd.arg(arg);
        }
        cmd.env(oxdock_fs::env::oxdock::WORKSPACE_ROOT, root.display());
        let output = cmd
            .output()
            .expect("run cli in exclusive mode with script args");
        assert!(
            !output.success(),
            "exclusive mode must fail with staged script args"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("runs alone"),
            "exclusivity error missing runs-alone wording: {stderr}"
        );
    }
}
