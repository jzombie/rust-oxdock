use anyhow::{Context, Result};
use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, PathResolver, to_forward_slashes};
use oxdock_pipe::SharedOutput;
use std::io::Write;
use std::sync::{Arc, Mutex};

fn repo_root() -> Result<String> {
    let manifest_dir = to_forward_slashes(
        &std::env::var(oxdock_fs::env::cargo::MANIFEST_DIR)
            .context("CARGO_MANIFEST_DIR missing")?,
    );
    Ok(manifest_dir
        .strip_suffix("crates/oxdock-logic-tests")
        .context("test must live under crates/oxdock-logic-tests")?
        .trim_end_matches('/')
        .to_string())
}

fn load_repo_file(relative: &str) -> Result<String> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    resolver
        .read_to_string(&resolver.root().join(relative)?)
        .with_context(|| format!("read {relative}"))
}

struct PinRun {
    _temp: oxdock_fs::GuardedTempDir,
    root: GuardedPath,
    out: GuardedPath,
    steps: Vec<oxdock_parser::Step>,
    io: ExecIo,
    stderr: Arc<Mutex<Vec<u8>>>,
}

/// The `FAIL` diagnostic lands on stderr, so the harness captures it:
/// failures assert the guard by name, not just the exit code.
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("stderr capture lock")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn pin_harness(workflow_ref: &str, event_name: &str) -> Result<PinRun> {
    let text = load_repo_file(".github/scripts/gha-rust-pin.oxfile")?;
    let steps = oxdock_core::parse_script(&text)
        .map_err(|err| anyhow::anyhow!("gha-rust-pin.oxfile failed to parse: {err}"))?;
    let temp = GuardedPath::tempdir().context("tempdir")?;
    let root = temp.as_guarded_path().clone();
    let setup = PathResolver::new_guarded(root.clone(), root.clone())?;
    let helpers = load_repo_file(".github/scripts/helpers/ci-helpers.oxfile")?;
    setup.create_dir_all(&root.join("helpers")?)?;
    setup.write_file(&root.join("helpers/ci-helpers.oxfile")?, helpers.as_bytes())?;
    let out = root.join("github_output")?;
    let mut io = ExecIo::new();
    io.insert_inherit_env("GITHUB_WORKFLOW_REF", workflow_ref);
    io.insert_inherit_env("GITHUB_EVENT_NAME", event_name);
    io.insert_inherit_env("GITHUB_OUTPUT", out.as_path().display().to_string());
    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let sink: SharedOutput = Arc::new(Mutex::new(Captured(captured.clone())));
    io.set_stderr(Some(sink));
    Ok(PinRun {
        _temp: temp,
        root,
        out,
        steps,
        io,
        stderr: captured,
    })
}

/// Every pin shape in one table: the workflow ref, the event, and the
/// expected `pin=` value, or `None` when resolution must fail closed.
const CASES: &[(&str, &str, Option<&str>)] = &[
    (
        "jzombie/rust-oxdock/.github/workflows/docs.yml@refs/tags/v0.27.1",
        "workflow_dispatch",
        Some("v0.27.1"),
    ),
    (
        "jzombie/rust-oxdock/.github/workflows/docs.yml@refs/heads/main",
        "workflow_dispatch",
        Some("main"),
    ),
    (
        "jzombie/rust-oxdock/.github/workflows/docs.yml@main",
        "workflow_dispatch",
        Some("main"),
    ),
    (
        "jzombie/rust-oxdock/.github/workflows/docs.yml@da39a3ee5e6b4b0d3255bfef95601890afd80709",
        "workflow_dispatch",
        Some("da39a3ee5e6b4b0d3255bfef95601890afd80709"),
    ),
    (
        "jzombie/rust-oxdock/.github/workflows/docs.yml@refs/pull/7/merge",
        "pull_request",
        Some("refs/pull/7/merge"),
    ),
    (
        "jzombie/rust-oxdock/.github/workflows/docs.yml@refs/pull/7/merge",
        "workflow_dispatch",
        None,
    ),
    (
        "jzombie/rust-oxdock/.github/workflows/docs.yml",
        "workflow_dispatch",
        None,
    ),
    (
        "jzombie/rust-oxdock/.github/workflows/docs.yml@",
        "workflow_dispatch",
        None,
    ),
    (
        "jzombie/rust-oxdock/.github/workflows/docs.yml@refs/tags/",
        "workflow_dispatch",
        None,
    ),
    (
        "jzombie/rust-oxdock/.github/workflows/docs.yml@refs/heads/",
        "workflow_dispatch",
        None,
    ),
    (
        "jzombie/rust-oxdock/.github/workflows/docs.yml@refs/tags/refs/heads/main",
        "workflow_dispatch",
        None,
    ),
    (
        "jzombie/rust-oxdock/.github/workflows/docs.yml@refs/notes/review",
        "schedule",
        None,
    ),
];

