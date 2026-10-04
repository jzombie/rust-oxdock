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
//! - Files cross as declared path pairs resolved against the active root:
//!   the runner streams bytes straight from disk with O(chunk) memory, so
//!   no transfer size cap exists anywhere. Undeclared guest writes never
//!   cross; guest deletions affect nothing on the host.
//! - Bytes cross through `WITH_IO` backends passed live to the runner.
//! - The host never switches workspace selection for a remote block.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use oxdock_fs::{EntryKind, GuardedPath};
use oxdock_parser::{Step, render_dsl_literal, render_quoted, render_structural};
use oxdock_pipe::PipeInner;
use oxdock_process::ProcessManager;

use super::io::write_stdout;
use super::steps::StepCtx;

/// Cap on path entries collected for one declared transfer. The walk
/// records names only (never bytes), so this guards metadata bombs, not
/// payload size: legit bulk movement rides explicit repeated blocks.
const MAX_TRANSFER_ENTRIES: usize = 100_000;

/// Collected workspace entries for one declared transfer: guest-dest
/// names plus host guarded paths. The runner opens readers lazily while
/// packing, so collection itself is metadata-only.
type FetchedEntries = (Vec<(String, GuardedPath)>, Vec<String>);

/// Acknowledge a flagged `COPY --from-host` / `--to-host` step at execution
/// time. `--from-host` entries were already fulfilled by the session
/// before execution, so they succeed without re-copying. `--to-host`
/// entries resolve their paths against live scope and append the evaluated
/// `(guest_src, host_dst)` pair to the execution manifest (plus the staged
/// sink when present): the guest packs exactly executed declarations, so
/// dead branches contribute nothing and dynamic paths (`$var`,
/// `{{ ... }}`) resolve normally. Outside guest mode these steps are
/// unreachable (the parser rejects flagged copies outside `REMOTE`
/// bodies, and hosts never execute `REMOTE` bodies), so reaching here
/// bails defensively.
pub(super) fn copy_transfer<P: ProcessManager>(
    cx: &mut StepCtx<'_, P>,
    from_host: bool,
    to_host: bool,
    from: &oxdock_parser::Arg,
    to: &oxdock_parser::Arg,
) -> Result<()> {
    if !cx.state.io.is_remote_guest() {
        bail!("COPY --from-host and --to-host execute only inside a remote guest session");
    }
    if to_host {
        // Resolve now, against live scope: the manifest records what ran,
        // not what was written. Dynamic paths evaluate normally here.
        let guest_src = super::args::resolve_arg(from, cx)?;
        let host_dst = super::args::resolve_arg(to, cx)?;
        cx.state
            .push_manifest
            .push((guest_src.clone(), host_dst.clone()));
        if let Some(sink) = cx.state.io.push_manifest_sink()
            && let Ok(mut guard) = sink.lock()
        {
            guard.push((guest_src, host_dst));
        }
    }
    let _ = from_host;
    Ok(())
}

/// One collected transfer declaration: direction flags plus cloned paths.
/// Resolution against live scope happens at interception.
struct TransferDecl {
    from_host: bool,
    /// False under guards or control flow: missing sources skip silently
    /// (the guest fails loudly with the path if actually executed).
    unconditional: bool,
    from: oxdock_parser::Arg,
    to: oxdock_parser::Arg,
    from_workspace: Option<oxdock_parser::WorkspaceTarget>,
}

/// Staging for one transfer tarball: memory below, spill file on disk
/// above. Only [`TransferStage::new_mem`] and [`TransferStage::new_spill`]
/// construct: native builds spill through a guarded temp file past 8 MiB,
/// Miri/unix-minimal builds stay memory-only (transports never run there;
/// mocks use tiny payloads). Either way the caller sees streaming `Read`
/// + `Write` + rewind with O(chunk) memory.
pub enum TransferStage {
    Mem(std::io::Cursor<Vec<u8>>),
    #[cfg(not(miri))]
    File(oxdock_fs::SpillFile),
}

impl TransferStage {
    /// Memory-backed stage (tests, Miri, tiny control frames).
    pub fn new_mem() -> Self {
        Self::Mem(std::io::Cursor::new(Vec::new()))
    }

