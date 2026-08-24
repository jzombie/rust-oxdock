use anyhow::{Context, Result, anyhow, bail};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::process::ExitStatus;
use std::sync::{Arc, Condvar, Mutex};

use oxdock_fs::{EntryKind, GuardedPath, PathResolver, WorkspaceFs, to_forward_slashes};
use oxdock_parser::{IoStream, Step, StepKind, TemplateString, WorkspaceTarget};
use oxdock_process::{
    BackgroundHandle, BuiltinEnv, CommandContext, CommandOptions, CommandResult, CommandStderr,
    CommandStdout, ProcessManager, SharedInput, SharedOutput, default_process_manager,
    expand_command_env,
};
use sha2::{Digest, Sha256};

#[derive(Clone)]
enum PipeEndpoint {
    Stream(SharedOutput),
    Script(ScriptPipeEndpoint),
    Inherit,
}

impl PipeEndpoint {
    fn stream(writer: SharedOutput) -> Self {
        PipeEndpoint::Stream(writer)
    }

    fn script(endpoint: ScriptPipeEndpoint) -> Self {
        PipeEndpoint::Script(endpoint)
    }

    fn to_stream_handle(&self) -> StreamHandle {
        match self {
            PipeEndpoint::Stream(writer) => StreamHandle::Stream(writer.clone()),
            PipeEndpoint::Script(endpoint) => StreamHandle::Stream(endpoint.stream_handle()),
            PipeEndpoint::Inherit => StreamHandle::Inherit,
        }
    }
}

#[derive(Clone, Default)]
struct PipeOutputs {
    stdout: Option<PipeEndpoint>,
    stderr: Option<PipeEndpoint>,
}

struct ScriptPipe {
    inner: Arc<PipeInner>,
    reader: SharedInput,
}

impl ScriptPipe {
    fn new() -> Self {
        let inner = Arc::new(PipeInner::new());
        let reader: SharedInput = Arc::new(Mutex::new(PipeReader::new(inner.clone())));
        Self { inner, reader }
    }

    fn reader(&self) -> SharedInput {
        self.reader.clone()
    }

    fn endpoint(&self) -> ScriptPipeEndpoint {
        ScriptPipeEndpoint::new(self.inner.clone())
    }
}

#[derive(Clone)]
struct ScriptPipeEndpoint {
    inner: Arc<PipeInner>,
}

impl ScriptPipeEndpoint {
    fn new(inner: Arc<PipeInner>) -> Self {
        Self { inner }
    }

    fn stream_handle(&self) -> SharedOutput {
        Arc::new(Mutex::new(PipeWriter::new(self.inner.clone())))
    }
}

struct PipeInner {
    state: Mutex<PipeState>,
    ready: Condvar,
}

impl PipeInner {
    fn new() -> Self {
        Self {
            state: Mutex::new(PipeState::new()),
            ready: Condvar::new(),
        }
    }

    fn attach_writer(&self) {
        let mut state = self.lock_state();
        state.writers += 1;
        state.closed = false;
    }

    fn detach_writer(&self) {
        let mut state = self.lock_state();
        state.writers = state.writers.saturating_sub(1);
        if state.writers == 0 {
            state.closed = true;
        }
        drop(state);
        self.ready.notify_all();
    }

    fn push_bytes(&self, data: &[u8]) {
        let mut state = self.lock_state();
        state.buffer.extend(data.iter().copied());
        drop(state);
        self.ready.notify_all();
    }

    fn read_into(&self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut state = self.lock_state();
        loop {
            if !state.buffer.is_empty() {
                let mut read = 0;
                while read < buf.len() && !state.buffer.is_empty() {
                    if let Some(byte) = state.buffer.pop_front() {
                        buf[read] = byte;
                        read += 1;
                    }
                }
                return Ok(read);
            }
            if state.closed {
                return Ok(0);
            }
            state = self
                .ready
                .wait(state)
                .map_err(|_| io::Error::other("pipe wait poisoned"))?;
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, PipeState> {
        self.state.lock().expect("script pipe state poisoned")
    }
}

struct PipeState {
    buffer: VecDeque<u8>,
    writers: usize,
    closed: bool,
}

impl PipeState {
    fn new() -> Self {
        Self {
            buffer: VecDeque::new(),
            writers: 0,
            closed: false,
        }
    }
}

struct PipeReader {
    inner: Arc<PipeInner>,
}

impl PipeReader {
    fn new(inner: Arc<PipeInner>) -> Self {
        Self { inner }
    }
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read_into(buf)
    }
}

struct PipeWriter {
    inner: Arc<PipeInner>,
}

impl PipeWriter {
    fn new(inner: Arc<PipeInner>) -> Self {
        inner.attach_writer();
        Self { inner }
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.push_bytes(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        self.inner.detach_writer();
    }
}

#[derive(Clone, Default)]
pub struct ExecIo {
    stdin: Option<SharedInput>,
    stdout: Option<SharedOutput>,
    stderr: Option<SharedOutput>,
    input_pipes: HashMap<String, SharedInput>,
    output_pipes: HashMap<String, PipeOutputs>,
    inherit_env_overrides: HashMap<String, String>,
    inherit_env_removed: HashSet<String>,
}

#[derive(Clone)]
enum StreamHandle {
    Inherit,
    Stream(SharedOutput),
}

impl StreamHandle {
    fn to_stdout(&self) -> CommandStdout {
        match self {
            StreamHandle::Stream(writer) => CommandStdout::Stream(writer.clone()),
            StreamHandle::Inherit => CommandStdout::Inherit,
        }
    }

    fn to_stderr(&self) -> CommandStderr {
        match self {
            StreamHandle::Stream(writer) => CommandStderr::Stream(writer.clone()),
            StreamHandle::Inherit => CommandStderr::Inherit,
        }
    }
}

fn write_stdout<F>(handle: Option<StreamHandle>, op: F) -> Result<()>
where
    F: FnOnce(&mut dyn Write) -> Result<()>,
{
    if let Some(StreamHandle::Stream(writer)) = handle {
        if let Ok(mut guard) = writer.lock() {
            op(&mut *guard)?;
        }
        Ok(())
    } else {
        let mut stdout = io::stdout();
        op(&mut stdout)
    }
}

impl ExecIo {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_stdin(&mut self, stdin: Option<SharedInput>) {
        self.stdin = stdin;
    }

    pub fn set_stdout(&mut self, stdout: Option<SharedOutput>) {
        self.stdout = stdout.clone();
        if self.stderr.is_none() {
            self.stderr = stdout;
        }
    }

    pub fn set_stderr(&mut self, stderr: Option<SharedOutput>) {
        self.stderr = stderr;
    }

    pub fn insert_inherit_env<S: Into<String>, V: Into<String>>(&mut self, key: S, value: V) {
        let key = key.into();
        self.inherit_env_removed.remove(&key);
        self.inherit_env_overrides.insert(key, value.into());
    }

    pub fn remove_inherit_env<S: Into<String>>(&mut self, key: S) {
        let key = key.into();
        self.inherit_env_overrides.remove(&key);
        self.inherit_env_removed.insert(key);
    }

    pub fn inherit_env_value(&self, key: &str) -> Option<&String> {
        self.inherit_env_overrides.get(key)
    }

    pub fn inherit_env_is_removed(&self, key: &str) -> bool {
        self.inherit_env_removed.contains(key)
    }

    pub fn insert_input_pipe<S: Into<String>>(&mut self, name: S, reader: SharedInput) {
        self.input_pipes.insert(name.into(), reader);
    }

    pub fn insert_output_pipe<S: Into<String>>(&mut self, name: S, writer: SharedOutput) {
        let entry = self.output_pipes.entry(name.into()).or_default();
        entry.stdout = Some(PipeEndpoint::stream(writer.clone()));
        entry.stderr = Some(PipeEndpoint::stream(writer));
    }

    pub fn insert_output_pipe_stdout<S: Into<String>>(&mut self, name: S, writer: SharedOutput) {
        let entry = self.output_pipes.entry(name.into()).or_default();
        entry.stdout = Some(PipeEndpoint::stream(writer));
    }

    pub fn insert_output_pipe_stderr<S: Into<String>>(&mut self, name: S, writer: SharedOutput) {
        let entry = self.output_pipes.entry(name.into()).or_default();
        entry.stderr = Some(PipeEndpoint::stream(writer));
    }

    pub fn insert_output_pipe_stdout_inherit<S: Into<String>>(&mut self, name: S) {
        let entry = self.output_pipes.entry(name.into()).or_default();
        entry.stdout = Some(PipeEndpoint::Inherit);
    }

