use anyhow::{Context, Result, anyhow};
use oxdock_core::{run_steps_with_context_result_with_io, ExecIo};
use oxdock_fs::{
    GuardedPath, GuardedTempDir, PathResolver, discover_workspace_root, ensure_git_identity,
};
use oxdock_parser::{Step, StepKind};
use oxdock_process::{CommandBuilder, SharedInput, SharedOutput};
use oxdock_logic_tests::expectations::{self, ErrorExpectation};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::sync::{Arc, Mutex};
use toml_edit::{DocumentMut, Item, Table, Value};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Root {
    Snapshot,
    Local,
}

#[derive(Clone, Debug)]
enum BuildContext {
    Local,
    Snapshot,
    LocalSubdir(String),
}

#[derive(Clone, Default)]
struct RootExpect {
    files: BTreeMap<String, String>,
    missing: Vec<String>,
    dirs: Vec<String>,
}

struct PlatformExpect {
    unix: RootExpect,
    windows: RootExpect,
}

struct LsExpect {
    root: Root,
    file: String,
    entries: Vec<String>,
}

struct CwdExpect {
    root: Root,
    file: String,
    dir: String,
}

struct HashExpect {
    root: Root,
    file: String,
    source: String,
}

#[derive(Default)]
struct Expectations {
    snapshot: RootExpect,
    local: RootExpect,
    platform: Option<PlatformExpect>,
    ls: Option<LsExpect>,
    cwd: Option<CwdExpect>,
    hash: Option<HashExpect>,
    stdout: Option<String>,
}

struct CaseSpec {
    name: String,
    dir_name: String,
    script_rel: String,
    build_context: BuildContext,
    setup: Option<String>,
    stdin: Option<String>,
    env: Vec<(String, String)>,
    env_remove: Vec<String>,
    expect_error: Option<ErrorExpectation>,
    expectations: Expectations,
    pipes: BTreeMap<String, PipeSpec>,
}

struct CoverageSpec {
    coverage: HashMap<String, Vec<String>>,
    extras: Vec<String>,
}

