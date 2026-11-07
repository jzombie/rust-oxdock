use anyhow::{Context, Result};
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_logic_tests::recording::RecordingManager;

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

fn load_script(name: &str) -> Result<String> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    resolver
        .read_to_string(
            &resolver
                .root()
                .join(format!(".github/scripts/{name}").as_str())?,
        )
        .context("read script")
}

/// The resolver must always parse: the release workflow runs it before
/// the build matrix, so a breaking language change fails the PR that
/// makes the change, never release day.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads gha-rust-resolve.oxfile from the repository checkout layout"
)]
fn resolve_script_parses() -> Result<()> {
    let text = load_script("gha-rust-resolve.oxfile")?;
    oxdock_core::parse_script(&text)
        .map_err(|err| anyhow::anyhow!("gha-rust-resolve.oxfile failed to parse: {err}"))?;
    Ok(())
}

/// The stager must always parse, for the same pre-matrix reason.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads gha-rust-stage.oxfile from the repository checkout layout"
)]
fn stage_script_parses() -> Result<()> {
    let text = load_script("gha-rust-stage.oxfile")?;
    oxdock_core::parse_script(&text)
        .map_err(|err| anyhow::anyhow!("gha-rust-stage.oxfile failed to parse: {err}"))?;
    Ok(())
}

struct ResolveRun {
    _temp: oxdock_fs::GuardedTempDir,
    root: GuardedPath,
    out: GuardedPath,
    steps: Vec<oxdock_parser::Step>,
    io: ExecIo,
}

fn resolve_harness(manifest: &str, bins_input: &str, binaries: &str) -> Result<ResolveRun> {
    let text = load_script("gha-rust-resolve.oxfile")?;
    let steps = oxdock_core::parse_script(&text)
        .map_err(|err| anyhow::anyhow!("gha-rust-resolve.oxfile failed to parse: {err}"))?;
    let temp = GuardedPath::tempdir().context("tempdir")?;
    let root = temp.as_guarded_path().clone();
    let setup = PathResolver::new_guarded(root.clone(), root.clone())?;
    setup.write_file(&root.join("Cargo.toml")?, manifest.as_bytes())?;
    let out = root.join("github_output")?;
    let mut io = ExecIo::new();
    io.insert_inherit_env("OXDOCK_BINS_INPUT", bins_input);
    io.insert_inherit_env("OXDOCK_BINARIES", binaries);
    io.insert_inherit_env("GITHUB_WORKSPACE", root.as_path().display().to_string());
    io.insert_inherit_env("GITHUB_OUTPUT", out.as_path().display().to_string());
    Ok(ResolveRun {
        _temp: temp,
        root,
        out,
        steps,
        io,
    })
}

fn read_out(run: &ResolveRun) -> Result<String> {
    let reader = PathResolver::new_guarded(run.root.clone(), run.root.clone())?;
    reader.read_to_string(&run.out).context("read outputs")
}

fn manifest_with_bins(bins: &str) -> String {
    format!(
        "[workspace]\n[workspace.package]\nversion = \"9.9.9-t\"\n\n[workspace.metadata.oxdock]\nrelease_bins = [{bins}]\n"
    )
}

/// Workspace metadata resolves to both the space list and the matrix
/// array, one output line each.
#[test]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn resolve_reads_metadata_list() -> Result<()> {
    let run = resolve_harness(
        &manifest_with_bins("\"oxdock\", \"oxdock-compose\""),
        "",
        "true",
    )?;
    run_steps_with_context_result_with_io(&run.root, &run.root, &run.steps, run.io.clone())?;
    assert_eq!(
        read_out(&run)?,
        "bins_space=oxdock oxdock-compose\nbins_json=[\"oxdock\", \"oxdock-compose\"]\n",
    );
    Ok(())
}