    pub fn insert_output_pipe_stderr_inherit<S: Into<String>>(&mut self, name: S) {
        let entry = self.output_pipes.entry(name.into()).or_default();
        entry.stderr = Some(PipeEndpoint::Inherit);
    }

    fn ensure_script_pipe(&mut self, name: &str) {
        if self.input_pipes.contains_key(name) || self.output_pipes.contains_key(name) {
            return;
        }

        let pipe = ScriptPipe::new();
        self.input_pipes.insert(name.to_string(), pipe.reader());
        let endpoint = PipeEndpoint::script(pipe.endpoint());
        let outputs = PipeOutputs {
            stdout: Some(endpoint.clone()),
            stderr: Some(endpoint),
        };
        self.output_pipes.insert(name.to_string(), outputs);
    }

    pub fn stdin(&self) -> Option<SharedInput> {
        self.stdin.clone()
    }

    pub fn stdout(&self) -> Option<SharedOutput> {
        self.stdout.clone()
    }

    pub fn stderr(&self) -> Option<SharedOutput> {
        self.stderr.clone().or_else(|| self.stdout.clone())
    }

    pub fn input_pipe(&self, name: &str) -> Option<SharedInput> {
        self.input_pipes.get(name).cloned()
    }

    fn output_pipe_stdout(&self, name: &str) -> Option<PipeEndpoint> {
        self.output_pipes
            .get(name)
            .and_then(|pipe| pipe.stdout.clone())
    }