    /// Disk-backed stage under a guarded tempdir. Created through the
    /// resolver so access checks apply; the caller owns cleanup by
    /// dropping the tempdir after the transfer completes.
    #[cfg(not(miri))]
    pub fn new_spill(
        resolver: &oxdock_fs::PathResolver,
        dir: &GuardedPath,
        name: &str,
    ) -> Result<Self> {
        let path = dir.join(name)?;
        Ok(Self::File(resolver.create_spill_file(&path)?))
    }

    /// Rewind to the start for the read-back pass.
    pub fn rewind(&mut self) -> Result<()> {
        let result: std::io::Result<()> = match self {
            Self::Mem(cursor) => {
                cursor.set_position(0);
                Ok(())
            }
            #[cfg(not(miri))]
            Self::File(file) => {
                use std::io::Seek;
                file.seek(std::io::SeekFrom::Start(0)).map(|_| ())
            }
        };
        result.map_err(|err| anyhow::anyhow!("transfer stage rewind failed: {err:#}"))
    }
}

impl std::io::Read for TransferStage {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Mem(cursor) => cursor.read(buf),
            #[cfg(not(miri))]
            Self::File(file) => file.read(buf),
        }
    }
}

impl std::io::Write for TransferStage {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Mem(cursor) => cursor.write(buf),
            #[cfg(not(miri))]
            Self::File(file) => file.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Mem(cursor) => cursor.flush(),
            #[cfg(not(miri))]
            Self::File(file) => file.flush(),
        }
    }
}

/// Collect `COPY --from-host` / `--to-host` declarations transitively from
/// a `REMOTE` body. Args clone (declarations are few and parse-time): this
/// keeps the walker borrow-simple, matching the `contains_*` passes.
/// Each declaration records whether its position is unconditional
/// (top-level, unguarded): unconditional fetches fail fast on missing
/// sources, while conditional ones (guards, `IF`/`FOR`/`WHILE` bodies)
/// resolve best-effort so dead branches with absent files never abort the
/// block (the guest fails loudly with the path if actually executed).
/// Statically dead positions (`[bool:false]`, `IF false`) contribute
/// nothing at all.
fn collect_transfer_decls(body: &[Step]) -> Vec<TransferDecl> {
    let mut out = Vec::new();
    fn visit(
        kind: &oxdock_parser::StepKind,
        guarded: bool,
        dead: bool,
        out: &mut Vec<TransferDecl>,
    ) {
        if dead {
            return;
        }
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
                unconditional: !guarded,
                from: from.clone(),
                to: to.clone(),
                from_workspace: from_workspace.clone(),
            });
        }
        // Control flow nests conditionally; a constant-false position
        // prunes the whole subtree. Guards compose: an inner static-false
        // under any outer guard stays dead.
        match kind {
            oxdock_parser::StepKind::If {
                cond,
                then_body,
                else_ifs,
                else_body,
                ..
            } => {
                let cond_dead = matches!(
                    cond.as_ref(),
                    oxdock_parser::Expr::Literal(value) if value.as_bool() == Some(false)
                );
                for step in then_body {
                    visit_step(step, true, dead || cond_dead, out);
                }
                for (cond, body) in else_ifs {
                    let branch_dead = matches!(
                        cond.as_ref(),
                        oxdock_parser::Expr::Literal(value) if value.as_bool() == Some(false)
                    );
                    for step in body {
                        visit_step(step, true, dead || branch_dead, out);
                    }
                }
                if let Some(body) = else_body {
                    for step in body {
                        visit_step(step, true, dead, out);
                    }
                }
            }
            _ => kind.walk_child_kinds(&mut |child| {
                visit(child, true, dead, out);
            }),
        }
    }
    fn visit_step(step: &Step, guarded: bool, dead: bool, out: &mut Vec<TransferDecl>) {
        visit(
            &step.kind,
            guarded || step.guard.is_some() || statically_false_guard(step.guard.as_ref()),
            dead,
            out,
        );
    }
    for step in body {
        visit_step(step, false, false, &mut out);
    }
    out
}

