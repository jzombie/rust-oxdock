#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

//! Conformance for the setup-oxdock post-install script: the DSL that
//! smoke-checks the installed engine and publishes it on `GITHUB_PATH`
//! after every install. The shell renames before invoking (the engine
//! cannot rename the file it runs from on Windows), so this script only
//! verifies and publishes. Parse and marker pins run everywhere; live
//! execution runs on Unix only, where a shell-script fixture can stand
//! in for the engine (Windows cannot execute a fixture binary without
//! shipping one, so Windows coverage is the smoke matrix itself, which
//! runs this script on every platform per PR).

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

/// The script stays a smoke check plus a publish: the bridge contract
/// (three env names), the system workspace for paths outside the
/// checkout, the file-kind gate, the `--version` run, and the
/// `GITHUB_PATH` append. Retired names must not linger.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads post-install.oxfile from the repository checkout layout"
)]
fn post_install_script_pins_smoke_publish() -> Result<()> {
    let text = load_post_install_script()?;
    for marker in [
        "INHERIT_ENV [OXDOCK_ENGINE_DIR, OXDOCK_ENGINE_BIN, OXDOCK_EXPECTED_VERSION, GITHUB_PATH]",
        "WORKSPACE SYSTEM",
        "PATH_TYPE(",
        "\"--version\"",
        "ASSERT_EQ $reported \"oxdock {{ env:OXDOCK_EXPECTED_VERSION }}\"",
        "APPEND \"{{ env:GITHUB_PATH }}\"",
    ] {
        assert!(
            text.contains(marker),
            "post-install.oxfile must contain {marker:?}"
        );
    }
    for retired in ["OXDOCK_ENGINE_SRC", "OXDOCK_ENGINE_DST"] {
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
fn post_install_harness(bin: &str) -> Result<PostInstallRun> {
    let text = load_post_install_script()?;
    let steps = parse_post_install_script(&text)?;
    let temp = GuardedPath::tempdir().context("tempdir")?;
    let root = temp.as_guarded_path().clone();
    let setup = PathResolver::new_guarded(root.clone(), root.clone())?;
    let engine_dir = root.join("enginedir")?;
    setup.create_dir_all(&engine_dir)?;
    let gh_path = root.join("github_path")?;
    setup.write_file(&gh_path, b"")?;
    write_fixture_engine(&engine_dir.join(bin)?)?;
    let mut io = ExecIo::new();
    io.insert_inherit_env(
        "OXDOCK_ENGINE_DIR",
        engine_dir.as_path().display().to_string(),
    );
    io.insert_inherit_env("OXDOCK_ENGINE_BIN", bin);
    io.insert_inherit_env("OXDOCK_EXPECTED_VERSION", "0.0.0-test");
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

/// Installed engine verifies end to end: the file gates as a file, runs
/// and reports itself, and its directory lands on the `GITHUB_PATH`
/// file. A fixture that stops reporting fails the smoke gate instead.
#[test]
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "spawns a real OS process, which dies under Miri isolation"
)]
fn post_install_smokes_and_publishes() -> Result<()> {
    let run = post_install_harness("oxdock-engine-test")?;
    execute(&run)?;
    assert!(
        run.engine_dir
            .join("oxdock-engine-test")?
            .as_path()
            .exists(),
        "installed engine must exist"
    );
    let published = std::fs::read_to_string(run.gh_path.as_path())?;
    assert!(
        published.contains(&run.engine_dir.as_path().display().to_string()),
        "engine directory must land on GITHUB_PATH, got: {published:?}"
    );
    Ok(())
}
