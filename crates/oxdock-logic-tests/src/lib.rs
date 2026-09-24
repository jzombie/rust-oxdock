//! Workspace-level test harness crate (not published).

pub mod ast_runner;

/// Render an engine error exactly as fixture binaries report failures on
/// stderr (`fixture failed: {err:#}`).
///
/// In-process trial runners synthesize stderr with this helper so `expect`
/// assertions evaluate identical text across in-process (Tier 1/2) and
/// external-binary (Tier 3) execution paths.
pub fn format_fixture_stderr(err: &anyhow::Error) -> String {
    format!("fixture failed: {err:#}")
}

pub mod harness {
    use anyhow::{Context, Result, anyhow};
    use libtest_mimic::Trial;
    use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
    use oxdock_fixture::FixtureBuilder;
    use oxdock_fs::{
        EntryKind, GuardedPath, GuardedTempDir, PathResolver, command_path,
        discover_workspace_root, env as oxdock_env,
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use toml_edit::{DocumentMut, Item};

    #[derive(Clone)]
    pub struct FixtureSpec {
        pub name: String,
        pub template: String,
    }

    #[derive(Clone)]
    pub struct FixtureCase {
        pub name: String,
        pub args: Vec<String>,
        pub env: Vec<(String, String)>,
        pub env_remove: Vec<String>,
        pub stdin: Option<String>,
        pub expect_success: bool,
        pub error_expectation: Option<super::expectations::ErrorExpectation>,
        pub stdout_contains: Vec<String>,
        pub stdout_not_contains: Vec<String>,
        pub stderr_contains: Vec<String>,
        pub stderr_not_contains: Vec<String>,
    }

    #[derive(Clone)]
    pub struct HarnessConfig {
        pub fixtures_root: GuardedPath,
        pub exclude_root_dirs: Vec<String>,
        pub set_workspace_root_env: bool,
        pub set_temp_target_dir: bool,
        pub shared_target_dir: Option<GuardedPath>,
        pub case_config: Option<CaseConfig>,
        pub name: &'static str,
        /// Pre-built fixture binaries keyed by fixture directory name (e.g.
        /// `"copy_from_workspace"`). Trials for these fixtures spawn the
        /// binary directly instead of invoking `cargo run` per trial (Tier 2).
        /// Populate once via [`precompile_fixtures`] before `build_trials`.
        #[allow(clippy::disallowed_types)]
        pub precompiled_binaries: HashMap<String, std::path::PathBuf>,
    }

    impl HarnessConfig {
        pub fn new(name: &'static str, fixtures_root: GuardedPath) -> Self {
            Self {
                fixtures_root,
                exclude_root_dirs: Vec::new(),
                set_workspace_root_env: false,
                set_temp_target_dir: false,
                shared_target_dir: None,
                case_config: None,
                name,
                precompiled_binaries: HashMap::new(),
            }
        }
    }

    #[derive(Clone)]
    pub struct CaseConfig {
        pub fixture_name: String,
        pub cases_dir: String,
        pub case_env: String,
        pub coverage_env: Option<String>,
        pub coverage_case_name: String,
        pub smoke_cases: Vec<String>,
    }

    pub fn build_trials(resolver: &PathResolver, config: &HarnessConfig) -> Result<Vec<Trial>> {
        let fixtures = discover_fixtures(resolver, config)?;
        let tests: Vec<Trial> = fixtures
            .into_iter()
            .flat_map(|fixture| {
                let cases = load_fixture_cases(resolver, config, &fixture).unwrap_or_else(|err| {
                    eprintln!(
                        "{} harness failed to load expectations for {}: {err:#}",
                        config.name, fixture.name
                    );
                    std::process::exit(1);
                });
                let total_cases = cases.len();
                cases.into_iter().map(move |case| {
                    let config = config.clone();
                    let fixture = fixture.clone();
                    let name = case_display_name(&fixture.name, &case, total_cases);
                    Trial::test(name, move || run_fixture(&config, &fixture, &case))
                })
            })
            .collect();

        Ok(tests)
    }

    fn discover_fixtures(
        resolver: &PathResolver,
        config: &HarnessConfig,
    ) -> Result<Vec<FixtureSpec>> {
        let mut fixtures = Vec::new();
        discover_fixtures_recursive(
            resolver,
            &config.fixtures_root,
            "",
            &config.exclude_root_dirs,
            &mut fixtures,
        )?;
        fixtures.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(fixtures)
    }

    fn load_fixture_cases(
        resolver: &PathResolver,
        config: &HarnessConfig,
        spec: &FixtureSpec,
    ) -> Result<Vec<FixtureCase>> {
        if let Some(case_config) = &config.case_config
            && spec.name == case_config.fixture_name
        {
            return load_case_dir_fixtures(resolver, config, spec, case_config);
        }

        let fixture_root = config.fixtures_root.join(&spec.name)?;
        let case_path = fixture_root.join("case.toml")?;
        let cases_dir = fixture_root.join("cases")?;

        let has_case = matches!(resolver.entry_kind(&case_path), Ok(EntryKind::File));
        let has_cases_dir = matches!(resolver.entry_kind(&cases_dir), Ok(EntryKind::Dir));
        if has_case && has_cases_dir {
            return Err(anyhow!(
                "fixture {} defines both case.toml and cases/",
                spec.name
            ));
        }

        if has_cases_dir {
            return load_case_dir_cases(resolver, &cases_dir);
        }

        if has_case {
            let case = load_case_file(resolver, &case_path, &spec.name)?;
            return Ok(vec![case]);
        }

        Ok(vec![FixtureCase::default_case()])
    }

    fn load_case_dir_fixtures(
        resolver: &PathResolver,
        config: &HarnessConfig,
        spec: &FixtureSpec,
        case_config: &CaseConfig,
    ) -> Result<Vec<FixtureCase>> {
        let cases_root = config
            .fixtures_root
            .join(&spec.name)?
            .join(&case_config.cases_dir)?;
        let mut cases = Vec::new();

        let entries = resolver
            .read_dir_entries(&cases_root)
            .context("failed to read case directory")?;
        for entry in entries {
            let file_type = entry
                .file_type()
                .context("failed to read case entry type")?;
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }

            let case_dir = cases_root.join(&name)?;
            let case_toml = case_dir.join("case.toml")?;
            if resolver.entry_kind(&case_toml).is_err() {
                continue;
            }

            let mut case = FixtureCase::default_case();
            case.name = name.clone();
            case.env.push((case_config.case_env.clone(), name.clone()));
            cases.push(case);
        }

        cases.sort_by(|a, b| a.name.cmp(&b.name));

        // In smoke mode (default), run only the named representative cases.
        // With slow-integration feature, run all cases.
        #[allow(clippy::disallowed_macros)]
        let is_full_suite = cfg!(feature = "slow-integration");
        if !is_full_suite && !case_config.smoke_cases.is_empty() {
            cases.retain(|c| case_config.smoke_cases.contains(&c.name));
        }

        #[allow(clippy::disallowed_macros, clippy::collapsible_if)]
        if cfg!(feature = "slow-integration") {
            if let Some(coverage_env) = &case_config.coverage_env {
                let mut coverage = FixtureCase::default_case();
                coverage.name = case_config.coverage_case_name.clone();
                coverage.env.push((coverage_env.clone(), "1".to_string()));
                cases.insert(0, coverage);
            }
        }

        Ok(cases)
    }