#[derive(Clone, Default)]
struct PipeSpec {
    expect: Option<String>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("fixture failed: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let resolver = PathResolver::from_manifest_env().context("resolve fixture manifest dir")?;
    let coverage = load_coverage(&resolver)?;
    let case_filter = env::var("OXDOCK_AST_CASE").ok();
    let only_coverage = env::var_os("OXDOCK_AST_ONLY_COVERAGE").is_some();

    let mut cases = load_cases(&resolver)?;
    if let Some(filter) = &case_filter {
        cases = filter_cases(cases, filter)?;
    }

    let case_steps = load_case_steps(&resolver, &cases)?;

    // This fixture enforces AST coverage: every StepKind in ast.rs must be mapped in
    // coverage.toml and appear in at least one case, or the test fails.
    if case_filter.is_none() {
        assert_coverage(&case_steps, &coverage).context("validate AST command coverage")?;
    }
    if only_coverage {
        return Ok(());
    }

    for case in cases {
        println!("ast case: {}", case.name);
        let steps = case_steps
            .get(case.name.as_str())
            .ok_or_else(|| anyhow!("missing steps for case {}", case.name))?;
        run_case(&case, steps)
            .with_context(|| format!("case {}", case.name))?;
    }

    Ok(())
}

fn load_coverage(resolver: &PathResolver) -> Result<CoverageSpec> {
    let path = resolver.root().join("coverage.toml")?;
    let contents = resolver
        .read_to_string(&path)
        .context("read coverage.toml")?;
    let doc = contents
        .parse::<DocumentMut>()
        .context("parse coverage.toml")?;

    let mut coverage = HashMap::new();
    if let Some(table) = doc.get("coverage").and_then(|item| item.as_table()) {
        for (key, value) in table.iter() {
            let array = value
                .as_array()
                .ok_or_else(|| anyhow!("coverage.{key} must be an array"))?;
            let mut cases = Vec::new();
            for item in array.iter() {
                let case = item
                    .as_str()
                    .ok_or_else(|| anyhow!("coverage.{key} entries must be strings"))?;
                cases.push(case.to_string());
            }
            coverage.insert(key.to_string(), cases);
        }
    }

    let extras = doc
        .get("extras")
        .and_then(|item| item.as_table())
        .and_then(|table| table.get("cases"))
        .and_then(|item| item.as_array())
        .map(|array| {
            array
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(CoverageSpec { coverage, extras })
}

fn load_cases(resolver: &PathResolver) -> Result<Vec<CaseSpec>> {
    let cases_root = resolver.root().join("cases")?;
    let entries = resolver
        .read_dir_entries(&cases_root)
        .context("read cases directory")?;
    let mut cases = Vec::new();

    for entry in entries {
        let file_type = entry
            .file_type()
            .context("read case entry type")?;
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
        cases.push(load_case_spec(resolver, &case_dir, &name)?);
    }

    cases.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(cases)
}

fn filter_cases(cases: Vec<CaseSpec>, filter: &str) -> Result<Vec<CaseSpec>> {
    let mut filtered: Vec<CaseSpec> = cases
        .into_iter()
        .filter(|case| case.name == filter || case.dir_name == filter)
        .collect();
    if filtered.is_empty() {
        return Err(anyhow!("no AST case matched {filter}"));
    }
    filtered.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(filtered)
}

fn load_case_spec(
    resolver: &PathResolver,
    case_dir: &GuardedPath,
    dir_name: &str,
) -> Result<CaseSpec> {
    let case_path = case_dir.join("case.toml")?;
    let contents = resolver
        .read_to_string(&case_path)
        .with_context(|| format!("read {}", case_path.display()))?;
    let doc = contents
        .parse::<DocumentMut>()
        .context("parse case.toml")?;

    let name = doc
        .get("name")
        .and_then(|item| item.as_str())
        .unwrap_or(dir_name)
        .to_string();
    let script_name = doc
        .get("script")
        .and_then(|item| item.as_str())
        .unwrap_or("script.oxdock");
    let script_rel = format!("cases/{}/{}", dir_name, script_name);

    let build_context = parse_build_context(
        doc.get("build_context")
            .and_then(|item| item.as_str()),
    )?;
    let setup = doc
        .get("setup")
        .and_then(|item| item.as_str())
        .map(|s| s.to_string());
    let stdin = doc
        .get("stdin")
        .and_then(|item| item.as_str())
        .map(|s| s.to_string());
    let expect_error = expectations::parse_error_expectation(&doc)?;
    let expectations = parse_expectations(doc.get("expect").and_then(|i| i.as_table()))?;
    let pipes = doc
        .get("pipes")
        .and_then(|item| item.as_table())
        .map(parse_pipe_specs)
        .transpose()? 
        .unwrap_or_default();
    let env = doc
        .get("env")
        .and_then(|item| item.as_table())
        .map(parse_env_table)
        .transpose()? 
        .unwrap_or_default();
    let env_remove = doc
        .get("env_remove")
        .and_then(|item| item.as_array())
        .map(parse_string_array)
        .transpose()? 
        .unwrap_or_default();

    Ok(CaseSpec {
        name,
        dir_name: dir_name.to_string(),
        script_rel,
        build_context,
        setup,
        stdin,
        env,
        env_remove,
        expect_error,
        expectations,
        pipes,
    })
}

fn parse_build_context(value: Option<&str>) -> Result<BuildContext> {
    match value.unwrap_or("local") {
        "local" => Ok(BuildContext::Local),
        "snapshot" => Ok(BuildContext::Snapshot),
        other if other.starts_with("local_subdir:") => {
            Ok(BuildContext::LocalSubdir(other["local_subdir:".len()..].to_string()))
        }
        other => Err(anyhow!("unknown build_context {other}")),
    }
}

fn parse_expectations(expect: Option<&Table>) -> Result<Expectations> {
    let mut out = Expectations::default();
    let Some(expect) = expect else {
        return Ok(out);
    };

    out.snapshot = parse_root_expect(expect)?;

    if let Some(local_table) = expect.get("local").and_then(|item| item.as_table()) {
        out.local = parse_root_expect(local_table)?;
    }

    if let Some(platform_table) = expect.get("platform").and_then(|item| item.as_table()) {
        let unix = platform_table
            .get("unix")
            .and_then(|item| item.as_table())
            .map(parse_root_expect)
            .transpose()?
            .unwrap_or_default();
        let windows = platform_table
            .get("windows")
            .and_then(|item| item.as_table())
            .map(parse_root_expect)
            .transpose()?
            .unwrap_or_default();
        if !unix.files.is_empty()
            || !unix.missing.is_empty()
            || !unix.dirs.is_empty()
            || !windows.files.is_empty()
            || !windows.missing.is_empty()
            || !windows.dirs.is_empty()
        {
            out.platform = Some(PlatformExpect { unix, windows });
        }
    }

    out.ls = expect
        .get("ls")
        .and_then(|item| item.as_table())
        .map(parse_ls_expect)
        .transpose()?;
    out.cwd = expect
        .get("cwd")
        .and_then(|item| item.as_table())
        .map(parse_cwd_expect)
        .transpose()?;
    out.hash = expect
        .get("hash")
        .and_then(|item| item.as_table())
        .map(parse_hash_expect)
        .transpose()?;
    out.stdout = expect
        .get("stdout")
        .and_then(|item| item.as_str())
        .map(|s| s.to_string());

    Ok(out)
}

fn parse_pipe_specs(table: &Table) -> Result<BTreeMap<String, PipeSpec>> {
    let mut pipes = BTreeMap::new();
    for (name, entry) in table.iter() {
        let pipe_table = entry
            .as_table()
            .ok_or_else(|| anyhow!("pipes.{name} must be a table"))?;
        let expect = pipe_table
            .get("expect")
            .and_then(|value| value.as_str())
            .map(|s| s.to_string());
        pipes.insert(name.to_string(), PipeSpec { expect });
    }
    Ok(pipes)
}

fn parse_env_table(table: &Table) -> Result<Vec<(String, String)>> {
    let mut env = Vec::new();
    for (key, value) in table.iter() {
        let value = value
            .as_str()
            .ok_or_else(|| anyhow!("env.{key} must be a string"))?;
        env.push((key.to_string(), value.to_string()));
    }
    Ok(env)
}

fn parse_root_expect(table: &Table) -> Result<RootExpect> {
    let mut out = RootExpect::default();
    if let Some(files_item) = table.get("files") {
        out.files = parse_files_table(files_item)?;
    }
    if let Some(missing) = table.get("missing").and_then(|item| item.as_array()) {
        out.missing = parse_string_array(missing)?;
    }
    if let Some(dirs) = table.get("dirs").and_then(|item| item.as_array()) {
        out.dirs = parse_string_array(dirs)?;
    }
    Ok(out)
}

fn parse_files_table(item: &Item) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    match item {
        Item::Table(table) => {
            for (key, value) in table.iter() {
                let value = value
                    .as_str()
                    .ok_or_else(|| anyhow!("files.{key} must be a string"))?;
                out.insert(key.to_string(), value.to_string());
            }
        }
        Item::Value(Value::InlineTable(table)) => {
            for (key, value) in table.iter() {
                let value = value
                    .as_str()
                    .ok_or_else(|| anyhow!("files.{key} must be a string"))?;
                out.insert(key.to_string(), value.to_string());
            }
        }
        _ => return Err(anyhow!("files must be a table")),
    }
    Ok(out)
}

fn parse_string_array(array: &toml_edit::Array) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for item in array.iter() {
        let value = item
            .as_str()
            .ok_or_else(|| anyhow!("array entries must be strings"))?;
        out.push(value.to_string());
    }
    Ok(out)
}

fn parse_root(value: Option<&str>) -> Result<Root> {
    match value.unwrap_or("snapshot") {
        "snapshot" => Ok(Root::Snapshot),
        "local" => Ok(Root::Local),
        other => Err(anyhow!("unknown root {other}")),
    }
}

fn parse_ls_expect(table: &Table) -> Result<LsExpect> {
    let file = table
        .get("file")
        .and_then(|item| item.as_str())
        .ok_or_else(|| anyhow!("expect.ls.file is required"))?
        .to_string();
    let entries = table
        .get("entries")
        .and_then(|item| item.as_array())
        .map(parse_string_array)
        .transpose()?
        .unwrap_or_default();
    let root = parse_root(table.get("root").and_then(|item| item.as_str()))?;
    Ok(LsExpect { root, file, entries })
}

fn parse_cwd_expect(table: &Table) -> Result<CwdExpect> {
    let file = table
        .get("file")
        .and_then(|item| item.as_str())
        .ok_or_else(|| anyhow!("expect.cwd.file is required"))?
        .to_string();
    let dir = table
        .get("dir")
        .and_then(|item| item.as_str())
        .ok_or_else(|| anyhow!("expect.cwd.dir is required"))?
        .to_string();
    let root = parse_root(table.get("root").and_then(|item| item.as_str()))?;
    Ok(CwdExpect { root, file, dir })
}

fn parse_hash_expect(table: &Table) -> Result<HashExpect> {
    let file = table
        .get("file")
        .and_then(|item| item.as_str())
        .ok_or_else(|| anyhow!("expect.hash.file is required"))?
        .to_string();
    let source = table
        .get("source")
        .and_then(|item| item.as_str())
        .ok_or_else(|| anyhow!("expect.hash.source is required"))?
        .to_string();
    let root = parse_root(table.get("root").and_then(|item| item.as_str()))?;
    Ok(HashExpect { root, file, source })
}

fn load_case_steps(
    resolver: &PathResolver,
    cases: &[CaseSpec],
) -> Result<HashMap<String, Vec<Step>>> {
    let mut out = HashMap::new();
    let placeholders = command_placeholders();
    for case in cases {
        let path = resolver
            .root()
            .join(&case.script_rel)
            .with_context(|| format!("resolve script {}", case.script_rel))?;
        let template = resolver
            .read_to_string(&path)
            .with_context(|| format!("read script {}", case.script_rel))?;
        let rendered = apply_placeholders(&template, &placeholders)
            .with_context(|| format!("render script {}", case.script_rel))?;
        let steps = oxdock_parser::parse_script(&rendered)
            .with_context(|| format!("parse script {}", case.script_rel))?;
        out.insert(case.name.clone(), steps);
    }
    Ok(out)
}

fn apply_placeholders(template: &str, placeholders: &HashMap<&'static str, String>) -> Result<String> {
    let mut rendered = template.to_string();
    for (key, value) in placeholders {
        rendered = rendered.replace(key, value);
    }
    for key in ["@RUN_CMD@", "@RUN_BG_CMD@", "@BG_WAIT_CMD@", "@CAPTURE_RUN_CMD@"] {
        if rendered.contains(key) {
            return Err(anyhow!("script contains unresolved placeholder {key}"));
        }
    }
    Ok(rendered)
}

fn command_placeholders() -> HashMap<&'static str, String> {
    #[allow(clippy::disallowed_macros)]
    let run_cmd = if cfg!(windows) {
        "echo %FOO%> run.txt".to_string()
    } else {
        "printf %s \"$FOO\" > run.txt".to_string()
    };

