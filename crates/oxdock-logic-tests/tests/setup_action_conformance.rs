#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

//! Conformance for the collapsed installer CI tail: `install.oxfile`
//! serves users and CI from one path. Parse and marker pins run
//! everywhere; live execution runs on Unix only, where a shell-script
//! fixture can stand in for the engine (Windows coverage is the smoke
//! matrix itself).

use anyhow::{Context, Result};
#[cfg(unix)]
use oxdock_core::{ExecIo, run_steps_with_manager_with_modules};
use oxdock_fs::{GuardedPath, PathResolver};

fn repo_root() -> Result<String> {
    let manifest_dir = std::env::var(oxdock_fs::env::cargo::MANIFEST_DIR)
        .context("CARGO_MANIFEST_DIR missing")?
        .replace('\\', "/");
    Ok(manifest_dir
        .strip_suffix("crates/oxdock-logic-tests")
        .context("test must live under crates/oxdock-logic-tests")?
        .trim_end_matches('/')
        .to_string())
}

fn load_install_script() -> Result<String> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let script_path = resolver.root().join("install.oxfile")?;
    resolver.read_to_string(&script_path)
}

fn parse_install_script(text: &str) -> Result<Vec<oxdock_parser::Step>> {
    oxdock_core::parse_script(text)
        .map_err(|err| anyhow::anyhow!("install.oxfile failed to parse: {err}"))
}

/// The installer must always parse: the setup action runs it under the
/// freshly installed engine on every CI job.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads install.oxfile from the repository checkout layout"
)]
fn install_script_parses_for_action() -> Result<()> {
    let text = load_install_script()?;
    parse_install_script(&text)?;
    Ok(())
}

/// Single installer, no side channel: binary-name, skip flag, nested
/// dual gate, and PATH publish all live in `install.oxfile`.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads install.oxfile from the repository checkout layout"
)]
fn install_script_pins_single_path() -> Result<()> {
    let text = load_install_script()?;
    for marker in [
        "OXDOCK_BINARY_NAME",
        "OXDOCK_SKIP_DOWNLOAD",
        "LET $skip: BOOL",
        "TEXT_TRIM_SUFFIX",
        "TEXT_TRIM_PREFIX",
        "LET $has_ghpath: BOOL",
        "LET $has_expected: BOOL",
        "INSTALL $bin $dest",
        "\"--version\"",
        "ASSERT_EQ $reported \"oxdock {{ $expected }}\"",
        "APPEND \"{{ env:GITHUB_PATH }}\"",
        "OXDOCK_ENGINE_DIR=",
    ] {
        assert!(
            text.contains(marker),
            "install.oxfile must contain {marker:?}"
        );
    }
    for retired in ["OXDOCK_ENGINE_BIN", "post-install.oxfile"] {
        assert!(
            !text.contains(retired),
            "install.oxfile must not contain {retired:?}"
        );
    }
    Ok(())
}

#[cfg(unix)]
struct CiTailRun {
    _temp: oxdock_fs::GuardedTempDir,
    root: GuardedPath,
    engine_dir: GuardedPath,
    gh_path: GuardedPath,
    gh_env: GuardedPath,
    steps: Vec<oxdock_parser::Step>,
    io: ExecIo,
}

#[cfg(unix)]
fn write_fixture_engine(path: &GuardedPath) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path.as_path(), "#!/bin/sh\necho \"oxdock 0.0.0-test\"\n")?;
    std::fs::set_permissions(path.as_path(), std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(unix)]