    fn case_display_name(fixture_name: &str, case: &FixtureCase, total: usize) -> String {
        if total == 1 && case.name == "default" {
            fixture_name.to_string()
        } else {
            format!("{fixture_name}::{case}", case = case.name)
        }
    }

    fn run_fixture(
        config: &HarnessConfig,
        spec: &FixtureSpec,
        case: &FixtureCase,
    ) -> std::result::Result<(), libtest_mimic::Failed> {
        run_fixture_inner(config, spec, case)
            .map_err(|err| libtest_mimic::Failed::from(format!("{err:#}")))
    }

    fn run_fixture_inner(
        config: &HarnessConfig,
        spec: &FixtureSpec,
        case: &FixtureCase,
    ) -> Result<()> {
        // Tier 1: `ast_commands` cases execute fully in-process via the shared
        // runner : no fixture copy, no cargo invocation, no child process.
        if let Some(case_config) = &config.case_config
            && spec.name == case_config.fixture_name
        {
            return run_ast_trial(config, spec, case);
        }

        // Tier 1: script-driven fixtures (a `script.oxfile` plus stdout
        // expectations) execute in-process through the core engine.
        if is_script_fixture(spec) {
            return run_script_fixture_trial(config, spec, case);
        }

        // Tier 2: pre-compiled fixture binary spawned directly per trial.
        // The binary is built once via `precompile_fixtures` before the
        // harness thread pool starts; trials never invoke `cargo` here.
        if let Some(binary) = config.precompiled_binaries.get(fixture_short_name(spec)) {
            return run_precompiled_case(config, spec, case, binary);
        }

        // Tier 3: fixtures that intrinsically need Cargo (proc-macros,
        // build scripts, `cargo check`/`cargo test` behavior) keep the
        // per-trial `cargo` invocation against a shared target dir.
        run_cargo_fixture_trial(config, spec, case)
    }

    /// Fixture names whose template carries a `script.oxfile` executed
    /// in-process by [`run_script_fixture_trial`].
    ///
    /// Explicit allowlist (not auto-detection) so newly added fixtures keep
    /// the Tier-3 `cargo run` behavior until deliberately converted. Matched
    /// against the fixture directory name (last `spec.name` segment).
    const IN_PROCESS_SCRIPT_FIXTURES: &[&str] = &["inherit_env_scoping"];

    /// Fixture directory name: discovery flattens nesting into `spec.name`
    /// (e.g. `"integration/copy_from_workspace"`), so tier routing matches
    /// on the last segment.
    fn fixture_short_name(spec: &FixtureSpec) -> &str {
        spec.name.rsplit('/').next().unwrap_or(&spec.name)
    }

    fn is_script_fixture(spec: &FixtureSpec) -> bool {
        IN_PROCESS_SCRIPT_FIXTURES.contains(&fixture_short_name(spec))
    }

    fn template_root(spec: &FixtureSpec) -> Result<GuardedPath> {
        #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
        {
            GuardedPath::new_root(std::path::Path::new(&spec.template))
                .with_context(|| format!("guard fixture template {}", spec.template))
        }
    }

    fn run_ast_trial(config: &HarnessConfig, spec: &FixtureSpec, case: &FixtureCase) -> Result<()> {
        let template = template_root(spec)?;

        // Mirror the process path: skip symlink-dependent cases on hosts
        // that cannot create symlinks instead of failing the harness.
        let needs_symlink = case.name.contains("symlink")
            || case.name == "copy_broken_symlink"
            || case.name == "copy_complex";
        if needs_symlink && !oxdock_sys_test_utils::can_create_symlinks(template.as_path()) {
            eprintln!("skipping {}::{}: symlink unsupported", spec.name, case.name);
            return Ok(());
        }

        let is_coverage_trial = config
            .case_config
            .as_ref()
            .is_some_and(|case_config| case.name == case_config.coverage_case_name);
        if is_coverage_trial {
            return super::ast_runner::run_ast_coverage(&template);
        }
        super::ast_runner::run_ast_case(&template, &case.name)
    }

    fn run_script_fixture_trial(
        _config: &HarnessConfig,
        spec: &FixtureSpec,
        case: &FixtureCase,
    ) -> Result<()> {
        let template = template_root(spec)?;
        let script_path = template.join("script.oxfile")?;
        let reader = PathResolver::new(template.as_path(), template.as_path())?;
        let script = reader
            .read_to_string(&script_path)
            .with_context(|| format!("read {}", script_path.display()))?;
        let steps = oxdock_core::parse_script(&script).context("parse script.oxfile")?;

        // Fresh tempdir as the execution root (filesystem fidelity:
        // `PathResolver`, not a mock); the fixture template stays the build
        // context, mirroring `oxdock-cli::execute_with_result`.
        let tempdir = GuardedPath::tempdir().context("create script fixture tempdir")?;
        let fs_root = tempdir.as_guarded_path().clone();

        let stdin: Option<oxdock_process::SharedInput> = case.stdin.as_ref().map(|s| {
            let cursor = std::io::Cursor::new(s.as_bytes().to_vec());
            Arc::new(Mutex::new(cursor)) as oxdock_process::SharedInput
        });
        let stdout_buf = Arc::new(Mutex::new(Vec::new()));
        let stdout: oxdock_process::SharedOutput = stdout_buf.clone();
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let stderr: oxdock_process::SharedOutput = stderr_buf.clone();

        let mut io = ExecIo::new();
        io.set_stdin(stdin);
        io.set_stdout(Some(stdout));
        io.set_stderr(Some(stderr));
        for (key, value) in &case.env {
            io.insert_inherit_env(key.clone(), value.clone());
        }
        for key in &case.env_remove {
            io.remove_inherit_env(key.clone());
        }

        match run_steps_with_context_result_with_io(&fs_root, &template, &steps, io) {
            Ok(_) => {
                if !case.expect_success {
                    anyhow::bail!(
                        "fixture {} unexpectedly succeeded. stdout:\n{}",
                        spec.name,
                        String::from_utf8_lossy(&stdout_buf.lock().unwrap()),
                    );
                }
            }
            Err(err) => {
                if case.expect_success {
                    anyhow::bail!(
                        "fixture {} failed. stdout:\n{}\nstderr:\n{}",
                        spec.name,
                        String::from_utf8_lossy(&stdout_buf.lock().unwrap()),
                        super::format_fixture_stderr(&err),
                    );
                }
                if let Some(expectation) = &case.error_expectation {
                    // Assert against the same text the binary path prints to
                    // stderr (`fixture failed: {err:#}`) for Tier parity.
                    super::expectations::assert_text_matches(
                        expectation,
                        &super::format_fixture_stderr(&err),
                        &format!("fixture {} error output", spec.name),
                    )?;
                } else {
                    // Failure was expected but no finer expectation declared.
                }
            }
        }

        let stdout = String::from_utf8_lossy(&stdout_buf.lock().unwrap()).into_owned();
        let stderr = String::from_utf8_lossy(&stderr_buf.lock().unwrap()).into_owned();

        assert_contains(&stdout, &case.stdout_contains, "stdout", spec)?;
        assert_not_contains(&stdout, &case.stdout_not_contains, "stdout", spec)?;
        assert_contains(&stderr, &case.stderr_contains, "stderr", spec)?;
        assert_not_contains(&stderr, &case.stderr_not_contains, "stderr", spec)?;

        Ok(())
    }

