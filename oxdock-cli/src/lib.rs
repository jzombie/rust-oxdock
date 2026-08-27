use anyhow::{Context, Result, bail};
use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver, discover_workspace_root, init_temp_gc};
#[cfg(windows)]
use oxdock_process::CommandBuilder;
use oxdock_process::SharedInput;
use std::env;
use std::io::{self, IsTerminal, Read};
use std::sync::{Arc, Mutex};

use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
pub use oxdock_core::{run_steps, run_steps_with_context, run_steps_with_context_result};
pub use oxdock_parser::{Guard, Step, StepKind, parse_script};
pub use oxdock_process::shell_program;

pub fn run() -> Result<()> {
    init_temp_gc();
    let workspace_root = discover_workspace_root().context("guard workspace root")?;

    let mut args = std::env::args().skip(1);
    let opts = Options::parse(&mut args, &workspace_root)?;
    execute(opts, workspace_root)
}

#[derive(Debug, Clone)]
pub enum ScriptSource {
    Path(GuardedPath),
    Stdin,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub script: ScriptSource,
    pub shell: bool,
}

impl Options {
    pub fn parse(
        args: &mut impl Iterator<Item = String>,
        workspace_root: &GuardedPath,
    ) -> Result<Self> {
        let mut script: Option<ScriptSource> = None;
        let mut shell = false;
        while let Some(arg) = args.next() {
            if arg.is_empty() {
                continue;
            }
            match arg.as_str() {
                "--script" => {
                    let p = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--script requires a path"))?;
                    if p == "-" {
                        script = Some(ScriptSource::Stdin);
                    } else {
                        script = Some(ScriptSource::Path(
                            workspace_root
                                .join(&p)
                                .with_context(|| format!("guard script path {p}"))?,
                        ));
                    }
                }
                "--shell" => {
                    shell = true;
                }
                other => bail!("unexpected flag: {}", other),
            }
        }

        let script = script.unwrap_or(ScriptSource::Stdin);

        Ok(Self { script, shell })
    }
}

pub fn execute(opts: Options, workspace_root: GuardedPath) -> Result<()> {
    init_temp_gc();
    execute_with_shell_runner(opts, workspace_root, run_shell, true)
}

pub struct ExecutionResult {
    pub tempdir: GuardedTempDir,
    pub final_cwd: GuardedPath,
}

