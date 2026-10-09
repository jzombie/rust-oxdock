use anyhow::{Context, Result};
use oxdock_core::{ExecIo, run_steps_with_manager_with_modules};
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_logic_tests::recording::RecordingManager;
#[cfg(windows)]
use oxdock_logic_tests::recording::argv_calls;

/// sha256 of `INSTALL_PAYLOAD`, computed once and pinned beside it: if
/// the bytes change without the digest, the success test fails loudly
/// instead of verifying against itself.
const INSTALL_PAYLOAD: &[u8] = b"oxdock-install-fixture-payload-v1";
const INSTALL_DIGEST: &str = "8e6873401de1e38cfc5b986fd9330e26d7299469509d968f0b49031a7cfa1734";

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

/// The installer script must always parse: it ships inside every asset
/// tarball's download flow, so a breaking language change that
/// invalidates it fails the PR that makes the change.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads install.oxfile from the repository checkout layout"
)]
fn install_script_parses() -> Result<()> {
    let text = load_install_script()?;
    parse_install_script(&text)?;
    Ok(())
}

struct InstallRun {
    _temp: oxdock_fs::GuardedTempDir,
    root: GuardedPath,
    manager: RecordingManager,
    steps: Vec<oxdock_parser::Step>,
    io: ExecIo,
}

fn install_harness(payload: &[u8], digest: &str, dir: Option<&str>) -> Result<InstallRun> {
    let text = load_install_script()?;
    let steps = parse_install_script(&text)?;
    let temp = GuardedPath::tempdir().context("tempdir")?;
    let root = temp.as_guarded_path().clone();
    let setup = PathResolver::new_guarded(root.clone(), root.clone())?;
    setup.write_file(&root.join("fixture.tar.gz")?, payload)?;
    setup.create_dir_all(&root.join("x")?)?;
    setup.write_file(&root.join("x/oxdock")?, payload)?;
    let manager = RecordingManager::new();
    let mut io = ExecIo::new();
    io.insert_inherit_env(
        "OXDOCK_ASSET",
        root.join("fixture.tar.gz")?.as_path().display().to_string(),
    );
    io.insert_inherit_env("OXDOCK_SHA", digest);
    io.insert_inherit_env(
        "OXDOCK_BIN",
        root.join("x/oxdock")?.as_path().display().to_string(),
    );
    if let Some(dir) = dir {
        io.insert_inherit_env(
            "OXDOCK_DIR",
            root.join(dir)?.as_path().display().to_string(),
        );
    } else {
        io.insert_inherit_env("HOME", root.join("home")?.as_path().display().to_string());
        io.insert_inherit_env(
            "USERPROFILE",
            root.join("home")?.as_path().display().to_string(),
        );
    }
    io.insert_inherit_env("OXDOCK_VERSION", "9.9.9-t");
    Ok(InstallRun {
        _temp: temp,
        root,
        manager,
        steps,
        io,
    })
}

impl InstallRun {
    fn calls(&self) -> Vec<oxdock_logic_tests::recording::Call> {
        self.manager.calls()
    }

    fn execute(&self) -> Result<()> {
        let fs: Box<dyn oxdock_fs::WorkspaceFs> = Box::new(PathResolver::new_guarded(
            self.root.clone(),
            self.root.clone(),
        )?);
        run_steps_with_manager_with_modules(
            fs,
            &self.steps,
            self.manager.clone(),
            self.io.clone(),
            Vec::new(),
            Vec::new(),
        )?;
        Ok(())
    }
}

