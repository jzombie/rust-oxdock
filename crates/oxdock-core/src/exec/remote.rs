//! Sealed remote execution blocks (`REMOTE <target> [...] { ... }`).
//!
//! Core owns the keyword, the scope discipline, and the interception
//! plumbing. Core never spawns `ssh`, never frames `muxio`, and never
//! packs tarballs: a [`RemoteRunner`] performs the transport, registered
//! through [`ExecIo::set_remote_runner`](super::ExecIo::set_remote_runner)
//! (the NET plugin registers its SSH session at CLI startup; tests stage
//! a mock). Without a runner every `REMOTE` step bails with an actionable
//! error naming the missing feature.
//!
//! The boundary contract, enforced here:
//! - Nothing crosses as variables. Header-listed `$var` / `env:NAME`
//!   entries render to `LET` / `ENV` source lines prepended to the shipped
//!   text, so the guest parses ordinary steps and no value protocol exists.
//! - Files cross as relative path and byte pairs collected from the active
//!   root on entry and applied to the same root on exit. Deletions propagate
//!   by manifest diff, deepest first.
//! - Bytes cross through `WITH_IO` backends passed live to the runner.
//! - The host never switches workspace selection for a remote block.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use oxdock_fs::EntryKind;
use oxdock_parser::{Step, Value};
use oxdock_pipe::PipeInner;
use oxdock_process::ProcessManager;

use super::io::write_stdout;
use super::steps::StepCtx;

/// Cap on entries collected for one declared transfer. Workspaces are
/// small by construction; an unbounded walk would let a stray huge tree
/// exhaust memory before the transport runs.
const MAX_TRANSFER_ENTRIES: usize = 100_000;

/// Collected workspace entries for one declared transfer: guest-dest
/// file bytes plus guest-dest directories.
type FetchedEntries = (Vec<(String, Vec<u8>)>, Vec<String>);

/// Acknowledge a flagged `COPY --from-host` / `--to-host` step at execution
/// time. Inside a guest serve loop (guest mode) these are transfer
/// declarations already fulfilled by the session, so they succeed without
/// re-copying. Anywhere else they are unreachable (the parser rejects
/// flagged copies outside `REMOTE` bodies, and hosts never execute
/// `REMOTE` bodies), so reaching here bails defensively.
pub(super) fn copy_transfer<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    from_host: bool,
    to_host: bool,
) -> Result<()> {
    if cx.state.io.is_remote_guest() {
        let _ = (from_host, to_host);
        return Ok(());
    }
    bail!("COPY --from-host and --to-host execute only inside a remote guest session")
}

/// One collected transfer declaration: direction flags plus cloned paths.
/// Resolution against live scope happens at interception.
struct TransferDecl {
    from_host: bool,
    from: oxdock_parser::Arg,
    to: oxdock_parser::Arg,
    from_workspace: Option<oxdock_parser::WorkspaceTarget>,
}

/// Collect `COPY --from-host` / `--to-host` declarations transitively from
/// a `REMOTE` body. Args clone (declarations are few and parse-time): this
/// keeps the walker borrow-simple, matching the `contains_*` passes.
fn collect_transfer_decls(body: &[Step]) -> Vec<TransferDecl> {
    let mut out = Vec::new();
    fn visit(kind: &oxdock_parser::StepKind, out: &mut Vec<TransferDecl>) {
        if let oxdock_parser::StepKind::Copy {
            from_workspace,
            from_host,
            to_host,
            from,
            to,
        } = kind
            && (*from_host || *to_host)
        {
            out.push(TransferDecl {
                from_host: *from_host,
                from: from.clone(),
                to: to.clone(),
                from_workspace: from_workspace.clone(),
            });
        }
        kind.walk_child_kinds(&mut |child| visit(child, out));
    }
    for step in body {
        visit(&step.kind, &mut out);
    }
    out
}