    // Background command should stay alive long enough for foreground steps to complete.
    #[allow(clippy::disallowed_macros)]
    let bg_cmd = if cfg!(windows) {
        "ping -n 3 127.0.0.1 > NUL & echo %FOO%> bg.txt".to_string()
    } else {
        "sleep 0.2 && printf %s \"$FOO\" > bg.txt".to_string()
    };

    #[allow(clippy::disallowed_macros)]
    let bg_wait = if cfg!(windows) {
        "ping -n 2 127.0.0.1 > NUL".to_string()
    } else {
        "sleep 0.4".to_string()
    };

    #[allow(clippy::disallowed_macros)]
    let capture_run = if cfg!(windows) {
        "echo hello".to_string()
    } else {
        "printf %s \"hello\"".to_string()
    };

    let mut placeholders = HashMap::new();
    placeholders.insert("@RUN_CMD@", run_cmd);
    placeholders.insert("@RUN_BG_CMD@", bg_cmd);
    placeholders.insert("@BG_WAIT_CMD@", bg_wait);
    placeholders.insert("@CAPTURE_RUN_CMD@", capture_run);
    placeholders
}

fn assert_coverage(
    cases: &HashMap<String, Vec<Step>>,
    spec: &CoverageSpec,
) -> Result<()> {
    let step_kinds = step_kind_variants().context("load StepKind variants")?;

    let mut missing = Vec::new();
    for kind in &step_kinds {
        if !spec.coverage.contains_key(kind) {
            missing.push(kind.clone());
        }
    }
    if !missing.is_empty() {
        missing.sort();
        return Err(anyhow!(
            "coverage.toml missing StepKind entries: {}",
            missing.join(", ")
        ));
    }

    for key in spec.coverage.keys() {
        if !step_kinds.contains(key) {
            return Err(anyhow!("coverage.toml has unknown StepKind {}", key));
        }
    }

    let mut referenced = BTreeSet::new();
    for (kind, case_list) in &spec.coverage {
        if case_list.is_empty() {
            return Err(anyhow!("coverage entry {} is empty", kind));
        }
        for case_name in case_list {
            let steps = cases
                .get(case_name)
                .ok_or_else(|| anyhow!("coverage references unknown case {}", case_name))?;
            let script_kinds = step_kinds_in_steps(steps);
            if !script_kinds.contains(kind) {
                return Err(anyhow!(
                    "coverage case {} expects {}, but script does not include it",
                    case_name,
                    kind
                ));
            }
            referenced.insert(case_name.clone());
        }
    }

    for extra in &spec.extras {
        if !cases.contains_key(extra) {
            return Err(anyhow!("extras references unknown case {}", extra));
        }
        referenced.insert(extra.clone());
    }

    if referenced.is_empty() {
        return Err(anyhow!("coverage.toml did not reference any cases"));
    }

    Ok(())
}

