// TODO: Can this be rewritten using OxDock itself?

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{EntryKind, GuardedPath, PathResolver};
use oxdock_parser::{COMMANDS, parse_script};

const README_NAME: &str = "README.md";
const PLATFORM_TAGS: [&str; 5] = ["unix", "windows", "macos", "mac", "linux"];

#[derive(Debug)]
struct DslBlock {
    line_no: usize,
    body: String,
    directives: Vec<Directive>,
}

#[derive(Debug)]
struct Directive {
    platforms: Vec<String>,
    kind: DirectiveKind,
}

#[derive(Debug)]
enum DirectiveKind {
    Env { key: String, value: String },
    Stdin(String),
    Stdout(String),
    Error(String),
    Files { path: String, content: String },
    ContextFiles { path: String, content: String },
    Missing(String),
    Dirs(String),
    Pipes { name: String, contains: String },
    UnifiedRoots,
}

fn fence_ticks(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    let ticks = trimmed.len() - trimmed.trim_start_matches('`').len();
    (ticks >= 3).then_some(ticks)
}

fn extract_blocks(markdown: &str) -> Result<Vec<DslBlock>> {
    let mut blocks = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    let mut info: Option<String> = None;
    let mut body: Vec<&str> = Vec::new();

    for (idx, line) in markdown.lines().enumerate() {
        let line_no = idx + 1;
        match (open, fence_ticks(line)) {
            (None, Some(ticks)) => {
                let trimmed = line.trim_start();
                open = Some((line_no, ticks));
                info = Some(trimmed[ticks..].trim().to_string());
                body.clear();
            }
            (Some((open_no, open_ticks)), Some(ticks)) => {
                let trimmed = line.trim_start();
                if ticks >= open_ticks && trimmed[ticks..].trim().is_empty() {
                    if info.as_deref() == Some("oxdock") {
                        blocks.push(build_block(open_no, &body.join("\n"))?);
                    }
                    open = None;
                    info = None;
                } else {
                    body.push(line);
                }
            }
            (Some(_), None) => body.push(line),
            (None, None) => {}
        }
    }
    if let Some((open_no, _)) = open {
        bail!("{README_NAME}:{open_no}: code fence is never closed");
    }
    Ok(blocks)
}

fn build_block(open_no: usize, raw: &str) -> Result<DslBlock> {
    let mut body_lines: Vec<&str> = Vec::new();
    let mut directives: Vec<Directive> = Vec::new();

    for (offset, line) in raw.lines().enumerate() {
        let line_no = open_no + offset + 1;
        if let Some(rest) = line.strip_prefix("#>") {
            directives.push(parse_directive(rest, line_no)?);
        } else if let Some(rest) = line.strip_prefix("#|") {
            let last = directives.last_mut().ok_or_else(|| {
                anyhow::anyhow!(
                    "{README_NAME}:{line_no}: '#|' continuation without a preceding '#>' directive"
                )
            })?;
            append_continuation(last, rest);
        } else {
            body_lines.push(line);
        }
    }

    Ok(DslBlock {
        line_no: open_no,
        body: body_lines.join("\n"),
        directives,
    })
}