    /// Copy a fixture template into an isolated tempdir with workspace path
    /// dependencies patched to the current checkout.
    ///
    /// Shared by the per-trial Tier-3 cargo path and by [`precompile_fixtures`]
    /// (which instantiates once per fixture, then builds once).
    ///
    /// Process-wide locks serializing `cargo` trials of the same fixture
    /// (see `run_cargo_fixture_trial`). One entry per fixture name; bounded
    /// by the fixture count.
    static FIXTURE_CARGO_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();

    fn fixture_cargo_lock(name: &str) -> Arc<Mutex<()>> {
        let locks = FIXTURE_CARGO_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
        locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn instantiate_trial_fixture(
        spec: &FixtureSpec,
        set_workspace_root_env: bool,
    ) -> Result<oxdock_fixture::FixtureInstance> {
        let workspace_root =
            discover_workspace_root().context("failed to locate workspace root")?;

        let mut builder = FixtureBuilder::new(spec.template.as_str())
            .context("failed to load fixture template")?
            .with_workspace_manifest_root(workspace_root.as_path());
        if set_workspace_root_env {
            builder = builder.with_workspace_root(workspace_root.as_path());
        }

        builder
            .with_path_dependency(
                "oxdock-macros",
                workspace_root.join("oxdock-macros")?.to_string(),
            )
            .with_path_dependency(
                "oxdock-build",
                workspace_root.join("oxdock-build")?.to_string(),
            )
            .with_path_dependency(
                "oxdock-embed",
                workspace_root.join("crates/oxdock-embed")?.to_string(),
            )
            .with_path_dependency(
                "oxdock-fs",
                workspace_root.join("crates/sys/oxdock-fs")?.to_string(),
            )
            .with_path_dependency(
                "oxdock-core",
                workspace_root.join("crates/oxdock-core")?.to_string(),
            )
            .with_path_dependency(
                "oxdock-parser",
                workspace_root.join("crates/oxdock-parser")?.to_string(),
            )
            .with_path_dependency(
                "oxdock-process",
                workspace_root
                    .join("crates/sys/oxdock-process")?
                    .to_string(),
            )
            .with_path_dependency(
                "oxdock-logic-tests",
                workspace_root
                    .join("crates/oxdock-logic-tests")?
                    .to_string(),
            )
            .with_path_dependency("oxdock-cli", workspace_root.join("oxdock-cli")?.to_string())
            .instantiate()
            .context("failed to instantiate fixture")
    }

    fn run_cargo_fixture_trial(
        config: &HarnessConfig,
        spec: &FixtureSpec,
        case: &FixtureCase,
    ) -> Result<()> {
        // Trials of the same fixture share one package identity (name,
        // version, features) across distinct source copies, and their builds
        // can depend on per-trial env (e.g. `build_exit_fail` vscode
        // detection). Sharing a target dir across concurrent same-fixture
        // trials lets Cargo reuse a sibling trial's fingerprint/artifacts
        // built under different env, so same-fixture cargo trials serialize
        // on a per-fixture lock. Different fixtures keep distinct units and
        // run in parallel (Cargo self-synchronizes shared-target access).
        let fixture_lock = fixture_cargo_lock(&spec.name);
        let _cargo_guard = fixture_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let fixture = instantiate_trial_fixture(spec, config.set_workspace_root_env)?;

        let owned_target = if config.set_temp_target_dir && config.shared_target_dir.is_none() {
            Some(GuardedPath::tempdir().context("create temp target dir")?)
        } else {
            None
        };
        let temp_target = if config.set_temp_target_dir {
            if let Some(shared) = &config.shared_target_dir {
                Some(shared.clone())
            } else {
                owned_target
                    .as_ref()
                    .map(|temp| temp.as_guarded_path().clone())
            }
        } else {
            None
        };

        let mut cmd = fixture.cargo();
        maybe_enable_incremental(&mut cmd);
        // If this case requires symlink support but the host cannot create symlinks,
        // skip it rather than failing the entire harness. This avoids CI breakage on
        // Windows hosts without developer symlink privileges.
        if let Some(target) = &temp_target {
            let needs_symlink = case.name.contains("symlink")
                || case.name == "copy_broken_symlink"
                || case.name == "copy_complex";
            if needs_symlink && !oxdock_sys_test_utils::can_create_symlinks(target.as_path()) {
                eprintln!(
                    "skipping fixture case {}::{}: symlink unsupported on host",
                    spec.name, case.name
                );
                return Ok(());
            }
        }
        // Cargo's unit fingerprint does not distinguish same-package fixture
        // copies sharing a target dir: a trial can otherwise reuse a deleted
        // sibling trial's artifacts built under different env (e.g. the
        // `build_exit_fail` vscode-detection matrix), silently skipping the
        // rebuild that applies this trial's environment. Evicting the fixture
        // package forces its rebuild with the trial env below while keeping
        // (env-independent) dependency artifacts warm. Same-fixture trials
        // already serialize on the per-fixture lock above, so the
        // clean+build sequence is atomic per trial.
        if temp_target.is_some() {
            let package = fixture_package_name(&fixture)?;
            evict_fixture_from_shared_target(fixture.root(), temp_target.as_ref(), &package, spec)?;
        }
        cmd.args(&case.args);
        if let Some(target) = &temp_target {
            cmd.env(
                oxdock_env::CARGO_TARGET_DIR,
                command_path(target).into_owned(),
            );
        }
        for (key, value) in &case.env {
            cmd.env(key, value);
        }
        for key in &case.env_remove {
            cmd.env_remove(key);
        }
        let (status, stdout_bytes, stderr_bytes) = if let Some(stdin_content) = &case.stdin {
            #[allow(
                clippy::disallowed_types,
                clippy::disallowed_methods,
                clippy::disallowed_macros
            )]
            {
                use std::io::Write;
                use std::process::{Command, Stdio};

                let snap = cmd.snapshot();
                let mut child = Command::new(&snap.program);
                for arg in &snap.args {
                    child.arg(arg);
                }
                for (key, value) in &snap.envs {
                    child.env(key, value);
                }
                if let Some(cwd) = &snap.cwd {
                    child.current_dir(cwd);
                }
                child
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());

                let mut child = child.spawn().context("failed to spawn fixture")?;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(stdin_content.as_bytes())?;
                    drop(stdin);
                }
                let result = child.wait_with_output().context("failed to run fixture")?;
                (result.status, result.stdout, result.stderr)
            }
        } else {
            let result = cmd.output().context("failed to run fixture")?;
            (result.status, result.stdout, result.stderr)
        };

        let stdout = String::from_utf8_lossy(&stdout_bytes);
        let stderr = String::from_utf8_lossy(&stderr_bytes);

        if case.expect_success && !status.success() {
            anyhow::bail!(
                "fixture {} failed. stdout:\n{}\nstderr:\n{}",
                spec.name,
                stdout,
                stderr
            );
        }
        if !case.expect_success && status.success() {
            anyhow::bail!(
                "fixture {} unexpectedly succeeded. stdout:\n{}\nstderr:\n{}",
                spec.name,
                stdout,
                stderr
            );
        }

        if let Some(expectation) = &case.error_expectation {
            if status.success() {
                anyhow::bail!("fixture {} expected error, got success", spec.name);
            }
            super::expectations::assert_text_matches(
                expectation,
                &stderr,
                &format!("fixture {} error output", spec.name),
            )?;
        }

        assert_contains(&stdout, &case.stdout_contains, "stdout", spec)?;
        assert_not_contains(&stdout, &case.stdout_not_contains, "stdout", spec)?;
        assert_contains(&stderr, &case.stderr_contains, "stderr", spec)?;
        assert_not_contains(&stderr, &case.stderr_not_contains, "stderr", spec)?;

        Ok(())
    }

    #[allow(clippy::disallowed_types)]
    fn run_precompiled_case(
        config: &HarnessConfig,
        spec: &FixtureSpec,
        case: &FixtureCase,
        binary: &std::path::Path,
    ) -> Result<()> {
        #[allow(
            clippy::disallowed_types,
            clippy::disallowed_methods,
            clippy::disallowed_macros
        )]
        {
            use std::io::Write;
            use std::process::{Command, Stdio};
            use std::thread;

            let template_path = std::path::Path::new(&spec.template);
            let mut cmd = Command::new(binary);
            cmd.env(oxdock_env::CARGO_MANIFEST_DIR, template_path);
            for (key, value) in &case.env {
                cmd.env(key, value);
            }
            for key in &case.env_remove {
                cmd.env_remove(key);
            }
            if let Some(target) = &config.shared_target_dir {
                let target_dir = oxdock_fs::command_path(target).into_owned();
                cmd.env(oxdock_env::CARGO_TARGET_DIR, target_dir);
            }

            // Symlink capability check : independent of shared_target_dir
            let needs_symlink = case.name.contains("symlink")
                || case.name == "copy_broken_symlink"
                || case.name == "copy_complex";
            if needs_symlink && !oxdock_sys_test_utils::can_create_symlinks(template_path) {
                eprintln!("skipping {}::{}: symlink unsupported", spec.name, case.name);
                return Ok(());
            }

            // Only pipe stdin when the case provides input; otherwise null it
            if case.stdin.is_some() {
                cmd.stdin(Stdio::piped());
            } else {
                cmd.stdin(Stdio::null());
            }
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

            let mut child = cmd.spawn().context("failed to spawn pre-compiled binary")?;

            // Write stdin on a dedicated thread to avoid pipe deadlock:
            // child may emit stdout/stderr before reading all of stdin.
            let stdin_handle = if let Some(stdin_content) = &case.stdin {
                let mut stdin = child.stdin.take().expect("stdin was piped");
                let content = stdin_content.clone();
                Some(thread::spawn(move || {
                    let _ = stdin.write_all(content.as_bytes());
                }))
            } else {
                None
            };

            let result = child
                .wait_with_output()
                .context("failed to run pre-compiled binary")?;
            if let Some(handle) = stdin_handle {
                let _ = handle.join();
            }

            let (status, stdout_bytes, stderr_bytes) =
                (result.status, result.stdout, result.stderr);
            let stdout = String::from_utf8_lossy(&stdout_bytes);
            let stderr = String::from_utf8_lossy(&stderr_bytes);

            if case.expect_success && !status.success() {
                anyhow::bail!(
                    "fixture {}::{} failed. stdout:\n{}\nstderr:\n{}",
                    spec.name,
                    case.name,
                    stdout,
                    stderr
                );
            }
            if !case.expect_success && status.success() {
                anyhow::bail!(
                    "fixture {}::{} unexpectedly succeeded. stdout:\n{}\nstderr:\n{}",
                    spec.name,
                    case.name,
                    stdout,
                    stderr
                );
            }

            if let Some(expectation) = &case.error_expectation {
                if status.success() {
                    anyhow::bail!(
                        "fixture {}::{} expected error, got success",
                        spec.name,
                        case.name
                    );
                }
                super::expectations::assert_text_matches(
                    expectation,
                    &stderr,
                    &format!("fixture {}::{} error output", spec.name, case.name),
                )?;
            }

            assert_contains(&stdout, &case.stdout_contains, "stdout", spec)?;
            assert_not_contains(&stdout, &case.stdout_not_contains, "stdout", spec)?;
            assert_contains(&stderr, &case.stderr_contains, "stderr", spec)?;
            assert_not_contains(&stderr, &case.stderr_not_contains, "stderr", spec)?;

            Ok(())
        }
    }

    /// A fixture instantiated once and built once, kept alive for the whole
    /// harness run so trials can spawn its binary without invoking Cargo.
    ///
    /// The `instance` field is never read: it owns the tempdir holding the
    /// instantiated fixture copy, which must outlive every trial.
    pub struct PrecompiledFixture {
        /// Fixture name as discovered (e.g. `"copy_from_workspace"`).
        pub name: String,
        /// Built binary to execute per trial.
        #[allow(clippy::disallowed_types)]
        pub binary: std::path::PathBuf,
        #[allow(dead_code)]
        instance: oxdock_fixture::FixtureInstance,
    }

    /// Build the named fixtures once into `target_dir` (Tier 2).
    ///
    /// Each fixture is instantiated (template copy + manifest patch, same as
    /// the per-trial path) and `cargo build`ed a single time. Call this in
    /// the harness `main` **before** `libtest_mimic::run` spawns worker
    /// threads, keep the returned vec alive until the run completes, and copy
    /// each `binary` into [`HarnessConfig::precompiled_binaries`].
    ///
    /// Only suitable for fixtures whose trials run the built binary directly
    /// (plain `cargo run` cases). Fixtures that test Cargo itself
    /// (`cargo check` / `cargo test`, build scripts, proc-macros, feature
    /// selection per case) must stay on the Tier-3 cargo path.
    pub fn precompile_fixtures(
        fixtures_root: &GuardedPath,
        names: &[&str],
        target_dir: &GuardedPath,
        set_workspace_root_env: bool,
    ) -> Result<Vec<PrecompiledFixture>> {
        let resolver = PathResolver::new(fixtures_root.as_path(), fixtures_root.as_path())?;
        let mut out = Vec::new();
        for name in names {
            let spec = find_fixture_spec(&resolver, fixtures_root, name)
                .with_context(|| format!("locate precompiled fixture template {name}"))?;
            let instance = instantiate_trial_fixture(&spec, set_workspace_root_env)?;
            let binary = build_fixture_binary(&instance, target_dir)
                .with_context(|| format!("pre-compile fixture {name}"))?;
            eprintln!("pre-compiled fixture {name}: {}", binary.display());
            out.push(PrecompiledFixture {
                name: (*name).to_string(),
                binary,
                instance,
            });
        }
        Ok(out)
    }

    /// Locate a fixture template by directory name, searching recursively
    /// under `root` (mirrors trial discovery, which flattens nesting).
    fn find_fixture_spec(
        resolver: &PathResolver,
        root: &GuardedPath,
        name: &str,
    ) -> Result<FixtureSpec> {
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = resolver
                .read_dir_entries(&dir)
                .with_context(|| format!("read {}", dir.display()))?;
            for entry in entries {
                let file_type = entry.file_type().context("read fixture entry type")?;
                if !file_type.is_dir() {
                    continue;
                }
                let entry_name = entry.file_name().to_string_lossy().to_string();
                if entry_name.starts_with('.') || entry_name == "target" {
                    continue;
                }
                let candidate = dir.join(&entry_name)?;
                if entry_name == name
                    && matches!(
                        resolver.entry_kind(&candidate.join("Cargo.toml")?),
                        Ok(EntryKind::File)
                    )
                {
                    return Ok(FixtureSpec {
                        name: (*name).to_string(),
                        template: candidate.to_string(),
                    });
                }
                stack.push(candidate);
            }
        }
        Err(anyhow!("fixture template {name} not found"))
    }

    #[allow(clippy::disallowed_types)]
    fn build_fixture_binary(
        instance: &oxdock_fixture::FixtureInstance,
        target_dir: &GuardedPath,
    ) -> Result<std::path::PathBuf> {
        #[allow(
            clippy::disallowed_types,
            clippy::disallowed_methods,
            clippy::disallowed_macros
        )]
        {
            use std::process::Command;

            let target_dir = oxdock_fs::command_path(target_dir).into_owned();
            let mut build = Command::new("cargo");
            build
                .arg("build")
                .arg("--message-format=json")
                .current_dir(instance.root().as_path())
                .env(oxdock_env::CARGO_TARGET_DIR, target_dir);
            if std::env::var_os(oxdock_env::CARGO_INCREMENTAL).is_none() {
                build.env(oxdock_env::CARGO_INCREMENTAL, "1");
            }
            let output = build
                .output()
                .context("failed to run cargo build for fixture")?;
            if !output.status.success() {
                anyhow::bail!(
                    "pre-compilation failed:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut executables = Vec::new();
            for line in stdout.lines() {
                let Ok(msg) = serde_json::from_str::<cargo_metadata::Message>(line) else {
                    continue;
                };
                if let cargo_metadata::Message::CompilerArtifact(artifact) = msg
                    && let Some(executable) = artifact.executable
                {
                    executables.push(executable.into_std_path_buf());
                }
            }
            let package = fixture_package_name(instance)?;
            // Cargo emits the binary filename from the package name
            // (hyphens preserved); match liberally across `-`/`_` spellings.
            let normalized = package.replace('-', "_");
            #[allow(clippy::disallowed_methods)]
            let binary = executables
                .iter()
                .find(|p| {
                    let stem = p.file_stem().map(|s| s.to_string_lossy().replace('-', "_"));
                    stem.as_deref() == Some(normalized.as_str())
                })
                .cloned()
                .or_else(|| {
                    if executables.len() == 1 {
                        executables.into_iter().next()
                    } else {
                        None
                    }
                });
            match binary {
                Some(binary) => {
                    #[allow(clippy::disallowed_methods)]
                    let binary = binary
                        .canonicalize()
                        .context("canonicalize fixture binary path")?;
                    Ok(binary)
                }
                None => anyhow::bail!("fixture executable not found in cargo build output"),
            }
        }
    }

    /// Read `[package] name` from an instantiated fixture's manifest.
    fn fixture_package_name(instance: &oxdock_fixture::FixtureInstance) -> Result<String> {
        let manifest = instance.manifest_path().context("fixture manifest path")?;
        let resolver = PathResolver::new(manifest.root(), manifest.root())?;
        let contents = resolver
            .read_to_string(&manifest)
            .context("read fixture manifest")?;
        let doc = contents
            .parse::<DocumentMut>()
            .context("parse fixture manifest")?;
        doc.get("package")
            .and_then(|item| item.as_table())
            .and_then(|table| table.get("name"))
            .and_then(|item| item.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("fixture manifest missing [package] name"))
    }

    /// Evict one fixture package's artifacts from a shared target dir.
    ///
    /// Runs `cargo clean -p <package>` with `CARGO_TARGET_DIR` pointed at the
    /// shared dir so the upcoming trial rebuilds the fixture crate with its
    /// own environment. Dependency artifacts are untouched and stay warm.
    /// Callers must serialize same-fixture trials (see the per-fixture lock
    /// in `run_cargo_fixture_trial`) so the clean+build sequence is atomic.
    fn evict_fixture_from_shared_target(
        instance_root: &GuardedPath,
        target_dir: Option<&GuardedPath>,
        package: &str,
        spec: &FixtureSpec,
    ) -> Result<()> {
        let Some(target_dir) = target_dir else {
            return Ok(());
        };
        #[allow(
            clippy::disallowed_types,
            clippy::disallowed_methods,
            clippy::disallowed_macros
        )]
        {
            use std::process::Command;

            let target_dir = oxdock_fs::command_path(target_dir).into_owned();
            let output = Command::new("cargo")
                .arg("clean")
                .arg("--package")
                .arg(package)
                .current_dir(instance_root.as_path())
                .env(oxdock_env::CARGO_TARGET_DIR, target_dir)
                .output()
                .with_context(|| format!("evict fixture {} from shared target", spec.name))?;
            if !output.status.success() {
                anyhow::bail!(
                    "evicting fixture {} failed:\n{}",
                    spec.name,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(())
        }
    }

    /// Point per-trial tempdirs at shared memory when available on Linux.
    ///
    /// `GuardedPath::tempdir` honors `TMPDIR`. On Linux, setting
    /// `TMPDIR=/dev/shm` keeps tempdir I/O RAM-backed while preserving full
    /// OS filesystem fidelity. Windows and macOS ignore this optimization
    /// and use native temporary paths (Windows resolves tempdirs via
    /// `%TMP%`/`%TEMP%`, which ignore POSIX `TMPDIR`).
    ///
    /// MUST be called at the top of the harness `main`, before
    /// `libtest_mimic::run` spawns worker threads: mutating process-global
    /// environment after threads exist is unsound. Never calls `set_var`
    /// when `TMPDIR` is already set.
    pub fn prefer_tmpfs_for_tempdirs() {
        #[cfg(target_os = "linux")]
        #[allow(
            clippy::disallowed_types,
            clippy::disallowed_methods,
            clippy::disallowed_macros
        )]
        {
            if std::env::var_os(oxdock_env::TMPDIR).is_some() {
                return;
            }
            if !std::path::Path::new("/dev/shm").exists() {
                return;
            }
            // Safety: called sequentially in main() before worker threads spawn.
            unsafe {
                std::env::set_var(oxdock_env::TMPDIR, "/dev/shm");
            }
        }
    }

    /// Resolve the shared Cargo target dir for fixture trials.
    ///
    /// Unlike a per-run tempdir, the resolved directory is PERSISTENT so
    /// fixture dependency artifacts survive across harness runs: the first
    /// run pays the dependency compilation once, later runs reuse it and
    /// each trial rebuilds only its own fixture crate. Resolution order:
    /// `$OXDOCK_FIXTURE_TARGET_DIR`, then
    /// `$CARGO_TARGET_DIR/oxdock-fixtures`, then
    /// `<workspace>/target/oxdock-fixtures`. Falls back to a tempdir (with
    /// a loud warning) when nothing is creatable.
    ///
    /// Call in the harness `main` before `libtest_mimic::run`; keep the
    /// returned keepalive alive until the run completes (it owns the
    /// fallback tempdir when used, and is `None` for persistent dirs).
    /// `cargo clean` wipes the persistent dir along with the rest of
    /// `target/`. Concurrent harness runs share it safely via Cargo's own
    /// target-dir locking.
    pub fn resolve_shared_target_dir() -> Result<(GuardedPath, Option<GuardedTempDir>)> {
        #[allow(
            clippy::disallowed_types,
            clippy::disallowed_methods,
            clippy::disallowed_macros
        )]
        {
            if let Ok(dir) = std::env::var(oxdock_env::FIXTURE_TARGET_DIR) {
                let guarded = ensure_target_dir(std::path::Path::new(&dir))?;
                eprintln!("fixture target dir (override): {}", guarded.display());
                return Ok((guarded, None));
            }
            if let Ok(target) = std::env::var(oxdock_env::CARGO_TARGET_DIR) {
                let root = std::path::Path::new(&target).join("oxdock-fixtures");
                let guarded = ensure_target_dir(&root)?;
                eprintln!("fixture target dir (shared): {}", guarded.display());
                return Ok((guarded, None));
            }
        }
        let workspace_root =
            discover_workspace_root().context("failed to locate workspace root")?;
        let candidate = workspace_root.join("target")?.join("oxdock-fixtures")?;
        match ensure_target_dir_guarded(&candidate) {
            Ok(guarded) => {
                eprintln!("fixture target dir (shared): {}", guarded.display());
                Ok((guarded, None))
            }
            Err(err) => {
                eprintln!(
                    "fixture target dir unavailable ({err:#}); falling back to a tempdir — expect cold fixture builds"
                );
                let temp = GuardedPath::tempdir().context("create temp target dir")?;
                let guarded = temp.as_guarded_path().clone();
                Ok((guarded, Some(temp)))
            }
        }
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn ensure_target_dir(path: &std::path::Path) -> Result<GuardedPath> {
        // PathResolver::new creates a missing root (see Backend::new), so
        // this both creates and guards in one step.
        let _resolver = PathResolver::new(path, path)?;
        GuardedPath::new_root(path)
    }

    fn ensure_target_dir_guarded(path: &GuardedPath) -> Result<GuardedPath> {
        #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
        {
            let _resolver = PathResolver::new(path.as_path(), path.as_path())?;
        }
        Ok(path.clone())
    }

    /// Request incremental compilation for a fixture Cargo invocation unless
    /// the host environment already expresses a choice. Fixture trials are
    /// near-identical rebuilds of the same crates, which is exactly the
    /// workload incremental compilation accelerates.
    pub fn maybe_enable_incremental(cmd: &mut oxdock_process::CommandBuilder) {
        #[allow(clippy::disallowed_macros)]
        if std::env::var_os(oxdock_env::CARGO_INCREMENTAL).is_none() {
            cmd.env(oxdock_env::CARGO_INCREMENTAL, "1");
        }
    }

    impl FixtureCase {
        fn default_case() -> Self {
            Self {
                name: "default".to_string(),
                args: vec!["run".to_string(), "--quiet".to_string()],
                env: Vec::new(),
                env_remove: Vec::new(),
                stdin: None,
                expect_success: true,
                error_expectation: None,
                stdout_contains: Vec::new(),
                stdout_not_contains: Vec::new(),
                stderr_contains: Vec::new(),
                stderr_not_contains: Vec::new(),
            }
        }
    }

    fn load_case_dir_cases(
        resolver: &PathResolver,
        cases_dir: &GuardedPath,
    ) -> Result<Vec<FixtureCase>> {
        let entries = resolver
            .read_dir_entries(cases_dir)
            .context("failed to read cases directory")?;
        let mut cases = Vec::new();

        for entry in entries {
            let file_type = entry
                .file_type()
                .context("failed to read case entry type")?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }

            let entry_path = cases_dir.join(&name)?;
            if file_type.is_dir() {
                let case_path = entry_path.join("case.toml")?;
                if matches!(resolver.entry_kind(&case_path), Ok(EntryKind::File)) {
                    cases.push(load_case_file(resolver, &case_path, &name)?);
                }
            } else if file_type.is_file() && name.ends_with(".toml") {
                let default_name = name.trim_end_matches(".toml");
                cases.push(load_case_file(resolver, &entry_path, default_name)?);
            }
        }

        cases.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(cases)
    }

    fn load_case_file(
        resolver: &PathResolver,
        case_path: &GuardedPath,
        default_name: &str,
    ) -> Result<FixtureCase> {
        let contents = resolver
            .read_to_string(case_path)
            .with_context(|| format!("read {}", case_path.display()))?;
        let doc = contents.parse::<DocumentMut>().context("parse case.toml")?;
        let mut case = FixtureCase::default_case();
        case.name = doc
            .get("name")
            .and_then(|item| item.as_str())
            .unwrap_or(default_name)
            .to_string();

        if let Some(item) = doc.get("args") {
            case.args = parse_string_list(item, "args")?;
        }
        if let Some(item) = doc.get("env") {
            case.env = parse_env_table(item)?;
        }
        if let Some(item) = doc.get("env_remove") {
            case.env_remove = parse_string_list(item, "env_remove")?;
        }
        if let Some(item) = doc.get("stdin") {
            case.stdin = item.as_str().map(|s| s.to_string());
        }

        let mut expect_success_override = None;
        if let Some(expect) = doc.get("expect").and_then(|item| item.as_table()) {
            if let Some(status) = expect.get("status").and_then(|item| item.as_str()) {
                expect_success_override = Some(parse_expect_status(status)?);
            }
            if let Some(stdout) = expect.get("stdout").and_then(|item| item.as_table()) {
                if let Some(item) = stdout.get("contains") {
                    case.stdout_contains = parse_string_list(item, "expect.stdout.contains")?;
                }
                if let Some(item) = stdout.get("not_contains") {
                    case.stdout_not_contains =
                        parse_string_list(item, "expect.stdout.not_contains")?;
                }
            }
            if let Some(stderr) = expect.get("stderr").and_then(|item| item.as_table()) {
                if let Some(item) = stderr.get("contains") {
                    case.stderr_contains = parse_string_list(item, "expect.stderr.contains")?;
                }
                if let Some(item) = stderr.get("not_contains") {
                    case.stderr_not_contains =
                        parse_string_list(item, "expect.stderr.not_contains")?;
                }
            }
        }

        // Allow per-platform expectation sections in `case.toml`, e.g.
        // `[unix]` or `[windows]` containing `expect_error_contains` or
        // `expect_error_equals`. Prefer the platform-specific table when
        // present; otherwise fall back to the top-level expectation.
        case.error_expectation = super::expectations::parse_error_expectation(&doc)?;
        if case.error_expectation.is_some() {
            if expect_success_override == Some(true) {
                return Err(anyhow!(
                    "case {} cannot expect success with expect.error",
                    case.name
                ));
            }
            if expect_success_override.is_none() {
                case.expect_success = false;
            }
        }
        if let Some(expect_success) = expect_success_override {
            case.expect_success = expect_success;
        }

        Ok(case)
    }

    fn parse_env_table(item: &Item) -> Result<Vec<(String, String)>> {
        let table = item
            .as_table()
            .ok_or_else(|| anyhow!("env must be a table"))?;
        let mut env = Vec::new();
        for (key, value) in table.iter() {
            let value = value
                .as_str()
                .ok_or_else(|| anyhow!("env {} must be a string", key))?;
            env.push((key.to_string(), value.to_string()));
        }
        Ok(env)
    }

    fn parse_string_list(item: &Item, label: &str) -> Result<Vec<String>> {
        if let Some(array) = item.as_array() {
            let mut values = Vec::new();
            for entry in array.iter() {
                let value = entry
                    .as_str()
                    .ok_or_else(|| anyhow!("{label} entries must be strings"))?;
                values.push(value.to_string());
            }
            return Ok(values);
        }
        if let Some(value) = item.as_str() {
            return Ok(vec![value.to_string()]);
        }
        Err(anyhow!("{label} must be a string or array of strings"))
    }

    fn parse_expect_status(value: &str) -> Result<bool> {
        match value {
            "success" => Ok(true),
            "failure" => Ok(false),
            other => Err(anyhow!(
                "expect.status must be success or failure, got {}",
                other
            )),
        }
    }

    fn assert_contains(
        haystack: &str,
        needles: &[String],
        stream: &str,
        spec: &FixtureSpec,
    ) -> Result<()> {
        for needle in needles {
            if !haystack.contains(needle) {
                return Err(anyhow!(
                    "fixture {} {} missing expected text: {}",
                    spec.name,
                    stream,
                    needle
                ));
            }
        }
        Ok(())
    }

    fn assert_not_contains(
        haystack: &str,
        needles: &[String],
        stream: &str,
        spec: &FixtureSpec,
    ) -> Result<()> {
        for needle in needles {
            if haystack.contains(needle) {
                return Err(anyhow!(
                    "fixture {} {} contained unexpected text: {}",
                    spec.name,
                    stream,
                    needle
                ));
            }
        }
        Ok(())
    }

    fn discover_fixtures_recursive(
        resolver: &PathResolver,
        root: &GuardedPath,
        rel: &str,
        exclude_root_dirs: &[String],
        fixtures: &mut Vec<FixtureSpec>,
    ) -> Result<()> {
        let entries = resolver
            .read_dir_entries(root)
            .context("failed to read fixtures directory")?;

        for entry in entries {
            let file_type = entry
                .file_type()
                .context("failed to read fixtures entry type")?;
            if !file_type.is_dir() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "target" {
                continue;
            }
            if rel.is_empty() && exclude_root_dirs.contains(&name) {
                continue;
            }

            let candidate = root.join(&name)?;
            let manifest = candidate.join("Cargo.toml")?;
            let rel_name = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };

            if resolver.entry_kind(&manifest).is_ok() {
                fixtures.push(FixtureSpec {
                    name: rel_name,
                    template: candidate.to_string(),
                });
            } else {
                discover_fixtures_recursive(
                    resolver,
                    &candidate,
                    &rel_name,
                    exclude_root_dirs,
                    fixtures,
                )?;
            }
        }

        Ok(())
    }
}

