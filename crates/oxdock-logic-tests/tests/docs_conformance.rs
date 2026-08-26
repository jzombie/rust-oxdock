use std::io::Cursor;

use anyhow::{Context, Result, bail};
use line_ending::LineEnding;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_parser::{COMMANDS, FencedBlock, extract_fenced_blocks, parse_script};

const README_NAME: &str = "README.md";

fn load_readme_markdown() -> Result<String> {
    // Normalize separators first: Windows CARGO_MANIFEST_DIR uses backslashes.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .context("CARGO_MANIFEST_DIR missing")?
        .replace('\\', "/");
    let repo_root = manifest_dir
        .strip_suffix("crates/oxdock-logic-tests")
        .context("test must live under crates/oxdock-logic-tests")?
        .trim_end_matches('/')
        .to_string();
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let readme_path = resolver.root().join(README_NAME)?;
    resolver.read_to_string(&readme_path)
}

fn load_readme_blocks() -> Result<Vec<FencedBlock>> {
    extract_fenced_blocks(&load_readme_markdown()?, "oxdock")
}

/// True when `keyword` occurs in `haystack` with non-identifier boundaries.
fn contains_keyword(haystack: &str, keyword: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(keyword) {
        let abs = start + pos;
        let end = abs + keyword.len();
        let boundary = |b: Option<u8>| {
            b.map(|c| !(c.is_ascii_alphanumeric() || c == b'_'))
                .unwrap_or(true)
        };
        let before = if abs == 0 { None } else { Some(bytes[abs - 1]) };
        if boundary(before) && boundary(bytes.get(end).copied()) {
            return true;
        }
        start = abs + 1;
    }
    false
}

#[test]
fn readme_snippets_parse_and_cover_every_command() -> Result<()> {
    let blocks = load_readme_blocks()?;
    assert!(
        blocks.len() >= 20,
        "expected a substantial number of ```oxdock examples in {README_NAME}, found {}",
        blocks.len()
    );

    for block in &blocks {
        parse_script(&block.body).map_err(|e| {
            anyhow::anyhow!(
                "{README_NAME}:{0}: snippet failed to parse: {e}",
                block.line_no
            )
        })?;
    }

    let bodies: String = blocks
        .iter()
        .map(|b| b.body.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for command in COMMANDS {
        let keyword = command.as_str();
        assert!(
            contains_keyword(&bodies, keyword),
            "{README_NAME}: no executable example demonstrates the '{keyword}' command"
        );
    }

    for marker in ["or(", "{{ env:", "[env:"] {
        assert!(
            bodies.contains(marker),
            "{README_NAME}: language reference must document structural feature '{marker}'"
        );
    }
    Ok(())
}

#[test]
#[cfg_attr(miri, ignore = "requires the repository checkout layout")]
fn readme_references_resolve() -> Result<()> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .context("CARGO_MANIFEST_DIR missing")?
        .replace('\\', "/");
    let repo_root_str = manifest_dir
        .strip_suffix("crates/oxdock-logic-tests")
        .context("test must live under crates/oxdock-logic-tests")?
        .trim_end_matches('/')
        .to_string();
    let root = GuardedPath::new_root_from_str(&repo_root_str)?;
    let resolver = PathResolver::new_guarded(root.clone(), root.clone())?;
    let markdown = load_readme_markdown()?;

    // Every relative Markdown link target must exist on disk. Anchors are
    // stripped first; targets that are pure anchors are skipped.
    let mut scanned = &markdown[..];
    while let Some(pos) = scanned.find("](") {
        let after = &scanned[pos + 2..];
        let Some(close) = after.find(')') else {
            bail!("unterminated link target near: {after:.60}");
        };
        let raw_target = &after[..close];
        scanned = &after[close..];

        if raw_target.starts_with("http://")
            || raw_target.starts_with("https://")
            || raw_target.starts_with("mailto:")
        {
            continue;
        }
        // Drop Markdown title attributes, then anchors, then ./ prefixes.
        let raw_path = raw_target.split_whitespace().next().unwrap_or_default();
        let target = raw_path
            .split('#')
            .next()
            .unwrap_or_default()
            .trim_start_matches("./");
        if target.is_empty() {
            continue;
        }
        let candidate = root
            .join(target)
            .with_context(|| format!("link target '{raw_target}'"))?;
        assert!(
            resolver.entry_kind(&candidate).is_ok(),
            "{README_NAME}: broken relative link '{raw_target}' (resolved {candidate})",
            candidate = candidate.display()
        );
    }

    // Bash fences: referenced repo scripts and --path packages must exist.
    for block in extract_fenced_blocks(&markdown, "bash")? {
        let mut previous: Option<&str> = None;
        for raw_token in block.body.split_whitespace() {
            // Strip trailing shell syntax before path checks.
            let token = raw_token.trim_matches(|c| matches!(c, ';' | ')' | '"' | '\''));
            if let Some(script_rel) = token.strip_prefix("scripts/") {
                let candidate = root.join("scripts/")?.join(script_rel)?;
                assert!(
                    resolver.entry_kind(&candidate).is_ok(),
                    "{README_NAME}: bash fence references missing script '{}'",
                    candidate.display()
                );
            }
            if previous == Some("--path") && !token.starts_with('$') {
                let candidate = root.join(token)?;
                assert!(
                    resolver.entry_kind(&candidate).is_ok(),
                    "{README_NAME}: bash fence references missing package path '{}'",
                    candidate.display()
                );
            }
            previous = Some(token);
        }
    }
    Ok(())
}

