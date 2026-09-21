use anyhow::{Context, Result, bail};
use line_ending::LineEnding;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_parser::{COMMANDS, FencedBlock, extract_fenced_blocks};
use std::collections::HashSet;

const README_NAME: &str = "README.md";
const OXDOCK_README_NAME: &str = "oxdock/README.md";
const CRATE_DOCS_NAME: &str = "oxdock/docs/crate_docs.md";

/// Documents under conformance: the workspace README carries the full
/// command reference, the `oxdock` README mirrors the shared sections
/// without bundling it, and the crate docs feed rustdoc.
const FENCE_DOCUMENTS: &[&str] = &[README_NAME, OXDOCK_README_NAME];
const ANCHOR_DOCUMENTS: &[&str] = &[README_NAME, OXDOCK_README_NAME, CRATE_DOCS_NAME];

fn repo_root() -> Result<String> {
    // Normalize separators first: Windows CARGO_MANIFEST_DIR uses backslashes.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .context("CARGO_MANIFEST_DIR missing")?
        .replace('\\', "/");
    Ok(manifest_dir
        .strip_suffix("crates/oxdock-logic-tests")
        .context("test must live under crates/oxdock-logic-tests")?
        .trim_end_matches('/')
        .to_string())
}

fn load_markdown(name: &str) -> Result<String> {
    let repo_root = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let readme_path = resolver.root().join(name)?;
    resolver.read_to_string(&readme_path)
}

fn load_blocks(name: &str) -> Result<Vec<FencedBlock>> {
    extract_fenced_blocks(&load_markdown(name)?, "oxdock")
}

fn load_readme_blocks() -> Result<Vec<FencedBlock>> {
    load_blocks(README_NAME)
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

/// Lines outside fenced blocks: headings and links inside fences are
/// examples, not document structure.
fn prose_lines(markdown: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut fenced = false;
    for (idx, line) in markdown.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !fenced {
            out.push((idx + 1, line));
        }
    }
    out
}

/// GitHub-style heading slug: lowercase, punctuation dropped (underscores
/// kept), spaces become hyphens.
fn slugify(heading: &str) -> String {
    heading
        .trim()
        .trim_end_matches('#')
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .collect::<String>()
        .replace(' ', "-")
}

fn heading_slugs(markdown: &str) -> HashSet<String> {
    prose_lines(markdown)
        .into_iter()
        .filter_map(|(_, line)| {
            let hashes = line.trim_start().chars().take_while(|c| *c == '#').count();
            let rest = &line.trim_start()[hashes.min(line.trim_start().len())..];
            if (1..=6).contains(&hashes) && rest.starts_with(' ') {
                Some(slugify(rest))
            } else {
                None
            }
        })
        .collect()
}

/// Pure in-page anchor targets (`](#slug)`), with 1-based line numbers.
fn anchor_targets(markdown: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (line_no, line) in prose_lines(markdown) {
        let mut scanned = line;
        while let Some(pos) = scanned.find("](#") {
            let after = &scanned[pos + 3..];
            if let Some(close) = after.find(')') {
                out.push((line_no, after[..close].to_string()));
                scanned = &after[close..];
            } else {
                break;
            }
        }
    }
    out
}