pub mod expectations {
    use anyhow::{Context, Result, anyhow};
    use oxdock_fs::{EntryKind, GuardedPath, PathResolver};
    use toml_edit::{DocumentMut, Item};

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub enum ErrorExpectation {
        Contains(String),
        Equals(String),
    }

    pub fn load_error_expectation(
        resolver: &PathResolver,
        case_root: &GuardedPath,
    ) -> Result<Option<ErrorExpectation>> {
        let case_path = case_root.join("case.toml")?;
        if !matches!(resolver.entry_kind(&case_path), Ok(EntryKind::File)) {
            return Ok(None);
        }
        let contents = resolver
            .read_to_string(&case_path)
            .with_context(|| format!("read {}", case_path.display()))?;
        let doc = contents.parse::<DocumentMut>().context("parse case.toml")?;
        // Prefer a platform-specific override when present. This allows fixture
        // authors to declare `[windows]` or `[unix]` sections containing
        // `expect_error_contains` / `expect_error_equals` that are only used on
        // the matching platform. Fall back to the top-level expectation.
        #[cfg(windows)]
        {
            if let Some(item) = doc.get("windows") {
                return parse_error_expectation_from_item(item);
            }
        }
        #[cfg(not(windows))]
        {
            if let Some(item) = doc.get("unix") {
                return parse_error_expectation_from_item(item);
            }
        }

        parse_error_expectation(&doc)
    }