/// An explicit input wins over the metadata list.
#[test]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn resolve_input_overrides_metadata() -> Result<()> {
    let run = resolve_harness(&manifest_with_bins("\"oxdock\""), "solo", "true")?;
    run_steps_with_context_result_with_io(&run.root, &run.root, &run.steps, run.io.clone())?;
    assert_eq!(read_out(&run)?, "bins_space=solo\nbins_json=[\"solo\"]\n",);
    Ok(())
}

/// Surrounding whitespace never leaks into the outputs: fields split
/// and rejoin canonically.
#[test]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn resolve_trims_ragged_input() -> Result<()> {
    let run = resolve_harness(
        "[package]\nname = \"solo\"\nversion = \"0.1.0\"\n",
        "  alpha   beta ",
        "true",
    )?;
    run_steps_with_context_result_with_io(&run.root, &run.root, &run.steps, run.io.clone())?;
    assert_eq!(
        read_out(&run)?,
        "bins_space=alpha beta\nbins_json=[\"alpha\", \"beta\"]\n",
    );
    Ok(())
}

/// Empty products with binaries on fail before any output lands: no
/// partial outputs file for downstream jobs to consume.
#[test]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn resolve_empty_with_binaries_fails_closed() -> Result<()> {
    let run = resolve_harness(
        "[package]\nname = \"solo\"\nversion = \"0.1.0\"\n",
        "",
        "true",
    )?;
    let err = run_steps_with_context_result_with_io(&run.root, &run.root, &run.steps, run.io)
        .map(|_| ())
        .expect_err("empty products must fail resolution");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("EXIT requested with code 1"),
        "empty products exit the resolver: {rendered}"
    );
    let reader = PathResolver::new_guarded(run.root.clone(), run.root.clone())?;
    assert!(
        reader.read_to_string(&run.out).is_err(),
        "failed resolution writes no outputs file"
    );
    Ok(())
}

/// Empty products with binaries off resolve to the empty matrix: the
/// release ships notes only.
#[test]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn resolve_empty_without_binaries_resolves_empty() -> Result<()> {
    let run = resolve_harness(
        "[package]\nname = \"solo\"\nversion = \"0.1.0\"\n",
        "",
        "false",
    )?;
    run_steps_with_context_result_with_io(&run.root, &run.root, &run.steps, run.io.clone())?;
    assert_eq!(read_out(&run)?, "bins_space=\nbins_json=[]\n",);
    Ok(())
}

/// Shared helpers backing the stage script: read from the repo
/// checkout and staged into each synthetic fixture root so the
/// script's IMPORT resolves exactly like the checkout layout.
fn load_helpers_module() -> Result<String> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    resolver
        .read_to_string(
            &resolver
                .root()
                .join(".github/scripts/helpers/ci-helpers.oxfile")?,
        )
        .context("read helpers")
}

fn stage_metadata_json() -> String {
    "{\"packages\": [{\"name\": \"demo\", \"version\": \"1.0.0\", \"targets\": [{\"name\": \"demo\", \"kind\": [\"bin\"]}, {\"name\": \"demo-child\", \"kind\": [\"bin\"]}, {\"name\": \"demo-lib\", \"kind\": [\"lib\"]}]}, {\"name\": \"other-pkg\", \"version\": \"1.0.0\", \"targets\": [{\"name\": \"other\", \"kind\": [\"bin\"]}]}]}"
        .to_string()
}

struct StageRun {
    _temp: oxdock_fs::GuardedTempDir,
    root: GuardedPath,
    manager: RecordingManager,
    steps: Vec<oxdock_parser::Step>,
    io: ExecIo,
}

