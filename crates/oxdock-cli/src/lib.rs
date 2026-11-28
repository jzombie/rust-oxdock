use anyhow::{Result, bail};
use oxdock_fs::{
    GuardedPath, LazyGuardedTempDir, PathResolver, WorkspaceFs, discover_workspace_root,
    env as oxdock_env, init_temp_gc,
};
#[cfg(windows)]
use oxdock_process::CommandBuilder;
use oxdock_process::{DefaultProcessManager, SharedInput};
use std::env;
use std::io::{self, IsTerminal, Read};
use std::sync::{Arc, Mutex};

use oxdock_core::ExecIo;
pub use oxdock_core::{
    Engine, EngineOutput, ExecState, ExitRequest, FuncKind, FuncMeta, FuncParam, HostModule,
    HostRegistration, NativeFn, OxDockFn, OxDockType, PureFn, PushManifestSink, StepCtx,
    TypeDescriptor, Value, dry_run_closure, exit_code_of, parse_script, parse_script_with_modules,
    run_steps, run_steps_with_context, run_steps_with_context_result,
    run_steps_with_lazy_snapshot_and_modules_and_entry, run_steps_with_manager_with_modules,
    run_steps_with_manager_with_modules_and_entry, run_steps_without_snapshot,
};
pub use oxdock_parser::{Guard, Step, StepKind};
pub use oxdock_process::shell_program;
use std::collections::BTreeMap;

mod endpoints;
pub use endpoints::EndpointFlags;
mod errors;
pub use errors::{
    endpoint_duplicate_inner, endpoint_duplicate_outer, feature_required, offline_conflict, usage,
};
#[cfg(feature = "mcp")]
mod mcp;

// CLI usage bodies live in [`errors`]: `usage()` is user-facing CLI
// text, so it is owned alongside the `cli.*` error templates.
#[cfg(feature = "net")]
pub use endpoints::build_registry;
#[cfg(feature = "net")]
use oxdock_net_plugin::EndpointRegistry;
#[cfg(feature = "net")]
mod serve;

/// Host modules bundled into the CLI runner. With `net` this exposes STD
/// plus the NET virtual-endpoint toolkit (`NET_LISTEN`, `NET_ACCEPT`,
/// `NET_CLOSE`, `NET_CONNECT`, `NET_PORT`, `NET_ADDR`); with `--features
/// ssh` it additionally registers the SSH server and client (`SSH_SERVE`,
/// `SSH_ACCEPT`, `SSH_DEQUEUE`, `SSH_PUMP_CHANNEL`, `SSH_CLOSE`,
/// `SSH_CONNECT`, `SSH_PUMP`) from oxdock-ssh-plugin; with `--features
/// markdown` it registers the MARKDOWN helpers (`MAP_TO_MD_TABLE`,
/// `TOC`, `PARSE`, `SECTION`). Without features only STD builtins remain.
fn cli_host_modules() -> Vec<HostModule<DefaultProcessManager>> {
    cli_host_modules_with(None)
}

/// Module surface for the guest serve loop: identical to the host runner
/// surface so handshake language-surface identity actually compares the
/// same tables both ends run.
#[cfg(feature = "net")]
fn cli_host_modules_for_serve() -> Vec<HostModule<DefaultProcessManager>> {
    cli_host_modules_with(None)
}

/// Module-table digest for the remote handshake: sha256 over sorted
/// `module::function` entries from the serve surface.
#[cfg(feature = "net")]
fn remote_session_module_hash(entries: &[(String, Vec<String>)]) -> String {
    oxdock_net_plugin::module_hash(entries)
}

/// Host modules resolving virtual endpoints through `registry`: the CLI
/// builds it from `--listen`/`-p`/`--offline` before parsing so bind
/// conflicts fail fast. `None` registers default module instances (enough
/// for parse-time name resolution); without `net` no modules are pushed.
/// The parameter type is feature-dependent because the registry type only
/// exists with `net`; callers pass `Some` with `net` and `None` without.
fn cli_host_modules_with(
    #[cfg(feature = "net")] registry: Option<&Arc<EndpointRegistry>>,
    #[cfg(not(feature = "net"))] registry: Option<&Arc<()>>,
) -> Vec<HostModule<DefaultProcessManager>> {
    #[cfg(any(feature = "net", feature = "markdown"))]
    let mut modules: Vec<HostModule<DefaultProcessManager>> = Vec::new();
    #[cfg(not(any(feature = "net", feature = "markdown")))]
    let modules: Vec<HostModule<DefaultProcessManager>> = Vec::new();
    #[cfg(feature = "net")]
    {
        let net_module = match registry {
            Some(registry) => oxdock_net_plugin::module_with_endpoints(Arc::clone(registry)),
            None => oxdock_net_plugin::module(),
        };
        modules.push(net_module);
        #[cfg(feature = "ssh")]
        {
            let ssh_module = match registry {
                Some(registry) => oxdock_ssh_plugin::module_with_endpoints(Arc::clone(registry)),
                None => oxdock_ssh_plugin::module(),
            };
            modules.push(ssh_module);
        }
    }
    #[cfg(feature = "markdown")]
    modules.push(oxdock_markdown_plugin::module());
    let _ = registry;
    modules
}