    pub fn parse_error_expectation(doc: &DocumentMut) -> Result<Option<ErrorExpectation>> {
        // Prefer a platform-specific table at runtime. First look for an
        // exact OS key (e.g. "linux", "macos", "windows"). If that
        // doesn't exist, prefer a generic `unix` table for unix-like
        // platforms. Finally fall back to the top-level expectation.
        let os = std::env::consts::OS;
        if let Some(item) = doc.get(os) {
            return parse_error_expectation_from_item(item);
        }

        #[allow(clippy::disallowed_macros)]
        if cfg!(unix)
            && let Some(item) = doc.get("unix")
        {
            return parse_error_expectation_from_item(item);
        }

        parse_error_expectation_from_item(doc.as_item())
    }

    // Removed helper for `Table`-based parsing: parsing is centralized on `Item`.
    // `parse_error_expectation_from_item` is the canonical implementation and
    // is used by callers (including platform-specific overrides). The
    // `DocumentMut` -> `Item` conversion is performed where needed.

    pub fn parse_error_expectation_from_item(item: &Item) -> Result<Option<ErrorExpectation>> {
        let mut out = None;

        if let Some(value) = item
            .get("expect_error_contains")
            .and_then(|item| item.as_str())
        {
            set_expectation(&mut out, ErrorExpectation::Contains(value.to_string()))?;
        }
        if let Some(value) = item
            .get("expect_error_equals")
            .and_then(|item| item.as_str())
        {
            set_expectation(&mut out, ErrorExpectation::Equals(value.to_string()))?;
        }

        if let Some(expect) = item.get("expect").and_then(|item| item.as_table())
            && let Some(error) = expect.get("error").and_then(|item| item.as_table())
        {
            if let Some(value) = error.get("contains").and_then(|item| item.as_str()) {
                set_expectation(&mut out, ErrorExpectation::Contains(value.to_string()))?;
            }
            if let Some(value) = error.get("equals").and_then(|item| item.as_str()) {
                set_expectation(&mut out, ErrorExpectation::Equals(value.to_string()))?;
            }
        }

        Ok(out)
    }

