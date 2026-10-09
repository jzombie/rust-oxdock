use anyhow::{Context, Result};
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, PathResolver};

fn repo_root() -> Result<String> {
    // Same layout derivation as docs_conformance: normalize separators
    // first since Windows CARGO_MANIFEST_DIR uses backslashes.
    let manifest_dir = std::env::var(oxdock_fs::env::CARGO_MANIFEST_DIR)
        .context("CARGO_MANIFEST_DIR missing")?
        .replace('\\', "/");
    Ok(manifest_dir
        .strip_suffix("crates/oxdock-logic-tests")
        .context("test must live under crates/oxdock-logic-tests")?
        .trim_end_matches('/')
        .to_string())
}

fn load_release_script() -> Result<String> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let script_path = resolver.root().join("gha-rust-release.oxfile")?;
    resolver.read_to_string(&script_path)
}

/// The release script imports MARKDOWN, so it parses against a module
/// table carrying the plugin, mirroring the CLI `--features markdown`
/// build the release workflow runs.
#[cfg(feature = "markdown")]
fn parse_release_script(text: &str) -> Result<Vec<oxdock_parser::Step>> {
    let mut engine = oxdock_core::Engine::new();
    engine.register_module(oxdock_markdown_plugin::module());
    oxdock_core::parse_script_with_modules(text, engine.module_table())
        .map_err(|err| anyhow::anyhow!("gha-rust-release.oxfile failed to parse: {err}"))
}

/// The release script must always parse: a breaking language change that
/// invalidates it fails the PR that makes the change, never release day.
/// Parsing is the compile check; every behavior below executes the real
/// file against a recording process manager, never its text.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(
    miri,
    ignore = "reads gha-rust-release.oxfile from the repository checkout layout"
)]
fn release_script_parses() -> Result<()> {
    let text = load_release_script()?;
    parse_release_script(&text)?;
    Ok(())
}

/// The standalone version gate must always parse: it guards the build
/// matrix, so a breaking language change that invalidates it fails the
/// PR that makes the change, never release day.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads gha-rust-validate.oxfile from the repository checkout layout"
)]
fn validate_script_parses_and_gates_on_version() -> Result<()> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let script_path = resolver.root().join("gha-rust-validate.oxfile")?;
    let text = resolver.read_to_string(&script_path)?;
    oxdock_core::parse_script(&text)
        .map_err(|err| anyhow::anyhow!("gha-rust-validate.oxfile failed to parse: {err}"))?;
    Ok(())
}

/// The version fallback executes for real: the actual
/// `gha-rust-validate.oxfile` (side-effect-free: it only reads and
/// asserts) runs to completion against synthetic roots of every
/// manifest layout, with the confirmation bridged to match. No
/// duplicated logic: the harness owns the root, the file owns the
/// branching.
#[test]
#[cfg_attr(miri, ignore = "needs host tempdir for synthetic Cargo roots")]
fn version_fallback_covers_all_manifest_layouts() -> Result<()> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let script_path = resolver.root().join("gha-rust-validate.oxfile")?;
    let text = resolver.read_to_string(&script_path)?;
    let steps = oxdock_core::parse_script(&text)
        .map_err(|err| anyhow::anyhow!("gha-rust-validate.oxfile failed to parse: {err}"))?;
    for (manifest, expected) in [
        (
            "[workspace]\n[workspace.package]\nversion = \"1.2.3-ws\"\n",
            "1.2.3-ws",
        ),
        (
            "[package]\nname = \"solo\"\nversion = \"4.5.6-solo\"\n",
            "4.5.6-solo",
        ),
        (
            "[package]\nname = \"trad\"\nversion = \"7.8.9-trad\"\n\n[workspace]\nmembers = [\"crates/*\"]\n",
            "7.8.9-trad",
        ),
    ] {
        let temp = GuardedPath::tempdir().context("tempdir")?;
        let fixture = temp.as_guarded_path().clone();
        let fixture_resolver = PathResolver::new_guarded(fixture.clone(), fixture.clone())?;
        let cargo_path = fixture.join("Cargo.toml")?;
        fixture_resolver.write_file(&cargo_path, manifest.as_bytes())?;
        let mut io = ExecIo::new();
        io.insert_inherit_env("RELEASE_CONFIRM", expected);
        run_steps_with_context_result_with_io(&fixture, &fixture, &steps, io)
            .map_err(|err| anyhow::anyhow!("validate must pass for version {expected}: {err:#}"))?;
    }
    Ok(())
}