const COVERAGE_IGNORED_STEP_KINDS: &[&str] = &["WithIoBlock"];

fn step_kind_variants() -> Result<HashSet<String>> {
    let workspace_root = discover_workspace_root().context("locate workspace root")?;
    let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())?;
    let ast_path = workspace_root.join("crates/oxdock-parser/src/ast.rs")?;
    let ast_source = resolver.read_to_string(&ast_path).context("read ast.rs")?;
    let filtered: HashSet<_> = extract_step_kind_variants(&ast_source)
        .into_iter()
        .filter(|name| {
            !COVERAGE_IGNORED_STEP_KINDS
                .iter()
                .any(|ignored| ignored == name)
        })
        .collect();
    if filtered.is_empty() {
        return Err(anyhow!("failed to extract StepKind variants from ast.rs"));
    }
    Ok(filtered)
}

fn step_kinds_in_steps(steps: &[Step]) -> HashSet<String> {
    let mut kinds = HashSet::new();
    for step in steps {
        collect_step_kinds(&step.kind, &mut kinds);
    }
    kinds
}

fn collect_step_kinds(kind: &StepKind, kinds: &mut HashSet<String>) {
    kinds.insert(step_kind_name(kind).to_string());
    match kind {
        StepKind::WithIo { cmd, .. } => collect_step_kinds(cmd, kinds),
        _ => {}
    }
}

