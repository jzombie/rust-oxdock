use anyhow::{Context, Result};
use oxdock_fs::{GuardedPath, PathResolver};

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
        blocks.len() >= 8,
        "expected build plus seven legs, found {} oxdock blocks",
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
        "installed oxdock",
    ] {
        assert!(
            joined.contains(marker),
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