/// Host types bundled into the CLI runner alongside [`cli_host_modules`].
fn cli_host_types() -> Vec<&'static TypeDescriptor> {
    #[cfg(feature = "net")]
    let net: &[&'static TypeDescriptor] = &[oxdock_net_plugin::NetListenerTag::descriptor()];
    #[cfg(not(feature = "net"))]
    let net: &[&'static TypeDescriptor] = &[];
    #[cfg(feature = "ssh")]
    let ssh: &[&'static TypeDescriptor] = &[
        oxdock_ssh_plugin::SshServerTag::descriptor(),
        oxdock_ssh_plugin::SshSessionTag::descriptor(),
    ];
    #[cfg(not(feature = "ssh"))]
    let ssh: &[&'static TypeDescriptor] = &[];
    net.iter().chain(ssh.iter()).copied().collect()
}

/// Run parsed steps with the CLI host surface under a lazy snapshot:
/// one place where the CLI module set, types, and entry file meet the
/// lazy-snapshot runner, so the net/non-net module wiring lives here
/// instead of at every call site. Callers differ only in IO setup
/// (stdin handling, remote runners) and what they do with the output.
fn run_cli_steps(
    workspace_root: &GuardedPath,
    steps: &[Step],
    io: ExecIo,
    #[cfg(feature = "net")] registry: Option<&Arc<EndpointRegistry>>,
    #[cfg(not(feature = "net"))] registry: Option<&Arc<()>>,
    entry_file: Option<GuardedPath>,
) -> Result<oxdock_core::exec::LazyRunOutput> {
    Ok(run_steps_with_lazy_snapshot_and_modules_and_entry(
        workspace_root,
        steps,
        io,
        cli_host_modules_with(registry),
        cli_host_types(),
        entry_file,
    )?)
}

/// Parse a CLI script against STD plus any bundled host modules. Without
/// extra modules this is exactly `parse_script`, so base-build behavior
/// never changes.
fn parse_cli_script(script: &str) -> Result<Vec<Step>> {
    parse_script_with_modules(script, cli_module_table()).map_err(|e| {
        if let Some(unlinked) = unlinked_module_error(script) {
            return unlinked;
        }
        e
    })
}

/// Modules the CLI knows but this build did not link, with the Cargo
/// feature that would link each. Empty in full-feature builds, so the
/// scan below never fires there.
fn unlinked_modules() -> Vec<(&'static str, &'static str)> {
    let modules: &[(&'static str, &'static str)] = &[
        #[cfg(not(feature = "net"))]
        ("NET", "net"),
        #[cfg(not(feature = "ssh"))]
        ("SSH", "ssh"),
    ];
    modules.to_vec()
}

/// Known-but-unlinked module error when the script actually imports one.
/// Matched on the lexed token stream, not raw text: comments lex as
/// `Comment` and quoted paths as `Str`, so `// IMPORT [NET]` or
/// `ECHO "IMPORT [NET]"` alongside an unrelated syntax error keeps its
/// genuine parse error. Returns `None` on lex failure (broken below the
/// token level: the genuine error rides through), in full-feature
/// builds (nothing is unlinked), or when no unlinked module is
/// imported (genuine parse error rides through unchanged).
fn unlinked_module_error(script: &str) -> Option<anyhow::Error> {
    let unlinked = unlinked_modules();
    if unlinked.is_empty() {
        return None;
    }
    let toks = oxdock_parser::lex_str(script).ok()?;
    for (module, feature) in unlinked {
        if imports_module(&toks, module) {
            return Some(errors::module_unlinked(module, feature));
        }
    }
    None
}

/// True when a token stream holds `IMPORT [...]` naming `module` as a
/// bare word. Only `IMPORT` immediately followed by a bracket group
/// counts (newlines/comments between are skipped); quoted file paths
/// lex as `Str`, never `Word`, so they never match. Recurses into
/// brace/paren groups for block-nested imports.
fn imports_module(toks: &[oxdock_parser::Tok], module: &str) -> bool {
    use oxdock_parser::{GroupDelim, TokKind};
    let mut i = 0;
    while i < toks.len() {
        match &toks[i].kind {
            TokKind::Word(w) if w == "IMPORT" => {
                let mut j = i + 1;
                while j < toks.len()
                    && matches!(
                        toks[j].kind,
                        TokKind::Newline | TokKind::Comment(_) | TokKind::Semicolon
                    )
                {
                    j += 1;
                }
                if let Some(next) = toks.get(j)
                    && let TokKind::Group {
                        delim: GroupDelim::Bracket,
                        inner,
                        ..
                    } = &next.kind
                    && inner
                        .iter()
                        .any(|t| matches!(&t.kind, TokKind::Word(w) if w == module))
                {
                    return true;
                }
            }
            TokKind::Group { inner, .. } if imports_module(inner, module) => {
                return true;
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Module table for CLI parsing: STD plus bundled host modules.
/// Imported files (#115) parse against this same table.
fn cli_module_table() -> oxdock_parser::ModuleTable {
    let modules = cli_host_modules();
    if modules.is_empty() {
        return oxdock_core::std_module_table();
    }
    let mut engine = Engine::new();
    for module in modules {
        engine.register_module(module);
    }
    engine.module_table()
}

/// Entry script file for `#115` imports, if the run started from one:
/// top-level relative `IMPORT` paths resolve against its parent.
fn entry_file_of(script: &ScriptSource) -> Option<GuardedPath> {
    match script {
        ScriptSource::Path(path) => Some(path.clone()),
        ScriptSource::Stdin => None,
    }
}

/// Print the transitive import closure (`file:line CMD`, #115 preview)
/// instead of executing. Pure parser + loader: no handlers run, no
/// processes spawn, no FS writes. Validates first through the shared
/// run boundary so dry-run fails exactly where execution and `--mcp`
/// would.
fn print_dry_run(
    steps: &[Step],
    script: &ScriptSource,
    workspace_root: &GuardedPath,
) -> Result<()> {
    let table = cli_module_table();
    let entry_file = entry_file_of(script);
    // Shared validation before any listing: same closure check as
    // execution and MCP init, with default module instances (no socket
    // binds, preserving the preview contract).
    {
        let resolver = PathResolver::new_lazy(workspace_root.clone())?;
        let fs: Box<dyn WorkspaceFs> = Box::new(resolver);
        oxdock_core::validate_steps_with_fs_modules_and_entry(
            fs,
            steps,
            cli_host_modules_with(None),
            cli_host_types(),
            entry_file.clone(),
        )?;
    }
    let (label, base) = match script {
        ScriptSource::Path(path) => (
            path.display(),
            path.parent().unwrap_or_else(|| workspace_root.clone()),
        ),
        ScriptSource::Stdin => ("<stdin>".to_string(), workspace_root.clone()),
    };
    let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())?;
    let listing = dry_run_closure(steps, &label, &base, &resolver, &table)?;
    for item in listing {
        println!("{}:{} {}", item.file, item.step.line, item.step.kind);
    }
    Ok(())
}

pub fn run() -> Result<()> {
    init_temp_gc();
    let workspace_root = discover_workspace_root()
        .map_err(|e| errors::guard_workspace_root(&anyhow::Error::from(e)))?;

    let mut args = std::env::args().skip(1);
    // `--help`/`-h` and `--version`/`-V` surface as text in the parse
    // error (parse must not exit the process itself: it is public
    // library API). Print it and succeed so the binary exits 0.
    let opts = match Options::parse(&mut args, &workspace_root) {
        Ok(opts) => opts,
        Err(err) if err.to_string() == version() => {
            println!("{err}");
            return Ok(());
        }
        Err(err) if err.to_string() == usage() => {
            print!("{err}");
            return Ok(());
        }
        Err(err) => return Err(err),
    };
    // Guest serve mode short-circuits everything: no script, no snapshot,
    // no shell. Binary stdio contract enforced inside.
    #[cfg(feature = "net")]
    if opts.remote_serve {
        if opts.shell
            || !opts.remotes.is_empty()
            || !matches!(opts.script, ScriptSource::Stdin)
            || !opts.script_args.is_empty()
        {
            return Err(errors::remote_serve_runs_alone());
        }
        return serve::serve();
    }
    #[cfg(not(feature = "net"))]
    if opts.remote_serve {
        return Err(errors::feature_required("--remote-serve", "net"));
    }
    // MCP tool hosting owns stdio like the guest serve loop: no script
    // execution past init, no shell, no remote serve alongside it.
    if opts.mcp {
        #[cfg(feature = "mcp")]
        {
            if opts.shell || opts.remote_serve || !opts.remotes.is_empty() {
                return Err(errors::mcp_runs_alone(&[
                    "--shell",
                    "--remote-serve",
                    "--remote",
                ]));
            }
            if !opts.endpoints.listens.is_empty() || !opts.endpoints.publishes.is_empty() {
                return Err(errors::mcp_runs_alone(&["--listen/-p"]));
            }
            if opts.dry_run {
                return Err(errors::mcp_runs_alone(&["--dry-run"]));
            }
            if !opts.script_args.is_empty() {
                return Err(errors::mcp_runs_alone(&["script args"]));
            }
            return mcp::serve_mcp(opts, workspace_root);
        }
        #[cfg(not(feature = "mcp"))]
        {
            return Err(errors::feature_required("--mcp", "mcp"));
        }
    }
    execute(opts, workspace_root)
}

/// Process exit status for a failed CLI run: relays an `EXIT` code (host
/// or guest-relayed) clamped to the portable 0..=255 range, 1 for every
/// other failure.
pub fn process_exit_code(err: &anyhow::Error) -> i32 {
    exit_code_of(err)
        .map(|code| code.clamp(0, 255) as i32)
        .unwrap_or(1)
}

#[derive(Debug, Clone)]
pub enum ScriptSource {
    Path(GuardedPath),
    Stdin,
}

/// URLs never resolve to workspace files: fail with the input intact
/// instead of letting normalization mangle the scheme. Present fact
/// about this argument, not a claim about imports.
fn reject_url_script(text: &str) -> Result<()> {
    if text.contains("://") {
        return Err(errors::script_not_local(text));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Options {
    pub script: ScriptSource,
    pub shell: bool,
    /// Print the transitive import closure (`file:line CMD`, #115) instead
    /// of executing. No handlers run, no processes spawn, no FS writes.
    pub dry_run: bool,
    pub endpoints: EndpointFlags,
    pub remote_serve: bool,
    /// Serve `EXPORT`ed FUNCs as MCP tools over stdio instead of running
    /// linear steps. Requires the `mcp` feature; see `--mcp`.
    pub mcp: bool,
    /// Raw `--remote <target>="<command>"` pairs, in flag order. Validated
    /// and tokenized at inventory build; empty in lean builds (flag bails).
    pub remotes: Vec<(String, String)>,
    /// Verbatim tail after the script position (#226): everything after
    /// the script (or after `--script <file>`), with one leading `--`
    /// stripped when present. Never option-parsed, so `--dry-run` after
    /// the script stays data. Surfaced script-side via `STD::ARGV()`.
    pub script_args: Vec<String>,
}

impl Options {
    pub fn parse(
        args: &mut impl Iterator<Item = String>,
        workspace_root: &GuardedPath,
    ) -> Result<Self> {
        use lexopt::Arg::{Long, Short, Value};

        let mut script: Option<ScriptSource> = None;
        let mut shell = false;
        let mut dry_run = false;
        let mut endpoints = EndpointFlags::default();
        let mut remote_serve = false;
        let mut mcp = false;
        // Pushed only under `net`; without it the binding never mutates.
        #[cfg_attr(not(feature = "net"), allow(unused_mut))]
        let mut remotes: Vec<(String, String)> = Vec::new();
        let mut script_args: Vec<String> = Vec::new();
        // Verbatim tail after the script is fixed (#226): drain lexopt's
        // remaining raw args without option parsing. When the script was
        // just fixed and the tail sits entirely in the raw stream, strip
        // one leading `--` separator. When `text` itself already holds
        // the tail head (`--` stdin marker or dash-leading first arg),
        // that separator (or first arg) is already accounted for, so the
        // remainder stays verbatim. Bare empties stay intact here: only
        // pre-script positionals skip them.
        let drain_tail =
            |parser: &mut lexopt::Parser, strip_leading: bool| -> Result<Vec<String>> {
                let mut raw = parser.raw_args().map_err(|e| errors::arg_lex_error(&e))?;
                let Some(first_os) = raw.next() else {
                    return Ok(Vec::new());
                };
                let first = value_string(first_os)?;
                if strip_leading && first == "--" {
                    raw.map(value_string).collect()
                } else {
                    std::iter::once(Ok(first))
                        .chain(raw.map(value_string))
                        .collect()
                }
            };
        // Pass the stream unfiltered: an explicit empty value (for
        // example `--script ""`) is a real token boundary, so stripping
        // empties up front would shift every following value. Bare empty
        // positionals are skipped in the `Value` arm instead.
        let mut parser = lexopt::Parser::from_args(args.by_ref());
        while let Some(arg) = parser.next()? {
            match arg {
                Long("script") => {
                    let path = value_string(
                        parser
                            .value()
                            .map_err(|_| errors::script_flag_needs_path())?,
                    )?;
                    if path.is_empty() {
                        return Err(errors::script_flag_needs_path());
                    }
                    if path == "-" {
                        script = Some(ScriptSource::Stdin);
                    } else {
                        reject_url_script(&path)?;
                        script = Some(ScriptSource::Path(
                            workspace_root.join_script(&path).map_err(|e| {
                                errors::guard_script_path(&path, &anyhow::Error::from(e))
                            })?,
                        ));
                    }
                    // After the script, stop option parsing entirely:
                    // everything else is verbatim script args.
                    script_args = drain_tail(&mut parser, true)?;
                    break;
                }
                Long("shell") => {
                    shell = true;
                }
                Long("dry-run") => {
                    dry_run = true;
                }
                Long("listen") => {
                    let raw = value_string(
                        parser
                            .value()
                            .map_err(|_| errors::listen_requires_address())?,
                    )?;
                    #[cfg(not(feature = "net"))]
                    {
                        // Evaluate the pure validator so it stays compiled
                        // and covered in lean builds; the feature error below
                        // still wins regardless of the value.
                        let _ = endpoints::parse_listen_arg(&raw);
                        return Err(errors::feature_required("--listen/-p", "net"));
                    }
                    #[cfg(feature = "net")]
                    endpoints.listens.push(endpoints::parse_listen_arg(&raw)?);
                }
                Short('p') => {
                    let raw = value_string(
                        parser
                            .value()
                            .map_err(|_| errors::publish_requires_shape(""))?,
                    )?;
                    #[cfg(not(feature = "net"))]
                    {
                        // Same as `--listen` above: keep the pure outer
                        // validation live in lean builds; the feature error
                        // below still wins.
                        let _ = endpoints::parse_publish_arg(&raw);
                        return Err(errors::feature_required("--listen/-p", "net"));
                    }
                    #[cfg(feature = "net")]
                    endpoints
                        .publishes
                        .push(endpoints::parse_publish_arg(&raw)?);
                }
                Long("offline") => {
                    endpoints.offline = true;
                }
                Long("remote-serve") => {
                    remote_serve = true;
                }
                Long("mcp") => {
                    mcp = true;
                }
                Long("remote") => {
                    let raw = value_string(
                        parser
                            .value()
                            .map_err(|_| errors::remote_requires_value())?,
                    )?;
                    #[cfg(not(feature = "net"))]
                    {
                        // Keep the pure validator live in lean builds; the
                        // feature error below still wins.
                        let _ = oxdock_net_plugin_stub_parse_remote(&raw);
                        return Err(errors::feature_required("--remote", "net"));
                    }
                    #[cfg(feature = "net")]
                    remotes.push(oxdock_net_plugin::parse_remote_arg(&raw)?);
                }
                Long("help") | Short('h') => {
                    bail!("{}", usage());
                }
                Long("version") | Short('V') | Short('v') => {
                    bail!("{}", version());
                }
                Value(value) => {
                    let text = value_string(value)?;
                    if text.is_empty() {
                        continue;
                    }
                    if text == "--" {
                        // Second `--` with no script yet: stdin script with
                        // the remainder as args (`oxdock -- -- foo`). The
                        // text itself is the one stripped separator.
                        script = Some(ScriptSource::Stdin);
                        script_args = drain_tail(&mut parser, false)?;
                        break;
                    }
                    if text == "-" {
                        script = Some(ScriptSource::Stdin);
                        script_args = drain_tail(&mut parser, true)?;
                        break;
                    }
                    if text.starts_with('-') {
                        // Dash-leading Value only surfaces after lexopt's
                        // `--` separator, so this is `oxdock -- --foo`:
                        // stdin script with dash-leading args, not a file
                        // named `--foo` (pass those via `--script`). The
                        // text is already the tail head, so the remainder
                        // stays verbatim to preserve inner `--` entries.
                        script = Some(ScriptSource::Stdin);
                        let mut tail = drain_tail(&mut parser, false)?;
                        tail.insert(0, text);
                        script_args = tail;
                        break;
                    }
                    reject_url_script(&text)?;
                    script = Some(ScriptSource::Path(
                        workspace_root.join_script(&text).map_err(|e| {
                            errors::guard_script_path(&text, &anyhow::Error::from(e))
                        })?,
                    ));
                    script_args = drain_tail(&mut parser, true)?;
                    break;
                }
                Long(other) => return Err(errors::unexpected_flag(other, true)),
                Short(other) => return Err(errors::unexpected_flag(&other.to_string(), false)),
            }
        }

        let script = script.unwrap_or(ScriptSource::Stdin);

        Ok(Self {
            script,
            shell,
            dry_run,
            endpoints,
            remote_serve,
            mcp,
            remotes,
            script_args,
        })
    }
}

/// Option/positional text out of a lexopt value. Inputs arrive as
/// `String`, so non-UTF8 is unreachable in practice; fail loudly anyway.
fn value_string(value: std::ffi::OsString) -> Result<String> {
    value.into_string().map_err(|_| errors::arg_not_utf8())
}

/// Human-readable CLI version, printed for `--version`/`-V`/`-v`.
pub fn version() -> String {
    format!("oxdock {}", env!("CARGO_PKG_VERSION"))
}

pub fn execute(opts: Options, workspace_root: GuardedPath) -> Result<()> {
    init_temp_gc();
    execute_with_shell_runner(opts, workspace_root, run_shell, true)
}

/// Output of a script execution (issue #131).
///
/// The snapshot directory is created lazily: [`ExecutionResult::snapshot`]
/// stays unmaterialized when the script never touches snapshot-rooted state
/// (e.g. `WORKSPACE LOCAL`-only or empty scripts), in which case
/// [`ExecutionResult::has_snapshot`] is false and `final_cwd` points under
/// the workspace root.
pub struct ExecutionResult {
    /// Shared ownership of the snapshot backing dir. The physical directory
    /// lives exactly as long as the last surviving clone (normally this
    /// struct, since execution-internal clones are dropped before return).
    pub snapshot: Arc<LazyGuardedTempDir>,
    /// Actual final cwd: inside the snapshot when materialized, otherwise
    /// under the workspace/local root.
    pub final_cwd: GuardedPath,
    /// Top-level script variable bindings captured at completion, keyed by
    /// variable name. Empty when the script is empty. Populated exclusively
    /// by [`execute_with_result`]; `--shell` runs never produce one.
    pub bindings: BTreeMap<String, Value>,
}

impl ExecutionResult {
    /// Whether the run materialized the snapshot tempdir.
    pub fn has_snapshot(&self) -> bool {
        self.snapshot.is_materialized()
    }

    /// Borrow the snapshot root iff materialized.
    pub fn snapshot_path(&self) -> Option<&GuardedPath> {
        self.snapshot.get()
    }
}

pub fn execute_with_result(opts: Options, workspace_root: GuardedPath) -> Result<ExecutionResult> {
    if opts.shell {
        return Err(errors::shell_unsupported_in_result());
    }

    // Read + parse BEFORE any tempdir exists so LOCAL-only scripts never
    // create a snapshot directory they never use (issue #131).
    let script = read_script(&opts.script)?;

    let mut final_cwd = workspace_root.clone();
    let snapshot = Arc::new(LazyGuardedTempDir::new());
    if !script.trim().is_empty() {
        let steps = parse_cli_script(&script)?;
        // Dry-run previews the import closure without binding sockets,
        // remotes, or execution state.
        if opts.dry_run {
            print_dry_run(&steps, &opts.script, &workspace_root)?;
            return Ok(ExecutionResult {
                snapshot,
                final_cwd,
                bindings: BTreeMap::new(),
            });
        }
        // Bind endpoint sockets before running: conflicts fail fast,
        // never parse-then-fail-on-bind.
        #[cfg(feature = "net")]
        let registry = build_registry(&opts.endpoints)?;
        #[cfg(not(feature = "net"))]
        check_no_net_endpoints(&opts.endpoints)?;
        #[cfg(not(feature = "net"))]
        check_no_net_remotes(&opts.remotes)?;
        // Validate REMOTE targets against inventory after parsing (targets
        // live in AST nodes) and before execution: unknown targets fail
        // here, never mid-run.
        #[cfg(feature = "net")]
        let inventory = {
            let pairs = opts.remotes.clone();
            let inventory = oxdock_net_plugin::RemoteInventory::build(pairs)?;
            inventory.log_resolutions();
            inventory.validate_script(&steps)?;
            inventory
        };
        let entry_file = entry_file_of(&opts.script);
        #[cfg(feature = "net")]
        let io_cfg = {
            let mut io_cfg = ExecIo::new();
            io_cfg.set_script_args(opts.script_args.clone());
            register_remote_runners(&mut io_cfg, &inventory);
            io_cfg
        };
        #[cfg(not(feature = "net"))]
        let io_cfg = {
            let mut io_cfg = ExecIo::new();
            io_cfg.set_script_args(opts.script_args.clone());
            io_cfg
        };
        #[cfg(feature = "net")]
        let output = run_cli_steps(&workspace_root, &steps, io_cfg, Some(&registry), entry_file)?;
        #[cfg(not(feature = "net"))]
        let output = run_cli_steps(&workspace_root, &steps, io_cfg, None, entry_file)?;
        final_cwd = output.final_cwd;
        return Ok(ExecutionResult {
            snapshot: output.snapshot,
            final_cwd,
            bindings: output.bindings,
        });
    }

    Ok(ExecutionResult {
        snapshot,
        final_cwd,
        bindings: BTreeMap::new(),
    })
}

/// Register one SSH stdio session per inventory target on the run IO.
/// Lazy spawn still applies (no process exists until a block entry), but
/// registration happens up front so `REMOTE` interception always finds a
/// runner when the target validated.
#[cfg(feature = "net")]
fn register_remote_runners(io: &mut ExecIo, inventory: &oxdock_net_plugin::RemoteInventory) {
    use std::sync::Arc;
    let entries = surface_entries();
    let module_hash = oxdock_net_plugin::module_hash(&entries);
    let modules: Vec<String> = entries.iter().map(|(name, _)| name.clone()).collect();
    for target in inventory.targets() {
        let Some(argv) = inventory.argv(&target) else {
            continue;
        };
        let config = oxdock_net_plugin::SessionConfig {
            argv: argv.to_vec(),
            oxdock_version: env!("CARGO_PKG_VERSION").to_string(),
            modules: modules.clone(),
            module_hash: module_hash.clone(),
        };
        io.set_remote_runner_for_target(
            target,
            Arc::new(oxdock_net_plugin::StdioSession::new(config)),
        );
    }
}

/// Module-table surface for handshake identity and session config: the
/// same tables the run executes against.
#[cfg(feature = "net")]
fn surface_entries() -> Vec<(String, Vec<String>)> {
    let mut engine = Engine::new();
    for module in cli_host_modules() {
        engine.register_module(module);
    }
    let table = engine.module_table();
    let mut entries: Vec<(String, Vec<String>)> = table
        .modules
        .into_iter()
        .map(|(name, module_funcs)| {
            let mut func_names: Vec<String> = module_funcs
                .map(|surface| surface.func_names.into_iter().collect())
                .unwrap_or_default();
            func_names.sort();
            (name, func_names)
        })
        .collect();
    entries.sort();
    entries
}
#[cfg(feature = "net")]
fn report_ephemeral_publishes(flags: &EndpointFlags, registry: &Arc<EndpointRegistry>) {
    use oxdock_net_plugin::{EndpointKey, Protocol, parse_endpoint_ref};
    for (outer, inner) in &flags.publishes {
        if outer.port() != 0 {
            continue;
        }
        let Ok((protocol, endpoint)) = parse_endpoint_ref(inner, "-p") else {
            continue;
        };
        let key = EndpointKey {
            protocol: protocol.unwrap_or(Protocol::Tcp),
            endpoint,
        };
        match registry.resolve_endpoint(&key) {
            Ok(addr) => eprintln!("oxdock: published {addr} -> {inner}"),
            Err(_) => eprintln!("oxdock: published <unbound> -> {inner}"),
        }
    }
}

/// Without `net`, remote targets cannot exist: `--remote` is rejected
/// at flag parse time; this covers programmatic `Options` carrying pairs.
#[cfg(not(feature = "net"))]
fn oxdock_net_plugin_stub_parse_remote(raw: &str) -> Result<(String, String)> {
    // Mirror the NET plugin's shape check without the dependency so the
    // flag stays diagnosed (not silently swallowed) in lean builds.
    match raw.split_once('=') {
        Some((target, command)) if !target.trim().is_empty() && !command.trim().is_empty() => {
            Ok((target.trim().to_string(), command.to_string()))
        }
        _ => Err(errors::remote_requires_shape(raw)),
    }
}

/// Without `net`, remote targets cannot exist.
#[cfg(not(feature = "net"))]
fn check_no_net_remotes(remotes: &[(String, String)]) -> Result<()> {
    if !remotes.is_empty() {
        return Err(errors::feature_required("--remote", "net"));
    }
    Ok(())
}

/// Without `net`, endpoint sockets cannot exist: `--listen`/`-p` are
/// rejected (parse time already bails; this covers programmatic `Options`),
/// while `--offline` or no flags proceed socketless.
#[cfg(not(feature = "net"))]
fn check_no_net_endpoints(flags: &EndpointFlags) -> Result<()> {
    if !flags.listens.is_empty() || !flags.publishes.is_empty() {
        return Err(errors::feature_required("--listen/-p", "net"));
    }
    Ok(())
}

/// Read the script source without creating any execution state. The
/// read resolves against the script path's own root, so scripts
/// outside the execution workspace load exactly like scripts inside it.
fn read_script(source: &ScriptSource) -> Result<String> {
    match source {
        ScriptSource::Path(path) => {
            let resolver = PathResolver::new(path.root(), path.root())?;
            resolver.read_to_string(path).map_err(|e| {
                errors::read_script_at(&path.display().to_string(), &anyhow::Error::new(e))
            })
        }
        ScriptSource::Stdin => {
            let mut buf = String::new();
            io::stdin()
                .lock()
                .read_to_string(&mut buf)
                .map_err(|e| errors::read_script_stdin(&anyhow::Error::from(e)))?;
            Ok(buf)
        }
    }
}

fn execute_with_shell_runner<F>(
    opts: Options,
    workspace_root: GuardedPath,
    shell_runner: F,
    require_tty: bool,
) -> Result<()>
where
    F: FnOnce(&GuardedPath, &GuardedPath) -> Result<()>,
{
    #[cfg(windows)]
    maybe_reexec_shell_to_temp(&opts)?;

    // Interpret a tiny Dockerfile-ish script. No tempdir exists yet: the
    // snapshot materializes lazily on first snapshot-targeted step, so an
    // empty non-shell run creates nothing at all (issue #131).
    let script = match &opts.script {
        ScriptSource::Path(_) => read_script(&opts.script)?,
        ScriptSource::Stdin => {
            let stdin = io::stdin();
            if stdin.is_terminal() {
                // No piped script provided. If the caller requested `--shell`
                // allow running with an initially-empty script so we can either
                // drop into the interactive shell or open the editor later.
                // Otherwise, require a script on stdin.
                if opts.shell {
                    String::new()
                } else {
                    return Err(errors::script_stdin_missing());
                }
            } else {
                let mut buf = String::new();
                stdin
                    .lock()
                    .read_to_string(&mut buf)
                    .map_err(|e| errors::read_script_stdin(&anyhow::Error::from(e)))?;
                buf
            }
        }
    };

    // Parse and run steps if we have a non-empty script. Empty scripts are
    // valid when `--shell` is requested and the caller didn't pipe a script.
    // Use the caller's workspace as the build context so WORKSPACE LOCAL can
    // hop back and so COPY can source from the original tree if needed.
    // Capture the final working directory so shells inherit whatever WORKDIR
    // the script ended on.
    let mut final_cwd = workspace_root.clone();
    let mut snapshot = Arc::new(LazyGuardedTempDir::new());
    let mut fs: Option<Box<dyn WorkspaceFs>> = None;
    if !script.trim().is_empty() {
        let steps = parse_cli_script(&script)?;
        // Dry-run previews the import closure without binding sockets,
        // remotes, or execution state.
        if opts.dry_run {
            print_dry_run(&steps, &opts.script, &workspace_root)?;
            return Ok(());
        }
        // Bind endpoint sockets before running: conflicts fail fast,
        // never parse-then-fail-on-bind.
        #[cfg(feature = "net")]
        let registry = build_registry(&opts.endpoints)?;
        #[cfg(not(feature = "net"))]
        check_no_net_endpoints(&opts.endpoints)?;
        #[cfg(not(feature = "net"))]
        check_no_net_remotes(&opts.remotes)?;
        #[cfg(feature = "net")]
        report_ephemeral_publishes(&opts.endpoints, &registry);
        // Validate REMOTE targets against inventory after parsing (targets
        // live in AST nodes) and before execution: unknown targets fail
        // here, never mid-run.
        #[cfg(feature = "net")]
        let inventory = {
            let inventory = oxdock_net_plugin::RemoteInventory::build(opts.remotes.clone())?;
            inventory.log_resolutions();
            inventory.validate_script(&steps)?;
            inventory
        };
        // If we are running a script from a file, we might have stdin available for the script itself.
        // If we read the script from stdin, then stdin is consumed.
        // But if opts.script is ScriptSource::Path, stdin is still available.

        let mut stdin_handle: Option<SharedInput> = None;
        if let ScriptSource::Path(_) = opts.script {
            let stdin = io::stdin();
            if !stdin.is_terminal() {
                // Wrap stdin in SharedInput (Arc<Mutex<dyn Read + Send>>)
                // Note: std::io::Stdin is a handle, but we need an owned Read + Send.
                // std::io::stdin() returns Stdin, which implements Read + Send.
                // However, we need to be careful about locking.
                // We can wrap the Stdin struct directly.
                stdin_handle = Some(Arc::new(Mutex::new(stdin)));
            }
        }

        let mut io_cfg = ExecIo::new();
        io_cfg.set_stdin(stdin_handle);
        io_cfg.set_script_args(opts.script_args.clone());
        #[cfg(feature = "net")]
        register_remote_runners(&mut io_cfg, &inventory);
        let entry_file = entry_file_of(&opts.script);
        #[cfg(feature = "net")]
        let output = run_cli_steps(&workspace_root, &steps, io_cfg, Some(&registry), entry_file)?;
        #[cfg(not(feature = "net"))]
        let output = run_cli_steps(&workspace_root, &steps, io_cfg, None, entry_file)?;
        final_cwd = output.final_cwd;
        snapshot = output.snapshot;
        fs = Some(output.fs);
    }

    // If requested, drop into an interactive shell after running the script.
    if opts.shell {
        if require_tty && !has_controlling_tty() {
            return Err(errors::shell_needs_tty());
        }
        // The shell needs a concrete directory: materialize here at shell
        // entry (not at startup) when the script left the snapshot pending,
        // then converge the cwd onto the shared concrete root. An empty
        // script starts the shell in a fresh snapshot directory.
        match fs.as_ref() {
            Some(fs) => {
                if fs.is_snapshot_pending() {
                    snapshot
                        .materialize()
                        .map_err(|e| errors::create_shell_temp_dir(&anyhow::Error::from(e)))?;
                }
                final_cwd = fs.concretize_cwd(&final_cwd);
            }
            None => {
                snapshot
                    .materialize()
                    .map_err(|e| errors::create_shell_temp_dir(&anyhow::Error::from(e)))?;
                final_cwd = snapshot
                    .get()
                    .cloned()
                    .expect("shell snapshot materialized above");
            }
        }
        return shell_runner(&final_cwd, &workspace_root);
    }

    Ok(())
}

#[cfg(test)]
fn execute_for_test<F>(opts: Options, workspace_root: GuardedPath, shell_runner: F) -> Result<()>
where
    F: FnOnce(&GuardedPath, &GuardedPath) -> Result<()>,
{
    execute_with_shell_runner(opts, workspace_root, shell_runner, false)
}

fn has_controlling_tty() -> bool {
    // Prefer checking whether stdin or stderr is a terminal. This avoids
    // directly opening device files via `std::fs` while still detecting
    // whether an interactive tty is available in the common cases.
    #[cfg(unix)]
    {
        io::stdin().is_terminal() || io::stderr().is_terminal()
    }

    #[cfg(windows)]
    {
        io::stdin().is_terminal() || io::stderr().is_terminal()
    }

    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

#[cfg(windows)]
fn maybe_reexec_shell_to_temp(opts: &Options) -> Result<()> {
    // Only used for interactive shells. Copy the binary to a temp path and run it there so the
    // original target exe is free for rebuilding while the shell stays open.
    if !opts.shell {
        return Ok(());
    }
    if std::env::var(oxdock_env::oxdock::SHELL_REEXEC)
        .ok()
        .as_deref()
        == Some("1")
    {
        return Ok(());
    }

    let self_path = std::env::current_exe()
        .map_err(|e| errors::determine_current_exe(&anyhow::Error::from(e)))?;
    let base_temp = GuardedPath::new_root(std::env::temp_dir().as_path())
        .map_err(|e| errors::guard_system_temp_dir(&e))?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let temp_file = base_temp
        .join(&format!("oxdock-shell-{ts}-{}.exe", std::process::id()))
        .map_err(|e| errors::construct_temp_shell_path(&e))?;

    // Copy the current executable into the temporary location via a
    // resolver whose root is the temp directory. The source may live
    // outside the temp dir, so use `copy_file_from_external`.
    let temp_root_guard = temp_file
        .parent()
        .ok_or_else(errors::shell_temp_missing_parent)?;
    let resolver_temp = PathResolver::new(temp_root_guard.as_path(), temp_root_guard.as_path())?;
    let dest = temp_file;
    #[allow(clippy::disallowed_types)]
    let source = oxdock_fs::UnguardedPath::external(self_path);
    resolver_temp
        .copy_file_from_unguarded(&source, &dest)
        .map_err(|e| errors::copy_shell_runner(&dest.display().to_string(), &e))?;

    let mut cmd = CommandBuilder::new(dest.as_path());
    cmd.args(std::env::args_os().skip(1));
    cmd.env(oxdock_env::oxdock::SHELL_REEXEC, "1");
    cmd.spawn().map_err(|e| {
        errors::spawn_shell_from(&dest.display().to_string(), &anyhow::Error::from(e))
    })?;

    // Exit immediately so the original binary can be rebuilt while the shell child stays running.
    std::process::exit(0);
}

pub fn run_script(workspace_root: &GuardedPath, steps: &[Step]) -> Result<()> {
    Ok(run_steps_with_context(
        workspace_root,
        workspace_root,
        steps,
    )?)
}

fn shell_banner(cwd: &GuardedPath, workspace_root: &GuardedPath) -> String {
    #[cfg(windows)]
    let cwd_disp = oxdock_fs::command_path(cwd).as_ref().display().to_string();
    #[cfg(windows)]
    let workspace_disp = oxdock_fs::command_path(workspace_root)
        .as_ref()
        .display()
        .to_string();

    #[cfg(not(windows))]
    let cwd_disp = cwd.display().to_string();
    #[cfg(not(windows))]
    let workspace_disp = workspace_root.display().to_string();

    let pkg = env::var(oxdock_env::cargo::PKG_NAME)
        .unwrap_or_else(|_| oxdock_env::oxdock::FALLBACK_APP_NAME.to_string());
    indoc::formatdoc! {"
        {pkg} shell workspace
          cwd: {cwd_disp}
          source: workspace root at {workspace_disp}
          lifetime: temporary directory created for this shell session; it disappears when you exit
          creation: temp workspace starts empty unless your script copies files into it

          WARNING: This shell still runs on your host filesystem and is **not** isolated!
    "}
}

fn run_shell(cwd: &GuardedPath, workspace_root: &GuardedPath) -> Result<()> {
    Ok(oxdock_process::spawn_interactive_shell(
        cwd,
        workspace_root,
        &shell_banner(cwd, workspace_root),
    )?)
}

// `command_path` now lives in `oxdock-fs` to centralize Path usage.

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use indoc::indoc;
    use oxdock_fs::PathResolver;
    use std::cell::{Cell, RefCell};

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn shell_runner_receives_final_workdir() -> Result<()> {
        let workspace = GuardedPath::tempdir()?;
        let workspace_root = workspace.as_guarded_path().clone();
        let script_path = workspace_root.join("script.ox")?;
        let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())?;
        let script = indoc! {"
            WRITE temp.txt 123
            WORKDIR sub
        "};
        resolver.write_file(&script_path, script.as_bytes())?;

        let opts = Options {
            script: ScriptSource::Path(script_path),
            shell: true,
            dry_run: false,
            endpoints: EndpointFlags::default(),
            remote_serve: false,
            mcp: false,
            remotes: Vec::new(),
            script_args: Vec::new(),
        };

        let observed = Cell::new(false);
        execute_for_test(opts, workspace_root.clone(), |cwd, _| {
            assert!(
                cwd.as_path().ends_with("sub"),
                "final cwd should end in WORKDIR target, got {}",
                cwd.display()
            );

            let temp_root = GuardedPath::new_root(cwd.root())
                .context("construct guard for temp workspace root")?;
            let sub_dir = temp_root.join("sub")?;
            assert_eq!(
                cwd.as_path(),
                sub_dir.as_path(),
                "shell runner cwd should match guarded sub dir"
            );
            let temp_file = temp_root.join("temp.txt")?;
            let temp_resolver = PathResolver::new(temp_root.as_path(), temp_root.as_path())?;
            let contents = temp_resolver.read_to_string(&temp_file)?;
            assert!(
                contents.contains("123"),
                "expected WRITE command to materialize temp file"
            );
            observed.set(true);
            Ok(())
        })?;

        assert!(
            observed.into_inner(),
            "shell runner closure should have been invoked"
        );
        Ok(())
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_requires_script_path_value() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let mut args = vec!["--script".to_string()].into_iter();
        let err = Options::parse(&mut args, workspace.as_guarded_path())
            .expect_err("expected missing path error");
        assert!(err.to_string().contains("--script requires a path"));
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_script_flag_takes_dash_leading_path() {
        // #226: dash-named files ride `--script` (bare dash-leading
        // positionals are stdin args, never files).
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let mut args = vec!["--script".to_string(), "--weird.ox".to_string()].into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        match opts.script {
            ScriptSource::Path(path) => assert_eq!(
                path,
                workspace_root.join("--weird.ox").expect("script path")
            ),
            ScriptSource::Stdin => panic!("expected path script"),
        }
        assert!(opts.script_args.is_empty());
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_script_path_and_shell() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let script_path = workspace_root.join("script.txt").expect("script path");
        let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())
            .expect("resolver");
        resolver
            .write_file(&script_path, b"WRITE out.txt hi")
            .expect("write script");
        // #226: interpreter flags come before the script; anything
        // after is verbatim script args.
        let mut args = vec![
            "--shell".to_string(),
            "--script".to_string(),
            "script.txt".to_string(),
        ]
        .into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert!(opts.shell);
        match opts.script {
            ScriptSource::Path(path) => assert_eq!(path, script_path),
            ScriptSource::Stdin => panic!("expected path script"),
        }
        assert!(opts.script_args.is_empty());

        let mut args = vec![
            "--script".to_string(),
            "script.txt".to_string(),
            "--shell".to_string(),
        ]
        .into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert!(
            !opts.shell,
            "--shell after the script is data, not the interpreter flag"
        );
        assert_eq!(opts.script_args, vec!["--shell"]);
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_mcp_flag() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let mut args = vec!["--mcp".to_string(), "tools.oxdock".to_string()].into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert!(opts.mcp);
        match opts.script {
            ScriptSource::Path(path) => assert_eq!(
                path,
                workspace_root.join("tools.oxdock").expect("script path")
            ),
            ScriptSource::Stdin => panic!("expected path script"),
        }
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_mcp_defaults_off() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let mut args = vec!["script.txt".to_string()].into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert!(!opts.mcp);
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_positional_script_path() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let mut args = vec!["script.txt".to_string()].into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert!(!opts.shell);
        match opts.script {
            ScriptSource::Path(path) => assert_eq!(
                path,
                workspace_root.join("script.txt").expect("script path")
            ),
            ScriptSource::Stdin => panic!("expected path script"),
        }
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_positional_dash_reads_stdin() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let mut args = vec!["-".to_string()].into_iter();
        let opts = Options::parse(&mut args, workspace.as_guarded_path()).expect("parse");
        assert!(matches!(opts.script, ScriptSource::Stdin));
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_second_positional_is_script_arg() {
        // #226: after the script, option parsing stops entirely, so a
        // second positional (or `--script` there) is verbatim script
        // args, never a duplicate-script error.
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let mut args = vec![
            "a.ox".to_string(),
            "--script".to_string(),
            "b.ox".to_string(),
        ]
        .into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        match &opts.script {
            ScriptSource::Path(path) => assert_eq!(
                path,
                &workspace_root.join("a.ox").expect("script path"),
                "first positional fixes the script"
            ),
            ScriptSource::Stdin => panic!("expected path script"),
        }
        assert_eq!(opts.script_args, vec!["--script", "b.ox"]);

        let mut args = vec!["a.ox".to_string(), "b.ox".to_string()].into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert_eq!(opts.script_args, vec!["b.ox"]);
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_absolute_script_outside_execution_workspace_loads() {
        // Stdin parity: an explicit absolute location loads wherever
        // it lives; only relative paths resolve under the workspace.
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let outside = GuardedPath::tempdir().expect("outside");
        let target = outside
            .as_guarded_path()
            .as_path()
            .join("step.ox")
            .to_string_lossy()
            .into_owned();
        let mut args = vec!["--script".to_string(), target.clone()].into_iter();
        let opts = Options::parse(&mut args, workspace.as_guarded_path()).expect("parse");
        match opts.script {
            ScriptSource::Path(path) => {
                assert_eq!(path.as_path().to_string_lossy(), target);
                assert_eq!(path.root(), outside.as_guarded_path().as_path());
            }
            ScriptSource::Stdin => panic!("absolute script must not parse as stdin"),
        }
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_relative_escape_stays_rejected() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let mut args = vec!["--script".to_string(), "../escape.ox".to_string()].into_iter();
        let err =
            Options::parse(&mut args, workspace.as_guarded_path()).expect_err("escape must fail");
        assert!(err.to_string().contains("guard script path"), "{err:?}");
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_rejects_unknown_flags() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let mut args = vec!["--frobnicate".to_string()].into_iter();
        let err = Options::parse(&mut args, workspace.as_guarded_path())
            .expect_err("expected unknown flag error");
        assert!(err.to_string().contains("unexpected flag"), "{err:?}");
    }

    #[test]
    fn usage_describes_positional_script_and_help() {
        let text = usage();
        assert!(text.contains("Usage: oxdock"), "{text}");
        assert!(text.contains("SCRIPT"), "{text}");
        assert!(text.contains("--script"), "{text}");
        assert!(text.contains("--help"), "{text}");
        assert!(text.contains("--offline"), "{text}");
        #[cfg(feature = "net")]
        {
            assert!(text.contains("--listen"), "{text}");
            assert!(text.contains("-p <[host:]outer:inner>"), "{text}");
        }
        #[cfg(not(feature = "net"))]
        {
            assert!(!text.contains("--listen <addr>"), "{text}");
            assert!(!text.contains("-p <[host:]outer:inner>"), "{text}");
            assert!(text.contains("require the `net` feature"), "{text}");
        }
        // Tagline is single-sourced from the package manifest, not hardcoded.
        assert!(text.contains(env!("CARGO_PKG_DESCRIPTION")), "{text}");
    }

    #[cfg(feature = "net")]
    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_endpoint_flags() {
        // Pure flag parsing: no sockets open, but tempdir keeps the
        // ignore uniform with the neighboring parse tests.
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let mut args = vec![
            "--listen".to_string(),
            "0.0.0.0:2251".to_string(),
            "-p".to_string(),
            "2222:demo-proxy".to_string(),
            "-p".to_string(),
            "0:2252".to_string(),
            "-".to_string(),
        ]
        .into_iter();
        let opts = Options::parse(&mut args, workspace.as_guarded_path()).expect("parse");
        assert_eq!(opts.endpoints.listens.len(), 1);
        assert_eq!(opts.endpoints.publishes.len(), 2);
        assert!(!opts.endpoints.offline);
        assert!(matches!(opts.script, ScriptSource::Stdin));
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_offline_flag() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let mut args = vec!["--offline".to_string(), "-".to_string()].into_iter();
        let opts = Options::parse(&mut args, workspace.as_guarded_path()).expect("parse");
        assert!(opts.endpoints.offline);
    }

    #[cfg(feature = "net")]
    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_rejects_bad_endpoint_flags() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        for args in [
            vec!["--listen"],
            vec!["--listen", "0.0.0.0:0"],
            vec!["-p"],
            vec!["-p", "2222"],
            vec!["-p", "2222:0"],
        ] {
            let mut args = args.into_iter().map(str::to_string);
            Options::parse(&mut args, workspace.as_guarded_path())
                .expect_err("bad endpoint flag must fail");
        }
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_empty_values_hold_token_boundaries() {
        // Regression: explicit empty values must not shift the stream.
        // `--script ""` bails instead of consuming the next token, and a
        // bare empty positional is skipped like before.
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let mut args = vec![
            "--script".to_string(),
            "".to_string(),
            "--shell".to_string(),
        ]
        .into_iter();
        let err = Options::parse(&mut args, workspace.as_guarded_path())
            .expect_err("empty script path must fail");
        assert!(
            err.to_string().contains("--script requires a path"),
            "{err:?}"
        );
        let mut args = vec!["".to_string(), "-".to_string()].into_iter();
        let opts = Options::parse(&mut args, workspace.as_guarded_path()).expect("parse");
        assert!(matches!(opts.script, ScriptSource::Stdin));
    }

    #[cfg(feature = "net")]
    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_accepts_equals_and_attached_forms() {
        // lexopt-native spellings: `--flag=value`, attached short values,
        // and `--` separating positionals.
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let mut args = vec![
            "--listen=0.0.0.0:2251".to_string(),
            "-p2222:demo-proxy".to_string(),
            "--".to_string(),
            "script.ox".to_string(),
        ]
        .into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert_eq!(opts.endpoints.listens.len(), 1);
        assert_eq!(opts.endpoints.publishes.len(), 1);
        match opts.script {
            ScriptSource::Path(path) => {
                assert_eq!(path, workspace_root.join("script.ox").expect("script path"))
            }
            ScriptSource::Stdin => panic!("expected path script after --"),
        }
        assert!(opts.script_args.is_empty());
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_routes_verbatim_tail_to_script_args() {
        // #226: after the script, option parsing stops. Dash-leading
        // tails survive without `--`, and one leading `--` strips.
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let mut args = vec![
            "my.ox".to_string(),
            "--dry-run".to_string(),
            "--listen".to_string(),
            "0.0.0.0:2251".to_string(),
        ]
        .into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert_eq!(
            opts.script_args,
            vec!["--dry-run", "--listen", "0.0.0.0:2251"]
        );

        let mut args = vec![
            "my.ox".to_string(),
            "--".to_string(),
            "--dry-run".to_string(),
        ]
        .into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert_eq!(opts.script_args, vec!["--dry-run"]);

        let mut args = vec![
            "--script".to_string(),
            "my.ox".to_string(),
            "--".to_string(),
            "a".to_string(),
            "--".to_string(),
            "b".to_string(),
        ]
        .into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert_eq!(opts.script_args, vec!["a", "--", "b"]);
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_help_after_script_is_data() {
        // `--help`/`--version` stay interpreter-only before the script.
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let mut args = vec!["my.ox".to_string(), "--help".to_string()].into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert_eq!(opts.script_args, vec!["--help"]);
        let mut args = vec!["my.ox".to_string(), "--version".to_string()].into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert_eq!(opts.script_args, vec!["--version"]);
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_dash_leading_after_separator_is_stdin_args() {
        // `oxdock -- --foo` is stdin plus args, not a file named `--foo`
        // (pass those via `--script`). Plain names after `--` stay files.
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let mut args = vec!["--".to_string(), "--foo".to_string()].into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert!(matches!(opts.script, ScriptSource::Stdin));
        assert_eq!(opts.script_args, vec!["--foo"]);

        let mut args = vec!["--".to_string(), "script.ox".to_string()].into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        match opts.script {
            ScriptSource::Path(path) => {
                assert_eq!(path, workspace_root.join("script.ox").expect("script path"))
            }
            ScriptSource::Stdin => panic!("expected path script"),
        }
        assert!(opts.script_args.is_empty());

        let mut args = vec!["--".to_string(), "--".to_string(), "a".to_string()].into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert!(matches!(opts.script, ScriptSource::Stdin));
        assert_eq!(opts.script_args, vec!["a"]);
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_preserves_inner_separators_in_tail() {
        // Only one leading `--` is stripped. Inner separators stay data.
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let mut args = vec![
            "--".to_string(),
            "--foo".to_string(),
            "--".to_string(),
            "--bar".to_string(),
        ]
        .into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert!(matches!(opts.script, ScriptSource::Stdin));
        assert_eq!(opts.script_args, vec!["--foo", "--", "--bar"]);

        let mut args = vec![
            "my.ox".to_string(),
            "a".to_string(),
            "--".to_string(),
            "b".to_string(),
        ]
        .into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert_eq!(opts.script_args, vec!["a", "--", "b"]);
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_version_returns_version_without_exiting() {
        // Same contract as --help: version surfaces as the parse error
        // (parse must not exit the process itself: it is public library
        // API). Print it and succeed so the binary exits 0.
        let workspace = GuardedPath::tempdir().expect("tempdir");
        for flag in ["--version", "-V", "-v"] {
            let mut args = vec![flag.to_string()].into_iter();
            let err = Options::parse(&mut args, workspace.as_guarded_path())
                .expect_err("version flag must not parse as options");
            assert_eq!(err.to_string(), version());
        }
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_help_returns_usage_error_without_exiting() {
        // Regression: parse is public library API and must return instead of
        // terminating the process; `run()` turns this error into a clean exit 0.
        let workspace = GuardedPath::tempdir().expect("tempdir");
        for flag in ["--help", "-h"] {
            let mut args = vec![flag.to_string()].into_iter();
            let err = Options::parse(&mut args, workspace.as_guarded_path())
                .expect_err("help flag must not parse as options");
            assert_eq!(err.to_string(), usage());
        }
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn options_parse_rejects_remote_script_with_input_intact() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        for args in [
            vec!["https://example.com/process.oxfile".to_string()],
            vec![
                "--script".to_string(),
                "https://example.com/process.oxfile".to_string(),
            ],
        ] {
            let err = Options::parse(&mut args.into_iter(), workspace.as_guarded_path())
                .expect_err("remote script must not parse as a path");
            let rendered = format!("{err:#}");
            assert!(
                rendered.contains("not a local path")
                    && rendered.contains("https://example.com/process.oxfile"),
                "rejection names the intact input: {rendered}"
            );
            assert!(
                !rendered.contains("https:/example.com"),
                "scheme must never come out mangled: {rendered}"
            );
        }
    }

    #[test]
    fn execute_with_result_runs_script() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let script_path = workspace_root.join("script.txt").expect("script path");
        let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())
            .expect("resolver");
        resolver
            .write_file(&script_path, b"WRITE out.txt hi")
            .expect("write script");
        let opts = Options {
            script: ScriptSource::Path(script_path),
            shell: false,
            dry_run: false,
            endpoints: EndpointFlags::default(),
            remote_serve: false,
            mcp: false,
            remotes: Vec::new(),
            script_args: Vec::new(),
        };
        let result = execute_with_result(opts, workspace_root).expect("execute");
        let snapshot = result
            .snapshot_path()
            .expect("default WRITE materializes the snapshot");
        assert_eq!(snapshot, &result.final_cwd);
        let temp_resolver = PathResolver::new(snapshot.root(), snapshot.root()).expect("resolver");
        let out = snapshot.join("out.txt").expect("out path");
        let contents = temp_resolver.read_to_string(&out).expect("read out");
        assert_eq!(contents.trim(), "hi");
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn execute_with_result_routes_script_args_to_argv() {
        // #226: verbatim tail lands in `STD::ARGV()`, never merged
        // with interpreter opts.
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let script_path = workspace_root.join("script.txt").expect("script path");
        let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())
            .expect("resolver");
        let script = indoc! {r#"
            IMPORT [STD]
            LET $args: LIST<STRING> = ARGV()
            LET $n: INT = LEN($args)
            ASSERT_EQ $n 2
            ASSERT_EQ $args.0 "--dry-run"
            ASSERT_EQ $args.1 "serve"
            WRITE out.txt "ok"
        "#};
        resolver
            .write_file(&script_path, script.as_bytes())
            .expect("write script");
        // Parse through the real CLI boundary so the routing (strip
        // one `--`, keep `--dry-run` as data) is covered too.
        let mut args = vec![
            "script.txt".to_string(),
            "--".to_string(),
            "--dry-run".to_string(),
            "serve".to_string(),
        ]
        .into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert_eq!(opts.script_args, vec!["--dry-run", "serve"]);
        let result = execute_with_result(opts, workspace_root).expect("execute");
        let snapshot = result
            .snapshot_path()
            .expect("WRITE materializes the snapshot");
        let temp_resolver = PathResolver::new(snapshot.root(), snapshot.root()).expect("resolver");
        let out = snapshot.join("out.txt").expect("out path");
        assert_eq!(
            temp_resolver.read_to_string(&out).expect("read").trim(),
            "ok"
        );
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn execute_with_result_local_only_creates_no_snapshot() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let script_path = workspace_root.join("script.txt").expect("script path");
        let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())
            .expect("resolver");
        resolver
            .write_file(&script_path, b"WORKSPACE LOCAL\nWRITE out.txt hi")
            .expect("write script");
        let opts = Options {
            script: ScriptSource::Path(script_path),
            shell: false,
            dry_run: false,
            endpoints: EndpointFlags::default(),
            remote_serve: false,
            mcp: false,
            remotes: Vec::new(),
            script_args: Vec::new(),
        };
        let result = execute_with_result(opts, workspace_root.clone()).expect("execute");
        assert!(
            !result.has_snapshot(),
            "WORKSPACE LOCAL-only script must not create a snapshot tempdir"
        );
        assert!(result.snapshot_path().is_none());
        // The write landed in the live workspace tree instead.
        let out = workspace_root.join("out.txt").expect("out path");
        let contents = resolver.read_to_string(&out).expect("read out");
        assert_eq!(contents.trim(), "hi");
        // The reported cwd stays under the workspace root.
        assert_eq!(result.final_cwd.root(), workspace_root.as_path());
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn execute_with_result_empty_script_creates_no_snapshot() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let script_path = workspace_root.join("empty.txt").expect("script path");
        let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())
            .expect("resolver");
        resolver
            .write_file(&script_path, b"")
            .expect("write script");
        let opts = Options {
            script: ScriptSource::Path(script_path),
            shell: false,
            dry_run: false,
            endpoints: EndpointFlags::default(),
            remote_serve: false,
            mcp: false,
            remotes: Vec::new(),
            script_args: Vec::new(),
        };
        let result = execute_with_result(opts, workspace_root.clone()).expect("execute");
        assert!(
            !result.has_snapshot(),
            "empty script must not create a snapshot tempdir"
        );
        assert_eq!(result.final_cwd, workspace_root);
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn execute_for_test_invokes_shell_runner() -> Result<()> {
        let workspace = GuardedPath::tempdir()?;
        let workspace_root = workspace.as_guarded_path().clone();
        let script_path = workspace_root.join("empty.txt")?;
        let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())?;
        resolver.write_file(&script_path, b"")?;
        let opts = Options {
            script: ScriptSource::Path(script_path),
            shell: true,
            dry_run: false,
            endpoints: EndpointFlags::default(),
            remote_serve: false,
            mcp: false,
            remotes: Vec::new(),
            script_args: Vec::new(),
        };
        let called = RefCell::new(None::<(String, String)>);
        execute_for_test(opts, workspace_root.clone(), |cwd, workspace| {
            called.replace(Some((cwd.display(), workspace.display())));
            // Shell entry converges onto a concrete, existing directory even
            // when the script never touched the snapshot (issue #131).
            assert!(
                cwd.exists(),
                "shell cwd must exist on disk, got {}",
                cwd.display()
            );
            Ok(())
        })?;
        let seen = called.borrow().clone().expect("shell runner called");
        assert_eq!(seen.1, workspace_root.display());
        Ok(())
    }

    /// The `ssh` feature wires the SSH host module into the real CLI
    /// runner: serve an ephemeral server and close it through
    /// `execute_with_result`, no client needed.
    #[cfg(feature = "ssh")]
    #[cfg_attr(
        miri,
        ignore = "loopback TCP plus threads plus a Tokio runtime; also GuardedPath::tempdir"
    )]
    #[test]
    fn ssh_feature_serves_and_closes() -> Result<()> {
        let workspace = GuardedPath::tempdir()?;
        let workspace_root = workspace.as_guarded_path().clone();
        let script_path = workspace_root.join("ssh-serve.ox")?;
        let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())?;
        let script = indoc! {"
            IMPORT [STD, SSH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE(\"cli-ssh-svc\", {username: \"test\", password: \"test123\"})
            SSH_CLOSE($m.server)
        "};
        resolver.write_file(&script_path, script.as_bytes())?;
        let opts = Options {
            script: ScriptSource::Path(script_path),
            shell: false,
            dry_run: false,
            endpoints: EndpointFlags {
                publishes: vec![(
                    std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                    "cli-ssh-svc".to_string(),
                )],
                ..EndpointFlags::default()
            },
            remote_serve: false,
            mcp: false,
            remotes: Vec::new(),
            script_args: Vec::new(),
        };
        execute_with_result(opts, workspace_root)?;
        Ok(())
    }

    /// The NET module ships with the `net` feature: bind an ephemeral
    /// loopback port and close it through `execute_with_result`, no
    /// client needed.
    #[cfg(feature = "net")]
    #[cfg_attr(
        miri,
        ignore = "loopback TCP plus GuardedPath::tempdir; blocked under Miri isolation"
    )]
    #[test]
    fn net_module_listens_and_closes() -> Result<()> {
        let workspace = GuardedPath::tempdir()?;
        let workspace_root = workspace.as_guarded_path().clone();
        let script_path = workspace_root.join("net-listen.ox")?;
        let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())?;
        let script = indoc! {"
            IMPORT [STD, NET]
            LET $l: NET_LISTEN_INFO = NET_LISTEN(\"23501\", {})
            NET_CLOSE($l.listener)
        "};
        resolver.write_file(&script_path, script.as_bytes())?;
        let opts = Options {
            script: ScriptSource::Path(script_path),
            shell: false,
            dry_run: false,
            // The port key is only a registry name here: the outer bind
            // is ephemeral, so no fixed host port is ever touched.
            endpoints: EndpointFlags {
                publishes: vec![(
                    std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                    "23501".to_string(),
                )],
                ..EndpointFlags::default()
            },
            remote_serve: false,
            mcp: false,
            remotes: Vec::new(),
            script_args: Vec::new(),
        };
        execute_with_result(opts, workspace_root)?;
        Ok(())
    }

    /// A `-p`-mapped outer port is observable in-script: route an
    /// ephemeral `-p 0:<name>` resolution through `NET_PORT`/`NET_ADDR`
    /// into files an inner `RUN` could equally consume via `ENV`.
    #[cfg(feature = "net")]
    #[cfg_attr(
        miri,
        ignore = "loopback TCP plus GuardedPath::tempdir; blocked under Miri isolation"
    )]
    #[test]
    fn net_port_addr_observe_publish_mapping() -> Result<()> {
        let workspace = GuardedPath::tempdir()?;
        let workspace_root = workspace.as_guarded_path().clone();
        let script_path = workspace_root.join("net-port.ox")?;
        let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())?;
        let script = indoc! {"
            IMPORT [STD, NET]
            LET $port: INT = NET_PORT(\"routed-svc\")
            WRITE port.txt \"{{ $port }}\"
            LET $addr: STRING = NET_ADDR(\"routed-svc\")
            WRITE addr.txt \"{{ $addr }}\"
        "};
        resolver.write_file(&script_path, script.as_bytes())?;
        let opts = Options {
            script: ScriptSource::Path(script_path),
            shell: false,
            dry_run: false,
            endpoints: EndpointFlags {
                publishes: vec![(
                    std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
                    "routed-svc".to_string(),
                )],
                ..EndpointFlags::default()
            },
            remote_serve: false,
            mcp: false,
            remotes: Vec::new(),
            script_args: Vec::new(),
        };
        let result = execute_with_result(opts, workspace_root)?;
        let snapshot = result
            .snapshot_path()
            .expect("WRITE materializes the snapshot");
        let snapshot_resolver = PathResolver::new(snapshot.root(), snapshot.root())?;
        let port_path = snapshot.join("port.txt")?;
        let port_text = snapshot_resolver.read_to_string(&port_path)?;
        let port: u16 = port_text
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("expected a resolved port, got {port_text:?}"))?;
        assert_ne!(port, 0, "ephemeral outer port must resolve");
        let addr_path = snapshot.join("addr.txt")?;
        let addr_text = snapshot_resolver.read_to_string(&addr_path)?;
        assert!(
            addr_text.trim().ends_with(&format!(":{port}")),
            "dial string must carry the resolved port, got {addr_text:?}"
        );
        Ok(())
    }

    /// Without the `ssh` feature the same script must fail to parse:
    /// SSH names stay unknown instead of silently changing meaning.
    #[cfg(not(feature = "ssh"))]
    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn ssh_scripts_rejected_without_feature() -> Result<()> {
        let workspace = GuardedPath::tempdir()?;
        let workspace_root = workspace.as_guarded_path().clone();
        let script_path = workspace_root.join("ssh-serve.ox")?;
        let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())?;
        let script = indoc! {"
            IMPORT [STD, SSH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE(\"23301\", {username: \"test\", password: \"test123\"})
            SSH_CLOSE($m.server)
        "};
        resolver.write_file(&script_path, script.as_bytes())?;
        let opts = Options {
            script: ScriptSource::Path(script_path),
            shell: false,
            dry_run: false,
            endpoints: EndpointFlags::default(),
            remote_serve: false,
            mcp: false,
            remotes: Vec::new(),
            script_args: Vec::new(),
        };
        let err = match execute_with_result(opts, workspace_root) {
            Ok(_) => panic!("SSH names must be unknown without the feature"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("SSH"), "{err}");
        let diag = err
            .downcast_ref::<oxdock_diagnostics::Diagnostic>()
            .expect("CLI errors stay typed");
        assert_eq!(diag.key(), "cli.module_unlinked", "{diag}");
        Ok(())
    }

    /// Without the `net` feature `--listen`/`-p` are rejected with the
    /// feature message, while `--offline` stays accepted as trivially
    /// satisfied.
    #[cfg(not(feature = "net"))]
    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn net_flags_rejected_without_feature() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        for args in [
            vec!["--listen", "0.0.0.0:2251", "-"],
            vec!["-p", "2222:2251", "-"],
            vec!["--listen=0.0.0.0:2251", "-"],
        ] {
            let mut args = args.into_iter().map(str::to_string);
            let err = Options::parse(&mut args, workspace.as_guarded_path())
                .expect_err("endpoint flag must fail without net");
            assert!(err.to_string().contains("`net` feature"), "{err:?}");
            // Typed path: stable key survives rewording.
            let diag = err
                .downcast_ref::<oxdock_diagnostics::Diagnostic>()
                .expect("CLI errors stay typed");
            assert_eq!(diag.key(), "cli.feature_required");
        }
        let mut args = vec!["--offline".to_string(), "-".to_string()].into_iter();
        let opts = Options::parse(&mut args, workspace.as_guarded_path()).expect("parse");
        assert!(opts.endpoints.offline);
    }

    /// Without the `net` feature the same script must fail to parse:
    /// NET names stay unknown instead of silently changing meaning.
    #[cfg(not(feature = "net"))]
    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn net_scripts_rejected_without_feature() -> Result<()> {
        let workspace = GuardedPath::tempdir()?;
        let workspace_root = workspace.as_guarded_path().clone();
        let script_path = workspace_root.join("net-listen.ox")?;
        let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())?;
        let script = indoc! {"
            IMPORT [STD, NET]
            LET $l: NET_LISTEN_INFO = NET_LISTEN(\"23501\", {})
            LET $body: STRING = NET_FETCH(\"https://example.com/x\")
            NET_CLOSE($l.listener)
        "};
        resolver.write_file(&script_path, script.as_bytes())?;
        let opts = Options {
            script: ScriptSource::Path(script_path),
            shell: false,
            dry_run: false,
            endpoints: EndpointFlags::default(),
            remote_serve: false,
            mcp: false,
            remotes: Vec::new(),
            script_args: Vec::new(),
        };
        let err = match execute_with_result(opts, workspace_root) {
            Ok(_) => panic!("NET names must be unknown without the feature"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("NET"), "{err}");
        // Known-but-unlinked: the failure names the module and the key.
        let diag = err
            .downcast_ref::<oxdock_diagnostics::Diagnostic>()
            .expect("CLI errors stay typed");
        assert_eq!(diag.key(), "cli.module_unlinked", "{diag}");
        Ok(())
    }

    /// Token matching never fires on comments or strings: `// IMPORT
    /// [NET]` and `ECHO "IMPORT [NET]"` name no module, and neither do
    /// other brackets (`WITH_IO` bindings) or quoted file paths.
    #[test]
    fn imports_module_ignores_comments_strings_and_other_brackets() {
        use oxdock_parser::lex_str;
        let toks = lex_str("IMPORT [STD, NET]\n").expect("lex");
        assert!(super::imports_module(&toks, "NET"));
        assert!(!super::imports_module(&toks, "SSH"));
        let toks = lex_str("IF true {\nIMPORT [SSH]\n}\n").expect("lex");
        assert!(super::imports_module(&toks, "SSH"));
        let toks = lex_str("IMPORT [\"./a.oxdock\" AS LIB]\n").expect("lex");
        assert!(!super::imports_module(&toks, "NET"));
        // A module name inside the quoted path never counts either.
        let toks = lex_str("IMPORT [\"./NET.oxdock\" AS LIB]\n").expect("lex");
        assert!(!super::imports_module(&toks, "NET"));
        let toks = lex_str("// IMPORT [NET]\nECHO hi\n").expect("lex");
        assert!(!super::imports_module(&toks, "NET"));
        let toks = lex_str("ECHO \"IMPORT [NET]\"\n").expect("lex");
        assert!(!super::imports_module(&toks, "NET"));
        let toks = lex_str("WITH_IO [stdout=$p] ECHO hi\n").expect("lex");
        assert!(!super::imports_module(&toks, "NET"));
    }

    /// Lean builds map a genuine `IMPORT [NET]` failure to the typed
    /// unlinked-module error, but comment/string lookalikes keep `None`
    /// so the real parse error rides through.
    #[cfg(not(feature = "net"))]
    #[test]
    fn unlinked_module_error_needs_a_real_import() {
        let err =
            super::unlinked_module_error("IMPORT [STD, NET]\n").expect("real import must map");
        assert_eq!(
            err.downcast_ref::<oxdock_diagnostics::Diagnostic>()
                .expect("typed")
                .key(),
            "cli.module_unlinked"
        );
        assert!(super::unlinked_module_error("// IMPORT [NET]\nECHO hi\n").is_none());
        assert!(super::unlinked_module_error("ECHO \"IMPORT [NET]\"\n").is_none());
        assert!(super::unlinked_module_error("ECHO hi\n").is_none());
    }

    #[test]
    fn process_exit_code_relays_exit_requests() {
        let exit = anyhow::Error::new(ExitRequest::new(3));
        assert_eq!(process_exit_code(&exit), 3);
        let relayed =
            anyhow::Error::new(ExitRequest::new(3)).context("REMOTE 'prod' exited with code 3");
        assert_eq!(process_exit_code(&relayed), 3);
        let keyed = anyhow::Error::new(oxdock_core::exec::diagnostics::eval_exit_requested(3));
        assert_eq!(process_exit_code(&keyed), 3);
        let keyed_wrapped =
            anyhow::Error::new(oxdock_core::exec::diagnostics::eval_exit_requested(3))
                .context("step 1");
        assert_eq!(process_exit_code(&keyed_wrapped), 3);
        assert_eq!(process_exit_code(&anyhow::anyhow!("boom")), 1);
        assert_eq!(
            process_exit_code(&anyhow::Error::new(ExitRequest::new(300))),
            255,
            "codes clamp to the portable range"
        );
        assert_eq!(
            process_exit_code(&anyhow::Error::new(ExitRequest::new(-1))),
            0,
            "negative codes clamp to zero"
        );
    }
}

#[cfg(all(test, windows))]
mod windows_shell_tests {
    use super::*;

    #[test]
    fn command_path_strips_verbatim_prefix() -> Result<()> {
        let temp = GuardedPath::tempdir()?;
        let converted = oxdock_fs::command_path(temp.as_guarded_path());
        let as_str = converted.as_ref().display().to_string();
        assert!(
            !as_str.starts_with(r"\\?\"),
            "expected non-verbatim path, got {as_str}"
        );
        Ok(())
    }
}