/// What the host hands a [`RemoteRunner`]: rendered script text, declared
/// file transfers, and live pipe backends. The runner owns streaming and
/// the guest lifecycle; core owns rendering, declaration resolution, and
/// application.
///
/// No bulk sync exists: the guest starts empty and only declared entries
/// cross. `fetch_files` carries host-resolved bytes keyed by guest-dest
/// paths; `push_decls` carries guest-source to host-dest path pairs the
/// runner fulfills after successful execution.
///
/// Stdio contract: the runner MAY write guest output incrementally into
/// the provided backends (bounded memory for large outputs); any bytes it
/// returns in the response are APPENDED by core. Runners that never touch
/// the backends return everything. Core always force-closes the stdout
/// backend after routing, so downstream readers observe EOF either way.
pub struct RemoteRequest {
    /// Inventory target name, for session selection and diagnostics.
    pub target: String,
    /// `LET` / `ENV` injection lines plus the body rendered to source text.
    pub script_text: String,
    /// Declared `--from-host` entries as `(guest_rel, bytes)`, resolved by
    /// core against the host active root under guard.
    pub fetch_files: Vec<(String, Vec<u8>)>,
    /// Declared `--from-host` directories as guest-relative paths.
    pub fetch_dirs: Vec<String>,
    /// Declared `--to-host` entries as `(guest_rel, host_rel)` string
    /// pairs for the runner to fulfill after successful execution.
    pub push_decls: Vec<(String, String)>,
    /// Live stdin backend when `WITH_IO [stdin=$p]` wraps the block.
    pub stdin: Option<Arc<PipeInner>>,
    /// Live stdout backend when `WITH_IO [stdout=$p]` wraps the block.
    /// The runner writes guest output incrementally (bounded memory);
    /// anything returned in the response is APPENDED by core.
    pub stdout: Option<Arc<PipeInner>>,
    /// Live stderr sink when the step error stream is a shared writer.
    /// Same incremental contract as `stdout`; `None` (OS pipes) means the
    /// runner returns all stderr bytes in the response instead.
    pub stderr_sink: Option<oxdock_process::SharedOutput>,
    /// Host cancellation token (`CANCEL` / `TIMEOUT` / drop): the runner
    /// polls this on every pump tick and tears the session down promptly
    /// when set, so a blocked transport never strands the pipeline.
    pub cancelled: Arc<AtomicBool>,
}

/// What a [`RemoteRunner`] returns: stdio bytes plus declared push
/// results. Push entries arrive as host-dest paths plus bytes; push
/// symlinks as host-dest paths plus raw targets (core materializes each
/// link only when the target resolves inside the active root, and aborts
/// the session otherwise). Undeclared guest writes never cross: guest
/// deletions affect nothing on the host.
pub struct RemoteResponse {
    /// Guest stdout bytes, routed to the backend or step stream by core.
    pub stdout_bytes: Vec<u8>,
    /// Guest stderr bytes, routed to the step error stream by core.
    pub stderr_bytes: Vec<u8>,
    /// Declared `--to-host` files as host-relative paths plus bytes.
    pub push_files: Vec<(String, Vec<u8>)>,
    /// Declared `--to-host` directories as host-relative paths.
    pub push_dirs: Vec<String>,
    /// Declared `--to-host` symlinks as host-relative path plus raw target.
    pub push_symlinks: Vec<(String, String)>,
}

/// Transport behind a `REMOTE` block: SSH plus framing in production, an
/// in-process mock in tests. Object safe so runners stage through
/// [`ExecIo`](super::ExecIo) as `Arc<dyn RemoteRunner>`.
pub trait RemoteRunner: Send + Sync {
    /// Execute rendered script text with the given files and stdio
    /// backends. Errors fail the `REMOTE` step on the host (host wins).
    fn run_remote(&self, request: RemoteRequest) -> Result<RemoteResponse>;
}

/// Canonical DSL string escaping: the exact inverse of runtime
/// `expand_string` (`args.rs` documents `\\`, `\{{`, `\n`, `\t`, `\r`,
/// `\"`). Every backslash the renderer emits precedes a recognized escape,
/// so injected values reparse byte identical and can never break out of
/// their literal into guest instructions. Separate from the `quote_*`
/// `Display` helpers, which do not cover control characters.
pub fn escape_dsl_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // A `{{` pair triggers interpolation; escape the first brace
            // and leave the second for the next iteration, so `{{`
            // renders as exactly `\{{`.
            '{' if chars.peek() == Some(&'{') => out.push_str("\\{"),
            _ => out.push(ch),
        }
    }
    out
}