/// Verified bytes install end to end: the placed file lands in the
/// destination holding the exact payload bytes. Unix places natively
/// (no processes spawn); Windows places through powershell because
/// guarded writes outside the workspace are denied there.
#[test]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn verified_asset_installs_bytes() -> Result<()> {
    let run = install_harness(INSTALL_PAYLOAD, INSTALL_DIGEST, Some("dest"))?;
    run.execute()?;
    #[cfg(unix)]
    let placed = "dest/oxdock";
    #[cfg(windows)]
    let placed = "dest/oxdock.exe";
    let reader = PathResolver::new_guarded(run.root.clone(), run.root.clone())?;
    let body = reader.read_file(&run.root.join(placed)?)?;
    assert_eq!(
        body, INSTALL_PAYLOAD,
        "placed file holds the verified bytes"
    );
    #[cfg(unix)]
    assert!(
        run.calls().is_empty(),
        "native placement spawns no processes: {:?}",
        run.calls()
    );
    #[cfg(windows)]
    assert_eq!(
        argv_calls(&run.calls()),
        vec![
            vec![
                "powershell".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "New-Item -ItemType Directory -Force -Path 'dest'".to_string(),
            ],
            vec![
                "powershell".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "Copy-Item 'x/oxdock' 'dest/oxdock.exe' -Force".to_string(),
            ],
        ],
        "dir then place through powershell"
    );
    Ok(())
}

/// Tampered bytes abort at the hash gate: the failure names the
/// mismatch and no extract or install process ever spawns.
#[test]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn tampered_asset_aborts_before_place() -> Result<()> {
    let run = install_harness(INSTALL_PAYLOAD, &"0".repeat(64), Some("dest"))?;
    let err = run
        .execute()
        .expect_err("tampered asset must fail verification");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("ASSERT_EQ mismatch"),
        "failure names the mismatch: {rendered}"
    );
    assert!(
        run.calls().is_empty(),
        "no process spawned after a failed verify: {:?}",
        run.calls()
    );
    Ok(())
}

/// Unbridged dir defaults under home: the placed file lands in
/// `.local` beneath the overridden home, with no stub involvement.
#[test]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn empty_dir_defaults_under_home() -> Result<()> {
    let run = install_harness(INSTALL_PAYLOAD, INSTALL_DIGEST, None)?;
    run.execute()?;
    #[cfg(unix)]
    let placed = "home/.local/bin/oxdock";
    #[cfg(windows)]
    let placed = "home/.local/bin/oxdock.exe";
    let reader = PathResolver::new_guarded(run.root.clone(), run.root.clone())?;
    let body = reader.read_file(&run.root.join(placed)?)?;
    assert_eq!(
        body, INSTALL_PAYLOAD,
        "default destination holds the verified bytes"
    );
    Ok(())
}

/// Missing asset fails naming the file, before any process spawns.
#[test]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn missing_asset_fails_naming_the_file() -> Result<()> {
    let text = load_install_script()?;
    let steps = parse_install_script(&text)?;
    let temp = GuardedPath::tempdir().context("tempdir")?;
    let root = temp.as_guarded_path().clone();
    let manager = RecordingManager::new();
    let mut io = ExecIo::new();
    io.insert_inherit_env(
        "OXDOCK_ASSET",
        root.join("absent.tar.gz")?.as_path().display().to_string(),
    );
    io.insert_inherit_env("OXDOCK_SHA", INSTALL_DIGEST);
    io.insert_inherit_env(
        "OXDOCK_BIN",
        root.join("x/oxdock")?.as_path().display().to_string(),
    );
    io.insert_inherit_env(
        "OXDOCK_DIR",
        root.join("dest")?.as_path().display().to_string(),
    );
    io.insert_inherit_env("OXDOCK_VERSION", "9.9.9-t");
    let fs: Box<dyn oxdock_fs::WorkspaceFs> =
        Box::new(PathResolver::new_guarded(root.clone(), root.clone())?);
    let err = run_steps_with_manager_with_modules(
        fs,
        &steps,
        manager.clone(),
        io,
        Vec::new(),
        Vec::new(),
    )
    .map(|_| ())
    .expect_err("missing asset must fail");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("absent.tar.gz"),
        "failure names the missing file: {rendered}"
    );
    assert!(
        manager.calls().is_empty(),
        "no process spawned for a missing asset"
    );
    Ok(())
}