#[test]
#[cfg_attr(
    miri,
    ignore = "examples execute real processes (RUN/RUN_BG/git) against host tempdirs"
)]
fn readme_snippets_execute_as_documented() -> Result<()> {
    for block in load_readme_blocks()? {
        execute_block(&block).with_context(|| {
            format!(
                "{README_NAME}: while executing example opened at line {}",
                block.line_no
            )
        })?;
    }
    Ok(())
}

fn execute_block(block: &FencedBlock) -> Result<()> {
    let steps =
        parse_script(&block.body).map_err(|e| anyhow::anyhow!("snippet failed to parse: {e}"))?;

    // Tempdirs must outlive execution; dropping a GuardedTempDir removes it.
    let workspace_temp = GuardedPath::tempdir().context("failed to create workspace tempdir")?;
    let context_temp = if block.metadata.unified_roots {
        None
    } else {
        Some(GuardedPath::tempdir().context("failed to create context tempdir")?)
    };
    let fs_root = workspace_temp.as_guarded_path().clone();
    let context_root = match &context_temp {
        Some(temp) => temp.as_guarded_path().clone(),
        None => fs_root.clone(),
    };

    let mut io = ExecIo::new();
    for (key, value) in &block.metadata.env {
        io.insert_inherit_env(key.clone(), value.clone());
    }
    if let Some(stdin_payload) = &block.metadata.stdin {
        io.set_stdin(Some(std::sync::Arc::new(std::sync::Mutex::new(
            Cursor::new(LineEnding::normalize(stdin_payload).as_bytes().to_vec()),
        ))));
    }

    let execution = run_steps_with_context_result_with_io(&fs_root, &context_root, &steps, io);

    match (&execution, &block.metadata.expect_error) {
        (Ok(_), None) => {}
        (Ok(_), Some(expected)) => {
            bail!("snippet was expected to fail with '{expected}' but succeeded")
        }
        (Err(err), Some(expected)) => {
            let rendered = LineEnding::normalize(&format!("{err:#}"));
            if !rendered.contains(expected.as_str()) {
                bail!("error message did not contain '{expected}'; got: {rendered}");
            }
        }
        (Err(err), None) => bail!("snippet failed unexpectedly: {err:#}"),
    }
    Ok(())
}
