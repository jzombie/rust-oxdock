use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_parser::extract_fenced_blocks;
use oxdock_process::CommandBuilder;

#[cfg_attr(
    miri,
    ignore = "spawns the CLI binary; Miri does not support process execution"
)]
#[test]
fn readme_quickstart_runs_through_the_real_cli() {
    // Normalize separators first: Windows CARGO_MANIFEST_DIR uses backslashes.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR")
        .replace('\\', "/");
    let repo_root = manifest_dir
        .strip_suffix("/oxdock-cli")
        .expect("readme_quickstart must live under <repo>/oxdock-cli");
    let root = GuardedPath::new_root_from_str(repo_root).expect("repo root guard");
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");
    let readme_path = resolver.root().join("README.md").expect("README path");
    let markdown = resolver.read_to_string(&readme_path).expect("read README");

    let blocks = extract_fenced_blocks(&markdown, "oxdock").expect("extract blocks");
    let quickstart = blocks
        .first()
        .expect("README must open with an oxdock quick-start block");

    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let workspace = tempdir.as_guarded_path().clone();
    let workspace_resolver =
        PathResolver::new_guarded(workspace.clone(), workspace.clone()).expect("resolver");
    let script = workspace.join("Oxfile").expect("script path");
    workspace_resolver
        .write_file(&script, quickstart.body.as_bytes())
        .expect("write Oxfile");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_oxdock"));
    cmd.arg("--script").arg("Oxfile");
    cmd.env("OXDOCK_WORKSPACE_ROOT", workspace.display());
    cmd.current_dir(workspace.as_path());
    let output = cmd.output().expect("run cli on README quick start");
    assert!(
        output.success(),
        "quick-start snippet must succeed via CLI (the snippet's own \
         ASSERT_FILE/ASSERT_STDOUT verify artifacts inside the CLI workspace)"
    );

    // The snippet's LS step lists the dist directory it built.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello.txt"),
        "LS output missing from: {stdout}"
    );
}
