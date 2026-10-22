//! Persistent MCP host session: linear init once, then per-call dispatch.
//!
//! The reactor owns one `ExecState` for the process lifetime. `init`
//! executes the script's top-level steps (globals, `LET`, `WORKSPACE`,
//! `ENV`, function definitions) so exported tools see a fully built
//! ambient scope, then validates `EXPORT` and hands the live state to
//! the caller. Each `call` dispatches one exported `FUNC` by qualified
//! name, drains the capture sinks into the result, and truncates them
//! to zero before the next request (no unbounded growth).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use oxdock_parser::{Expr, ParsedScript};
use oxdock_process::{CommandStdin, ProcessManager, SharedOutput};

use super::handlers::call_func_value;
use super::io::{ExecIo, StreamHandle, teed_stderr, teed_stdout};
use super::native::{FuncMeta, HostModule};
use super::state::ExecState;
use super::steps::Flow;
use super::typing::TypeDescriptor;
use super::{Value, execute_steps};
use oxdock_fs::WorkspaceFs;

/// Captured output cap per call. Matches the assertion-capture scale:
/// larger outputs truncate with a marker instead of breaking framing.
const MCP_CAPTURE_CAP: usize = 8 * 1024 * 1024;

/// One tool invocation result: the return value plus drained captures.
#[derive(Debug, Clone)]
pub struct McpCallOutput {
    pub value: Value,
    pub stdout: String,
    pub stderr: String,
}

/// Persistent host session for `--mcp`. Owns the initialized `ExecState`,
/// the process manager, the export list, and the capture sinks.
pub struct McpSession<P: ProcessManager> {
    state: ExecState<P>,
    process: P,
    exports: Vec<String>,
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
}

impl<P: ProcessManager> McpSession<P> {
    /// Build the session: create state with capture sinks, register
    /// modules, install script docs, run static validation, execute the
    /// linear steps once, then validate `EXPORT`. Never discards steps.
    #[allow(clippy::too_many_arguments)]
    pub fn init(
        fs: Box<dyn WorkspaceFs>,
        process: P,
        modules: Vec<HostModule<P>>,
        types: Vec<&'static TypeDescriptor>,
        parsed: &ParsedScript,
    ) -> Result<Self> {
        let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stdout_sink: SharedOutput = stdout_buf.clone();
        let stderr_sink: SharedOutput = stderr_buf.clone();
        let mut io = ExecIo::new();
        io.set_stdout(Some(stdout_sink));
        io.set_stderr(Some(stderr_sink));

        let mut state = super::new_state(fs, io)?;
        for module in &modules {
            module.check_pre_run(&parsed.steps)?;
        }
        for module in modules {
            state.register_module(module);
        }
        for descriptor in types {
            state.register_type(descriptor);
        }
        state.functions.set_script_docs(&parsed.func_docs);

        state.type_aliases = super::typing::collect_type_aliases(&parsed.steps, &state)?;
        state.validate_function_type_tags()?;
        super::static_check::validate_script_types(&parsed.steps, &state)?;

        let assert_windows = Arc::clone(&state.assert_windows);
        let assert_windows_stderr = Arc::clone(&state.assert_windows_stderr);
        let exact_stdout = Arc::clone(&state.exact_stdout);
        let stdout = Some(StreamHandle::Stream(teed_stdout(
            state.io.stdout(),
            assert_windows,
            exact_stdout,
        )));
        let stderr = state
            .io
            .stderr()
            .map(|sink| StreamHandle::Stream(teed_stderr(Some(sink), assert_windows_stderr)));
        let mut process = process;
        let stdin = state.io.stdin().into();
        let flow = execute_steps(
            &mut state,
            &mut process,
            &parsed.steps,
            stdin,
            false,
            stdout,
            stderr,
            None,
            None,
            true,
        )?;
        match flow {
            Flow::Done => {}
            Flow::Break { idx } => anyhow::bail!("step {}: BREAK outside loop", idx + 1),
            Flow::Continue { idx } => anyhow::bail!("step {}: CONTINUE outside loop", idx + 1),
            Flow::Return { idx, .. } => {
                anyhow::bail!(
                    "step {}: RETURN outside function, ASYNC task, or LET block",
                    idx + 1
                )
            }
        }

        state.functions.exported_metas(&parsed.exports)?;
        drain_buf(&stdout_buf);
        drain_buf(&stderr_buf);

        Ok(Self {
            state,
            process,
            exports: parsed.exports.clone(),
            stdout_buf,
            stderr_buf,
        })
    }