/// The bridge script must always parse: the release workflow runs it
/// before the build matrix, so a breaking language change fails the
/// PR that makes the change, never release day.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads gha-rust-pin.oxfile from the repository checkout layout"
)]
fn pin_script_parses() -> Result<()> {
    let text = load_repo_file(".github/scripts/gha-rust-pin.oxfile")?;
    oxdock_core::parse_script(&text)
        .map_err(|err| anyhow::anyhow!("gha-rust-pin.oxfile failed to parse: {err}"))?;
    Ok(())
}

/// The helpers module must parse for the same pre-matrix reason: the
/// pin function ships inside it.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads ci-helpers.oxfile from the repository checkout layout"
)]
fn pin_helpers_parse() -> Result<()> {
    let text = load_repo_file(".github/scripts/helpers/ci-helpers.oxfile")?;
    oxdock_core::parse_script(&text)
        .map_err(|err| anyhow::anyhow!("ci-helpers.oxfile failed to parse: {err}"))?;
    Ok(())
}

enum PinOutcome {
    Pin(String),
    Rejected,
}

/// Runs the DSL bridge for one case, returning the outcome plus the
/// captured stderr for diagnostic assertions.
fn run_dsl(workflow_ref: &str, event_name: &str) -> Result<(PinOutcome, String)> {
    let run = pin_harness(workflow_ref, event_name)?;
    let reader = PathResolver::new_guarded(run.root.clone(), run.root.clone())?;
    let outcome =
        match run_steps_with_context_result_with_io(&run.root, &run.root, &run.steps, run.io)
            .map(|_| ())
        {
            Ok(()) => PinOutcome::Pin(reader.read_to_string(&run.out).context("read DSL output")?),
            Err(_) => {
                if reader.read_to_string(&run.out).is_ok() {
                    anyhow::bail!("failed resolution wrote an outputs file");
                }
                PinOutcome::Rejected
            }
        };
    let diagnostic = String::from_utf8_lossy(&run.stderr.lock().expect("stderr lock")).into_owned();
    Ok((outcome, diagnostic))
}

/// Every shape resolves or fails closed through the DSL, and every
/// failure names its guard.
#[test]
#[cfg_attr(miri, ignore = "needs host tempdir for the fixture root")]
fn pin_table() -> Result<()> {
    for (workflow_ref, event_name, expected) in CASES {
        let (outcome, diagnostic) = run_dsl(workflow_ref, event_name)?;
        match (expected, outcome) {
            (Some(pin), PinOutcome::Pin(out)) => {
                assert_eq!(out, format!("pin={pin}\n"), "DSL pin for {workflow_ref}");
            }
            (None, PinOutcome::Rejected) => {
                assert!(
                    diagnostic.contains("unsupported pipeline ref"),
                    "failure for {workflow_ref} names the guard: {diagnostic}"
                );
            }
            (expected, _) => {
                panic!("DSL diverges on {workflow_ref} (want {expected:?})");
            }
        }
    }
    Ok(())
}

/// Extracts every `run:` literal block under a `Resolve pipeline ref`
/// step. Fails loudly on any shape change (missing step, non-bash
/// shell, empty block) so a reformat cannot silently desync the
/// oracle: extraction errors fail the test, never pass it.
fn extract_resolve_blocks(text: &str, source: &str) -> Result<Vec<String>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() != "- name: Resolve pipeline ref" {
            i += 1;
            continue;
        }
        let rest = &lines[i..];
        let run_off = rest
            .iter()
            .position(|line| line.trim() == "run: |")
            .with_context(|| format!("{source} Resolve pipeline ref step has no run block"))?;
        let between = &rest[..run_off];
        if !between.iter().any(|line| line.trim() == "shell: bash") {
            anyhow::bail!("{source} Resolve pipeline ref step is not a bash step");
        }
        if between[1..]
            .iter()
            .any(|line| line.trim_start().starts_with("- "))
        {
            anyhow::bail!("{source} Resolve pipeline ref step holds more than its run block");
        }
        let run_indent = lines[i + run_off].len() - lines[i + run_off].trim_start().len();
        let mut block: Vec<&str> = Vec::new();
        for line in &lines[i + run_off + 1..] {
            if line.trim().is_empty() {
                continue;
            }
            if line.len() - line.trim_start().len() <= run_indent {
                break;
            }
            block.push(line);
        }
        if block.is_empty() {
            anyhow::bail!("{source} Resolve pipeline ref run block is empty");
        }
        let dent = block
            .iter()
            .map(|line| line.len() - line.trim_start().len())
            .min()
            .unwrap_or(0);
        blocks.push(
            block
                .iter()
                .map(|line| line[dent..].to_string())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        );
        i += run_off + 1;
    }
    Ok(blocks)
}

