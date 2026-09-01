use oxdock_core::{run_steps_with_context_result_with_io, ExecIo};
use oxdock_fs::{GuardedPath, PathResolver, command_path, ensure_git_identity};
use oxdock_core::parse_script;
use oxdock_process::CommandBuilder;
use std::collections::BTreeSet;
use std::error::Error;

fn git_cmd(repo: &GuardedPath) -> CommandBuilder {
    let mut cmd = CommandBuilder::new("git");
    let repo_arg = command_path(repo).into_owned();
    cmd.arg("-C").arg(repo_arg);
    cmd
}

fn collect_paths(
    resolver: &PathResolver,
    root: &GuardedPath,
    prefix: &str,
    out: &mut BTreeSet<String>,
) -> Result<(), Box<dyn Error>> {
    let entries = resolver.read_dir_entries(root)?;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        out.insert(rel.clone());
        if entry.file_type()?.is_dir() {
            let child = root.join(&name)?;
            collect_paths(resolver, &child, &rel, out)?;
        }
    }
    Ok(())
}

fn ensure(condition: bool, message: &str) -> Result<(), Box<dyn Error>> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let tempdir = GuardedPath::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let snapshot_root = tempdir.as_guarded_path().clone();
    let resolver = PathResolver::new_guarded(snapshot_root.clone(), snapshot_root.clone())
        .map_err(|e| format!("resolver: {e}"))?;

    let repo = snapshot_root.join("repo")?;
    resolver.create_dir_all(&repo)?;

    let deep_dir = repo.join("deep/dir/level/ten")?;
    resolver.create_dir_all(&deep_dir)?;
    let source = deep_dir.join("source.txt")?;
    resolver.write_file(&source, b"git hello")?;

    git_cmd(&repo)
        .arg("init")
        .arg("-q")
        .status()
        .map_err(|e| format!("git init failed: {e}"))?;
    git_cmd(&repo)
        .arg("add")
        .arg(".")
        .status()
        .map_err(|e| format!("git add failed: {e}"))?;
    ensure_git_identity(&repo).map_err(|e| format!("ensure git identity: {e}"))?;
    git_cmd(&repo)
        .arg("commit")
        .arg("-m")
        .arg("initial")
        .status()
        .map_err(|e| format!("git commit failed: {e}"))?;

    // Modify the file after commit to make the working tree dirty.
    resolver.write_file(&source, b"dirty hello")?;

    let script = "COPY_GIT --include-dirty HEAD deep/dir/level/ten output/nested/target";
    let steps = parse_script(script).map_err(|e| format!("parse script: {e}"))?;
    run_steps_with_context_result_with_io(&snapshot_root, &repo, &steps, ExecIo::new())
        .map_err(|e| format!("run steps: {e}"))?;

    let out_root = snapshot_root.join("output")?;
    let target_dir = out_root.join("nested/target")?;
    let out_file = target_dir.join("source.txt")?;
    let content = resolver.read_to_string(&out_file)?;
    ensure(content.trim_end() == "dirty hello", "dirty content missing")?;

    let mut paths = BTreeSet::new();
    collect_paths(&resolver, &out_root, "", &mut paths)?;

    let expected: BTreeSet<String> = [
        "nested".to_string(),
        "nested/target".to_string(),
        "nested/target/source.txt".to_string(),
    ]
    .into_iter()
    .collect();

    ensure(paths == expected, &format!("output tree mismatch: {paths:?}"))?;

    Ok(())
}