/// The tree version always has extractable notes: the same extraction
/// the release script runs, asserted on every PR, so a missing
/// CHANGELOG section fails long before release day.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(
    miri,
    ignore = "reads the repository manifest and changelog from the checkout"
)]
fn release_notes_extract_for_tree_version() -> Result<()> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let manifest_path = resolver.root().join("Cargo.toml")?;
    let manifest_text = resolver.read_to_string(&manifest_path)?;
    let document: toml_edit::DocumentMut = manifest_text
        .parse()
        .map_err(|err| anyhow::anyhow!("Cargo.toml must parse: {err}"))?;
    let version = document["workspace"]["package"]["version"]
        .as_str()
        .context("workspace.package.version must exist")?;
    let log_path = resolver.root().join("CHANGELOG.md")?;
    let log = resolver.read_to_string(&log_path)?;
    let notes = oxdock_markdown_plugin::markdown::extract_section(&log, &format!("[{version}]"))
        .map_err(|err| anyhow::anyhow!("CHANGELOG must hold the tree version: {err}"))?;
    assert!(
        notes.starts_with(&format!("## [{version}]")),
        "notes open with the version heading"
    );
    assert!(
        notes.lines().count() > 1,
        "notes carry body beyond the heading"
    );
    Ok(())
}

/// A mismatched confirmation fails at the gate with no side effects:
/// version read plus assert run before any publish, tag, or release step.
/// The empty invocation log proves nothing spawned.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn release_gate_rejects_mismatched_confirmation() -> Result<()> {
    let run = release_harness("0.0.0-nope", "true", "true")?;
    let err = run
        .execute()
        .expect_err("mismatched confirmation must fail");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("ASSERT_EQ mismatch"),
        "gate names the mismatch: {rendered}"
    );
    assert!(
        run.calls().is_empty(),
        "no process spawned before the gate: {:?}",
        run.calls()
    );
    Ok(())
}

/// Recording process manager: stands in for cargo, git, and gh so the
/// real release scripts execute end to end with zero side effects.
/// Every invocation lands in one shared log; canned stdout is identical
/// everywhere, which is exactly what the tag identity gate compares.
#[derive(Clone, Debug, PartialEq)]
#[cfg(feature = "markdown")]
enum Call {
    Argv(Vec<String>),
    Shell(String),
}

#[derive(Clone)]
#[cfg(feature = "markdown")]
struct RecordingManager {
    log: std::sync::Arc<std::sync::Mutex<Vec<Call>>>,
}

#[derive(Clone)]
#[cfg(feature = "markdown")]
struct RecordingHandle;

#[cfg(feature = "markdown")]
fn exit_success() -> std::process::ExitStatus {
    #[cfg(unix)]
    {
        std::os::unix::process::ExitStatusExt::from_raw(0)
    }
    #[cfg(windows)]
    {
        std::os::windows::process::ExitStatusExt::from_raw(0)
    }
}

#[cfg(feature = "markdown")]
impl oxdock_process::BackgroundHandle for RecordingHandle {
    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(Some(exit_success()))
    }
    fn kill(&mut self) -> Result<()> {
        Ok(())
    }
    fn wait(&mut self) -> Result<std::process::ExitStatus> {
        Ok(exit_success())
    }
}

#[cfg(feature = "markdown")]
impl oxdock_process::ProcessManager for RecordingManager {
    type Handle = RecordingHandle;