    pub fn assert_error_matches(
        expectation: &ErrorExpectation,
        err: &anyhow::Error,
        context: &str,
    ) -> Result<()> {
        assert_text_matches(expectation, &err.to_string(), context)
    }

    pub fn assert_text_matches(
        expectation: &ErrorExpectation,
        actual: &str,
        context: &str,
    ) -> Result<()> {
        let actual = normalize_error_text(actual);
        match expectation {
            ErrorExpectation::Contains(expected) => {
                let expected = normalize_error_text(expected);
                if !actual.contains(&expected) {
                    anyhow::bail!(
                        "{context} did not contain expected error text.\nexpected fragment:\n{expected}\n\nactual:\n{actual}"
                    );
                }
            }
            ErrorExpectation::Equals(expected) => {
                let expected = normalize_error_text(expected);
                if actual != expected {
                    anyhow::bail!(
                        "{context} did not match expected error text.\nexpected:\n{expected}\n\nactual:\n{actual}"
                    );
                }
            }
        }
        Ok(())
    }

    fn set_expectation(slot: &mut Option<ErrorExpectation>, next: ErrorExpectation) -> Result<()> {
        if slot.is_some() {
            return Err(anyhow!("only one error expectation can be set"));
        }
        *slot = Some(next);
        Ok(())
    }