#[test]
#[cfg_attr(
    miri,
    ignore = "requires CARGO_MANIFEST_DIR and host filesystem for README resolution"
)]
fn readme_snippets_parse_and_cover_every_command() -> Result<()> {
    let blocks = load_readme_blocks()?;
    assert!(
        blocks.len() >= 20,
        "expected a substantial number of ```oxdock examples in {README_NAME}, found {}",
        blocks.len()
    );

    for block in &blocks {
        oxdock_core::parse_script(&block.body).map_err(|e| {
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

    for marker in ["any(", "{{ env:", "[env:"] {
        assert!(
            bodies.contains(marker),
            "{README_NAME}: DSL reference must document structural feature '{marker}'"
        );
    }
    Ok(())
}

#[test]
#[cfg_attr(
    miri,
    ignore = "requires CARGO_MANIFEST_DIR and host filesystem for README resolution"
)]
fn oxdock_readme_snippets_parse() -> Result<()> {
    // The `oxdock` README mirrors the shared sections without bundling the
    // full command reference, so per-command coverage stays workspace-only;
    // every snippet here must still parse.
    let blocks = load_blocks(OXDOCK_README_NAME)?;
    assert!(
        blocks.len() >= 10,
        "expected ```oxdock examples in {OXDOCK_README_NAME}, found {}",
        blocks.len()
    );

    for block in &blocks {
        oxdock_core::parse_script(&block.body).map_err(|e| {
            anyhow::anyhow!(
                "{OXDOCK_README_NAME}:{0}: snippet failed to parse: {e}",
                block.line_no
            )
        })?;
    }
    Ok(())
}

/// Plugin READMEs under conformance: every ```oxdock snippet in the
/// generated plugin references executes end to end, not just parses.
/// Session flows need live servers and clients, so these run against
/// loopback with the plugin modules registered (miri-ignored like all
/// socket tests). Kept separate from FENCE_DOCUMENTS: that list feeds
/// the module-unaware STD-only executor, while plugin fences need their
/// modules and network.
const PLUGIN_FENCE_DOCUMENTS: &[&str] = &[
    "crates/plugins/oxdock-ssh-plugin/README.md",
    "crates/plugins/oxdock-net-plugin/README.md",
];

fn plugin_module_table() -> oxdock_parser::ModuleTable {
    let mut engine = oxdock_core::Engine::new();
    engine.register_module(oxdock_ssh_plugin::module());
    engine.register_module(oxdock_net_plugin::module());
    engine.module_table()
}

#[test]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus a Tokio runtime for plugin fences"
)]
fn plugin_readme_snippets_execute() -> Result<()> {
    for name in PLUGIN_FENCE_DOCUMENTS {
        for block in load_blocks(name)? {
            execute_plugin_block(&block, name)?;
        }
    }
    Ok(())
}

/// Execute one plugin fence end to end with the plugin modules
/// registered, mirroring `execute_block` (tempdir isolation, fence
/// metadata, expected-error matching). A join timeout fails loudly
/// instead of hanging the suite if an example ever strands.
fn execute_plugin_block(block: &FencedBlock, name: &str) -> Result<()> {
    use std::time::Duration;

    let table = plugin_module_table();
    let steps = oxdock_core::parse_script_with_modules(&block.body, table)
        .map_err(|e| anyhow::anyhow!("{name}:{0}: snippet failed to parse: {e}", block.line_no))?;

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

    let mut resolver =
        PathResolver::new_guarded(fs_root.clone(), context_root.clone()).context("fs setup")?;
    resolver.set_workspace_root(context_root.clone());
    let fs: Box<dyn oxdock_fs::WorkspaceFs> = Box::new(resolver);
    // Doc fences name logical services, never physical addresses, so
    // the harness maps them the way a CLI runner would: ephemeral
    // loopback binds resolved through the registry. Unmapped names
    // would fall back to memory rendezvous, which SSH rejects.
    let ssh_registry = std::sync::Arc::new(oxdock_net_plugin::EndpointRegistry::new(false));
    ssh_registry
        .add_mapping(
            &oxdock_net_plugin::VirtualEndpoint::Name("doc-ssh-demo".to_string()),
            oxdock_net_plugin::BindingSpec::Loopback { port: 0 },
        )
        .context("map doc service")?;
    ssh_registry.bind_all().context("bind doc service")?;
    let modules = vec![
        oxdock_ssh_plugin::module_with_endpoints(ssh_registry),
        oxdock_net_plugin::module(),
    ];
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let execution = oxdock_core::run_steps_with_manager_with_modules(
            fs,
            &steps,
            oxdock_process::default_process_manager(),
            io,
            modules,
            Vec::new(),
        );
        let _ = done_tx.send(execution.map(|_| ()).map_err(|err| format!("{err:#}")));
    });
    let execution: Result<()> = match done_rx.recv_timeout(Duration::from_secs(120)) {
        Ok(result) => result.map_err(|text| anyhow::anyhow!("{text}")),
        Err(_) => Err(anyhow::anyhow!(
            "{name}: snippet opened at line {} hung past 120s",
            block.line_no
        )),
    };

    match (&execution, &block.metadata.expect_error) {
        (Ok(_), None) => {}
        (Ok(_), Some(expected)) => {
            bail!("{name}: snippet was expected to fail with '{expected}' but succeeded")
        }
        (Err(err), Some(expected)) => {
            let rendered = LineEnding::normalize(&format!("{err:#}"));
            if !rendered.contains(expected.as_str()) {
                bail!("{name}: error message did not contain '{expected}'; got: {rendered}");
            }
        }
        (Err(err), None) => bail!("{name}: snippet failed unexpectedly: {err:#}"),
    }
    Ok(())
}