    fn run_command(
        &mut self,
        _ctx: &oxdock_process::CommandContext,
        script: &str,
        options: oxdock_process::CommandOptions,
    ) -> Result<oxdock_process::CommandResult<Self::Handle>> {
        self.log
            .lock()
            .expect("log")
            .push(Call::Shell(script.to_string()));
        match options.stdout {
            oxdock_process::CommandStdout::Capture => Ok(oxdock_process::CommandResult::Captured(
                b"deadbeef\n".to_vec(),
            )),
            _ => Ok(oxdock_process::CommandResult::Completed),
        }
    }

    fn run_argv(
        &mut self,
        _ctx: &oxdock_process::CommandContext,
        argv: &[String],
        options: oxdock_process::CommandOptions,
    ) -> Result<oxdock_process::CommandResult<Self::Handle>> {
        self.log
            .lock()
            .expect("log")
            .push(Call::Argv(argv.to_vec()));
        match options.stdout {
            oxdock_process::CommandStdout::Capture => Ok(oxdock_process::CommandResult::Captured(
                b"deadbeef\n".to_vec(),
            )),
            _ => Ok(oxdock_process::CommandResult::Completed),
        }
    }
}

/// Fixture root carrying a workspace Cargo.toml plus a CHANGELOG whose
/// wanted section precedes a decoy, so notes slicing proves itself.
#[cfg(feature = "markdown")]
const FIXTURE_VERSION: &str = "9.9.9-t";
#[cfg(feature = "markdown")]
const FIXTURE_TAG: &str = "v9.9.9-t";

fn fixture_manifest() -> String {
    "[workspace]\n[workspace.package]\nversion = \"9.9.9-t\"\n".to_string()
}

fn fixture_changelog() -> String {
    "# Changelog\n\n## [9.9.9-t] - 2026-10-09\n\n### Added\n\n- Thing.\n\n## [0.0.0-old] - 2026-01-01\n\n- Decoy.\n".to_string()
}

#[cfg(feature = "markdown")]
struct ReleaseRun {
    _temp: oxdock_fs::GuardedTempDir,
    root: GuardedPath,
    manager: RecordingManager,
    steps: Vec<oxdock_parser::Step>,
    io: ExecIo,
}

#[cfg(feature = "markdown")]
#[cfg(feature = "markdown")]
fn release_harness(confirm: &str, dry_run: &str, binaries: &str) -> Result<ReleaseRun> {
    let text = load_release_script()?;
    let steps = parse_release_script(&text)?;
    let temp = GuardedPath::tempdir().context("tempdir")?;
    let root = temp.as_guarded_path().clone();
    let setup = PathResolver::new_guarded(root.clone(), root.clone())?;
    setup.write_file(&root.join("Cargo.toml")?, fixture_manifest().as_bytes())?;
    setup.write_file(&root.join("CHANGELOG.md")?, fixture_changelog().as_bytes())?;
    setup.create_dir_all(&root.join("target")?)?;
    let manager = RecordingManager {
        log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let mut io = ExecIo::new();
    io.insert_inherit_env("RELEASE_CONFIRM", confirm);
    io.insert_inherit_env("RELEASE_DRY_RUN", dry_run);
    io.insert_inherit_env("RELEASE_BINARY", "demo");
    io.insert_inherit_env("RELEASE_BINARIES", binaries);
    Ok(ReleaseRun {
        _temp: temp,
        root,
        manager,
        steps,
        io,
    })
}

#[cfg(feature = "markdown")]
#[cfg(feature = "markdown")]
impl ReleaseRun {
    fn calls(&self) -> Vec<Call> {
        self.manager.log.lock().expect("log").clone()
    }

    fn execute(self) -> Result<()> {
        let modules = vec![oxdock_markdown_plugin::module_with::<RecordingManager>()];
        let fs: Box<dyn oxdock_fs::WorkspaceFs> = Box::new(PathResolver::new_guarded(
            self.root.clone(),
            self.root.clone(),
        )?);
        oxdock_core::run_steps_with_manager_with_modules(
            fs,
            &self.steps,
            self.manager.clone(),
            self.io,
            modules,
            Vec::new(),
        )?;
        Ok(())
    }

    fn notes(&self) -> Result<String> {
        let reader = PathResolver::new_guarded(self.root.clone(), self.root.clone())?;
        reader.read_to_string(&self.root.join("target/release-notes.md")?)
    }
}

#[cfg(feature = "markdown")]
fn argv_calls(calls: &[Call]) -> Vec<Vec<String>> {
    calls
        .iter()
        .filter_map(|call| match call {
            Call::Argv(argv) => Some(argv.clone()),
            Call::Shell(_) => None,
        })
        .collect()
}

#[cfg(feature = "markdown")]
fn shell_calls<'a>(calls: &'a [Call]) -> Vec<&'a str> {
    calls
        .iter()
        .filter_map(|call| match call {
            Call::Argv(_) => None,
            Call::Shell(script) => Some(script.as_str()),
        })
        .collect()
}