    fn normalize_error_text(input: &str) -> String {
        input.replace("\r\n", "\n").trim_end().to_string()
    }

    #[cfg(test)]
    mod tests {
        use indoc::indoc;
        use toml_edit::DocumentMut;

        fn doc(s: &str) -> DocumentMut {
            s.parse::<DocumentMut>().expect("parse toml")
        }

        #[test]
        fn exact_os_table_is_preferred() {
            let os = std::env::consts::OS;
            let toml = format!("[{os}]\nexpect_error_contains = \"os-specific\"\n", os = os);
            let doc = doc(&toml);
            let got = super::parse_error_expectation(&doc)
                .expect("parse")
                .expect("some");
            assert_eq!(got, super::ErrorExpectation::Contains("os-specific".into()));
        }

        #[cfg(unix)]
        #[test]
        fn unix_table_is_used_as_fallback_on_unix() {
            let toml = indoc!(
                r#"
                [unix]
                expect_error_contains = "unix-only"
            "#
            );
            let doc = doc(toml);
            let got = super::parse_error_expectation(&doc)
                .expect("parse")
                .expect("some");
            assert_eq!(got, super::ErrorExpectation::Contains("unix-only".into()));
        }

        #[test]
        fn top_level_expect_error_contains_parsed() {
            let toml = indoc!(
                r#"
                expect_error_contains = "top-level"
            "#
            );
            let doc = doc(toml);
            let got = super::parse_error_expectation(&doc)
                .expect("parse")
                .expect("some");
            assert_eq!(got, super::ErrorExpectation::Contains("top-level".into()));
        }

        #[test]
        fn nested_expect_error_table() {
            let toml = indoc!(
                r#"
                [expect.error]
                contains = "nested"
            "#
            );
            let doc = doc(toml);
            let got = super::parse_error_expectation(&doc)
                .expect("parse")
                .expect("some");
            assert_eq!(got, super::ErrorExpectation::Contains("nested".into()));
        }
    }
}