fn run_case(case: &CaseSpec, steps: &[Step]) -> Result<()> {
    let snapshot_temp = GuardedPath::tempdir().context("create snapshot tempdir")?;
    let snapshot = guard_root(&snapshot_temp);
    let local_temp = GuardedPath::tempdir().context("create local tempdir")?;
    let local = guard_root(&local_temp);

    if let Some(setup) = &case.setup {
        run_setup(setup, &case.name, &snapshot, &local)?;
    }

    let build_context = match &case.build_context {
        BuildContext::Local => local.clone(),
        BuildContext::Snapshot => snapshot.clone(),
        BuildContext::LocalSubdir(rel) => local
            .join(rel)
            .with_context(|| format!("resolve build context {}", rel))?,
    };

    let stdin: Option<SharedInput> = case.stdin.as_ref().map(|s| {
        let cursor = std::io::Cursor::new(s.as_bytes().to_vec());
        Arc::new(Mutex::new(cursor)) as SharedInput
    });
    
    let stdout_buf = Arc::new(Mutex::new(Vec::new()));
    let stdout: SharedOutput = stdout_buf.clone();

    let mut io_cfg = ExecIo::new();
    io_cfg.set_stdout(Some(stdout.clone()));
    io_cfg.set_stdin(stdin);

    for key in &case.env_remove {
        io_cfg.remove_inherit_env(key.clone());
    }
    for (key, value) in &case.env {
        io_cfg.insert_inherit_env(key.clone(), value.clone());
    }

    let mut pipe_buffers: Vec<(String, Arc<Mutex<Vec<u8>>>, PipeSpec)> = Vec::new();
    for (name, spec) in &case.pipes {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer: SharedOutput = buffer.clone();
        io_cfg.insert_output_pipe(name, writer);
        pipe_buffers.push((name.clone(), buffer, spec.clone()));
    }

    let result =
        run_steps_with_context_result_with_io(&snapshot, &build_context, steps, io_cfg).map(|_| ());
    match (&case.expect_error, result) {
        (Some(expectation), Err(err)) => {
            expectations::assert_error_matches(
                expectation,
                &err,
                &format!("AST case {} error", case.name),
            )?;
        }
        (Some(_), Ok(_)) => {
            return Err(anyhow!("expected error, got success"));
        }
        (None, Err(err)) => {
            return Err(err).with_context(|| format!("run {}", case.script_rel));
        }
        (None, Ok(_)) => {}
    }

    if let Some(expected_stdout) = &case.expectations.stdout {
        let actual_bytes = stdout_buf.lock().unwrap();
        let actual_str = String::from_utf8_lossy(&actual_bytes);
        if actual_str != *expected_stdout {
             return Err(anyhow!("stdout mismatch.\nExpected:\n{:?}\nActual:\n{:?}", expected_stdout, actual_str));
        }
    }

    verify_expectations(&case.expectations, &snapshot, &local)?;
    verify_pipes(&pipe_buffers)?;
    if let Some(setup) = &case.setup {
        run_cleanup(setup, &snapshot, &local)?;
    }
    Ok(())
}