/// Dry run publishes nothing: exactly one dry-run publish invocation,
/// notes sliced to the wanted section, no tag, no push, no gh.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn dry_run_runs_only_the_dry_publish() -> Result<()> {
    let run = release_harness(FIXTURE_VERSION, "true", "true")?;
    run.execute()?;
    assert_eq!(
        run.calls(),
        vec![Call::Argv(vec![
            "cargo".to_string(),
            "publish".to_string(),
            "--workspace".to_string(),
            "--dry-run".to_string(),
        ])],
        "dry run spawns exactly one process"
    );
    let notes = run.notes()?;
    assert!(
        notes.contains("Thing."),
        "notes carry the wanted section: {notes}"
    );
    assert!(
        !notes.contains("Decoy"),
        "notes exclude the decoy section: {notes}"
    );
    Ok(())
}

/// Full run with binaries: the exact eight-invocation pipeline in order.
/// Workspace publishing stays workspace-scoped (exact argv proves no
/// per-crate flags), the tag gate passes on identical canned output,
/// and creation carries every asset plus the checksums file.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn real_run_with_binaries_runs_the_full_pipeline() -> Result<()> {
    let run = release_harness(FIXTURE_VERSION, "false", "true")?;
    run.execute()?;
    let calls = run.calls();
    assert_eq!(calls.len(), 8, "eight invocations in order: {calls:?}");
    assert_eq!(
        argv_calls(&calls)[..2],
        vec![
            vec![
                "cargo".to_string(),
                "publish".to_string(),
                "--workspace".to_string(),
                "--dry-run".to_string(),
            ],
            vec![
                "cargo".to_string(),
                "publish".to_string(),
                "--workspace".to_string(),
            ],
        ],
        "workspace publish, dry first, no per-crate flags"
    );
    let shells = shell_calls(&calls);
    assert!(
        shells
            .iter()
            .any(|s| s.contains("rev-parse") && s.contains("HEAD")),
        "tag identity probes HEAD: {shells:?}"
    );
    assert!(
        shells
            .iter()
            .any(|s| s.contains("rev-list") && s.contains(FIXTURE_TAG)),
        "tag identity probes the tag: {shells:?}"
    );
    assert!(
        shells
            .iter()
            .any(|s| s.contains("gh release delete") && s.contains(FIXTURE_TAG)),
        "stale release cleanup runs: {shells:?}"
    );
    let argvs = argv_calls(&calls);
    let creates: Vec<&Vec<String>> = argvs
        .iter()
        .filter(|argv| argv.first().is_some_and(|exe| exe == "gh"))
        .collect();
    assert_eq!(
        creates,
        vec![&vec![
            "gh".to_string(),
            "release".to_string(),
            "create".to_string(),
            FIXTURE_TAG.to_string(),
            "--title".to_string(),
            FIXTURE_VERSION.to_string(),
            "--notes-file".to_string(),
            "target/release-notes.md".to_string(),
            "target/artifacts/demo-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            "target/artifacts/demo-aarch64-unknown-linux-gnu.tar.gz".to_string(),
            "target/artifacts/demo-aarch64-apple-darwin.tar.gz".to_string(),
            "target/artifacts/demo-x86_64-pc-windows-msvc.tar.gz".to_string(),
            "target/artifacts/demo-aarch64-pc-windows-msvc.tar.gz".to_string(),
            "target/artifacts/SHA256SUMS".to_string(),
        ]],
        "creation carries notes, every asset, and checksums"
    );
    Ok(())
}