    fn output_pipe_stderr(&self, name: &str) -> Option<PipeEndpoint> {
        self.output_pipes
            .get(name)
            .and_then(|pipe| pipe.stderr.clone())
    }
}

fn assemble_default_io(stdin: Option<SharedInput>, stdout: Option<SharedOutput>) -> ExecIo {
    let mut io = ExecIo::new();
    io.set_stdin(stdin);
    io.set_stdout(stdout.clone());
    io.set_stderr(stdout);
    io
}

struct ExecState<P: ProcessManager> {
    fs: Box<dyn WorkspaceFs>,
    cargo_target_dir: GuardedPath,
    cwd: GuardedPath,
    envs: HashMap<String, String>,
    bg_children: Vec<P::Handle>,
    scope_stack: Vec<ScopeSnapshot>,
    io: ExecIo,
}

struct ScopeSnapshot {
    cwd: GuardedPath,
    root: GuardedPath,
    envs: HashMap<String, String>,
}

impl<P: ProcessManager> ExecState<P> {
    fn command_ctx(&self) -> Result<CommandContext> {
        // Build a CommandContext snapshot for this step. The `cargo_target_dir`
        // here is the executor default; if callers want to override it they
        // must do so via the env map (e.g. ENV CARGO_TARGET_DIR=...), which
        // apply_ctx respects when spawning processes.
        Ok(CommandContext::new(
            &self.cwd.clone().into(),
            &self.envs,
            &self.cargo_target_dir,
            self.fs.root(),
            self.fs.build_context(),
        ))
    }
}

fn expand_template(t: &TemplateString, ctx: &CommandContext) -> String {
    expand_command_env(&t.0, ctx)
}

pub fn run_steps(fs_root: &GuardedPath, steps: &[Step]) -> Result<()> {
    run_steps_with_context(fs_root, fs_root, steps)
}

pub fn run_steps_with_context(
    fs_root: &GuardedPath,
    build_context: &GuardedPath,
    steps: &[Step],
) -> Result<()> {
    run_steps_with_context_result(fs_root, build_context, steps, None, None).map(|_| ())
}

/// Execute the DSL and return the final working directory after all steps.
pub fn run_steps_with_context_result(
    fs_root: &GuardedPath,
    build_context: &GuardedPath,
    steps: &[Step],
    stdin: Option<SharedInput>,
    stdout: Option<SharedOutput>,
) -> Result<GuardedPath> {
    let io = assemble_default_io(stdin, stdout);
    run_steps_with_context_result_with_io(fs_root, build_context, steps, io)
}

pub fn run_steps_with_context_result_with_io(
    fs_root: &GuardedPath,
    build_context: &GuardedPath,
    steps: &[Step],
    io: ExecIo,
) -> Result<GuardedPath> {
    match run_steps_inner(fs_root, build_context, steps, io) {
        Ok(final_cwd) => Ok(final_cwd),
        Err(err) => {
            // Compose a single error message with the top cause plus a compact fs snapshot.
            let chain = err.chain().map(|e| e.to_string()).collect::<Vec<_>>();
            let mut primary = chain
                .first()
                .cloned()
                .unwrap_or_else(|| "unknown error".into());
            let rest = if chain.len() > 1 {
                let first_cause = chain[1].clone();
                primary = format!("{primary} ({first_cause})");
                if chain.len() > 2 {
                    let causes = chain
                        .iter()
                        .skip(2)
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join("\n  ");
                    format!("\ncauses:\n  {}", causes)
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            let fs = PathResolver::new(fs_root.as_path(), build_context.as_path())?;
            let tree = describe_dir(&fs, fs_root, 2, 24);
            let snapshot = format!(
                "filesystem snapshot (root {}):\n{}",
                fs_root.display(),
                tree
            );
            let msg = format!("{}{}\n{}", primary, rest, snapshot);
            Err(anyhow::anyhow!(msg))
        }
    }
}

fn run_steps_inner(
    fs_root: &GuardedPath,
    build_context: &GuardedPath,
    steps: &[Step],
    io: ExecIo,
) -> Result<GuardedPath> {
    let mut resolver = PathResolver::new_guarded(fs_root.clone(), build_context.clone())?;
    resolver.set_workspace_root(build_context.clone());
    run_steps_with_fs_with_io(Box::new(resolver), steps, io)
}

pub fn run_steps_with_fs(
    fs: Box<dyn WorkspaceFs>,
    steps: &[Step],
    stdin: Option<SharedInput>,
    stdout: Option<SharedOutput>,
) -> Result<GuardedPath> {
    let io = assemble_default_io(stdin, stdout);
    run_steps_with_fs_with_io(fs, steps, io)
}

pub fn run_steps_with_fs_with_io(
    fs: Box<dyn WorkspaceFs>,
    steps: &[Step],
    io: ExecIo,
) -> Result<GuardedPath> {
    run_steps_with_manager(fs, steps, default_process_manager(), io)
}

fn run_steps_with_manager<P: ProcessManager>(
    fs: Box<dyn WorkspaceFs>,
    steps: &[Step],
    process: P,
    io: ExecIo,
) -> Result<GuardedPath> {
    let fs_root = fs.root().clone();
    let cwd = fs.root().clone();
    let build_context = fs.build_context().clone();
    let envs = BuiltinEnv::collect(&build_context).into_envs();
    let mut state = ExecState {
        fs,
        cargo_target_dir: fs_root.join(".cargo-target")?,
        cwd,
        envs,
        bg_children: Vec::new(),
        scope_stack: Vec::new(),
        io,
    };

    let _default_stdout = io::stdout();
    let stdin = state.io.stdin();
    let stdout = state.io.stdout().map(StreamHandle::Stream);
    let stderr = state.io.stderr().map(StreamHandle::Stream);
    let mut proc_mgr = process;
    execute_steps(
        &mut state,
        &mut proc_mgr,
        steps,
        stdin,
        false,
        stdout,
        stderr,
        true,
    )?;

    Ok(state.cwd)
}

#[allow(clippy::too_many_arguments)]
fn execute_steps<P: ProcessManager>(
    state: &mut ExecState<P>,
    process: &mut P,
    steps: &[Step],
    stdin: Option<SharedInput>,
    expose_stdin: bool,
    out: Option<StreamHandle>,
    err: Option<StreamHandle>,
    wait_at_end: bool,
) -> Result<()> {
    let fs_root = state.fs.root().clone();
    let build_context = state.fs.build_context().clone();

    // Inject stdin into envs if available and utf8
    // Note: We cannot peek at the stream without consuming it.
    // So we can't inject "stdin" env var if we are streaming.
    // The user wants streaming, so we skip env injection if we have a stream.
    // Or we only inject if it's not streaming?
    // The previous implementation took &[u8], so it was always buffered.
    // Now we have a stream. We can't buffer it just to put it in env.

    // let old_stdin = state.envs.get("stdin").cloned();
    // if let Some(data) = stdin {
    //     if let Ok(s) = std::str::from_utf8(data) {
    //         state.envs.insert("stdin".to_string(), s.to_string());
    //     }
    // }

    let check_bg = |bg: &mut Vec<P::Handle>| -> Result<Option<ExitStatus>> {
        let mut finished: Option<ExitStatus> = None;
        for child in bg.iter_mut() {
            if let Some(status) = child.try_wait()? {
                finished = Some(status);
                break;
            }
        }
        if let Some(status) = finished {
            // Tear down remaining background children.
            for child in bg.iter_mut() {
                if child.try_wait()?.is_none() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            bg.clear();
            return Ok(Some(status));
        }
        Ok(None)
    };

    for (idx, step) in steps.iter().enumerate() {
        if step.scope_enter > 0 {
            for _ in 0..step.scope_enter {
                state.scope_stack.push(ScopeSnapshot {
                    cwd: state.cwd.clone(),
                    root: state.fs.root().clone(),
                    envs: state.envs.clone(),
                });
            }
        }

        let should_run = oxdock_parser::guard_option_allows(step.guard.as_ref(), &state.envs);
        let step_result: Result<()> = if !should_run {
            Ok(())
        } else {
            match &step.kind {
                StepKind::InheritEnv { keys } => {
                    for key in keys {
                        if state.io.inherit_env_is_removed(key) {
                            state.envs.remove(key);
                            continue;
                        }
                        if let Some(value) = state.io.inherit_env_value(key).cloned() {
                            state.envs.insert(key.clone(), value);
                            continue;
                        }
                        if let Ok(value) = std::env::var(key) {
                            state.envs.insert(key.clone(), value);
                        }
                    }
                    Ok(())
                }
                StepKind::Workdir(path) => {
                    let ctx = state.command_ctx()?;
                    let rendered = expand_template(path, &ctx);
                    state.cwd = state
                        .fs
                        .resolve_workdir(&state.cwd, &rendered)
                        .with_context(|| format!("step {}: WORKDIR {}", idx + 1, rendered))?;
                    Ok(())
                }
                StepKind::Workspace(target) => {
                    match target {
                        WorkspaceTarget::Snapshot => {
                            state.fs.set_root(fs_root.clone());
                            state.cwd = state.fs.root().clone();
                        }
                        WorkspaceTarget::Local => {
                            state.fs.set_root(build_context.clone());
                            state.cwd = state.fs.root().clone();
                        }
                    }
                    Ok(())
                }
                StepKind::Env { key, value } => {
                    let ctx = state.command_ctx()?;
                    let rendered = expand_template(value, &ctx);
                    state.envs.insert(key.clone(), rendered);
                    Ok(())
                }
                StepKind::Run(cmd) => {
                    let ctx = state.command_ctx()?;
                    let rendered = expand_template(cmd, &ctx);
                    let step_stdin = if expose_stdin { stdin.clone() } else { None };

                    // Check for an environment variable that forces stdout inheritance.
                    // This is useful when we want to bypass output capturing (e.g. for build steps)
                    // and stream directly to the terminal, even if a capture stream was provided.
                    let inherit_override = state
                        .envs
                        .get("OXDOCK_INHERIT_STDOUT")
                        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                        .unwrap_or(false);

                    if std::env::var("OXBOOK_DEBUG").is_ok() {
                        eprintln!(
                            "DEBUG: step RUN {} inherit_override={}",
                            rendered, inherit_override
                        );
                    }

                    let stdout_mode = if inherit_override {
                        CommandStdout::Inherit
                    } else {
                        out.clone()
                            .map(|handle| handle.to_stdout())
                            .unwrap_or(CommandStdout::Inherit)
                    };
                    let stderr_mode = if inherit_override {
                        CommandStderr::Inherit
                    } else {
                        err.clone()
                            .map(|handle| handle.to_stderr())
                            .unwrap_or(CommandStderr::Inherit)
                    };

                    let mut options = CommandOptions::foreground();
                    options.stdin = step_stdin;
                    options.stdout = stdout_mode;
                    options.stderr = stderr_mode;
                    match process
                        .run_command(&ctx, &rendered, options)
                        .with_context(|| format!("step {}: RUN {}", idx + 1, rendered))?
                    {
                        CommandResult::Completed => Ok(()),
                        CommandResult::Captured(_) => {
                            bail!(
                                "step {}: RUN {} unexpectedly captured output",
                                idx + 1,
                                rendered
                            )
                        }
                        CommandResult::Background(_) => {
                            bail!(
                                "step {}: RUN {} returned background handle",
                                idx + 1,
                                rendered
                            )
                        }
                    }
                }
                StepKind::Echo(msg) => {
                    let ctx = state.command_ctx()?;
                    let rendered = expand_template(msg, &ctx);
                    write_stdout(out.clone(), |writer| {
                        writeln!(writer, "{}", rendered)?;
                        Ok(())
                    })?;
                    Ok(())
                }
                StepKind::RunBg(cmd) => {
                    let ctx = state.command_ctx()?;
                    let rendered = expand_template(cmd, &ctx);
                    let step_stdin = if expose_stdin { stdin.clone() } else { None };
                    let stdout_mode = out
                        .clone()
                        .map(|handle| handle.to_stdout())
                        .unwrap_or(CommandStdout::Inherit);
                    let stderr_mode = err
                        .clone()
                        .map(|handle| handle.to_stderr())
                        .unwrap_or(CommandStderr::Inherit);
                    let mut options = CommandOptions::background();
                    options.stdin = step_stdin;
                    options.stdout = stdout_mode;
                    options.stderr = stderr_mode;
                    match process
                        .run_command(&ctx, &rendered, options)
                        .with_context(|| format!("step {}: RUN_BG {}", idx + 1, rendered))?
                    {
                        CommandResult::Background(handle) => {
                            state.bg_children.push(handle);
                            Ok(())
                        }
                        CommandResult::Completed => {
                            bail!(
                                "step {}: RUN_BG {} finished synchronously",
                                idx + 1,
                                rendered
                            )
                        }
                        CommandResult::Captured(_) => {
                            bail!(
                                "step {}: RUN_BG {} attempted to capture output",
                                idx + 1,
                                rendered
                            )
                        }
                    }
                }
                StepKind::Copy {
                    from_current_workspace,
                    from,
                    to,
                } => {
                    let ctx = state.command_ctx()?;
                    let from_rendered = expand_template(from, &ctx);
                    let to_rendered = expand_template(to, &ctx);
                    let from_abs = if *from_current_workspace {
                        state
                            .fs
                            .resolve_copy_source_from_workspace(&from_rendered)
                            .with_context(|| {
                                format!("step {}: COPY {} {}", idx + 1, from_rendered, to_rendered)
                            })?
                    } else {
                        state
                            .fs
                            .resolve_copy_source(&from_rendered)
                            .with_context(|| {
                                format!("step {}: COPY {} {}", idx + 1, from_rendered, to_rendered)
                            })?
                    };
                    let to_abs = state
                        .fs
                        .resolve_write(&state.cwd, &to_rendered)
                        .with_context(|| {
                            format!("step {}: COPY {} {}", idx + 1, from_rendered, to_rendered)
                        })?;
                    copy_entry(state.fs.as_ref(), &from_abs, &to_abs).with_context(|| {
                        format!("step {}: COPY {} {}", idx + 1, from_rendered, to_rendered)
                    })?;
                    Ok(())
                }
                StepKind::CopyGit {
                    rev,
                    from,
                    to,
                    include_dirty,
                } => {
                    let ctx = state.command_ctx()?;
                    let rev_rendered = expand_template(rev, &ctx);
                    let from_rendered = expand_template(from, &ctx);
                    let to_rendered = expand_template(to, &ctx);
                    let to_abs = state
                        .fs
                        .resolve_write(&state.cwd, &to_rendered)
                        .with_context(|| {
                            format!(
                                "step {}: COPY_GIT {} {} {}",
                                idx + 1,
                                rev_rendered,
                                from_rendered,
                                to_rendered
                            )
                        })?;
                    state
                        .fs
                        .copy_from_git(&rev_rendered, &from_rendered, &to_abs, *include_dirty)
                        .with_context(|| {
                            format!(
                                "step {}: COPY_GIT {} {} {}",
                                idx + 1,
                                rev_rendered,
                                from_rendered,
                                to_rendered
                            )
                        })?;
                    Ok(())
                }
                StepKind::HashSha256 { path } => {
                    let ctx = state.command_ctx()?;
                    let rendered = expand_template(path, &ctx);
                    let target = state
                        .fs
                        .resolve_read(&state.cwd, &rendered)
                        .with_context(|| format!("step {}: HASH_SHA256 {}", idx + 1, rendered))?;
                    let mut hasher = Sha256::new();
                    hash_path(state.fs.as_ref(), &target, "", &mut hasher)?;
                    let digest = hasher.finalize();
                    write_stdout(out.clone(), |writer| {
                        writeln!(writer, "{:x}", digest)?;
                        Ok(())
                    })?;
                    Ok(())
                }

                StepKind::Symlink { from, to } => {
                    let ctx = state.command_ctx()?;
                    let from_rendered = expand_template(from, &ctx);
                    let to_rendered = expand_template(to, &ctx);
                    let to_abs = state
                        .fs
                        .resolve_write(&state.cwd, &to_rendered)
                        .with_context(|| {
                            format!(
                                "step {}: SYMLINK {} {}",
                                idx + 1,
                                from_rendered,
                                to_rendered
                            )
                        })?;
                    let from_abs =
                        state
                            .fs
                            .resolve_copy_source(&from_rendered)
                            .with_context(|| {
                                format!(
                                    "step {}: SYMLINK {} {}",
                                    idx + 1,
                                    from_rendered,
                                    to_rendered
                                )
                            })?;
                    state.fs.symlink(&from_abs, &to_abs).with_context(|| {
                        format!(
                            "step {}: SYMLINK {} {}",
                            idx + 1,
                            from_rendered,
                            to_rendered
                        )
                    })?;
                    Ok(())
                }
                StepKind::Mkdir(path) => {
                    let ctx = state.command_ctx()?;
                    let rendered = expand_template(path, &ctx);
                    let target = state
                        .fs
                        .resolve_write(&state.cwd, &rendered)
                        .with_context(|| format!("step {}: MKDIR {}", idx + 1, rendered))?;
                    state
                        .fs
                        .create_dir_all(&target)
                        .with_context(|| format!("failed to create dir {}", target.display()))?;
                    Ok(())
                }
                StepKind::Ls(arg) => {
                    let ctx = state.command_ctx()?;
                    let target_dir = if let Some(p) = arg {
                        let rendered = expand_template(p, &ctx);
                        state
                            .fs
                            .resolve_read(&state.cwd, &rendered)
                            .with_context(|| format!("step {}: LS {}", idx + 1, rendered))?
                    } else {
                        state.cwd.clone()
                    };
                    let mut entries =
                        state.fs.read_dir_entries(&target_dir).with_context(|| {
                            format!("step {}: LS {}", idx + 1, target_dir.display())
                        })?;
                    entries.sort_by_key(|e| e.file_name());
                    write_stdout(out.clone(), |writer| {
                        writeln!(writer, "{}:", target_dir.display())?;
                        for entry in &entries {
                            writeln!(writer, "{}", entry.file_name().to_string_lossy())?;
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
                StepKind::Cwd => {
                    // Print the canonical (physical) current working directory to stdout.
                    let real = canonical_cwd(state.fs.as_ref(), &state.cwd).with_context(|| {
                        format!(
                            "step {}: CWD failed to canonicalize {}",
                            idx + 1,
                            state.cwd.display()
                        )
                    })?;
                    write_stdout(out.clone(), |writer| {
                        writeln!(writer, "{}", real)?;
                        Ok(())
                    })?;
                    Ok(())
                }
                StepKind::Read(path_opt) => {
                    let data = if let Some(path) = path_opt {
                        let ctx = state.command_ctx()?;
                        let rendered = expand_template(path, &ctx);
                        let target = state
                            .fs
                            .resolve_read(&state.cwd, &rendered)
                            .with_context(|| format!("step {}: READ {}", idx + 1, rendered))?;
                        state
                            .fs
                            .read_file(&target)
                            .with_context(|| format!("failed to read {}", target.display()))?
                    } else {
                        let mut buf = Vec::new();
                        if let Some(input_stream) = stdin.clone()
                            && let Ok(mut guard) = input_stream.lock()
                        {
                            guard
                                .read_to_end(&mut buf)
                                .context("failed to read from stdin")?;
                        }

                        buf
                    };
                    write_stdout(out.clone(), |writer| {
                        writer
                            .write_all(&data)
                            .context("failed to write to output")?;
                        Ok(())
                    })?;
                    Ok(())
                }
                StepKind::Write { path, contents } => {
                    let ctx = state.command_ctx()?;
                    let path_rendered = expand_template(path, &ctx);
                    let target = state
                        .fs
                        .resolve_write(&state.cwd, &path_rendered)
                        .with_context(|| format!("step {}: WRITE {}", idx + 1, path_rendered))?;
                    state.fs.ensure_parent_dir(&target).with_context(|| {
                        format!("failed to create parent for {}", target.display())
                    })?;
                    if let Some(body) = contents {
                        let rendered = expand_template(body, &ctx);
                        state
                            .fs
                            .write_file(&target, rendered.as_bytes())
                            .with_context(|| format!("failed to write {}", target.display()))?;
                    } else {
                        let Some(input_stream) = stdin.clone() else {
                            bail!(
                                "step {}: WRITE {} requires stdin (use WITH_IO [stdin=...] WRITE)",
                                idx + 1,
                                path_rendered
                            );
                        };
                        let mut guard = input_stream
                            .lock()
                            .map_err(|_| anyhow!("failed to lock stdin for WRITE"))?;
                        let mut data = Vec::new();
                        guard
                            .read_to_end(&mut data)
                            .context("failed to read from stdin for WRITE")?;
                        drop(guard);
                        state
                            .fs
                            .write_file(&target, &data)
                            .with_context(|| format!("failed to write {}", target.display()))?;
                    }
                    Ok(())
                }
                StepKind::WithIo { bindings, cmd } => {
                    let inner_step = Step {
                        guard: None,
                        kind: *cmd.clone(),
                        scope_enter: 0,
                        scope_exit: 0,
                    };
                    let steps = vec![inner_step];

                    let mut step_stdin = None;
                    let mut step_stdout = out.clone();
                    let mut step_stderr = err.clone();
                    let mut next_expose_stdin = false;
                    let mut seen_stdin = false;
                    let mut seen_stdout = false;
                    let mut seen_stderr = false;

                    for binding in bindings {
                        if let Some(pipe) = &binding.pipe {
                            state.io.ensure_script_pipe(pipe);
                        }
                        match binding.stream {
                            IoStream::Stdin => {
                                if seen_stdin {
                                    bail!(
                                        "step {}: WITH_IO declared stdin more than once",
                                        idx + 1
                                    );
                                }
                                seen_stdin = true;
                                next_expose_stdin = true;
                                step_stdin = if let Some(pipe) = &binding.pipe {
                                    Some(state.io.input_pipe(pipe).ok_or_else(|| {
                                        anyhow!(
                                            "step {}: WITH_IO stdin pipe '{}' is undefined",
                                            idx + 1,
                                            pipe
                                        )
                                    })?)
                                } else {
                                    stdin.clone()
                                };
                            }
                            IoStream::Stdout => {
                                if seen_stdout {
                                    bail!(
                                        "step {}: WITH_IO declared stdout more than once",
                                        idx + 1
                                    );
                                }
                                seen_stdout = true;
                                step_stdout = if let Some(pipe) = &binding.pipe {
                                    Some(
                                        state
                                            .io
                                            .output_pipe_stdout(pipe)
                                            .ok_or_else(|| {
                                                anyhow!(
                                                    "step {}: WITH_IO stdout pipe '{}' is undefined",
                                                    idx + 1,
                                                    pipe
                                                )
                                            })?
                                            .to_stream_handle(),
                                    )
                                } else {
                                    out.clone()
                                };
                            }
                            IoStream::Stderr => {
                                if seen_stderr {
                                    bail!(
                                        "step {}: WITH_IO declared stderr more than once",
                                        idx + 1
                                    );
                                }
                                seen_stderr = true;
                                step_stderr = if let Some(pipe) = &binding.pipe {
                                    Some(
                                        state
                                            .io
                                            .output_pipe_stderr(pipe)
                                            .ok_or_else(|| {
                                                anyhow!(
                                                    "step {}: WITH_IO stderr pipe '{}' is undefined",
                                                    idx + 1,
                                                    pipe
                                                )
                                            })?
                                            .to_stream_handle(),
                                    )
                                } else {
                                    err.clone()
                                };
                            }
                        }
                    }

                    execute_steps(
                        state,
                        process,
                        &steps,
                        step_stdin,
                        next_expose_stdin,
                        step_stdout,
                        step_stderr,
                        false,
                    )?;
                    Ok(())
                }
                StepKind::WithIoBlock { .. } => {
                    bail!("WITH_IO block should have been expanded during parsing")
                }

                StepKind::Exit(code) => {
                    for child in state.bg_children.iter_mut() {
                        if child.try_wait()?.is_none() {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                    }
                    state.bg_children.clear();
                    bail!("EXIT requested with code {}", code);
                }
            }
        };

        let restore_result = restore_scopes(state, step.scope_exit);
        step_result?;
        restore_result?;
        if let Some(status) = check_bg(&mut state.bg_children)? {
            if status.success() {
                return Ok(());
            } else {
                bail!("RUN_BG exited with status {}", status);
            }
        }
    }

    if wait_at_end && !state.bg_children.is_empty() {
        let mut first = state.bg_children.remove(0);
        let status = first.wait()?;
        for child in state.bg_children.iter_mut() {
            if child.try_wait()?.is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        state.bg_children.clear();
        if status.success() {
            return Ok(());
        } else {
            bail!("RUN_BG exited with status {}", status);
        }
    }

    Ok(())
}

fn restore_scopes<P: ProcessManager>(state: &mut ExecState<P>, count: usize) -> Result<()> {
    for _ in 0..count {
        let snapshot = state
            .scope_stack
            .pop()
            .ok_or_else(|| anyhow!("scope stack underflow during pop"))?;
        state.fs.set_root(snapshot.root.clone());
        state.cwd = snapshot.cwd;
        state.envs = snapshot.envs;
    }
    Ok(())
}

fn copy_entry(fs: &dyn WorkspaceFs, src: &GuardedPath, dst: &GuardedPath) -> Result<()> {
    match fs.entry_kind(src)? {
        EntryKind::Dir => {
            fs.copy_dir_recursive(src, dst)?;
        }
        EntryKind::File => {
            fs.ensure_parent_dir(dst)?;
            fs.copy_file(src, dst)?;
        }
    }
    Ok(())
}

fn canonical_cwd(fs: &dyn WorkspaceFs, cwd: &GuardedPath) -> Result<String> {
    Ok(fs.canonicalize(cwd)?.display().to_string())
}

fn describe_dir(
    fs: &dyn WorkspaceFs,
    root: &GuardedPath,
    max_depth: usize,
    max_entries: usize,
) -> String {
    fn helper(
        fs: &dyn WorkspaceFs,
        guard_root: &GuardedPath,
        path: &GuardedPath,
        depth: usize,
        max_depth: usize,
        left: &mut usize,
        out: &mut String,
    ) {
        if *left == 0 {
            return;
        }
        let indent = "  ".repeat(depth);
        if depth > 0 {
            out.push_str(&format!(
                "{}{}\n",
                indent,
                path.as_path()
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            ));
        }
        if depth >= max_depth {
            return;
        }
        let entries = match fs.read_dir_entries(path) {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut names: Vec<_> = entries.into_iter().collect();
        names.sort_by_key(|a| a.file_name());
        for entry in names {
            if *left == 0 {
                return;
            }
            *left -= 1;
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            let p = entry.path();
            let guarded_child = match GuardedPath::new(guard_root.root(), &p) {
                Ok(child) => child,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                helper(
                    fs,
                    guard_root,
                    &guarded_child,
                    depth + 1,
                    max_depth,
                    left,
                    out,
                );
            } else {
                out.push_str(&format!(
                    "{}  {}\n",
                    indent,
                    entry.file_name().to_string_lossy()
                ));
            }
        }
    }

    let mut out = String::new();
    let mut left = max_entries;
    helper(fs, root, root, 0, max_depth, &mut left, &mut out);
    out
}

fn hash_path(
    fs: &dyn WorkspaceFs,
    path: &GuardedPath,
    rel: &str,
    hasher: &mut Sha256,
) -> Result<()> {
    match fs.entry_kind(path)? {
        EntryKind::Dir => {
            hasher.update(b"D\0");
            hasher.update(rel.as_bytes());
            hasher.update(b"\0");
            let mut entries = fs.read_dir_entries(path)?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let name = entry.file_name().to_string_lossy().to_string();
                let child = path.join(&name)?;
                let child_rel = if rel.is_empty() {
                    name
                } else {
                    format!("{}/{}", rel, name)
                };
                let child_rel = to_forward_slashes(&child_rel);
                hash_path(fs, &child, &child_rel, hasher)?;
            }
        }
        EntryKind::File => {
            let data = fs.read_file(path)?;
            if rel.is_empty() {
                hasher.update(&data);
            } else {
                hasher.update(b"F\0");
                hasher.update(rel.as_bytes());
                hasher.update(b"\0");
                hasher.update(&data);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxdock_fs::{GuardedPath, MockFs};
    use oxdock_parser::{Guard, GuardExpr, IoBinding, IoStream};
    use oxdock_process::{MockProcessManager, MockRunCall};
    use std::collections::HashMap;

    #[test]
    fn run_records_env_and_cwd() {
        let root = GuardedPath::new_root_from_str(".").unwrap();
        let steps = vec![
            Step {
                guard: None,
                kind: StepKind::Env {
                    key: "FOO".into(),
                    value: "bar".into(),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Run("echo hi".into()),
                scope_enter: 0,
                scope_exit: 0,
            },
        ];
        let mock = MockProcessManager::default();
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
        let runs = mock.recorded_runs();
        assert_eq!(runs.len(), 1);
        let MockRunCall {
            script,
            cwd,
            envs,
            cargo_target_dir,
            ..
        } = &runs[0];
        assert_eq!(script, "echo hi");
        assert_eq!(cwd, root.as_path());
        assert_eq!(
            cargo_target_dir,
            &root.join(".cargo-target").unwrap().to_path_buf()
        );
        assert_eq!(envs.get("FOO"), Some(&"bar".into()));
    }

    #[test]
    fn run_expands_env_values() {
        let root = GuardedPath::new_root_from_str(".").unwrap();
        let steps = vec![
            Step {
                guard: None,
                kind: StepKind::Env {
                    key: "FOO".into(),
                    value: "bar".into(),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Run("echo {{ env:FOO }}".into()),
                scope_enter: 0,
                scope_exit: 0,
            },
        ];
        let mock = MockProcessManager::default();
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
        let runs = mock.recorded_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].script, "echo bar");
    }

    #[test]
    fn run_bg_completion_short_circuits_pipeline() {
        let root = GuardedPath::new_root_from_str(".").unwrap();
        let steps = vec![
            Step {
                guard: None,
                kind: StepKind::RunBg("sleep".into()),
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Run("echo after".into()),
                scope_enter: 0,
                scope_exit: 0,
            },
        ];
        let mock = MockProcessManager::default();
        mock.push_bg_plan(0, success_status());
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
        assert!(
            mock.recorded_runs().is_empty(),
            "foreground run should not execute when RUN_BG completes early"
        );
        let spawns = mock.spawn_log();
        let spawned: Vec<_> = spawns.iter().map(|c| c.script.as_str()).collect();
        assert_eq!(spawned, vec!["sleep"]);
    }

    #[test]
    fn exit_kills_background_processes() {
        let root = GuardedPath::new_root_from_str(".").unwrap();
        let steps = vec![
            Step {
                guard: None,
                kind: StepKind::RunBg("bg-task".into()),
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Exit(5),
                scope_enter: 0,
                scope_exit: 0,
            },
        ];
        let mock = MockProcessManager::default();
        mock.push_bg_plan(usize::MAX, success_status());
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        let err = run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap_err();
        assert!(
            err.to_string().contains("EXIT requested with code 5"),
            "unexpected error: {err}"
        );
        assert_eq!(mock.killed(), vec!["bg-task"]);
    }

    #[test]
    fn symlink_errors_report_underlying_cause() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = temp.as_guarded_path().clone();
        let steps = vec![
            Step {
                guard: None,
                kind: StepKind::Mkdir("client".into()),
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Symlink {
                    from: "client".into(),
                    to: "client".into(),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
        ];
        let err = run_steps(&root, &steps).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("step 2: SYMLINK client client"),
            "error should include step context: {msg}"
        );
        assert!(
            msg.contains("SYMLINK destination already exists"),
            "error should surface underlying cause: {msg}"
        );
    }

    #[test]
    fn guarded_run_waits_for_env_to_be_set() {
        let root = GuardedPath::new_root_from_str(".").unwrap();
        let guard = Guard::EnvEquals {
            key: "READY".into(),
            value: "1".into(),
            invert: false,
        };
        let steps = vec![
            Step {
                guard: Some(guard.clone().into()),
                kind: StepKind::Run("echo first".into()),
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Env {
                    key: "READY".into(),
                    value: "1".into(),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: Some(guard.into()),
                kind: StepKind::Run("echo second".into()),
                scope_enter: 0,
                scope_exit: 0,
            },
        ];
        let mock = MockProcessManager::default();
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
        let runs = mock.recorded_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].script, "echo second");
    }

    #[test]
    fn guard_groups_allow_any_matching_branch() {
        let root = GuardedPath::new_root_from_str(".").unwrap();
        let guard_alpha = Guard::EnvEquals {
            key: "MODE".into(),
            value: "alpha".into(),
            invert: false,
        };
        let guard_beta = Guard::EnvEquals {
            key: "MODE".into(),
            value: "beta".into(),
            invert: false,
        };
        let steps = vec![
            Step {
                guard: None,
                kind: StepKind::Env {
                    key: "MODE".into(),
                    value: "beta".into(),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: Some(GuardExpr::or(vec![guard_alpha.into(), guard_beta.into()])),
                kind: StepKind::Run("echo guarded".into()),
                scope_enter: 0,
                scope_exit: 0,
            },
        ];
        let mock = MockProcessManager::default();
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap();
        let runs = mock.recorded_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].script, "echo guarded");
    }

    #[test]
    fn with_io_pipe_routes_stdout_to_run_stdin() {
        let steps = vec![
            Step {
                guard: None,
                kind: StepKind::WithIo {
                    bindings: vec![IoBinding {
                        stream: IoStream::Stdout,
                        pipe: Some("shared".into()),
                    }],
                    cmd: Box::new(StepKind::Echo("hello".into())),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::WithIo {
                    bindings: vec![IoBinding {
                        stream: IoStream::Stdin,
                        pipe: Some("shared".into()),
                    }],
                    cmd: Box::new(StepKind::Run("cat".into())),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
        ];

        let fs = MockFs::new();
        let mut state = create_exec_state(fs);
        let mut proc = MockProcessManager::default();
        execute_steps(&mut state, &mut proc, &steps, None, false, None, None, true)
            .expect("pipeline executes");

        let runs = proc.recorded_runs();
        assert_eq!(runs.len(), 1);
        let MockRunCall { stdin, .. } = &runs[0];
        assert_eq!(stdin.as_deref(), Some(b"hello\n".as_slice()));
    }

    fn success_status() -> ExitStatus {
        exit_status_from_code(0)
    }

    #[cfg(unix)]
    fn exit_status_from_code(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatusExt::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn exit_status_from_code(code: i32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatusExt::from_raw(code as u32)
    }

    fn create_exec_state(fs: MockFs) -> ExecState<MockProcessManager> {
        let cargo = fs.root().join(".cargo-target").unwrap();
        ExecState {
            fs: Box::new(fs.clone()),
            cargo_target_dir: cargo,
            cwd: fs.root().clone(),
            envs: HashMap::new(),
            bg_children: Vec::new(),
            scope_stack: Vec::new(),
            io: ExecIo::new(),
        }
    }

    fn run_with_mock_fs(steps: &[Step]) -> (GuardedPath, HashMap<String, Vec<u8>>) {
        let fs = MockFs::new();
        let mut state = create_exec_state(fs.clone());
        let mut proc = MockProcessManager::default();
        execute_steps(&mut state, &mut proc, steps, None, false, None, None, true).unwrap();
        (state.cwd, fs.snapshot())
    }

    #[test]
    fn mock_fs_handles_workdir_and_write() {
        let steps = vec![
            Step {
                guard: None,
                kind: StepKind::Mkdir("app".into()),
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Workdir("app".into()),
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Write {
                    path: "out.txt".into(),
                    contents: Some("hi".into()),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Read(Some("out.txt".into())),
                scope_enter: 0,
                scope_exit: 0,
            },
        ];
        let (_cwd, files) = run_with_mock_fs(&steps);
        let written = files
            .iter()
            .find(|(k, _)| k.ends_with("app/out.txt"))
            .map(|(_, v)| String::from_utf8_lossy(v).to_string());
        assert_eq!(written, Some("hi".into()));
    }

    #[test]
    fn write_interpolates_env_values() {
        let steps = vec![
            Step {
                guard: None,
                kind: StepKind::Env {
                    key: "FOO".into(),
                    value: "bar".into(),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Env {
                    key: "BAZ".into(),
                    value: "{{ env:FOO }}-baz".into(),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Write {
                    path: "out.txt".into(),
                    contents: Some("val {{ env:BAZ }}".into()),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
        ];
        let (_cwd, files) = run_with_mock_fs(&steps);
        let written = files
            .iter()
            .find(|(k, _)| k.ends_with("out.txt"))
            .map(|(_, v)| String::from_utf8_lossy(v).to_string());
        assert_eq!(written, Some("val bar-baz".into()));
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
    #[test]
    fn cat_and_capture_expand_env_paths() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let steps = vec![
            Step {
                guard: None,
                kind: StepKind::Write {
                    path: "snippet.txt".into(),
                    contents: Some("payload".into()),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Env {
                    key: "SNIPPET".into(),
                    value: "snippet.txt".into(),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Env {
                    key: "OUT_FILE".into(),
                    value: "cat-{{ env:SNIPPET }}".into(),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::WithIo {
                    bindings: vec![IoBinding {
                        stream: IoStream::Stdout,
                        pipe: Some("cap-cat".to_string()),
                    }],
                    cmd: Box::new(StepKind::Read(Some("{{ env:SNIPPET }}".into()))),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::WithIo {
                    bindings: vec![IoBinding {
                        stream: IoStream::Stdin,
                        pipe: Some("cap-cat".to_string()),
                    }],
                    cmd: Box::new(StepKind::Write {
                        path: "{{ env:OUT_FILE }}".into(),
                        contents: None,
                    }),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
        ];
        run_steps(&root, &steps).expect("capture with env paths succeeds");
        let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
        let captured_path = root.join("cat-snippet.txt").expect("capture path");
        let contents = resolver
            .read_to_string(&captured_path)
            .expect("read captured output");
        assert_eq!(contents, "payload");
    }

    #[test]
    fn final_cwd_tracks_last_workdir() {
        let steps = vec![
            Step {
                guard: None,
                kind: StepKind::Write {
                    path: "temp.txt".into(),
                    contents: Some("123".into()),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Workdir("sub".into()),
                scope_enter: 0,
                scope_exit: 0,
            },
        ];
        let (cwd, snapshot) = run_with_mock_fs(&steps);
        assert!(
            cwd.as_path().ends_with("sub"),
            "expected final cwd to match last WORKDIR, got {}",
            cwd.display()
        );
        let keys: Vec<_> = snapshot.keys().cloned().collect();
        assert!(
            keys.iter().any(|path| path.ends_with("temp.txt")),
            "WRITE should produce temp file, snapshot: {:?}",
            keys
        );
    }

    #[test]
    fn mock_fs_normalizes_backslash_workdir() {
        let steps = vec![
            Step {
                guard: None,
                kind: StepKind::Mkdir("win\\nested".into()),
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Workdir("win\\nested".into()),
                scope_enter: 0,
                scope_exit: 0,
            },
            Step {
                guard: None,
                kind: StepKind::Write {
                    path: "inner.txt".into(),
                    contents: Some("ok".into()),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
        ];
        let (cwd, snapshot) = run_with_mock_fs(&steps);
        let cwd_display = cwd.display().to_string();
        assert!(
            cwd_display.ends_with("win\\nested") || cwd_display.ends_with("win/nested"),
            "expected cwd to normalize backslashes, got {cwd_display}"
        );
        assert!(
            snapshot
                .keys()
                .any(|path| path.ends_with("win/nested/inner.txt")),
            "expected file under normalized path, snapshot: {:?}",
            snapshot.keys()
        );
    }

    #[cfg(windows)]
    #[test]
    fn mock_fs_rejects_absolute_windows_paths() {
        let steps = vec![Step {
            guard: None,
            kind: StepKind::Workdir(r"C:\outside".into()),
            scope_enter: 0,
            scope_exit: 0,
        }];
        let fs = MockFs::new();
        let mut state = create_exec_state(fs);
        let mut proc = MockProcessManager::default();
        let sink = Arc::new(Mutex::new(Vec::new()));
        let err = execute_steps(
            &mut state,
            &mut proc,
            &steps,
            None,
            false,
            Some(StreamHandle::Stream(sink.clone())),
            Some(StreamHandle::Stream(sink)),
            false,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("escapes allowed root"),
            "unexpected error for absolute Windows path: {msg}"
        );
    }

    #[test]
    fn with_stdin_passes_content_to_run() {
        let steps = vec![Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![IoBinding {
                    stream: IoStream::Stdin,
                    pipe: None,
                }],
                cmd: Box::new(StepKind::Run("cat".into())),
            },
            scope_enter: 0,
            scope_exit: 0,
        }];

        let mock = MockProcessManager::default();
        let root = GuardedPath::new_root_from_str(".").unwrap();
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());

        let input = Arc::new(Mutex::new(std::io::Cursor::new(b"hello world".to_vec())));

        let mut io_cfg = ExecIo::new();
        io_cfg.set_stdin(Some(input));

        run_steps_with_manager(fs, &steps, mock.clone(), io_cfg).unwrap();

        let runs = mock.recorded_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].script, "cat");
        assert_eq!(runs[0].stdin, Some(b"hello world".to_vec()));
    }

    fn failing_status() -> ExitStatus {
        exit_status_from_code(9)
    }

    fn step<T>(kind: T) -> Step
    where
        T: Into<StepKind>,
    {
        Step {
            guard: None,
            kind: kind.into(),
            scope_enter: 0,
            scope_exit: 0,
        }
    }

    #[test]
    fn bg_failure_mid_pipeline_short_circuits_and_bails() {
        let root = GuardedPath::new_root_from_str(".").unwrap();
        let steps = vec![
            step(StepKind::RunBg("flaky-bg".into())),
            step(StepKind::Run("echo never".into())),
        ];
        let mock = MockProcessManager::default();
        mock.push_bg_plan(0, failing_status());
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        let err = run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap_err();

        assert!(
            err.to_string().contains("RUN_BG exited with status"),
            "unexpected error: {err}"
        );
        assert!(
            mock.recorded_runs().is_empty(),
            "pipeline must stop before the next RUN"
        );
        let spawns = mock.spawn_log();
        let spawned: Vec<&str> = spawns.iter().map(|call| call.script.as_str()).collect();
        assert_eq!(spawned, vec!["flaky-bg"]);
        drop(spawns);
        // The finished child is not killed again; only survivors would be.
        assert!(mock.killed().is_empty());
    }

    #[test]
    fn bg_failure_after_pipeline_end_reports_status() {
        let root = GuardedPath::new_root_from_str(".").unwrap();
        let steps = vec![step(StepKind::RunBg("late-failure".into()))];
        let mock = MockProcessManager::default();
        // usize::MAX models a child that never polls ready; end-of-pipeline
        // `wait()` must still surface its failing status.
        mock.push_bg_plan(usize::MAX, failing_status());
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        let err = run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap_err();
        assert!(
            err.to_string().contains("RUN_BG exited with status"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bg_success_after_pipeline_end_waits_cleanly() {
        let root = GuardedPath::new_root_from_str(".").unwrap();
        let steps = vec![step(StepKind::RunBg("late-success".into()))];
        let mock = MockProcessManager::default();
        mock.push_bg_plan(usize::MAX, success_status());
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new())
            .expect("successful late child must not fail the pipeline");
    }

    #[test]
    fn multi_child_teardown_kills_survivor_when_first_exits() {
        let root = GuardedPath::new_root_from_str(".").unwrap();
        let steps = vec![
            step(StepKind::RunBg("first-finisher".into())),
            step(StepKind::RunBg("survivor".into())),
        ];
        let mock = MockProcessManager::default();
        // First child reports finished on the SECOND poll (after the second
        // RUN_BG has spawned); the survivor must then be torn down.
        mock.push_bg_plan(1, success_status());
        mock.push_bg_plan(usize::MAX, success_status());
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new())
            .expect("first child succeeded");

        assert_eq!(mock.killed(), vec!["survivor".to_string()]);
    }

    #[test]
    fn exit_kills_all_background_children() {
        let root = GuardedPath::new_root_from_str(".").unwrap();
        let steps = vec![
            step(StepKind::RunBg("bg-a".into())),
            step(StepKind::RunBg("bg-b".into())),
            step(StepKind::Exit(3)),
        ];
        let mock = MockProcessManager::default();
        mock.push_bg_plan(usize::MAX, success_status());
        mock.push_bg_plan(usize::MAX, success_status());
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        let err = run_steps_with_manager(fs, &steps, mock.clone(), ExecIo::new()).unwrap_err();
        assert!(err.to_string().contains("EXIT requested with code 3"));
        assert_eq!(mock.killed(), vec!["bg-a".to_string(), "bg-b".to_string()]);
    }

    /// Minimal stub whose foreground commands fail by script name, letting us
    /// drive failure paths the stock mock cannot express.
    #[derive(Clone, Default)]
    struct FailingRunner {
        calls: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
        fail_script: String,
    }

    impl ProcessManager for FailingRunner {
        type Handle = oxdock_process::MockHandle;

        fn run_command(
            &mut self,
            _ctx: &CommandContext,
            script: &str,
            _options: CommandOptions,
        ) -> Result<CommandResult<Self::Handle>> {
            self.calls.borrow_mut().push(script.to_string());
            if script == self.fail_script {
                bail!("simulated failure")
            }
            Ok(CommandResult::Completed)
        }
    }

    #[test]
    fn failing_foreground_run_aborts_with_step_context() {
        let root = GuardedPath::new_root_from_str(".").unwrap();
        let runner = FailingRunner {
            calls: Default::default(),
            fail_script: "boom".into(),
        };
        let calls = runner.calls.clone();
        let steps = vec![
            step(StepKind::Run("ok-first".into())),
            step(StepKind::Run("boom".into())),
            step(StepKind::Run("never-reached".into())),
        ];
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        let err = run_steps_with_manager(fs, &steps, runner, ExecIo::new()).unwrap_err();

        let msg = format!("{err:#}");
        assert!(
            msg.contains("step 2: RUN boom") && msg.contains("simulated failure"),
            "error must carry step index and cause, got: {msg}"
        );
        assert_eq!(
            *calls.borrow(),
            vec!["ok-first".to_string(), "boom".to_string()]
        );
    }

    #[test]
    fn with_io_rejects_duplicate_stdout_binding() {
        let steps = vec![Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings: vec![
                    IoBinding {
                        stream: IoStream::Stdout,
                        pipe: Some("p".into()),
                    },
                    IoBinding {
                        stream: IoStream::Stdout,
                        pipe: Some("p".into()),
                    },
                ],
                cmd: Box::new(StepKind::Echo("x".into())),
            },
            scope_enter: 0,
            scope_exit: 0,
        }];
        let fs = MockFs::new();
        let mut state = create_exec_state(fs);
        let mut proc = MockProcessManager::default();
        let err = execute_steps(&mut state, &mut proc, &steps, None, false, None, None, true)
            .expect_err("duplicate stdout binding");
        assert!(
            err.to_string().contains("declared stdout more than once"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn with_io_rejects_duplicate_stdin_and_stderr_bindings() {
        for (variant, fragment) in [
            ("stdin", "stdin more than once"),
            ("stderr", "stderr more than once"),
        ] {
            let (stream_a, stream_b) = if variant == "stdin" {
                (IoStream::Stdin, IoStream::Stdin)
            } else {
                (IoStream::Stderr, IoStream::Stderr)
            };
            let steps = vec![Step {
                guard: None,
                kind: StepKind::WithIo {
                    bindings: vec![
                        IoBinding {
                            stream: stream_a,
                            pipe: Some("p".into()),
                        },
                        IoBinding {
                            stream: stream_b,
                            pipe: Some("p".into()),
                        },
                    ],
                    cmd: Box::new(StepKind::Echo("x".into())),
                },
                scope_enter: 0,
                scope_exit: 0,
            }];
            let fs = MockFs::new();
            let mut state = create_exec_state(fs);
            let mut proc = MockProcessManager::default();
            let err = execute_steps(&mut state, &mut proc, &steps, None, false, None, None, true)
                .expect_err("duplicate binding");
            assert!(
                err.to_string().contains(fragment),
                "expected '{fragment}', got: {err}"
            );
        }
    }

    #[test]
    fn with_io_block_form_bails_unexpanded() {
        let steps = vec![Step {
            guard: None,
            kind: StepKind::WithIoBlock {
                bindings: Vec::new(),
            },
            scope_enter: 0,
            scope_exit: 0,
        }];
        let fs = MockFs::new();
        let mut state = create_exec_state(fs);
        let mut proc = MockProcessManager::default();
        let err = execute_steps(&mut state, &mut proc, &steps, None, false, None, None, true)
            .expect_err("unexpanded WITH_IO block");
        assert!(err.to_string().contains("expanded during parsing"));
    }

    #[test]
    fn write_without_contents_or_stdin_bails() {
        let steps = vec![step(StepKind::Write {
            path: "out.txt".into(),
            contents: None,
        })];
        let fs = MockFs::new();
        let mut state = create_exec_state(fs);
        let mut proc = MockProcessManager::default();
        let err = execute_steps(&mut state, &mut proc, &steps, None, false, None, None, true)
            .expect_err("write without source");
        assert!(
            err.to_string().contains("requires stdin"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn stderr_stream_handle_reaches_manager() {
        let sink: SharedOutput = Arc::new(Mutex::new(Vec::<u8>::new()));
        let steps = vec![step(StepKind::Run("emits-stderr".into()))];
        let fs = MockFs::new();
        let mut state = create_exec_state(fs);
        let mut proc = MockProcessManager::default();
        execute_steps(
            &mut state,
            &mut proc,
            &steps,
            None,
            false,
            None,
            Some(StreamHandle::Stream(sink)),
            true,
        )
        .expect("run");

        let runs = proc.recorded_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].stderr_mode,
            oxdock_process::MockStreamMode::Stream,
            "stderr handle must be forwarded as CommandStderr::Stream"
        );
    }

    #[test]
    fn inherit_stdout_override_forces_inherit_modes() {
        let out_sink: SharedOutput = Arc::new(Mutex::new(Vec::<u8>::new()));
        let err_sink: SharedOutput = Arc::new(Mutex::new(Vec::<u8>::new()));
        let steps = vec![
            step(StepKind::Env {
                key: "OXDOCK_INHERIT_STDOUT".into(),
                value: "1".into(),
            }),
            step(StepKind::Run("captured-normally".into())),
        ];
        let fs = MockFs::new();
        let mut state = create_exec_state(fs);
        let mut proc = MockProcessManager::default();
        execute_steps(
            &mut state,
            &mut proc,
            &steps,
            None,
            false,
            Some(StreamHandle::Stream(out_sink)),
            Some(StreamHandle::Stream(err_sink)),
            true,
        )
        .expect("run");

        let runs = proc.recorded_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].stderr_mode,
            oxdock_process::MockStreamMode::Inherit,
            "OXDOCK_INHERIT_STDOUT must force stderr inheritance too"
        );
    }

    #[test]
    fn exec_io_stderr_precedence_and_stdout_fallback() {
        let out_sink: SharedOutput = Arc::new(Mutex::new(Vec::<u8>::new()));
        let err_sink: SharedOutput = Arc::new(Mutex::new(Vec::<u8>::new()));

        // set_stdout seeds stderr only while stderr is unset.
        let mut io = ExecIo::new();
        io.set_stdout(Some(out_sink.clone()));
        assert!(Arc::ptr_eq(&io.stderr().unwrap(), &out_sink));

        // An explicit stderr wins over the stdout fallback...
        io.set_stderr(Some(err_sink.clone()));
        assert!(Arc::ptr_eq(&io.stderr().unwrap(), &err_sink));
        // ...and survives later stdout changes.
        let replacement: SharedOutput = Arc::new(Mutex::new(Vec::<u8>::new()));
        io.set_stdout(Some(replacement));
        assert!(Arc::ptr_eq(&io.stderr().unwrap(), &err_sink));

        // With no streams at all, stderr falls back to nothing.
        let bare = ExecIo::new();
        assert!(bare.stderr().is_none());
    }

    #[test]
    fn exec_io_inherit_env_state_machine_round_trips() {
        let mut io = ExecIo::new();
        io.insert_inherit_env("K", "v1");
        assert_eq!(io.inherit_env_value("K"), Some(&"v1".to_string()));
        assert!(!io.inherit_env_is_removed("K"));

        io.remove_inherit_env("K");
        assert!(io.inherit_env_is_removed("K"));
        assert_eq!(io.inherit_env_value("K"), None);

        // Re-inserting after removal must clear the removed marker.
        io.insert_inherit_env("K", "v2");
        assert!(!io.inherit_env_is_removed("K"));
        assert_eq!(io.inherit_env_value("K"), Some(&"v2".to_string()));
    }

    #[test]
    fn exec_io_pipe_endpoints_expose_streams_and_inherit() {
        let mut io = ExecIo::new();
        let writer: SharedOutput = Arc::new(Mutex::new(Vec::<u8>::new()));

        io.insert_output_pipe_stdout("s-out", writer.clone());
        io.insert_output_pipe_stderr_inherit("s-inh");

        match io.output_pipe_stdout("s-out") {
            Some(PipeEndpoint::Stream(w)) => assert!(Arc::ptr_eq(&w, &writer)),
            Some(PipeEndpoint::Script(_)) => {
                panic!("expected streamed stdout endpoint, got script endpoint")
            }
            Some(PipeEndpoint::Inherit) => panic!("expected streamed stdout endpoint, got inherit"),
            None => panic!("endpoint missing entirely"),
        }
        match io.output_pipe_stderr("s-inh") {
            Some(PipeEndpoint::Inherit) => {}
            _ => panic!("expected inherit stderr endpoint"),
        }
    }

    #[test]
    fn hash_sha256_matches_known_digest_for_file() {
        // sha256("hello")
        const HELLO_DIGEST: &str =
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

        let backing = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink: SharedOutput = backing.clone();
        let steps = vec![
            step(StepKind::Write {
                path: "hello.txt".into(),
                contents: Some("hello".into()),
            }),
            step(StepKind::HashSha256 {
                path: "hello.txt".into(),
            }),
        ];
        let fs = MockFs::new();
        let mut state = create_exec_state(fs);
        let mut proc = MockProcessManager::default();
        execute_steps(
            &mut state,
            &mut proc,
            &steps,
            None,
            false,
            Some(StreamHandle::Stream(sink.clone())),
            None,
            true,
        )
        .expect("hash pipeline");

        let produced = String::from_utf8(backing.lock().unwrap().clone()).unwrap();
        assert_eq!(produced.trim(), HELLO_DIGEST);
    }

    #[test]
    fn hash_sha256_directory_digest_is_deterministic() {
        let digests: Vec<String> = (0..2)
            .map(|_| {
                let backing = Arc::new(Mutex::new(Vec::<u8>::new()));
                let sink: SharedOutput = backing.clone();
                let steps = vec![
                    step(StepKind::Mkdir("pkg".into())),
                    step(StepKind::Write {
                        path: "pkg/b.txt".into(),
                        contents: Some("22".into()),
                    }),
                    step(StepKind::Write {
                        path: "pkg/a.txt".into(),
                        contents: Some("1".into()),
                    }),
                    step(StepKind::HashSha256 { path: "pkg".into() }),
                ];
                let temp = GuardedPath::tempdir().unwrap();
                let root = temp.as_guarded_path().clone();
                let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
                run_steps_with_manager(
                    fs,
                    &steps,
                    MockProcessManager::default(),
                    assemble_default_io(None, Some(sink.clone())),
                )
                .expect("hash dir pipeline");
                String::from_utf8(backing.lock().unwrap().clone()).unwrap()
            })
            .collect();

        assert_eq!(digests[0], digests[1], "directory hashing must be stable");
        let hex = digests[0].trim();
        assert_eq!(hex.len(), 64, "full sha256 hex expected: {hex}");
    }

    #[test]
    fn copy_directory_branch_recurses_into_nested_target() {
        let steps = vec![
            step(StepKind::Mkdir("app".into())),
            step(StepKind::Write {
                path: "app/inner.txt".into(),
                contents: Some("nested".into()),
            }),
            Step {
                guard: None,
                kind: StepKind::Copy {
                    from_current_workspace: false,
                    from: "app".into(),
                    to: "copy-of-app".into(),
                },
                scope_enter: 0,
                scope_exit: 0,
            },
        ];

        let temp = GuardedPath::tempdir().unwrap();
        let root = temp.as_guarded_path().clone();
        let fs = Box::new(PathResolver::new_guarded(root.clone(), root.clone()).unwrap());
        run_steps_with_manager(fs, &steps, MockProcessManager::default(), ExecIo::new())
            .expect("copy pipeline");

        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
        let copied = root.join("copy-of-app/inner.txt").unwrap();
        assert_eq!(
            resolver.read_file(&copied).unwrap(),
            b"nested",
            "COPY must recurse into directories"
        );
    }

    #[test]
    fn public_entrypoint_returns_final_working_directory() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = temp.as_guarded_path().clone();
        let steps = vec![
            step(StepKind::Mkdir("app".into())),
            step(StepKind::Workdir("app".into())),
        ];
        let final_cwd =
            run_steps_with_context_result(&root, &root, &steps, None, None).expect("run");
        assert_eq!(final_cwd.as_path(), root.as_path().join("app"));
    }
}
