use anyhow::{Context, Result};
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

fn load_workflow() -> Result<String> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let workflow_path = resolver
        .root()
        .join(".github/workflows/install-smoke.yml")?;
    resolver.read_to_string(&workflow_path)
}

/// Extract `run:` literal blocks belonging to `shell: oxdock*` steps.
///
/// Style-bound by design: every oxdock-shell step must use a `run: |`
/// literal block. A folded (`>`) or single-line `run:` on such a step
/// fails naming it, so coverage degrades loudly instead of silently
/// skipping a block.
fn oxdock_blocks(text: &str) -> Result<Vec<(String, String)>> {
    let mut blocks = Vec::new();
    let mut step_name = "<unnamed>".to_string();
    let mut shell_oxdock = false;
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if indent == 6 && trimmed.starts_with("- name:") {
            step_name = trimmed["- name:".len()..].trim().to_string();
            shell_oxdock = false;
        } else if indent == 8 && trimmed.starts_with("shell:") {
            let shell = trimmed["shell:".len()..].trim();
            shell_oxdock = shell.starts_with("oxdock");
        } else if indent == 8 && trimmed.starts_with("run:") {
            let style = trimmed["run:".len()..].trim();
            if style == "|" {
                if shell_oxdock {
                    let mut body = Vec::new();
                    while let Some(next) = lines.peek() {
                        if next.trim().is_empty() {
                            body.push(String::new());
                            lines.next();
                        } else if next.len() - next.trim_start().len() >= 10 {
                            body.push(next[10..].to_string());
                            lines.next();
                        } else {
                            break;
                        }
                    }
                    blocks.push((step_name.clone(), body.join("\n")));
                }
            } else if shell_oxdock {
                anyhow::bail!(
                    "step '{step_name}' uses shell: oxdock without a `run: |` literal block"
                );
            }
        }
    }
    Ok(blocks)
}

/// Every inline DSL leg must always parse: smoke runs installer legs
/// in CI, so a breaking language change that invalidates one fails the
/// PR that makes the change, never the smoke matrix.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads install-smoke.yml from the repository checkout layout"
)]
fn smoke_steps_parse_and_cover_all_legs() -> Result<()> {
    let text = load_workflow()?;
    let blocks = oxdock_blocks(&text)?;
    assert!(
        blocks.len() >= 11,
        "expected build plus ten legs, found {} oxdock blocks",
        blocks.len()
    );
    let mut joined = String::new();
    for (name, body) in &blocks {
        oxdock_core::parse_script(body)
            .map_err(|err| anyhow::anyhow!("smoke step '{name}' failed to parse: {err}"))?;
        joined.push_str(body);
        joined.push('\n');
    }
    for marker in [
        "oxdock-stage",
        "via-stdin",
        "scripts/smoke/run-ephemeral",
        "SMOKE_TAG",
        "using cached",
        "install.sh | bash",
        "\"oxdock\", \"--version\"",
        // Gap closures: prebuilt reuses the override limb, isolated
        // proves silenced CI, every leg now routes CI files.
        "OXDOCK_SKIP_DOWNLOAD",
        "oxdock-prebuilt",
        "GITHUB_PATH, GITHUB_ENV",
        // Version identity against source truth, not a second binary.
        "LOAD_TOML(\"Cargo.toml\")",
        // Configured shell: Unix legs put the install dir on PATH so
        // bare names reach the product on both platforms.
        "/.local/bin:{{ env:PATH }}",
        "installed oxdock",
        "| iex",
        "INHERIT_ENV [OXDOCK_OXFILE_URL, SMOKE_PORT",
        // Terminal prompt: the isolated leg answers the destination
        // prompt through a pty master, never a `script(1)` shim, so a
        // revert to shell hacks fails loudly here.
        "WITH_IO [tty",
        // Native joins: legs build every derived path through PATH_JOIN,
        // never hand-rolled separators with per-OS overrides.
        "PATH_JOIN(",
    ] {
        assert!(
            joined.contains(marker),
            "smoke orchestration covers '{marker}'"
        );
    }
    // Renamed bootstrap: bare names in the blocks above can only mean
    // the binary just installed, with no shadow on PATH. These live in
    // yaml steps, so they assert against the whole workflow text.
    for marker in [
        "binary-name: oxdock-engine",
        "shell: oxdock-engine --script {0}",
    ] {
        assert!(
            text.contains(marker),
            "smoke orchestration covers '{marker}'"
        );
    }
    assert!(
        text.contains("127.0.0.1:9"),
        "offline leg bridges the blackhole proxies via env"
    );
    Ok(())
}