/// True for guards that can never pass regardless of environment:
/// `[bool:false]` (alone or conjunctively). Disjunctive and negated
/// forms stay conservative (collected, best-effort at resolve time).
fn statically_false_guard(guard: Option<&oxdock_parser::GuardExpr>) -> bool {
    use oxdock_parser::{Guard, GuardExpr};
    fn is_false(expr: &GuardExpr) -> bool {
        match expr {
            GuardExpr::Predicate(Guard::Attr {
                ns: oxdock_parser::Ns::Bool,
                val: Some(value),
                ..
            }) => !value.parse::<bool>().unwrap_or(true),
            GuardExpr::Predicate(_) => false,
            GuardExpr::All(children) => children.iter().any(is_false),
            GuardExpr::Or(_) => false,
            GuardExpr::Not(_) => false,
        }
    }
    guard.is_some_and(is_false)
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
    /// Declared `--from-host` entries as `(guest_rel, host GuardedPath)`,
    /// resolved by core against the host active root under guard. Paths
    /// only: the runner opens readers and streams straight to the wire,
    /// so collection itself is metadata-only.
    pub fetch_files: Vec<(String, GuardedPath)>,
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

/// What a [`RemoteRunner`] returns: stdio bytes only. Declared pushes are
/// applied directly against the passed filesystem by the runner, so no
/// file payload ever crosses this boundary in either direction.
pub struct RemoteResponse {
    /// Guest stdout bytes, routed to the backend or step stream by core.
    pub stdout_bytes: Vec<u8>,
    /// Guest stderr bytes, routed to the step error stream by core.
    pub stderr_bytes: Vec<u8>,
}

/// Transport behind a `REMOTE` block: SSH plus framing in production, an
/// in-process mock in tests. Object safe so runners stage through
/// [`ExecIo`](super::ExecIo) as `Arc<dyn RemoteRunner>`. The filesystem
/// handle lets the runner stream declared transfers straight between
/// disk and wire with O(chunk) memory; core never holds file bytes.
pub trait RemoteRunner: Send + Sync {
    /// Execute rendered script text with the given declarations and stdio
    /// backends, applying declared pushes to `fs`. Errors fail the
    /// `REMOTE` step on the host (host wins).
    fn run_remote(
        &self,
        request: RemoteRequest,
        fs: &dyn oxdock_fs::WorkspaceFs,
    ) -> Result<RemoteResponse>;
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
        // The declared type renders structurally, never as the coarse
        // word: the guest re-parses these lines under the same rules,
        // so a coarse word the guest rejects would break the seal.
        lines.push(format!(
            "LET ${name}: {} = {literal}",
            render_structural(&decl_type)
        ));
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
    /// False under guards or control flow: resolve best-effort (skip
    /// missing sources) instead of failing fast.
    unconditional: bool,
}

/// Resolve declared `--from-host` entries against the host side: each
/// source becomes a `(guest_rel, host GuardedPath)` pair, following
/// Docker destination semantics (file onto a trailing-slash dest lands
/// under its basename; directory sources fan their contents out under the
/// dest). Paths only, never bytes: the runner opens readers and streams
/// straight to the wire with O(chunk) memory. Missing sources fail fast
/// before any session byte ships (unconditional) or skip silently
/// (conditional dead branches; the guest fails loudly if executed).
/// Symlinks resolve through to content, exactly like local `COPY`
/// (`entry_kind` follows): the guest receives plain files.
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
    files: &mut Vec<(String, GuardedPath)>,
    dirs: &mut Vec<String>,
) -> Result<()> {
    let source = match &decl.from_workspace {
        Some(target) => cx.state.fs.resolve_copy_source_from_target(
            super::handlers::copy_source_root(target.clone()),
            &decl.host_rel,
        ),
        None => cx.state.fs.resolve_copy_source(&decl.host_rel),
    };
    let source = match source {
        Ok(source) => source,
        // Conditional declarations under guards or control flow resolve
        // best-effort: a missing source in a dead branch must not abort
        // the block. The guest fails loudly with the path if the branch
        // actually executes. Unconditional declarations fail fast here.
        Err(_) if !decl.unconditional => return Ok(()),
        Err(_) => {
            bail!("REMOTE fetch source missing: {}", decl.host_rel);
        }
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
                            files.push((child_rel, child));
                        }
                    }
                    if files.len() + dirs.len() > MAX_TRANSFER_ENTRIES {
                        bail!(
                            "REMOTE fetch hit the {MAX_TRANSFER_ENTRIES} entry cap: {} names too many paths",
                            decl.host_rel
                        );
                    }
                }
            }
        }
        Ok(_) => {
            // File source: trailing-slash dest duplicates inside under the
            // basename, mirroring `place_file_in_dir`.
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
            files.push((dest, source));
        }
        Err(err) => {
            bail!("REMOTE fetch source missing: {} ({err:#})", decl.host_rel);
        }
    }
    Ok(())
}

