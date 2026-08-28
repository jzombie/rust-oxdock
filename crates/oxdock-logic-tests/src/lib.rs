//! Workspace-level test harness crate (not published).

pub mod harness {
    use anyhow::{Context, Result, anyhow};
    use libtest_mimic::Trial;
    use oxdock_fixture::FixtureBuilder;
    use oxdock_fs::{EntryKind, GuardedPath, PathResolver, command_path, discover_workspace_root};
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

        if let Some(coverage_env) = &case_config.coverage_env {
            let mut coverage = FixtureCase::default_case();
            coverage.name = case_config.coverage_case_name.clone();
            coverage.env.push((coverage_env.clone(), "1".to_string()));
            cases.insert(0, coverage);
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
        let workspace_root =
            discover_workspace_root().context("failed to locate workspace root")?;

        let mut builder = FixtureBuilder::new(spec.template.as_str())
            .context("failed to load fixture template")?
            .with_workspace_manifest_root(workspace_root.as_path());
        if config.set_workspace_root_env {
            builder = builder.with_workspace_root(workspace_root.as_path());
        }

        let fixture = builder
            .with_path_dependency(
                "oxdock-buildtime-macros",
                workspace_root.join("oxdock-buildtime-macros")?.to_string(),
            )
            .with_path_dependency(
                "oxdock-buildtime-helpers",
                workspace_root.join("oxdock-buildtime-helpers")?.to_string(),
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
            .context("failed to instantiate fixture")?;

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
        cmd.args(&case.args);
        if let Some(target) = &temp_target {
            cmd.env("CARGO_TARGET_DIR", command_path(target).into_owned());
        }
        for (key, value) in &case.env {
            cmd.env(key, value);
        }
        for key in &case.env_remove {
            cmd.env_remove(key);
        }
        let result = cmd.output().context("failed to run fixture")?;
        let (status, stdout_bytes, stderr_bytes) = (result.status, result.stdout, result.stderr);

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

    impl FixtureCase {
        fn default_case() -> Self {
            Self {
                name: "default".to_string(),
                args: vec!["run".to_string(), "--quiet".to_string()],
                env: Vec::new(),
                env_remove: Vec::new(),
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
