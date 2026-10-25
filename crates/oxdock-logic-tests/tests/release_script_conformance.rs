use anyhow::{Context, Result};
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, PathResolver};
#[cfg(feature = "markdown")]
use oxdock_logic_tests::recording::{Call, RecordingManager, argv_calls, shell_calls};

fn repo_root() -> Result<String> {
    // Same layout derivation as docs_conformance: normalize separators
    // first since Windows CARGO_MANIFEST_DIR uses backslashes.
    let manifest_dir = std::env::var(oxdock_fs::env::cargo::MANIFEST_DIR)
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
    let script_path = resolver
        .root()
        .join("scripts/gha/gha-rust-release.oxfile")?;
    resolver.read_to_string(&script_path)
}

/// The release script imports MARKDOWN, so it parses against a module
/// table carrying the plugin, mirroring the CLI `--features markdown`
/// build the release workflow runs.
#[cfg(feature = "markdown")]
fn parse_release_script(text: &str) -> Result<Vec<oxdock_parser::Step>> {
    let mut engine = oxdock_core::Engine::new();
    engine.register_module(oxdock_markdown_plugin::module());
    oxdock_core::parse_script_with_modules(text, engine.module_table()).map_err(|err| {
        anyhow::anyhow!("scripts/gha/gha-rust-release.oxfile failed to parse: {err}")
    })
}

/// The release script must always parse: a breaking language change that
/// invalidates it fails the PR that makes the change, never release day.
/// Parsing is the compile check; every behavior below executes the real
/// file against a recording process manager, never its text.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(
    miri,
    ignore = "reads scripts/gha/gha-rust-release.oxfile from the repository checkout layout"
)]
fn release_script_parses() -> Result<()> {
    let text = load_release_script()?;
    parse_release_script(&text)?;
    Ok(())
}

/// The whole release script must parse on a small stack, not just a
/// roomy dev machine: LET-capture used to nest a second full parse
/// inside the first and trip pest's stack guard on small-stack
/// runners (musl static binary), failing release day with
/// `stack limit reached`. Parses here on a 256 KiB thread.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(
    miri,
    ignore = "reads scripts/gha/gha-rust-release.oxfile from the repository checkout layout"
)]
fn release_script_parses_on_small_stack() -> Result<()> {
    let text = load_release_script()?;
    let child = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || parse_release_script(&text))
        .context("spawn small-stack parse thread")?;
    child
        .join()
        .map_err(|_| anyhow::anyhow!("small-stack parse panicked"))??;
    Ok(())
}

/// The standalone version gate must always parse: it guards the build
/// matrix, so a breaking language change that invalidates it fails the
/// PR that makes the change, never release day.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads scripts/gha/gha-rust-validate.oxfile from the repository checkout layout"
)]
fn validate_script_parses_and_gates_on_version() -> Result<()> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let script_path = resolver
        .root()
        .join("scripts/gha/gha-rust-validate.oxfile")?;
    let text = resolver.read_to_string(&script_path)?;
    oxdock_core::parse_script(&text).map_err(|err| {
        anyhow::anyhow!("scripts/gha/gha-rust-validate.oxfile failed to parse: {err}")
    })?;
    Ok(())
}

/// The version fallback executes for real: the actual
/// `scripts/gha/gha-rust-validate.oxfile` (side-effect-free: it only reads and
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
    let script_path = resolver
        .root()
        .join("scripts/gha/gha-rust-validate.oxfile")?;
    let text = resolver.read_to_string(&script_path)?;
    let steps = oxdock_core::parse_script(&text).map_err(|err| {
        anyhow::anyhow!("scripts/gha/gha-rust-validate.oxfile failed to parse: {err}")
    })?;
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

/// Canned `cargo metadata` for a two-crate fixture registry at the
/// fixture version: `demo-a` plus `demo-b`, both publishable.
#[cfg(feature = "markdown")]
fn fixture_registry_json() -> String {
    format!(
        "{{\"packages\": [{{\"name\": \"demo-a\", \"version\": \"{FIXTURE_VERSION}\"}}, {{\"name\": \"demo-b\", \"version\": \"{FIXTURE_VERSION}\"}}]}}"
    )
}