/// Apply one declared push file under the active root through guarded
/// writes, streaming bytes from the reader with O(chunk) memory and
/// creating parent directories as needed. Shared by runners (real
/// sessions) and mocks through `&dyn WorkspaceFs`.
pub fn apply_push_file(
    fs: &dyn oxdock_fs::WorkspaceFs,
    root: &GuardedPath,
    rel: &str,
    reader: &mut dyn Read,
) -> Result<()> {
    let path = root.join(rel)?;
    if let Some(parent) = parent_rel(rel) {
        let dir = root.join(&parent)?;
        fs.create_dir_all(&dir)?;
    }
    let mut writer = fs.open_write(&path)?;
    let mut buf = [0u8; 65536];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
    }
    writer.flush()?;
    Ok(())
}

/// Apply one declared push directory under the active root.
pub fn apply_push_dir(
    fs: &dyn oxdock_fs::WorkspaceFs,
    root: &GuardedPath,
    rel: &str,
) -> Result<()> {
    if rel.is_empty() {
        return Ok(());
    }
    let path = root.join(rel)?;
    fs.create_dir_all(&path)?;
    Ok(())
}

fn parent_rel(rel: &str) -> Option<String> {
    rel.rfind('/').map(|idx| rel[..idx].to_string())
}

/// Route guest stdout bytes: into the `WITH_IO` backend when present,
/// else onto the step output stream. Guest stderr bytes always ride the
/// step error stream.
///
/// No explicit close here by design: the backend belongs to the
/// redirection scope, which concurrent REMOTE steps can share, so the
/// first finisher must not close it for the others. End of stream comes
/// from writer and keeper detach (session chunks are transient, task
/// workers hold keepers, redirection writers drop with their step), which
/// closes exactly when the last producer is gone.
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
            cx.out_pipe.clone(),
            cx.stdin_pipe.clone(),
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
                unconditional: decl.unconditional,
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
    // partial guest state is ever applied. The runner applies declared
    // pushes directly against the active filesystem.
    let fs = cx.state.fs.as_ref();
    let response = runner
        .run_remote(request, fs)
        .with_context(|| format!("REMOTE '{target}' transport failed"))?;
    route_stdio(
        cx,
        cx.out_pipe.clone(),
        &response.stdout_bytes,
        &response.stderr_bytes,
    )?;
    Ok(())
}

/// Materialize one declared push symlink under the active root: both the
/// link path and the resolved target route through `GuardedPath`
/// containment, and any escape aborts the session. Relative targets join
/// the link parent; absolute targets must already sit under the root.
/// Existing destinations are replaced through guarded remove plus create,
/// never when the destination is a real directory. Shared by runners
/// (real sessions) and mocks through `&dyn WorkspaceFs`.
pub fn apply_push_symlink(
    fs: &dyn oxdock_fs::WorkspaceFs,
    root: &GuardedPath,
    rel: &str,
    target: &str,
) -> Result<()> {
    let dst = root.join(rel).map_err(|err| {
        anyhow::anyhow!("REMOTE push symlink destination {rel:?} escapes the workspace: {err:#}")
    })?;
    // Join-then-guard is the whole verdict: `GuardedPath::join`
    // normalizes `..` lexically and rejects escapes, so chains like
    // `a/b/c/d/../../../../etc/passwd` fail here instead of passing a
    // hand-rolled depth counter.
    let src = if target.starts_with('/') {
        root.join(target).map_err(|err| {
            anyhow::anyhow!("REMOTE push symlink {rel:?} points outside the workspace: {err:#}")
        })?
    } else if let Some(parent) = rel.rfind('/') {
        root.join(&format!("{}/{target}", &rel[..parent]))
            .map_err(|err| {
                anyhow::anyhow!("REMOTE push symlink {rel:?} points outside the workspace: {err:#}")
            })?
    } else {
        root.join(target).map_err(|err| {
            anyhow::anyhow!("REMOTE push symlink {rel:?} points outside the workspace: {err:#}")
        })?
    };
    match fs.entry_kind_no_follow(&dst) {
        Ok(EntryKind::Dir) => {
            bail!("REMOTE sync-back cannot replace directory {rel:?} with a symlink");
        }
        Ok(_) => {
            fs.remove_file(&dst)?;
        }
        Err(_) => {}
    }
    if let Some(parent) = parent_rel(rel) {
        let dir = root.join(&parent)?;
        fs.create_dir_all(&dir)?;
    }
    fs.symlink(&src, &dst)?;
    Ok(())
}