pub fn execute_with_result(opts: Options, workspace_root: GuardedPath) -> Result<ExecutionResult> {
    if opts.shell {
        bail!("execute_with_result does not support --shell");
    }

    let tempdir = GuardedPath::tempdir().context("failed to create temp dir")?;
    let temp_root = tempdir.as_guarded_path().clone();

    let script = match &opts.script {
        ScriptSource::Path(path) => {
            let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())?;
            resolver
                .read_to_string(path)
                .with_context(|| format!("failed to read script at {}", path.display()))?
        }
        ScriptSource::Stdin => {
            let mut buf = String::new();
            io::stdin()
                .lock()
                .read_to_string(&mut buf)
                .context("failed to read script from stdin")?;
            buf
        }
    };

    let mut final_cwd = temp_root.clone();
    if !script.trim().is_empty() {
        let steps = parse_script(&script)?;
        final_cwd = run_steps_with_context_result_with_io(
            &temp_root,
            &workspace_root,
            &steps,
            ExecIo::new(),
        )?;
    }

    Ok(ExecutionResult { tempdir, final_cwd })
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

    let tempdir = GuardedPath::tempdir().context("failed to create temp dir")?;
    let temp_root = tempdir.as_guarded_path().clone();

    // Interpret a tiny Dockerfile-ish script
    let script = match &opts.script {
        ScriptSource::Path(path) => {
            // Read script path via PathResolver rooted at the workspace so
            // script files are validated to live under the workspace.
            let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())?;
            resolver
                .read_to_string(path)
                .with_context(|| format!("failed to read script at {}", path.display()))?
        }
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
                    bail!(
                        "no stdin detected; pass --script <file> or pipe a script into stdin (use --script - if explicit)"
                    );
                }
            } else {
                let mut buf = String::new();
                stdin
                    .lock()
                    .read_to_string(&mut buf)
                    .context("failed to read script from stdin")?;
                buf
            }
        }
    };

    // Parse and run steps if we have a non-empty script. Empty scripts are
    // valid when `--shell` is requested and the caller didn't pipe a script.
    let mut final_cwd = temp_root.clone();
    if !script.trim().is_empty() {
        let steps = parse_script(&script)?;
        // Use the caller's workspace as the build context so WORKSPACE LOCAL can hop back and so COPY
        // can source from the original tree if needed. Capture the final working directory so shells
        // inherit whatever WORKDIR the script ended on.

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
        final_cwd =
            run_steps_with_context_result_with_io(&temp_root, &workspace_root, &steps, io_cfg)?;
    }

    // If requested, drop into an interactive shell after running the script.
    if opts.shell {
        if require_tty && !has_controlling_tty() {
            bail!("--shell requires a tty (no controlling tty available)");
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
    if std::env::var("OXDOCK_SHELL_REEXEC").ok().as_deref() == Some("1") {
        return Ok(());
    }

    let self_path = std::env::current_exe().context("determine current executable")?;
    let base_temp =
        GuardedPath::new_root(std::env::temp_dir().as_path()).context("guard system temp dir")?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let temp_file = base_temp
        .join(&format!("oxdock-shell-{ts}-{}.exe", std::process::id()))
        .context("construct temp shell path")?;

    // Copy the current executable into the temporary location via a
    // resolver whose root is the temp directory. The source may live
    // outside the temp dir, so use `copy_file_from_external`.
    let temp_root_guard = temp_file
        .parent()
        .ok_or_else(|| anyhow::anyhow!("temp path unexpectedly missing parent"))?;
    let resolver_temp = PathResolver::new(temp_root_guard.as_path(), temp_root_guard.as_path())?;
    let dest = temp_file;
    #[allow(clippy::disallowed_types)]
    let source = oxdock_fs::UnguardedPath::external(self_path);
    resolver_temp
        .copy_file_from_unguarded(&source, &dest)
        .with_context(|| format!("failed to copy shell runner to {}", dest.display()))?;

    let mut cmd = CommandBuilder::new(dest.as_path());
    cmd.args(std::env::args_os().skip(1));
    cmd.env("OXDOCK_SHELL_REEXEC", "1");
    cmd.spawn()
        .with_context(|| format!("failed to spawn shell from {}", dest.display()))?;

    // Exit immediately so the original binary can be rebuilt while the shell child stays running.
    std::process::exit(0);
}

pub fn run_script(workspace_root: &GuardedPath, steps: &[Step]) -> Result<()> {
    run_steps_with_context(workspace_root, workspace_root, steps)
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

    let pkg = env::var("CARGO_PKG_NAME").unwrap_or_else(|_| "oxdock".to_string());
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
    oxdock_process::spawn_interactive_shell(cwd, workspace_root, &shell_banner(cwd, workspace_root))
}

// `command_path` now lives in `oxdock-fs` to centralize Path usage.

#[cfg(test)]
mod tests {
    use super::*;
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
    fn options_parse_script_path_and_shell() {
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let workspace_root = workspace.as_guarded_path().clone();
        let script_path = workspace_root.join("script.txt").expect("script path");
        let resolver = PathResolver::new(workspace_root.as_path(), workspace_root.as_path())
            .expect("resolver");
        resolver
            .write_file(&script_path, b"WRITE out.txt hi")
            .expect("write script");
        let mut args = vec![
            "--script".to_string(),
            "script.txt".to_string(),
            "--shell".to_string(),
        ]
        .into_iter();
        let opts = Options::parse(&mut args, &workspace_root).expect("parse");
        assert!(opts.shell);
        match opts.script {
            ScriptSource::Path(path) => assert_eq!(path, script_path),
            ScriptSource::Stdin => panic!("expected path script"),
        }
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
    )]
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
        };
        let ExecutionResult { tempdir, final_cwd } =
            execute_with_result(opts, workspace_root).expect("execute");
        assert_eq!(tempdir.as_guarded_path(), &final_cwd);
        let temp_resolver = PathResolver::new(
            tempdir.as_guarded_path().root(),
            tempdir.as_guarded_path().root(),
        )
        .expect("resolver");
        let out = tempdir.as_guarded_path().join("out.txt").expect("out path");
        let contents = temp_resolver.read_to_string(&out).expect("read out");
        assert_eq!(contents.trim(), "hi");
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
        };
        let called = RefCell::new(None::<(String, String)>);
        execute_for_test(opts, workspace_root.clone(), |cwd, workspace| {
            called.replace(Some((cwd.display(), workspace.display())));
            Ok(())
        })?;
        let seen = called.borrow().clone().expect("shell runner called");
        assert_eq!(seen.1, workspace_root.display());
        Ok(())
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