/// Full run without binaries: tag, push, and cleanup still run, but the
/// creation call carries notes alone with no asset paths.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn real_run_without_binaries_creates_notes_only() -> Result<()> {
    let run = release_harness(FIXTURE_VERSION, "false", "false")?;
    run.execute()?;
    let calls = run.calls();
    let argvs = argv_calls(&calls);
    let creates: Vec<&Vec<String>> = argvs
        .iter()
        .filter(|argv| argv.first().is_some_and(|exe| exe == "gh"))
        .collect();
    assert_eq!(
        creates,
        vec![&vec![
            "gh".to_string(),
            "release".to_string(),
            "create".to_string(),
            FIXTURE_TAG.to_string(),
            "--title".to_string(),
            FIXTURE_VERSION.to_string(),
            "--notes-file".to_string(),
            "target/release-notes.md".to_string(),
        ]],
        "notes-only creation carries no asset paths"
    );
    assert!(
        !calls.iter().any(|call| match call {
            Call::Argv(argv) => argv.iter().any(|arg| arg.contains(".tar.gz")),
            Call::Shell(script) => script.contains(".tar.gz"),
        }),
        "no asset path anywhere in a binaries-off run: {calls:?}"
    );
    Ok(())
}

/// Missing CHANGELOG section fails naming the needle, before any
/// publish, tag, or release step runs.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn missing_section_fails_naming_the_needle() -> Result<()> {
    let text = load_release_script()?;
    let steps = parse_release_script(&text)?;
    let temp = GuardedPath::tempdir().context("tempdir")?;
    let root = temp.as_guarded_path().clone();
    let setup = PathResolver::new_guarded(root.clone(), root.clone())?;
    setup.write_file(&root.join("Cargo.toml")?, fixture_manifest().as_bytes())?;
    setup.write_file(
        &root.join("CHANGELOG.md")?,
        "# Changelog\n\n## [0.0.0-old] - 2026-01-01\n\n- Decoy.\n".as_bytes(),
    )?;
    setup.create_dir_all(&root.join("target")?)?;
    let manager = RecordingManager {
        log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let mut io = ExecIo::new();
    io.insert_inherit_env("RELEASE_CONFIRM", FIXTURE_VERSION);
    io.insert_inherit_env("RELEASE_DRY_RUN", "true");
    io.insert_inherit_env("RELEASE_BINARY", "demo");
    io.insert_inherit_env("RELEASE_BINARIES", "true");
    let modules = vec![oxdock_markdown_plugin::module_with::<RecordingManager>()];
    let fs: Box<dyn oxdock_fs::WorkspaceFs> =
        Box::new(PathResolver::new_guarded(root.clone(), root.clone())?);
    let err = oxdock_core::run_steps_with_manager_with_modules(
        fs,
        &steps,
        manager.clone(),
        io,
        modules,
        Vec::new(),
    )
    .map(|_| ())
    .expect_err("missing section must fail");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("[9.9.9-t]"),
        "failure names the missing section: {rendered}"
    );
    assert!(
        manager.log.lock().expect("log").is_empty(),
        "no process spawned before notes extraction"
    );
    Ok(())
}