fn ci_tail_harness() -> Result<CiTailRun> {
    let text = load_install_script()?;
    let steps = parse_install_script(&text)?;
    let temp = GuardedPath::tempdir().context("tempdir")?;
    let root = temp.as_guarded_path().clone();
    let setup = PathResolver::new_guarded(root.clone(), root.clone())?;
    let engine_dir = root.join("enginedir")?;
    setup.create_dir_all(&engine_dir)?;
    let gh_path = root.join("github_path")?;
    setup.write_file(&gh_path, b"")?;
    let gh_env = root.join("github_env")?;
    setup.write_file(&gh_env, b"")?;
    let bin = "oxdock-engine-test";
    let src = root.join("srcbin")?;
    write_fixture_engine(&src)?;
    let mut io = ExecIo::new();
    io.insert_inherit_env("OXDOCK_ASSET", "");
    io.insert_inherit_env("OXDOCK_SHA", "");
    io.insert_inherit_env("OXDOCK_BIN", src.as_path().display().to_string());
    io.insert_inherit_env("OXDOCK_DIR", engine_dir.as_path().display().to_string());
    io.insert_inherit_env("OXDOCK_VERSION", "0.0.0-test");
    io.insert_inherit_env("OXDOCK_BINARY_NAME", bin);
    io.insert_inherit_env("OXDOCK_SKIP_DOWNLOAD", "1");
    io.insert_inherit_env("OXDOCK_EXPECTED_VERSION", "v0.0.0-test");
    io.insert_inherit_env("GITHUB_PATH", gh_path.as_path().display().to_string());
    io.insert_inherit_env("GITHUB_ENV", gh_env.as_path().display().to_string());
    Ok(CiTailRun {
        _temp: temp,
        root,
        engine_dir,
        gh_path,
        gh_env,
        steps,
        io,
    })
}

#[cfg(unix)]
fn execute(run: &CiTailRun) -> Result<()> {
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

/// Skip mode reuses the tail: version asserts under the placed binary,
/// directory lands on `GITHUB_PATH`, engine dir lands on `GITHUB_ENV`.
#[test]
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "spawns a real OS process, which dies under Miri isolation"
)]
fn ci_tail_smokes_and_publishes() -> Result<()> {
    let run = ci_tail_harness()?;
    execute(&run)?;
    let published = std::fs::read_to_string(run.gh_path.as_path())?;
    assert!(
        published.contains(&run.engine_dir.as_path().display().to_string()),
        "engine directory must land on GITHUB_PATH, got: {published:?}"
    );
    let env_out = std::fs::read_to_string(run.gh_env.as_path())?;
    assert!(
        env_out.contains("OXDOCK_ENGINE_DIR="),
        "engine dir must land on GITHUB_ENV, got: {env_out:?}"
    );
    Ok(())
}

/// No bridged version: the tail derives it from the binary and still
/// publishes. This is the `local` shape, which sets no expected tag.
#[test]
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "spawns a real OS process, which dies under Miri isolation"
)]
fn ci_tail_derives_version_when_unbridged() -> Result<()> {
    let mut run = ci_tail_harness()?;
    run.io.insert_inherit_env("OXDOCK_EXPECTED_VERSION", "");
    execute(&run)?;
    let published = std::fs::read_to_string(run.gh_path.as_path())?;
    assert!(
        published.contains(&run.engine_dir.as_path().display().to_string()),
        "engine directory must land on GITHUB_PATH, got: {published:?}"
    );
    Ok(())
}

/// No bridge, no override binary: the tail derives from the
/// pre-placed binary. This is the stub fresh-tail shape.
#[test]
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "spawns a real OS process, which dies under Miri isolation"
)]
fn ci_tail_derives_version_without_override() -> Result<()> {
    let mut run = ci_tail_harness()?;
    run.io.insert_inherit_env("OXDOCK_BIN", "");
    run.io.insert_inherit_env("OXDOCK_VERSION", "9.9.9-t");
    run.io.insert_inherit_env("OXDOCK_EXPECTED_VERSION", "");
    write_fixture_engine(&run.engine_dir.join("oxdock-engine-test")?)?;
    execute(&run)?;
    let published = std::fs::read_to_string(run.gh_path.as_path())?;
    assert!(
        published.contains(&run.engine_dir.as_path().display().to_string()),
        "engine directory must land on GITHUB_PATH, got: {published:?}"
    );
    Ok(())
}