/// Loads the `Resolve pipeline ref` blocks from rust-release.yml
/// (validate, release) and fails unless both exist and are
/// byte-identical: one rule, one text.
fn load_release_resolve_block() -> Result<String> {
    let release = load_repo_file(".github/workflows/rust-release.yml")?;
    let blocks = extract_resolve_blocks(&release, "rust-release.yml")?;
    if blocks.len() != 2 {
        anyhow::bail!(
            "rust-release.yml holds {} Resolve pipeline ref steps, want 2",
            blocks.len()
        );
    }
    if blocks[0] != blocks[1] {
        anyhow::bail!("Resolve pipeline ref blocks diverge across jobs");
    }
    Ok(blocks.into_iter().next().expect("one release block"))
}

/// The two `Resolve pipeline ref` blocks in rust-release.yml
/// (validate, release) are byte-identical: one rule, one text.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads workflow files from the repository checkout layout"
)]
fn resolve_blocks_identical() -> Result<()> {
    load_release_resolve_block().map(|_| ())
}

/// Runs the workflow-extracted bash for one case. Values ride argv,
/// never the script text, so metacharacters cannot leak into the
/// oracle.
#[cfg(unix)]
fn bash_pin(block: &str, workflow_ref: &str, event_name: &str) -> Result<PinOutcome> {
    for value in [workflow_ref, event_name] {
        assert!(
            !value.contains(['"', '\\', '\n']),
            "oracle argv must stay quotable: {value}"
        );
    }
    let temp = GuardedPath::tempdir().context("tempdir")?;
    let root = temp.as_guarded_path().clone();
    let setup = PathResolver::new_guarded(root.clone(), root.clone())?;
    let mut oracle = String::from(
        "GITHUB_WORKFLOW_REF=\"$1\"\nGITHUB_EVENT_NAME=\"$2\"\nGITHUB_OUTPUT=\"$3\"\n",
    );
    oracle.push_str(block);
    setup.write_file(&root.join("oracle.sh")?, oracle.as_bytes())?;
    let out = root.join("github_output")?;
    let steps = oxdock_core::parse_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH, GITHUB_WORKFLOW_REF, GITHUB_EVENT_NAME, GITHUB_OUTPUT, ORACLE_SH]
        RUN ["bash", "{{ env:ORACLE_SH }}", "{{ env:GITHUB_WORKFLOW_REF }}", "{{ env:GITHUB_EVENT_NAME }}", "{{ env:GITHUB_OUTPUT }}"]
    "#})
    .expect("oracle script parses");
    let mut io = ExecIo::new();
    io.insert_inherit_env("GITHUB_WORKFLOW_REF", workflow_ref);
    io.insert_inherit_env("GITHUB_EVENT_NAME", event_name);
    io.insert_inherit_env("GITHUB_OUTPUT", out.as_path().display().to_string());
    io.insert_inherit_env(
        "ORACLE_SH",
        root.join("oracle.sh")?.as_path().display().to_string(),
    );
    match run_steps_with_context_result_with_io(&root, &root, &steps, io).map(|_| ()) {
        Ok(()) => {
            let reader = PathResolver::new_guarded(root.clone(), root.clone())?;
            Ok(PinOutcome::Pin(
                reader.read_to_string(&out).context("read oracle output")?,
            ))
        }
        Err(_) => Ok(PinOutcome::Rejected),
    }
}

/// The extracted bootstrap bash and `PIPELINE_PIN` agree on every
/// table entry: pins match byte for byte, failures happen together.
#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns a real bash process")]
fn pin_bash_matches_dsl() -> Result<()> {
    let block = load_release_resolve_block()?;
    for (workflow_ref, event_name, expected) in CASES {
        let shell = bash_pin(&block, workflow_ref, event_name)?;
        let (dsl, _) = run_dsl(workflow_ref, event_name)?;
        match (expected, shell, dsl) {
            (Some(pin), PinOutcome::Pin(shell_out), PinOutcome::Pin(dsl_out)) => {
                assert_eq!(
                    shell_out,
                    format!("pin={pin}\n"),
                    "bash oracle pin for {workflow_ref}"
                );
                assert_eq!(
                    dsl_out,
                    format!("pin={pin}\n"),
                    "DSL pin for {workflow_ref}"
                );
            }
            (None, PinOutcome::Rejected, PinOutcome::Rejected) => {}
            (expected, _, _) => {
                panic!("bash and DSL diverge on {workflow_ref} (want {expected:?})");
            }
        }
    }
    Ok(())
}