/// Script the fixture registry state behind the release script's
/// probes: landed crates answer empty (present, like a quiet `cargo
/// info`), missing crates answer the sentinel (absent, like the
/// script's own `|| printf` fallback). Needles carry no quotes: the
/// engine strips them before the recorder sees the shell text.
#[cfg(feature = "markdown")]
fn script_registry(run: &ReleaseRun, landed: &[&str]) {
    run.manager
        .respond("cargo metadata", &fixture_registry_json());
    for name in ["demo-a", "demo-b"] {
        let probe = format!("cargo info {name}@{FIXTURE_VERSION}");
        if landed.contains(&name) {
            run.manager.respond(&probe, "");
        } else {
            run.manager.respond(&probe, "OXDOCK_PUBLISH_MISSING");
        }
    }
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
    let manager = RecordingManager::new();
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
impl ReleaseRun {
    fn calls(&self) -> Vec<Call> {
        self.manager.calls()
    }

    fn execute(&self) -> Result<()> {
        let modules = vec![oxdock_markdown_plugin::module_with::<RecordingManager>()];
        let fs: Box<dyn oxdock_fs::WorkspaceFs> = Box::new(PathResolver::new_guarded(
            self.root.clone(),
            self.root.clone(),
        )?);
        oxdock_core::run_steps_with_manager_with_modules(
            fs,
            &self.steps,
            self.manager.clone(),
            self.io.clone(),
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

/// Full run with binaries and a partial registry: `demo-a` landed so
/// the publish excludes it, `demo-b` is missing so the round runs.
/// Publishing stays workspace-scoped (one invocation, no per-crate
/// publishes); the tag gate passes on identical canned output, and
/// creation carries every asset plus the checksums file.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn real_run_with_binaries_runs_the_full_pipeline() -> Result<()> {
    let run = release_harness(FIXTURE_VERSION, "false", "true")?;
    script_registry(&run, &["demo-a"]);
    run.manager.respond("cargo publish --workspace", "");
    run.execute()?;
    let calls = run.calls();
    assert_eq!(calls.len(), 11, "eleven invocations in order: {calls:?}");
    assert_eq!(
        argv_calls(&calls)[0],
        vec![
            "cargo".to_string(),
            "publish".to_string(),
            "--workspace".to_string(),
            "--dry-run".to_string(),
        ],
        "dry run first"
    );
    let shells = shell_calls(&calls);
    let publish: Vec<&&str> = shells
        .iter()
        .filter(|shell| shell.contains("cargo publish --workspace"))
        .collect();
    assert_eq!(publish.len(), 1, "one real publish: {shells:?}");
    assert!(
        publish[0].contains("--exclude demo-a"),
        "landed crate excluded: {shells:?}"
    );
    assert!(
        !publish[0].contains("demo-b"),
        "missing crate never excluded: {shells:?}"
    );
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
            "target/artifacts/demo-x86_64-unknown-linux-musl.tar.gz".to_string(),
            "target/artifacts/demo-aarch64-unknown-linux-musl.tar.gz".to_string(),
            "target/artifacts/demo-aarch64-apple-darwin.tar.gz".to_string(),
            "target/artifacts/demo-x86_64-pc-windows-msvc.tar.gz".to_string(),
            "target/artifacts/demo-aarch64-pc-windows-msvc.tar.gz".to_string(),
            "target/artifacts/SHA256SUMS".to_string(),
            "target/artifacts/install.sh".to_string(),
            "target/artifacts/install.ps1".to_string(),
            "target/artifacts/install.oxfile".to_string(),
        ]],
        "creation carries notes, every asset, checksums, and installers"
    );
    Ok(())
}

/// Full run without binaries on a fully landed registry: every probe
/// answers present, so the loop converges without invoking publish at
/// all. Tag, push, and cleanup still run, and creation carries notes
/// alone with no asset paths.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn real_run_without_binaries_creates_notes_only() -> Result<()> {
    let run = release_harness(FIXTURE_VERSION, "false", "false")?;
    script_registry(&run, &["demo-a", "demo-b"]);
    run.execute()?;
    let calls = run.calls();
    let shells = shell_calls(&calls);
    assert!(
        !shells
            .iter()
            .any(|shell| shell.contains("cargo publish --workspace")),
        "publish skipped when nothing is missing: {shells:?}"
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

/// All-landed registry with binaries: the publish is skipped yet the
/// creation still carries every asset. Covers the zero-missing guard
/// independently of the binaries-off shape above.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn all_landed_with_binaries_skips_publish() -> Result<()> {
    let run = release_harness(FIXTURE_VERSION, "false", "true")?;
    script_registry(&run, &["demo-a", "demo-b"]);
    run.execute()?;
    let calls = run.calls();
    let shells = shell_calls(&calls);
    assert!(
        !shells
            .iter()
            .any(|shell| shell.contains("cargo publish --workspace")),
        "publish skipped when nothing is missing: {shells:?}"
    );
    let argvs = argv_calls(&calls);
    let creates: Vec<&Vec<String>> = argvs
        .iter()
        .filter(|argv| argv.first().is_some_and(|exe| exe == "gh"))
        .collect();
    assert_eq!(creates.len(), 1, "one creation: {creates:?}");
    assert!(
        creates[0].iter().any(|arg| arg.contains(".tar.gz")),
        "assets still attached: {creates:?}"
    );
    Ok(())
}

/// A failed round retries and converges: the first publish answers
/// the failure sentinel, diagnostics surface, and the second round
/// re-probes before publishing clean. Registry probes stay sticky
/// across rounds; only the publish answers once.
#[test]
#[cfg(feature = "markdown")]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn publish_retries_failed_round_then_converges() -> Result<()> {
    let run = release_harness(FIXTURE_VERSION, "false", "true")?;
    script_registry(&run, &["demo-a"]);
    run.manager
        .respond_once("cargo publish --workspace", "OXDOCK_PUBLISH_ROUND_FAILED");
    run.manager.respond("cargo publish --workspace", "");
    run.manager.respond("cat ", "boom");
    run.execute()?;
    let calls = run.calls();
    let shells = shell_calls(&calls);
    let publish: Vec<&&str> = shells
        .iter()
        .filter(|shell| shell.contains("cargo publish --workspace"))
        .collect();
    assert_eq!(
        publish.len(),
        2,
        "failed round plus converged round: {shells:?}"
    );
    assert!(
        shells.iter().any(|shell| shell.starts_with("cat ")),
        "failure diagnostics surface: {shells:?}"
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
    let manager = RecordingManager::new();
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
        manager.calls().is_empty(),
        "no process spawned before notes extraction"
    );
    Ok(())
}