/// Render one header-listed value to a DSL literal. Tight v1 set: scalars
/// plus `LIST` / `MAP` composed of scalars. `DURATION` renders quoted both
/// levels; nested durations arrive as `STRING` on the guest (only a
/// top-level `LET $d: DURATION` declaration drives coercion), which the
/// renderer documents rather than hides. Everything else bails naming the
/// variable and pointing at pipes or files.
pub fn render_dsl_literal(value: &Value) -> Result<String> {
    match value {
        _ if value.as_i64().is_some() => Ok(value.as_i64().unwrap_or(0).to_string()),
        _ if value.as_f64().is_some() => render_float(value.as_f64().unwrap_or(0.0)),
        _ if value.as_bool().is_some() => Ok(value.as_bool().unwrap_or(false).to_string()),
        _ if value.as_str().is_some() => Ok(render_quoted(value.as_str().unwrap_or(""))),
        _ if value.as_duration().is_some() => {
            let text = oxdock_parser::command::format_duration(&value.as_duration().unwrap_or(
                Duration::from_secs(0),
            ));
            Ok(render_quoted(&text))
        }
        _ if value.as_list().is_some() => {
            let mut items = Vec::new();
            for item in value.as_list().unwrap_or(&Vec::new()) {
                items.push(render_composite_item(item)?);
            }
            Ok(format!("[{}]", items.join(", ")))
        }
        _ if value.as_map().is_some() => {
            let mut entries = Vec::new();
            for (key, item) in value.as_map().unwrap_or(&BTreeMap::new()) {
                entries.push(format!("{}: {}", render_quoted(key), render_composite_item(item)?));
            }
            Ok(format!("{{{}}}", entries.join(", ")))
        }
        _ => bail!(
            "value of type {} cannot cross a REMOTE boundary",
            value.type_name()
        ),
    }
}

fn render_float(raw: f64) -> Result<String> {
    if !raw.is_finite() {
        bail!("non-finite FLOAT cannot cross a REMOTE boundary");
    }
    let mut text = format!("{raw}");
    if !text.contains('.') {
        text.push_str(".0");
    }
    Ok(text)
}

fn render_quoted(raw: &str) -> String {
    format!("\"{}\"", escape_dsl_string(raw))
}

fn render_composite_item(value: &Value) -> Result<String> {
    if value.as_duration().is_some() {
        let text = oxdock_parser::command::format_duration(
            &value.as_duration().unwrap_or(Duration::from_secs(0)),
        );
        return Ok(render_quoted(&text));
    }
    render_dsl_literal(value)
}

/// Resolve the header list against live host scope and render injection
/// lines. Unknown names bail; non-renderable types bail naming the
/// variable with the pipe-or-file remedy.
fn resolve_header<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    target: &str,
    vars: &[String],
    env_names: &[String],
) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for name in vars {
        let Some((decl_type, value)) = cx.state.get_var_typed(name) else {
            bail!("REMOTE '{target}' uses unknown variable ${name}");
        };
        let literal = render_dsl_literal(&value).with_context(|| {
            format!(
                "cannot pass ${name}: {} into REMOTE '{target}' (pass bytes via WITH_IO, files via the workspace)",
                value.type_name()
            )
        })?;
        lines.push(format!("LET ${name}: {decl_type} = {literal}"));
    }
    for name in env_names {
        let Some(value) = cx.get_env(name) else {
            bail!("REMOTE '{target}' uses unknown env {name}");
        };
        lines.push(format!("ENV {name}={}", render_quoted(&value)));
    }
    Ok(lines)
}

/// Resolve declared `--from-host` entries against the host side: each
/// source becomes file bytes keyed by guest-dest relative paths,
/// following Docker destination semantics (file onto a trailing-slash
/// dest lands under its basename; directory sources fan their contents
/// out under the dest). Missing sources fail fast before any session byte
/// ships. Symlinks resolve through to content, exactly like local `COPY`
/// (`entry_kind` follows): the guest receives plain files.
/// One host-resolved fetch declaration: source and destination relative
/// paths plus the workspace root the host side resolves against.
struct ResolvedFetch {
    host_rel: String,
    guest_rel: String,
    from_workspace: Option<oxdock_parser::WorkspaceTarget>,
}

fn resolve_fetch_entries<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    decls: &[ResolvedFetch],
) -> Result<FetchedEntries> {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for decl in decls {
        resolve_one_fetch(cx, decl, &mut files, &mut dirs)?;
    }
    Ok((files, dirs))
}