fn parse_directive(rest: &str, line_no: usize) -> Result<Directive> {
    let fail = |msg: &str| anyhow::anyhow!("{README_NAME}:{line_no}: {msg}");

    let rest = rest.trim_start();
    let (platforms, rest) = if let Some(after_open) = rest.strip_prefix('[') {
        let close = after_open
            .find(']')
            .ok_or_else(|| fail("platform prefix is missing ']'"))?;
        let tags: Vec<String> = after_open[..close]
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        for tag in &tags {
            if !PLATFORM_TAGS.contains(&tag.as_str()) {
                return Err(fail(&format!(
                    "unknown platform tag '{tag}' (expected one of {})",
                    PLATFORM_TAGS.join("|")
                )));
            }
        }
        (tags, after_open[close + 1..].trim_start())
    } else {
        (Vec::new(), rest)
    };

    let (key, value_raw) = if rest.contains(':') {
        rest.split_once(':')
            .ok_or_else(|| fail("directive is missing ':' after its key"))?
    } else {
        (rest.trim(), "")
    };
    let key = key.trim();
    let value = strip_single_space(value_raw);

    let kind = match key {
        "unified-roots" => DirectiveKind::UnifiedRoots,
        "env" => {
            let (k, v) = value
                .split_once('=')
                .ok_or_else(|| fail("env directive must be 'KEY=VALUE'"))?;
            DirectiveKind::Env {
                key: k.trim().to_string(),
                value: v.trim().to_string(),
            }
        }
        "stdin" => DirectiveKind::Stdin(value),
        "stdout" => DirectiveKind::Stdout(value),
        "error" => DirectiveKind::Error(value),
        "missing" => DirectiveKind::Missing(value),
        "dirs" => DirectiveKind::Dirs(value),
        "files" => {
            let (path, content) =
                split_kv(&value).ok_or_else(|| fail("files directive must be 'PATH = CONTENT'"))?;
            DirectiveKind::Files { path, content }
        }
        "context-files" => {
            let (path, content) = split_kv(&value)
                .ok_or_else(|| fail("context-files directive must be 'PATH = CONTENT'"))?;
            DirectiveKind::ContextFiles { path, content }
        }
        "pipes" => {
            let (name, contains) = split_kv(&value)
                .ok_or_else(|| fail("pipes directive must be 'NAME = SUBSTRING'"))?;
            DirectiveKind::Pipes { name, contains }
        }
        other => {
            return Err(fail(&format!(
                "unknown directive '{other}' (supported: unified-roots, env, stdin, stdout, error, files, context-files, missing, dirs, pipes)"
            )));
        }
    };
    Ok(Directive { platforms, kind })
}

fn append_continuation(directive: &mut Directive, rest: &str) {
    let chunk = format!("\n{}", strip_single_space(rest));
    match &mut directive.kind {
        DirectiveKind::Stdin(v)
        | DirectiveKind::Stdout(v)
        | DirectiveKind::Error(v)
        | DirectiveKind::Missing(v)
        | DirectiveKind::Dirs(v) => v.push_str(&chunk),
        DirectiveKind::Env { value, .. }
        | DirectiveKind::Pipes {
            contains: value, ..
        } => value.push_str(&chunk),
        DirectiveKind::Files { content, .. } | DirectiveKind::ContextFiles { content, .. } => {
            content.push_str(&chunk)
        }
        DirectiveKind::UnifiedRoots => {}
    }
}

fn split_kv(value: &str) -> Option<(String, String)> {
    let (lhs, rhs) = value.split_once('=')?;
    Some((lhs.trim().to_string(), strip_single_space(rhs)))
}

fn strip_single_space(s: &str) -> String {
    s.strip_prefix(' ').unwrap_or(s).to_string()
}

fn normalize(text: &str) -> String {
    text.replace('\r', "")
}

fn platform_active(tag: &str) -> bool {
    #[allow(clippy::disallowed_macros)]
    fn active(tag: &str) -> bool {
        match tag {
            "unix" => cfg!(unix),
            "windows" => cfg!(windows),
            "macos" | "mac" => cfg!(target_os = "macos"),
            "linux" => cfg!(target_os = "linux"),
            _ => false,
        }
    }
    active(tag)
}

fn applies_to_current_platform(directive: &Directive) -> bool {
    directive.platforms.iter().all(|t| platform_active(t))
}

fn read_workspace_file(resolver: &PathResolver, root: &GuardedPath, rel: &str) -> Result<String> {
    let file = root
        .join(rel)
        .with_context(|| format!("invalid path '{rel}'"))?;
    resolver.read_to_string(&file)
}

struct BlockRoots {
    _keepalive: Vec<oxdock_fs::GuardedTempDir>,
    fs_root: GuardedPath,
    context_root: GuardedPath,
}