fn verify_expectations(
    expect: &Expectations,
    snapshot: &GuardedPath,
    local: &GuardedPath,
) -> Result<()> {
    verify_root(&expect.snapshot, snapshot)?;
    verify_root(&expect.local, local)?;

    if let Some(platform) = &expect.platform {
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            verify_root(&platform.windows, snapshot)?;
        } else {
            verify_root(&platform.unix, snapshot)?;
        }
    }

    if let Some(ls) = &expect.ls {
        let root = match ls.root {
            Root::Snapshot => snapshot,
            Root::Local => local,
        };
        let content = read_trimmed_root(root, &ls.file)?;
        let mut lines: Vec<_> = content.lines().map(str::to_string).collect();
        if lines.is_empty() {
            return Err(anyhow!("LS output missing header"));
        }
        lines.remove(0);
        for entry in &ls.entries {
            if !lines.contains(entry) {
                return Err(anyhow!("LS output missing {}", entry));
            }
        }
    }

    if let Some(cwd) = &expect.cwd {
        let root = match cwd.root {
            Root::Snapshot => snapshot,
            Root::Local => local,
        };
        let expected = PathResolver::new(root.as_path(), root.as_path())?
            .canonicalize(&root.join(&cwd.dir)?)?
            .display()
            .to_string();
        let actual = read_trimmed_root(root, &cwd.file)?;
        if actual != expected {
            return Err(anyhow!("CWD output mismatch: expected {expected}, got {actual}"));
        }
    }

    if let Some(hash) = &expect.hash {
        let root = match hash.root {
            Root::Snapshot => snapshot,
            Root::Local => local,
        };
        let actual = read_trimmed_root(root, &hash.file)?;
        let mut hasher = Sha256::new();
        hasher.update(hash.source.as_bytes());
        let digest = hasher.finalize();
        let bytes: &[u8] = digest.as_ref();
        let expected: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        if actual != expected {
            return Err(anyhow!(
                "HASH_SHA256 output mismatch: expected {expected}, got {actual}"
            ));
        }
    }

    Ok(())
}

fn verify_root(expect: &RootExpect, root: &GuardedPath) -> Result<()> {
    if expect.files.is_empty() && expect.missing.is_empty() && expect.dirs.is_empty() {
        return Ok(());
    }

    for (path, expected) in &expect.files {
        let actual = read_trimmed_root(root, path)?;
        if actual != *expected {
            return Err(anyhow!(
                "expected {} to contain {}, got {}",
                path,
                expected,
                actual
            ));
        }
    }

    for path in &expect.missing {
        if exists(root, path)? {
            return Err(anyhow!("{} should not exist", path));
        }
    }

    if !expect.dirs.is_empty() {
        let resolver = PathResolver::new(root.as_path(), root.as_path())?;
        for path in &expect.dirs {
            let dir = root.join(path)?;
            let is_dir = matches!(resolver.entry_kind(&dir)?, oxdock_fs::EntryKind::Dir);
            if is_dir {
                continue;
            }
            return Err(anyhow!("{} should be a directory", path));
        }
    }

    Ok(())
}

fn verify_pipes(pipes: &[(String, Arc<Mutex<Vec<u8>>>, PipeSpec)]) -> Result<()> {
    for (name, buffer, spec) in pipes {
        if let Some(expected) = &spec.expect {
            let data = buffer.lock().unwrap();
            let actual = String::from_utf8(data.clone())
                .with_context(|| format!("pipe {name} output is not valid UTF-8"))?;
            if &actual != expected {
                return Err(anyhow!(
                    "pipe {name} mismatch. expected {:?}, got {:?}",
                    expected,
                    actual
                ));
            }
        }
    }
    Ok(())
}