fn resolve_one_fetch<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    decl: &ResolvedFetch,
    files: &mut Vec<(String, Vec<u8>)>,
    dirs: &mut Vec<String>,
) -> Result<()> {
    let source = match &decl.from_workspace {
        Some(target) => cx
            .state
            .fs
            .resolve_copy_source_from_target(
                super::handlers::copy_source_root(target.clone()),
                &decl.host_rel,
            )
            .with_context(|| {
                format!("REMOTE fetch source missing: {}", decl.host_rel)
            })?,
        None => cx
            .state
            .fs
            .resolve_copy_source(&decl.host_rel)
            .with_context(|| {
                format!("REMOTE fetch source missing: {}", decl.host_rel)
            })?,
    };
    // Guest-dest mapping first so directory fan-out anchors correctly.
    let dest_base = decl.guest_rel.clone();
    match cx.state.fs.entry_kind(&source) {
        Ok(EntryKind::Dir) => {
            dirs.push(dest_base.clone());
            let mut stack = vec![(source, dest_base)];
            while let Some((dir, rel)) = stack.pop() {
                let mut entries = cx.state.fs.read_dir_entries(&dir)?;
                entries.sort_by_key(|entry| entry.file_name());
                for entry in entries {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let child = dir.join(&name)?;
                    let child_rel = format!("{rel}/{name}");
                    match cx.state.fs.entry_kind(&child)? {
                        EntryKind::Dir => {
                            dirs.push(child_rel.clone());
                            stack.push((child, child_rel));
                        }
                        _ => {
                            let mut reader = cx.state.fs.open_read(&child)?;
                            let mut bytes = Vec::new();
                            std::io::Read::read_to_end(&mut reader, &mut bytes)?;
                            files.push((child_rel, bytes));
                        }
                    }
                    if files.len() + dirs.len() > MAX_TRANSFER_ENTRIES {
                        bail!(
                            "REMOTE fetch hit the {MAX_TRANSFER_ENTRIES} entry cap: {} is too large to ship",
                            decl.host_rel
                        );
                    }
                }
            }
        }
        Ok(_) => {
            // File source: trailing-slash dest (or an existing guest dir,
            // unknowable from here, so slash-only) duplicates inside.
            let dest = if dest_base.ends_with('/') || dest_base.ends_with('\\') {
                let base = source
                    .as_path()
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .with_context(|| {
                        format!("REMOTE fetch source has no file name: {}", decl.host_rel)
                    })?;
                format!("{dest_base}{base}")
            } else {
                dest_base
            };
            let mut reader = cx.state.fs.open_read(&source)?;
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut reader, &mut bytes)?;
            files.push((dest, bytes));
        }
        Err(err) => {
            bail!("REMOTE fetch source missing: {} ({err:#})", decl.host_rel);
        }
    }
    Ok(())
}

/// Apply declared push files under the active root through guarded
/// writes, creating parent directories as needed.
fn apply_push_files<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    files: &[(String, Vec<u8>)],
) -> Result<()> {
    let root = cx.state.fs.root().clone();
    for (rel, bytes) in files {
        let path = root.join(rel)?;
        if let Some(parent) = parent_rel(rel) {
            let dir = root.join(&parent)?;
            cx.state.fs.create_dir_all(&dir)?;
        }
        let mut writer = cx.state.fs.open_write(&path)?;
        writer.write_all(bytes)?;
        writer.flush()?;
    }
    Ok(())
}

fn parent_rel(rel: &str) -> Option<String> {
    rel.rfind('/').map(|idx| rel[..idx].to_string())
}

/// Route guest stdout bytes: into the `WITH_IO` backend when present
/// (then force-close so downstream readers observe EOF), else onto the
/// step output stream. Guest stderr bytes always ride the step error
/// stream.
fn route_stdio<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    stdout_backend: Option<Arc<PipeInner>>,
    stdout_bytes: &[u8],
    stderr_bytes: &[u8],
) -> Result<()> {
    if let Some(backend) = stdout_backend {
        if !stdout_bytes.is_empty() {
            let writer = backend.writer_handle();
            if let Ok(mut guard) = writer.lock() {
                guard.write_all(stdout_bytes)?;
                guard.flush()?;
            }
        }
        backend.force_close();
    } else if !stdout_bytes.is_empty() {
        let owned = stdout_bytes.to_vec();
        write_stdout(cx.out.clone(), |writer| {
            writer.write_all(&owned)?;
            writer.flush()?;
            Ok(())
        })?;
    }
    if !stderr_bytes.is_empty() {
        let owned = stderr_bytes.to_vec();
        write_stdout(cx.err.clone(), |writer| {
            writer.write_all(&owned)?;
            writer.flush()?;
            Ok(())
        })?;
    }
    Ok(())
}