/// Template-to-output fidelity: the rendered ephemeral commands must
/// carry the CI payload and hermetic stub sources. If values drift
/// from the template, this names it instead of failing in CI.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads rendered smoke commands from the repository checkout layout"
)]
fn rendered_ephemeral_commands_carry_ci_wiring() -> Result<()> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let sh = resolver.read_to_string(&resolver.root().join("scripts/smoke/run-ephemeral.sh")?)?;
    let ps1 = resolver.read_to_string(&resolver.root().join("scripts/smoke/run-ephemeral.ps1")?)?;
    for (name, body) in [
        ("run-ephemeral.sh", sh.as_str()),
        ("run-ephemeral.ps1", ps1.as_str()),
    ] {
        assert!(
            body.contains("via-ephemeral"),
            "{name} pipes the CI marker payload"
        );
    }
    assert!(
        sh.contains("cat ./install.sh"),
        "sh rendering reads the branch stub, never the network"
    );
    assert!(
        ps1.contains("Get-Content -Raw"),
        "ps1 rendering reads the branch stub, never the network"
    );
    Ok(())
}

/// Architecture coverage contract: every release target is installed by
/// smoke, on every leg. The aarch64-linux artifact shipped for releases
/// with zero executions before this test existed; adding a release
/// target without a matching runner (or dropping a runner from any leg
/// matrix) fails here instead of shipping another untested artifact.
/// Update the mapping below in the same commit as either workflow.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads workflow and stub files from the repository checkout layout"
)]
fn release_targets_all_have_smoke_legs() -> Result<()> {
    // (release target triple, smoke runner, DSL arch-branch marker,
    // release asset name).
    let coverage = [
        (
            "x86_64-unknown-linux-musl",
            "ubuntu-latest",
            "[os:linux, arch:x86_64]",
            "oxdock-x86_64-unknown-linux-musl.tar.gz",
        ),
        (
            "aarch64-unknown-linux-musl",
            "ubuntu-26.04-arm",
            "[os:linux, arch:aarch64]",
            "oxdock-aarch64-unknown-linux-musl.tar.gz",
        ),
        (
            "aarch64-apple-darwin",
            "macos-latest",
            "[os:macos]",
            "oxdock-aarch64-apple-darwin.tar.gz",
        ),
        (
            "x86_64-pc-windows-msvc",
            "windows-latest",
            "[os:windows, arch:x86_64]",
            "oxdock-x86_64-pc-windows-msvc.tar.gz",
        ),
        (
            "aarch64-pc-windows-msvc",
            "windows-11-arm",
            "[os:windows, arch:aarch64]",
            "oxdock-aarch64-pc-windows-msvc.tar.gz",
        ),
    ];
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let release =
        resolver.read_to_string(&resolver.root().join(".github/workflows/rust-release.yml")?)?;
    let smoke = load_workflow()?;
    let sh = resolver.read_to_string(&resolver.root().join("install.sh")?)?;
    let ps1 = resolver.read_to_string(&resolver.root().join("install.ps1")?)?;

    // Release side: every mapped triple is still shipped. A new target
    // without a mapping row fails here until one is added.
    for (triple, _, _, _) in &coverage {
        assert!(
            release.contains(triple),
            "release matrix no longer ships {triple}: update the coverage mapping"
        );
    }

    // Smoke side: every `os:` matrix (build plus ten legs) carries the
    // full runner set, in order. A leg dropping an architecture fails
    // here instead of silently untesting it.
    let runners: Vec<&str> = coverage.iter().map(|(_, runner, _, _)| *runner).collect();
    let mut matrices = 0;
    for line in smoke.lines() {
        let trimmed = line.trim_start();
        if let Some(list) = trimmed
            .strip_prefix("os: [")
            .and_then(|rest| rest.strip_suffix(']'))
        {
            matrices += 1;
            let found: Vec<&str> = list.split(',').map(|item| item.trim()).collect();
            assert_eq!(
                found, runners,
                "smoke matrix diverged from the coverage contract: {line}"
            );
        }
    }
    assert_eq!(
        matrices, 11,
        "expected build plus ten leg matrices, found {matrices}"
    );

    // DSL side: each mapping's arch branch and asset name resolve in the
    // workflow, so the runner actually selects its artifact.
    for (_, _, branch, asset) in &coverage {
        assert!(
            smoke.contains(branch),
            "smoke workflow lost arch branch {branch}"
        );
        assert!(
            smoke.contains(asset),
            "smoke workflow never selects asset {asset}"
        );
    }

    // Stub side: the user-facing download path maps both ARM targets.
    // These carried no executions before the coverage above existed.
    assert!(
        sh.contains("Linux-aarch64"),
        "install.sh lost its Linux ARM64 target mapping"
    );
    assert!(
        ps1.contains("'ARM64'"),
        "install.ps1 lost its Windows ARM64 target mapping"
    );
    Ok(())
}