fn stage_harness(bin: &str, target: &str, exe: &str, files: &[(&str, &[u8])]) -> Result<StageRun> {
    let text = load_script("gha-rust-stage.oxfile")?;
    let steps = oxdock_core::parse_script(&text)
        .map_err(|err| anyhow::anyhow!("gha-rust-stage.oxfile failed to parse: {err}"))?;
    let temp = GuardedPath::tempdir().context("tempdir")?;
    let root = temp.as_guarded_path().clone();
    let setup = PathResolver::new_guarded(root.clone(), root.clone())?;
    for (name, body) in files {
        let path = root.join(&format!("target/{target}/release/{name}"))?;
        setup.create_dir_all(&path.parent().context("parent")?)?;
        setup.write_file(&path, body)?;
    }
    // The script imports the shared helpers module: stage the real file
    // so the import resolves, mirroring the checkout layout.
    let helpers = load_helpers_module()?;
    setup.create_dir_all(&root.join("helpers")?)?;
    setup.write_file(&root.join("helpers/ci-helpers.oxfile")?, helpers.as_bytes())?;
    let manager = RecordingManager::new();
    manager.respond("cargo metadata", &stage_metadata_json());
    let mut io = ExecIo::new();
    io.insert_inherit_env("GITHUB_WORKSPACE", root.as_path().display().to_string());
    io.insert_inherit_env("OXDOCK_MATRIX_BIN", bin);
    io.insert_inherit_env("OXDOCK_MATRIX_TARGET", target);
    io.insert_inherit_env("OXDOCK_MATRIX_EXE", exe);
    Ok(StageRun {
        _temp: temp,
        root,
        manager,
        steps,
        io,
    })
}

impl StageRun {
    fn execute(&self) -> Result<()> {
        let fs: Box<dyn oxdock_fs::WorkspaceFs> = Box::new(PathResolver::new_guarded(
            self.root.clone(),
            self.root.clone(),
        )?);
        oxdock_core::run_steps_with_manager_with_modules(
            fs,
            &self.steps,
            self.manager.clone(),
            self.io.clone(),
            Vec::new(),
            Vec::new(),
        )?;
        Ok(())
    }

    fn staged(&self, name: &str) -> Result<Vec<u8>> {
        let reader = PathResolver::new_guarded(self.root.clone(), self.root.clone())?;
        reader
            .read_file(
                &self
                    .root
                    .join(format!("target/pkg-stage/{name}").as_str())?,
            )
            .context("read staged")
    }
}

/// Every binary target of the matrix package stages; library targets
/// and other packages never do.
#[test]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn stage_copies_package_bins_only() -> Result<()> {
    let run = stage_harness(
        "demo",
        "t1",
        "",
        &[("demo", b"bin-a"), ("demo-child", b"bin-b")],
    )?;
    run.execute()?;
    assert_eq!(run.staged("demo")?, b"bin-a");
    assert_eq!(run.staged("demo-child")?, b"bin-b");
    let reader = PathResolver::new_guarded(run.root.clone(), run.root.clone())?;
    assert!(
        reader
            .read_file(&run.root.join("target/pkg-stage/demo-lib")?)
            .is_err(),
        "library targets never stage"
    );
    assert!(
        reader
            .read_file(&run.root.join("target/pkg-stage/other")?)
            .is_err(),
        "other packages never stage"
    );
    Ok(())
}

/// The executable suffix rides along for Windows matrix legs.
#[test]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn stage_keeps_exe_suffix() -> Result<()> {
    let run = stage_harness(
        "demo",
        "t1",
        ".exe",
        &[("demo.exe", b"bin-a"), ("demo-child.exe", b"bin-b")],
    )?;
    run.execute()?;
    assert_eq!(run.staged("demo.exe")?, b"bin-a");
    assert_eq!(run.staged("demo-child.exe")?, b"bin-b");
    Ok(())
}

/// An unknown product fails naming itself before staging anything.
#[test]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn stage_unknown_product_fails_closed() -> Result<()> {
    let run = stage_harness("nope", "t1", "", &[("nope", b"bin-a")])?;
    let err = run
        .execute()
        .expect_err("unknown product must fail staging");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("EXIT requested with code 1"),
        "unknown product exits the stager: {rendered}"
    );
    let reader = PathResolver::new_guarded(run.root.clone(), run.root.clone())?;
    assert!(
        reader
            .read_file(&run.root.join("target/pkg-stage")?)
            .is_err(),
        "failed staging creates no stage dir"
    );
    Ok(())
}