fn read_trimmed_root(root: &GuardedPath, rel: &str) -> Result<String> {
    let resolver = PathResolver::new(root.as_path(), root.as_path())?;
    Ok(resolver.read_to_string(&root.join(rel)?)?.trim().to_string())
}

fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
    temp.as_guarded_path().clone()
}

fn write_text(path: &GuardedPath, contents: &str) -> Result<()> {
    let resolver = PathResolver::new(path.root(), path.root())?;
    resolver.write_file(path, contents.as_bytes())?;
    Ok(())
}

fn create_dirs(path: &GuardedPath) -> Result<()> {
    let resolver = PathResolver::new(path.root(), path.root())?;
    resolver.create_dir_all(path)?;
    Ok(())
}

fn exists(root: &GuardedPath, rel: &str) -> Result<bool> {
    Ok(root.join(rel)?.exists())
}

fn parent_guard(root: &GuardedPath) -> Result<GuardedPath> {
    let parent = root
        .as_path()
        .parent()
        .ok_or_else(|| anyhow!("root should have a parent"))?;
    GuardedPath::new_root(parent).context("guard parent path")
}

fn git_cmd(repo: &GuardedPath) -> CommandBuilder {
    let mut cmd = CommandBuilder::new("git");
    cmd.arg("-C").arg(repo.as_path());
    cmd
}

fn run_setup(
    name: &str,
    case_name: &str,
    snapshot: &GuardedPath,
    local: &GuardedPath,
) -> Result<()> {
    match name {
        "copy_inputs" => setup_copy_inputs(snapshot, local),
        "copy_complex" => setup_copy_complex(snapshot, local),
        "copy_broken_symlink" => setup_copy_broken_symlink(snapshot, local),
        "symlink_inputs" => setup_symlink_inputs(snapshot, local),
        "copy_git" => setup_copy_git(snapshot, local),
        "read_escape" => setup_read_escape(snapshot, local),
        "symlink_escape" => setup_symlink_escape(snapshot, local),
        "workspace_symlink" => setup_workspace_symlink(snapshot, local),
        _ => Err(anyhow!(
            "unknown setup {name} (available: copy_inputs, symlink_inputs, copy_git, read_escape, symlink_escape, workspace_symlink). referenced by {case_name}",
        )),
    }
}

fn run_cleanup(name: &str, snapshot: &GuardedPath, _local: &GuardedPath) -> Result<()> {
    match name {
        "read_escape" => cleanup_escape(snapshot, "secret.txt"),
        "symlink_escape" => cleanup_escape(snapshot, "symlink-secret.txt"),
        _ => Ok(()),
    }
}

fn cleanup_escape(snapshot: &GuardedPath, filename: &str) -> Result<()> {
    let parent = parent_guard(snapshot)?;
    let resolver = PathResolver::new(parent.as_path(), parent.as_path())?;
    let secret = parent.join(filename)?;
    let _ = resolver.remove_file(&secret);
    Ok(())
}

fn setup_copy_inputs(_snapshot: &GuardedPath, local: &GuardedPath) -> Result<()> {
    write_text(&local.join("source.txt")?, "from build")?;
    Ok(())
}

fn setup_copy_complex(_snapshot: &GuardedPath, local: &GuardedPath) -> Result<()> {
    let src = local.join("src")?;
    create_dirs(&src)?;
    write_text(&src.join("file.txt")?, "file content")?;

    let dir = src.join("dir")?;
    create_dirs(&dir)?;
    write_text(&dir.join("nested.txt")?, "nested content")?;

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("file.txt", src.join("symlink_file")?.as_path())?;
        std::os::unix::fs::symlink("dir", src.join("symlink_dir")?.as_path())?;
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file("file.txt", src.join("symlink_file")?.as_path())?;
        std::os::windows::fs::symlink_dir("dir", src.join("symlink_dir")?.as_path())?;
    }

    Ok(())
}

fn setup_copy_broken_symlink(_snapshot: &GuardedPath, local: &GuardedPath) -> Result<()> {
    let src = local.join("src")?;
    create_dirs(&src)?;

    #[cfg(unix)]
    std::os::unix::fs::symlink("nonexistent", src.join("broken")?.as_path())?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_file("nonexistent", src.join("broken")?.as_path())?;

    Ok(())
}