    /// Export list in source order.
    pub fn exports(&self) -> &[String] {
        &self.exports
    }

    /// Tool metadata for the export list, in source order. Fails fast
    /// naming the first missing export.
    pub fn tool_metas(&self) -> Result<Vec<FuncMeta>> {
        self.state.functions.exported_metas(&self.exports)
    }

    /// Script `#` docs for a qualified name, if collected at parse.
    pub fn tool_doc(&self, qualified: &str) -> Option<(String, String)> {
        self.state.functions.script_doc_for(qualified)
    }

    /// Dispatch one exported tool by bare base name with ordered args.
    /// Arity and type coercion ride the normal call path. Captures
    /// drain into the result and truncate to zero before returning.
    /// Qualified exports (`STD::INT`) answer under their base name;
    /// tool-name duplicates fail at table build, so the match is unique.
    pub fn call(&mut self, base: &str, args: Vec<Value>) -> Result<McpCallOutput> {
        let qualified = if self.exports.iter().any(|e| e == base) {
            oxdock_parser::qualify(oxdock_parser::SCRIPT_MODULE_NAME, base)
        } else {
            match self
                .exports
                .iter()
                .find(|e| e.rsplit("::").next() == Some(base))
            {
                Some(export) => export.clone(),
                None => anyhow::bail!("unknown MCP tool `{base}`"),
            }
        };
        let exprs: Vec<Expr> = args.into_iter().map(Expr::Literal).collect();
        let assert_windows = Arc::clone(&self.state.assert_windows);
        let assert_windows_stderr = Arc::clone(&self.state.assert_windows_stderr);
        let exact_stdout = Arc::clone(&self.state.exact_stdout);
        let out = Some(StreamHandle::Stream(teed_stdout(
            self.state.io.stdout(),
            assert_windows,
            exact_stdout,
        )));
        let err = self
            .state
            .io
            .stderr()
            .map(|sink| StreamHandle::Stream(teed_stderr(Some(sink), assert_windows_stderr)));
        let value = {
            let state_ptr: *mut ExecState<P> = &mut self.state;
            let process_ptr: *mut P = &mut self.process;
            let mut ctx = super::steps::StepCtx::for_mcp(
                unsafe { &mut *state_ptr },
                unsafe { &mut *process_ptr },
                CommandStdin::Null,
                out,
                err,
            );
            call_func_value(&mut ctx, 0, &qualified, &exprs)?
        };
        let stdout = capped_drain(&self.stdout_buf);
        let stderr = capped_drain(&self.stderr_buf);
        Ok(McpCallOutput {
            value,
            stdout,
            stderr,
        })
    }
}

/// Lock, clone, and truncate a concrete capture buffer.
fn drain_buf(buf: &Arc<Mutex<Vec<u8>>>) {
    if let Ok(mut guard) = buf.lock() {
        guard.clear();
    }
}

/// Drain a concrete buffer into a lossy string with an 8MiB cap.
/// Truncates to zero.
fn capped_drain(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    let mut text = String::new();
    if let Ok(mut guard) = buf.lock() {
        let bytes = guard.clone();
        guard.clear();
        if bytes.len() > MCP_CAPTURE_CAP {
            text = String::from_utf8_lossy(&bytes[..MCP_CAPTURE_CAP]).into_owned();
            text.push_str("\n[truncated]");
        } else {
            text = String::from_utf8_lossy(&bytes).into_owned();
        }
    }
    text
}

/// Script docs keyed by bare base name, for tests.
#[allow(dead_code)]
pub fn docs_for_test(
    docs: &HashMap<String, (String, String)>,
    base: &str,
) -> Option<(String, String)> {
    docs.get(base).cloned()
}