/// Execute one `REMOTE` block: resolve the header, render the script,
/// resolve declared `--from-host` transfers, run the transport, route
/// stdio, and apply declared `--to-host` pushes to the active root. Scope
/// unwinds through the normal step machinery; this function pushes
/// nothing and switches no selection. The guest starts empty: only
/// declared entries cross in either direction.
///
/// Guest mode (serve loop only): the shipped text keeps its `REMOTE`
/// wrapper so flagged `COPY` declarations validate, and the block body
/// executes locally through the standard scoped machinery. Header
/// resolution and transfer collection are skipped: injections already ran
/// as leading steps, and the session fulfilled the declarations.
pub(super) fn run_remote_block<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    target: &str,
    vars: &[String],
    env_names: &[String],
    body: &[Step],
    idx: usize,
) -> Result<()> {
    let _ = idx;
    if cx.state.io.is_remote_guest() {
        return super::steps::execute_scoped_remote_body(
            cx.state,
            cx.process,
            body,
            cx.stdin.clone(),
            cx.expose_stdin,
            cx.out.clone(),
            cx.err.clone(),
        );
    }
    let runner = cx.state.io.remote_runner(target).ok_or_else(|| {
        anyhow::anyhow!(
            "REMOTE '{target}' has no registered session (bind it with --remote {target}=\"<command>\")"
        )
    })?;
    let mut script_text = resolve_header(cx, target, vars, env_names)?.join("\n");
    if !script_text.is_empty() {
        script_text.push('\n');
    }
    // Ship the whole REMOTE step, wrapper included: the guest re-parses
    // flagged COPY declarations in place (placement validation passes) and
    // executes the body locally in guest mode. Shipping bare body steps
    // would strand the declarations at guest top level.
    let wrapped = oxdock_parser::StepKind::RemoteBlock {
        target: target.to_string(),
        vars: vars.to_vec(),
        env: env_names.to_vec(),
        body: body.to_vec(),
    };
    script_text.push_str(&wrapped.to_string());
    script_text.push('\n');
    let raw_decls = collect_transfer_decls(body);
    let mut fetch_decls = Vec::new();
    let mut push_decls = Vec::new();
    for decl in &raw_decls {
        let from_resolved = super::args::resolve_arg(&decl.from, cx)?;
        let to_resolved = super::args::resolve_arg(&decl.to, cx)?;
        if decl.from_host {
            // `COPY --from-host <host-path> <guest-path>`.
            fetch_decls.push(ResolvedFetch {
                host_rel: from_resolved,
                guest_rel: to_resolved,
                from_workspace: decl.from_workspace.clone(),
            });
        } else {
            // `COPY --to-host <guest-path> <host-path>`: stored
            // guest-source first for the runner.
            push_decls.push((from_resolved, to_resolved));
        }
    }
    let (fetch_files, fetch_dirs) = resolve_fetch_entries(cx, &fetch_decls)?;
    // Stderr streams live when the step error handle is a shared writer;
    // otherwise the runner returns all stderr bytes in the response.
    let stderr_sink = cx.err_shared();
    let request = RemoteRequest {
        target: target.to_string(),
        script_text,
        fetch_files,
        fetch_dirs,
        push_decls,
        stdin: cx.stdin_pipe.clone(),
        stdout: cx.out_pipe.clone(),
        stderr_sink,
        cancelled: Arc::clone(&cx.state.cancel_token),
    };
    // Host wins on partition: transport errors fail the step here and no
    // partial guest state is ever applied.
    let response = runner
        .run_remote(request)
        .with_context(|| format!("REMOTE '{target}' transport failed"))?;
    route_stdio(
        cx,
        cx.out_pipe.clone(),
        &response.stdout_bytes,
        &response.stderr_bytes,
    )?;
    apply_push_files(cx, &response.push_files)?;
    let root = cx.state.fs.root().clone();
    for dir in &response.push_dirs {
        if dir.is_empty() {
            continue;
        }
        let path = root.join(dir)?;
        cx.state.fs.create_dir_all(&path)?;
    }
    apply_push_symlinks(cx, &response.push_symlinks)?;
    Ok(())
}