fn prepare_roots(block: &DslBlock) -> Result<BlockRoots> {
    let unified = block
        .directives
        .iter()
        .any(|d| matches!(d.kind, DirectiveKind::UnifiedRoots));
    if unified {
        let temp = GuardedPath::tempdir().context("failed to create unified tempdir")?;
        let root = temp.as_guarded_path().clone();
        Ok(BlockRoots {
            _keepalive: vec![temp],
            fs_root: root.clone(),
            context_root: root,
        })
    } else {
        let workspace_temp =
            GuardedPath::tempdir().context("failed to create workspace tempdir")?;
        let context_temp = GuardedPath::tempdir().context("failed to create context tempdir")?;
        let fs_root = workspace_temp.as_guarded_path().clone();
        let context_root = context_temp.as_guarded_path().clone();
        Ok(BlockRoots {
            _keepalive: vec![workspace_temp, context_temp],
            fs_root,
            context_root,
        })
    }
}

fn run_block(block: &DslBlock) -> Result<()> {
    let at = |msg: String| format!("{README_NAME}:{0}: {msg}", block.line_no);

    let steps = parse_script(&block.body)
        .map_err(|e| anyhow::anyhow!(at(format!("snippet failed to parse: {e}"))))?;

    let roots = prepare_roots(block)?;
    let workspace_resolver =
        PathResolver::new_guarded(roots.fs_root.clone(), roots.fs_root.clone())?;
    let context_resolver =
        PathResolver::new_guarded(roots.context_root.clone(), roots.context_root.clone())?;

    let mut io = ExecIo::new();
    let mut pipes: Vec<(String, Arc<Mutex<Vec<u8>>>)> = Vec::new();

    for directive in &block.directives {
        if !applies_to_current_platform(directive) {
            continue;
        }
        match &directive.kind {
            DirectiveKind::Env { key, value } => io.insert_inherit_env(key.clone(), value.clone()),
            DirectiveKind::Stdin(content) => {
                io.set_stdin(Some(Arc::new(Mutex::new(Cursor::new(
                    normalize(content).into_bytes(),
                )))));
            }
            DirectiveKind::Pipes { name, .. }
                if !pipes.iter().any(|(pipe_name, _)| pipe_name == name) =>
            {
                let buffer = Arc::new(Mutex::new(Vec::new()));
                io.insert_output_pipe(name.clone(), buffer.clone());
                pipes.push((name.clone(), buffer));
            }
            _ => {}
        }
    }

    let stdout_buffer = Arc::new(Mutex::new(Vec::new()));
    io.set_stdout(Some(stdout_buffer.clone()));

    let expected_errors: Vec<String> = block
        .directives
        .iter()
        .filter(|d| applies_to_current_platform(d))
        .filter_map(|d| match &d.kind {
            DirectiveKind::Error(msg) => Some(normalize(msg)),
            _ => None,
        })
        .collect();

    let execution =
        run_steps_with_context_result_with_io(&roots.fs_root, &roots.context_root, &steps, io);

    match (execution, expected_errors.is_empty()) {
        (Ok(_), true) => {}
        (Ok(_), false) => bail!(at(format!(
            "snippet was expected to fail with '{}' but succeeded",
            expected_errors.join("', '")
        ))),
        (Err(err), false) => {
            let rendered = normalize(&format!("{err:#}"));
            for needle in &expected_errors {
                if !rendered.contains(needle) {
                    bail!(at(format!(
                        "error message did not contain '{needle}'; got: {rendered}"
                    )));
                }
            }
        }
        (Err(err), true) => bail!(at(format!("snippet failed unexpectedly: {err:#}"))),
    }

    let stdout_text = normalize(&String::from_utf8_lossy(
        &stdout_buffer.lock().expect("stdout lock poisoned"),
    ));

    for directive in &block.directives {
        if !applies_to_current_platform(directive) {
            continue;
        }
        let assert_failed = |msg: String| -> anyhow::Error { anyhow::anyhow!(at(msg)) };
        match &directive.kind {
            DirectiveKind::Stdout(needle) => {
                let needle = normalize(needle);
                if !stdout_text.contains(&needle) {
                    return Err(assert_failed(format!(
                        "stdout did not contain '{needle}'; captured:\n{stdout_text}"
                    )));
                }
            }
            DirectiveKind::Files { path, content } => {
                let actual = read_workspace_file(&workspace_resolver, &roots.fs_root, path)
                    .map_err(|e| {
                        assert_failed(format!("expected file '{path}' unreadable: {e:#}"))
                    })?;
                if normalize(&actual) != normalize(content) {
                    return Err(assert_failed(format!(
                        "file '{path}' content mismatch:\nexpected: {content:?}\nactual:   {actual:?}"
                    )));
                }
            }
            DirectiveKind::ContextFiles { path, content } => {
                let actual = read_workspace_file(&context_resolver, &roots.context_root, path)
                    .map_err(|e| {
                        assert_failed(format!("expected context file '{path}' unreadable: {e:#}"))
                    })?;
                if normalize(&actual) != normalize(content) {
                    return Err(assert_failed(format!(
                        "context file '{path}' content mismatch:\nexpected: {content:?}\nactual:   {actual:?}"
                    )));
                }
            }
            DirectiveKind::Missing(path) => {
                let target = roots
                    .fs_root
                    .join(path)
                    .map_err(|e| assert_failed(format!("invalid missing-path '{path}': {e}")))?;
                if workspace_resolver.exists(&target) {
                    return Err(assert_failed(format!("path '{path}' should not exist")));
                }
            }
            DirectiveKind::Dirs(path) => {
                let target = roots
                    .fs_root
                    .join(path)
                    .map_err(|e| assert_failed(format!("invalid dirs-path '{path}': {e}")))?;
                let kind = workspace_resolver.entry_kind(&target).map_err(|e| {
                    assert_failed(format!("directory '{path}' not accessible: {e:#}"))
                })?;
                if !matches!(kind, EntryKind::Dir) {
                    return Err(assert_failed(format!("path '{path}' is not a directory")));
                }
            }
            DirectiveKind::Pipes { name, contains } => {
                let buffer = pipes
                    .iter()
                    .find(|(pipe_name, _)| pipe_name == name)
                    .map(|(_, b)| b.clone())
                    .ok_or_else(|| {
                        assert_failed(format!("internal: pipe '{name}' was not registered"))
                    })?;
                let text = normalize(&String::from_utf8_lossy(
                    &buffer.lock().expect("pipe lock poisoned"),
                ));
                let contains = normalize(contains);
                if !text.contains(&contains) {
                    return Err(assert_failed(format!(
                        "pipe '{name}' did not contain '{contains}'; captured:\n{text}"
                    )));
                }
            }
            DirectiveKind::Env { .. }
            | DirectiveKind::Stdin(_)
            | DirectiveKind::Error(_)
            | DirectiveKind::UnifiedRoots => {}
        }
    }
    Ok(())
}

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
        let after = bytes.get(end).copied();
        if boundary(before) && boundary(after) {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn load_readme_blocks() -> Result<Vec<DslBlock>> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR missing")?;
    let repo_root = manifest_dir
        .strip_suffix("crates/oxdock-logic-tests")
        .context("test must live under crates/oxdock-logic-tests")?
        .trim_end_matches('/')
        .to_string();
    let root = GuardedPath::new_root_from_str(&repo_root)?;
    let resolver = PathResolver::new_guarded(root.clone(), root)?;
    let readme_path = resolver.root().join(README_NAME)?;
    let markdown = resolver.read_to_string(&readme_path)?;
    extract_blocks(&markdown)
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

    for marker in ["or(", "{{ ", "[env:"] {
        assert!(
            bodies.contains(marker),
            "{README_NAME}: language reference must document structural feature '{marker}'"
        );
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
        run_block(&block).with_context(|| {
            format!(
                "{README_NAME}: while executing example opened at line {}",
                block.line_no
            )
        })?;
    }
    Ok(())
}