#[test]
#[cfg_attr(miri, ignore = "requires the repository checkout layout")]
fn readme_references_resolve() -> Result<()> {
    let repo_root_str = repo_root()?;
    let root = GuardedPath::new_root_from_str(&repo_root_str)?;
    let resolver = PathResolver::new_guarded(root.clone(), root.clone())?;
    for name in FENCE_DOCUMENTS {
        let markdown = load_markdown(name)?;

        // Every relative Markdown link target must exist on disk. Anchors are
        // stripped first; targets that are pure anchors are skipped here and
        // pinned by `readme_anchors_resolve` instead.
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
                "{name}: broken relative link '{raw_target}' (resolved {candidate})",
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
                        "{name}: bash fence references missing script '{}'",
                        candidate.display()
                    );
                }
                if previous == Some("--path") && !token.starts_with('$') {
                    let candidate = root.join(token)?;
                    assert!(
                        resolver.entry_kind(&candidate).is_ok(),
                        "{name}: bash fence references missing package path '{}'",
                        candidate.display()
                    );
                }
                previous = Some(token);
            }
        }
    }
    Ok(())
}

#[test]
#[cfg_attr(miri, ignore = "requires the repository checkout layout")]
fn readme_anchors_resolve() -> Result<()> {
    for name in ANCHOR_DOCUMENTS {
        let markdown = load_markdown(name)?;
        let slugs = heading_slugs(&markdown);
        for (line_no, anchor) in anchor_targets(&markdown) {
            assert!(
                slugs.contains(&anchor),
                "{name}:{line_no}: anchor '#{anchor}' matches no heading"
            );
        }
    }
    Ok(())
}

#[test]
#[cfg_attr(
    miri,
    ignore = "examples execute real processes (RUN/ASYNC RUN/git) against host tempdirs"
)]
fn readme_snippets_execute_as_documented() -> Result<()> {
    for name in FENCE_DOCUMENTS {
        for block in load_blocks(name)? {
            execute_block(&block, name).with_context(|| {
                format!(
                    "{name}: while executing example opened at line {}",
                    block.line_no
                )
            })?;
        }
    }
    Ok(())
}

fn execute_block(block: &FencedBlock, name: &str) -> Result<()> {
    let steps = oxdock_core::parse_script(&block.body)
        .map_err(|e| anyhow::anyhow!("{name}: snippet failed to parse: {e}"))?;

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

    let execution = run_steps_with_context_result_with_io(&fs_root, &context_root, &steps, io);

    match (&execution, &block.metadata.expect_error) {
        (Ok(_), None) => {}
        (Ok(_), Some(expected)) => {
            bail!("{name}: snippet was expected to fail with '{expected}' but succeeded")
        }
        (Err(err), Some(expected)) => {
            let rendered = LineEnding::normalize(&format!("{err:#}"));
            if !rendered.contains(expected.as_str()) {
                bail!("{name}: error message did not contain '{expected}'; got: {rendered}");
            }
        }
        (Err(err), None) => bail!("{name}: snippet failed unexpectedly: {err:#}"),
    }
    Ok(())
}