/// Materialize declared push symlinks: each raw target must resolve inside the
/// active root (relative targets join the link parent; absolute targets
/// must already sit under the root), else the session aborts. Existing
/// destinations are replaced through guarded remove plus create.
fn apply_push_symlinks<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    symlinks: &[(String, String)],
) -> Result<()> {
    let root = cx.state.fs.root().clone();
    for (rel, target) in symlinks {
        let dst = root.join(rel)?;
        // Absolute raw targets must already sit under the root; relative
        // targets join the link parent. String-prefix containment keeps
        // this lexical (no filesystem probes on attacker bytes).
        let resolved = if target.starts_with('/') {
            target.clone()
        } else if let Some(parent) = rel.rfind('/') {
            format!("{}/{target}", &rel[..parent])
        } else {
            target.clone()
        };
        // Containment first: normalize `..` lexically and require the
        // result to stay under the root. Anything else aborts the sync.
        if !contained_rel(&resolved) {
            bail!("REMOTE sync-back symlink {rel:?} points outside the workspace");
        }
        let _ = root.join(&resolved)?;
        match cx.state.fs.entry_kind_no_follow(&dst) {
            Ok(EntryKind::Dir) => {
                bail!("REMOTE sync-back cannot replace directory {rel:?} with a symlink");
            }
            Ok(_) => {
                cx.state.fs.remove_file(&dst)?;
            }
            Err(_) => {}
        }
        if let Some(parent) = parent_rel(rel) {
            let dir = root.join(&parent)?;
            cx.state.fs.create_dir_all(&dir)?;
        }
        let src = root.join(&resolved)?;
        cx.state.fs.symlink(&src, &dst)?;
    }
    Ok(())
}

/// Lexical containment for a relative path: no `..` above the root, no
/// absolute paths, no empty components. Mirrors the tarball sanitizer in
/// `oxdock-remote-proto` so both ends enforce the identical boundary.
fn contained_rel(rel: &str) -> bool {
    if rel.is_empty() || rel.starts_with('/') {
        return false;
    }
    let mut depth = 0i32;
    for part in rel.split('/') {
        if part.is_empty() || part == "." {
            return false;
        }
        if part == ".." {
            depth -= 1;
        } else {
            depth += 1;
        }
        if depth < 0 {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_round_trips_hostile_strings() {
        for raw in [
            "plain",
            "quote \" inside",
            "back\\slash",
            "line\nbreak",
            "return\r carriage",
            "tab\there",
            "template {{ $x }} here",
            "semi; brace} hash#",
            "\\{{ already }}",
        ] {
            let rendered = render_quoted(raw);
            assert!(rendered.starts_with('"') && rendered.ends_with('"'));
            let inner = &rendered[1..rendered.len() - 1];
            // Every template pair must carry its escape backslash.
            let stripped = inner.replace("\\{{", "");
            assert!(
                !stripped.contains("{{"),
                "unescaped template pair in {rendered:?}"
            );
        }
    }

    #[test]
    fn escape_table_inverts_expand_string() {
        // Every escape the renderer emits must be one the runtime decodes.
        let rendered = escape_dsl_string("a\"b\\c\nd\re\tf{{g}}");
        assert_eq!(rendered, "a\\\"b\\\\c\\nd\\re\\tf\\{{g}}");
    }

    #[test]
    fn render_float_always_carries_a_point() {
        assert_eq!(render_float(3.0).unwrap(), "3.0");
        assert!(render_float(f64::NAN).is_err());
        assert!(render_float(f64::INFINITY).is_err());
    }

    #[test]
    fn render_rejects_handles_and_paths() {
        for value in [
            Value::pipe_fresh(),
            Value::handle(7),
            Value::path("/tmp/x".into()),
        ] {
            assert!(render_dsl_literal(&value).is_err());
        }
    }

    #[test]
    fn render_scalars_and_composites() {
        assert_eq!(render_dsl_literal(&Value::int(7)).unwrap(), "7");
        assert_eq!(
            render_dsl_literal(&Value::string("hi".to_string())).unwrap(),
            "\"hi\""
        );
        assert_eq!(render_dsl_literal(&Value::bool(true)).unwrap(), "true");
        let list = Value::list(vec![Value::int(1), Value::string("a\"b".to_string())]);
        assert_eq!(render_dsl_literal(&list).unwrap(), "[1, \"a\\\"b\"]");
        let mut entries = BTreeMap::new();
        entries.insert("k".to_string(), Value::int(2));
        assert_eq!(
            render_dsl_literal(&Value::map(entries)).unwrap(),
            "{\"k\": 2}"
        );
    }
}
