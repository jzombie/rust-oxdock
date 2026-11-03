use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_process::CommandBuilder;

/// The repo-root hello script is self-verifying: its ASSERT lines fail
/// the run when any demonstrated behavior regresses. This test stages
/// the committed file into a hermetic workspace and runs it through
/// the real CLI, so `oxdock hello.oxfile` stays green in CI on every
/// platform leg.
#[cfg_attr(
    miri,
    ignore = "spawns the CLI binary; Miri does not support process execution"
)]
#[test]
fn hello_oxfile_runs_green_through_the_real_cli() {
    // Normalize separators first: Windows CARGO_MANIFEST_DIR uses backslashes.
    let manifest_dir = std::env::var(oxdock_fs::env::cargo::MANIFEST_DIR)
        .expect(oxdock_fs::env::cargo::MANIFEST_DIR)
        .replace('\\', "/");
    let repo_root = manifest_dir
        .strip_suffix("/oxdock")
        .expect("hello test must live under <repo>/oxdock");
    let root = GuardedPath::new_root_from_str(repo_root).expect("repo root guard");
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");
    let hello_path = resolver.root().join("hello.oxfile").expect("hello path");
    let text = resolver
        .read_to_string(&hello_path)
        .expect("read hello.oxfile");

    // Hermetic workspace: the script's RUN lines execute here, and the
    // script's own ASSERTs verify every demonstrated behavior inside
    // the CLI run.
    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let workspace = tempdir.as_guarded_path().clone();
    let workspace_resolver =
        PathResolver::new_guarded(workspace.clone(), workspace.clone()).expect("resolver");
    let script = workspace.join("hello.oxfile").expect("script path");
    workspace_resolver
        .write_file(&script, text.as_bytes())
        .expect("write hello.oxfile");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_oxdock"));
    cmd.arg("--script").arg("hello.oxfile");
    cmd.env(oxdock_fs::env::oxdock::WORKSPACE_ROOT, workspace.display());
    cmd.current_dir(workspace.as_path());
    let output = cmd.output().expect("run cli on hello.oxfile");
    assert!(
        output.success(),
        "hello.oxfile must succeed via CLI (its ASSERTs verify artifacts inside the CLI workspace)"
    );
}
