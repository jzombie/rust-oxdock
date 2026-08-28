mod fs_ops;
mod handlers;
mod io;
mod pipe;
mod state;
mod steps;
#[cfg(test)]
mod tests;

pub use self::io::ExecIo;

use anyhow::Result;
use oxdock_fs::{GuardedPath, PathResolver, WorkspaceFs};
use oxdock_parser::{Step, TemplateString};
use oxdock_process::{
    BuiltinEnv, CommandContext, ProcessManager, SharedInput, SharedOutput, default_process_manager,
};

use std::sync::Arc;

use self::fs_ops::describe_dir;
use self::io::{StreamHandle, assemble_default_io, teed_stdout};
use self::state::ExecState;
use self::steps::execute_steps;

fn expand_template(t: &TemplateString, ctx: &CommandContext) -> String {
    let expander = oxdock_process::StreamingExpand::new(&[], ctx.envs());
    expander.expand_string(&t.0).unwrap_or_default()
}

pub(crate) fn expand_template_string(t: &TemplateString, ctx: &CommandContext) -> String {
    expand_template(t, ctx)
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
    let envs = Arc::new(BuiltinEnv::collect(&build_context).into_envs());
    let assert_windows = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut state = ExecState {
        fs,
        cargo_target_dir: fs_root.join(".cargo-target")?,
        cwd,
        envs,
        bg_children: Vec::new(),
        scope_stack: Vec::new(),
        io,
        assert_windows: assert_windows.clone(),
    };

    let _default_stdout = std::io::stdout();
    let stdin = state.io.stdin();
    // Every emitted byte flows through the tee so ASSERT_STDOUT sees both
    // interpreter output and streamed child output, even when no capture
    // sink was configured (forwarding to real stdout in that case).
    let stdout = Some(StreamHandle::Stream(teed_stdout(
        state.io.stdout(),
        assert_windows,
    )));
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
