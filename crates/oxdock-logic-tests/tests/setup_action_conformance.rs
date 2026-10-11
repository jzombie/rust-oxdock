#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

//! Conformance for the setup-oxdock post-install script: the DSL that
//! renames the bootstrap engine and publishes it on `GITHUB_PATH` after
//! every install. Parse and marker pins run everywhere; live execution
//! runs on Unix only, where a shell-script fixture can stand in for the
//! engine (Windows cannot execute a fixture binary without shipping one,
//! so Windows coverage is the smoke matrix itself, which runs this
//! script on every platform per PR).

use anyhow::{Context, Result};
#[cfg(unix)]
use oxdock_core::{ExecIo, run_steps_with_manager_with_modules};
use oxdock_fs::{GuardedPath, PathResolver};

fn repo_root() -> Result<String> {
    let manifest_dir = std::env::var(oxdock_fs::env::CARGO_MANIFEST_DIR)
        .context("CARGO_MANIFEST_DIR missing")?
        .replace('\\', "/");
    Ok(manifest_dir
        .strip_suffix("crates/oxdock-logic-tests")
        .context("test must live under crates/oxdock-logic-tests")?
        .trim_end_matches('/')
        .to_string())
}

fn load_post_install_script() -> Result<String> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let script_path = resolver
        .root()
        .join(".github/actions/setup-oxdock/post-install.oxfile")?;
    resolver.read_to_string(&script_path)
}

fn parse_post_install_script(text: &str) -> Result<Vec<oxdock_parser::Step>> {
    oxdock_core::parse_script(text)
        .map_err(|err| anyhow::anyhow!("post-install.oxfile failed to parse: {err}"))
}

/// The post-install script must always parse: the setup action runs it
/// under the freshly installed engine on every CI job, so a breaking
/// language change that invalidates it fails the PR that makes the
/// change, never the action.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads post-install.oxfile from the repository checkout layout"
)]
fn post_install_script_parses() -> Result<()> {
    let text = load_post_install_script()?;
    parse_post_install_script(&text)?;
    Ok(())
}

/// The script stays a rename plus a publish plus a smoke check: the
/// bridge contract (four env names), the system workspace for paths
/// outside the checkout, the copy-then-unlink rename, and the
/// `--version` gate before the `GITHUB_PATH` append.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads post-install.oxfile from the repository checkout layout"
)]
fn post_install_script_pins_rename_publish_smoke() -> Result<()> {
    let text = load_post_install_script()?;
    for marker in [
        "INHERIT_ENV [OXDOCK_ENGINE_DIR, OXDOCK_ENGINE_SRC, OXDOCK_ENGINE_DST, GITHUB_PATH]",
        "WORKSPACE SYSTEM",
        "INSTALL \"{{ $dir }}/{{ $src }}\" \"{{ $dir }}/{{ $dst }}\"",
        "REMOVE \"{{ $dir }}/{{ $src }}\"",
        "PATH_TYPE(",
        "\"--version\"",
        "ASSERT_CONTAINS $reported \"oxdock \"",
        "APPEND \"{{ env:GITHUB_PATH }}\"",
    ] {
        assert!(
            text.contains(marker),
            "post-install.oxfile must contain {marker:?}"
        );
    }
    for retired in ["COPY \"{{ $dir }}/{{ $src }}\""] {
        assert!(
            !text.contains(retired),
            "post-install.oxfile must not contain {retired:?}"
        );
    }
    Ok(())
}

#[cfg(unix)]
struct PostInstallRun {
    _temp: oxdock_fs::GuardedTempDir,
    root: GuardedPath,
    engine_dir: GuardedPath,
    gh_path: GuardedPath,
    steps: Vec<oxdock_parser::Step>,
    io: ExecIo,
}

/// Fixture engine: a shell script reporting a fixed version. The
/// executable bit is OS-defined, which is why live execution stays on
/// Unix while parse and marker pins run everywhere.
#[cfg(unix)]
fn write_fixture_engine(path: &GuardedPath) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path.as_path(), "#!/bin/sh\necho \"oxdock 0.0.0-test\"\n")?;
    std::fs::set_permissions(path.as_path(), std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(unix)]
fn post_install_harness(src: &str, dst: &str) -> Result<PostInstallRun> {
    let text = load_post_install_script()?;
    let steps = parse_post_install_script(&text)?;
    let temp = GuardedPath::tempdir().context("tempdir")?;
    let root = temp.as_guarded_path().clone();
    let setup = PathResolver::new_guarded(root.clone(), root.clone())?;
    let engine_dir = root.join("enginedir")?;
    setup.create_dir_all(&engine_dir)?;
    let gh_path = root.join("github_path")?;
    setup.write_file(&gh_path, b"")?;
    write_fixture_engine(&engine_dir.join(src)?)?;
    let mut io = ExecIo::new();
    io.insert_inherit_env(
        "OXDOCK_ENGINE_DIR",
        engine_dir.as_path().display().to_string(),
    );
    io.insert_inherit_env("OXDOCK_ENGINE_SRC", src);
    io.insert_inherit_env("OXDOCK_ENGINE_DST", dst);
    io.insert_inherit_env("GITHUB_PATH", gh_path.as_path().display().to_string());
    Ok(PostInstallRun {
        _temp: temp,
        root,
        engine_dir,
        gh_path,
        steps,
        io,
    })
}

#[cfg(unix)]
#[cfg(unix)]
fn execute(run: &PostInstallRun) -> Result<()> {
    let fs: Box<dyn oxdock_fs::WorkspaceFs> = Box::new(PathResolver::new_guarded(
        run.root.clone(),
        run.root.clone(),
    )?);
    run_steps_with_manager_with_modules(
        fs,
        &run.steps,
        oxdock_process::default_process_manager(),
        run.io.clone(),
        Vec::new(),
        Vec::new(),
    )?;
    Ok(())
}

/// Renamed bootstrap installs end to end: the source name unlinks, the
/// destination runs and reports itself (proving the copy kept its mode),
/// and the engine directory lands on the `GITHUB_PATH` file.
#[test]
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "spawns a real OS process, which dies under Miri isolation"
)]
fn post_install_renames_smokes_and_publishes() -> Result<()> {
    let run = post_install_harness("oxdock-src", "oxdock-engine-test")?;
    execute(&run)?;
    assert!(
        !run.engine_dir.join("oxdock-src")?.as_path().exists(),
        "renamed source must unlink"
    );
    let dst = run.engine_dir.join("oxdock-engine-test")?;
    assert!(dst.as_path().exists(), "renamed engine must exist");
    let published = std::fs::read_to_string(run.gh_path.as_path())?;
    assert!(
        published.contains(&run.engine_dir.as_path().display().to_string()),
        "engine directory must land on GITHUB_PATH, got: {published:?}"
    );
    Ok(())
}

/// Default name skips the rename: the installed file stays put, still
/// smokes, still publishes. The guard must never self-move.
#[test]
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "spawns a real OS process, which dies under Miri isolation"
)]
fn post_install_default_name_skips_rename() -> Result<()> {
    let run = post_install_harness("oxdock", "oxdock")?;
    execute(&run)?;
    assert!(
        run.engine_dir.join("oxdock")?.as_path().exists(),
        "default-named engine must stay in place"
    );
    let published = std::fs::read_to_string(run.gh_path.as_path())?;
    assert!(
        published.contains(&run.engine_dir.as_path().display().to_string()),
        "engine directory must land on GITHUB_PATH, got: {published:?}"
    );
    Ok(())
}