fn setup_symlink_inputs(snapshot: &GuardedPath, _local: &GuardedPath) -> Result<()> {
    let target_dir = snapshot.join("target_dir")?;
    create_dirs(&target_dir)?;
    write_text(&target_dir.join("inner.txt")?, "symlink target")?;
    Ok(())
}

fn setup_copy_git(_snapshot: &GuardedPath, local: &GuardedPath) -> Result<()> {
    let repo = local.join("repo")?;
    create_dirs(&repo)?;
    write_text(&repo.join("hello.txt")?, "git hello")?;

    git_cmd(&repo)
        .arg("init")
        .arg("-q")
        .status()
        .context("git init failed")?;
    git_cmd(&repo)
        .arg("add")
        .arg(".")
        .status()
        .context("git add failed")?;
    ensure_git_identity(&repo).context("ensure git identity")?;
    git_cmd(&repo)
        .arg("commit")
        .arg("-m")
        .arg("initial")
        .status()
        .context("git commit failed")?;

    Ok(())
}

fn setup_read_escape(snapshot: &GuardedPath, _local: &GuardedPath) -> Result<()> {
    let parent = parent_guard(snapshot)?;
    let resolver = PathResolver::new(parent.as_path(), parent.as_path())?;
    let secret = parent.join("secret.txt")?;
    resolver.write_file(&secret, b"nope")?;
    Ok(())
}

fn setup_symlink_escape(snapshot: &GuardedPath, _local: &GuardedPath) -> Result<()> {
    let parent = parent_guard(snapshot)?;
    let resolver = PathResolver::new(parent.as_path(), parent.as_path())?;
    let secret = parent.join("symlink-secret.txt")?;
    resolver.write_file(&secret, b"top secret")?;
    let link_path = snapshot.as_path().join("leak.txt");
    #[cfg(unix)]
    std::os::unix::fs::symlink(secret.as_path(), &link_path)
        .context("create unix symlink")?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(secret.as_path(), &link_path)
        .context("create windows symlink")?;
    Ok(())
}

fn setup_workspace_symlink(_snapshot: &GuardedPath, local: &GuardedPath) -> Result<()> {
    let client = local.join("client")?;
    create_dirs(&client)?;
    write_text(&client.join("version.txt")?, "1.2.3")?;
    Ok(())
}

fn extract_step_kind_variants(source: &str) -> Vec<String> {
    let mut variants = Vec::new();
    let mut in_enum = false;

    for line in source.lines() {
        let trimmed = line.trim_start();
        if !in_enum {
            if trimmed.starts_with("pub enum StepKind") {
                in_enum = true;
            }
            continue;
        }

        if trimmed == "}" {
            break;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('/') {
            continue;
        }

        let name: String = trimmed
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
            .collect();
        if !name.is_empty() && name.chars().next().map(|ch| ch.is_ascii_uppercase()) == Some(true)
        {
            variants.push(name);
        }
    }

    variants
}

fn step_kind_name(kind: &StepKind) -> &'static str {
    match kind {
        StepKind::InheritEnv { .. } => "InheritEnv",
        StepKind::Workdir(_) => "Workdir",
        StepKind::Workspace(_) => "Workspace",
        StepKind::Env { .. } => "Env",
        StepKind::Run(_) => "Run",
        StepKind::Echo(_) => "Echo",
        StepKind::RunBg(_) => "RunBg",
        StepKind::Copy { .. } => "Copy",
        StepKind::Symlink { .. } => "Symlink",
        StepKind::Mkdir(_) => "Mkdir",
        StepKind::Ls(_) => "Ls",
        StepKind::Cwd => "Cwd",
        StepKind::Read(_) => "Read",
        StepKind::Write { .. } => "Write",
        StepKind::Append { .. } => "Append",
        StepKind::AssertFile { .. } => "AssertFile",
        StepKind::AssertDir(_) => "AssertDir",
        StepKind::AssertAbsent(_) => "AssertAbsent",
        StepKind::AssertStdout(_) => "AssertStdout",
        StepKind::CopyGit { .. } => "CopyGit",
        StepKind::HashSha256 { .. } => "HashSha256",
        StepKind::Exit(_) => "Exit",
        StepKind::WithIo { .. } => "WithIo",
        StepKind::WithIoBlock { .. } => "WithIoBlock",
    }
}
