use anyhow::{Context, Result, bail};
use oxdock_fs::GuardedPath;
#[cfg(all(unix, not(miri)))]
use oxdock_fs::PathResolver;
use std::ffi::OsStr;
use std::fs::File;
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::process::{Command, ExitStatus, Stdio};

use crate::CommandBuilder;

pub fn shell_program() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd".to_string())
    }

    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string())
    }
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub(crate) fn shell_cmd(cmd: &str) -> Command {
    let program = shell_program();
    let mut c = Command::new(program);
    #[allow(clippy::disallowed_macros)]
    if cfg!(windows) {
        c.arg("/C").arg(cmd);
    } else {
        c.arg("-c").arg(cmd);
    }
    c
}

#[derive(Default)]
pub struct ShellLauncher;

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
impl ShellLauncher {
    pub fn run(&self, cmd: &mut Command) -> Result<()> {
        let status = cmd
            .status()
            .with_context(|| format!("failed to run {:?}", cmd))?;
        if !status.success() {
            bail!("command {:?} failed with status {}", cmd, status);
        }
        Ok(())
    }

    pub fn run_with_output(&self, cmd: &mut Command) -> Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = cmd
            .output()
            .with_context(|| format!("failed to run {:?}", cmd))?;
        Ok((output.status, output.stdout, output.stderr))
    }

    pub fn spawn(&self, cmd: &mut Command) -> Result<()> {
        cmd.spawn()
            .with_context(|| format!("failed to spawn {:?}", cmd))?;
        Ok(())
    }

    pub fn with_stdins<'a>(&self, cmd: &'a mut Command, stdin: Option<File>) -> &'a mut Command {
        if let Some(file) = stdin {
            cmd.stdin(file);
        }
        cmd
    }

    pub fn program_arg(&self, program: impl AsRef<OsStr>) -> Command {
        Command::new(program)
    }
}

/// Spawn an interactive shell rooted at `cwd`, printing `banner` before handing
/// control to the user's shell.
///
/// `workspace_root` anchors the guarded path resolver used to reattach the
/// controlling TTY on Unix.
pub fn spawn_interactive_shell(
    cwd: &GuardedPath,
    workspace_root: &GuardedPath,
    banner: &str,
) -> Result<()> {
    #[cfg(unix)]
    {
        let mut cmd = CommandBuilder::new(shell_program());
        cmd.current_dir(cwd.as_path());

        // Print a single banner inside the subshell, then exec the user's shell to stay interactive.
        // The banner travels via env and the shell program via `$1` so neither is interpolated into
        // the script string (paths with spaces or `%` would break quoting or printf parsing).
        const SCRIPT: &str = "printf '%s\\n' \"$OXDOCK_BANNER\"; exec \"$1\"";
        cmd.env("OXDOCK_BANNER", banner);
        cmd.arg("-c").arg(SCRIPT).arg("sh").arg(shell_program());

        // Reattach stdin to the controlling TTY so a piped-in script can still open an interactive shell.
        #[cfg(not(miri))]
        {
            #[allow(clippy::disallowed_types)]
            let tty_path = oxdock_fs::UnguardedPath::external("/dev/tty");
            if let Ok(resolver) =
                PathResolver::new(workspace_root.as_path(), workspace_root.as_path())
                && let Ok(tty) = resolver.open_file_unguarded(&tty_path)
            {
                cmd.stdin_file(tty);
            }
        }

        if try_shell_command_hook(&mut cmd)? {
            return Ok(());
        }

        let status = cmd.status()?;
        if !status.success() {
            bail!("shell exited with status {}", status);
        }
        Ok(())
    }

    #[cfg(windows)]
    {
        // Launch via `start` so Windows opens a real interactive console window. Normalize the path
        // and also set the parent process working directory to the temp workspace; this avoids
        // start's `/D` parsing quirks on paths with spaces or verbatim prefixes.
        let cwd_path = oxdock_fs::command_path(cwd);
        let banner_cmd = windows_banner_command(banner, cwd);
        let mut cmd = CommandBuilder::new("cmd");
        cmd.current_dir(cwd_path.as_ref())
            .arg("/C")
            .arg("start")
            .arg("oxdock shell")
            .arg("cmd")
            .arg("/K")
            .arg(banner_cmd);

        if try_shell_command_hook(&mut cmd)? {
            return Ok(());
        }

        // Fire-and-forget so the parent console regains control immediately; the child window is
        // fully interactive. If the launch fails, surface the error right away.
        cmd.spawn()
            .context("failed to start interactive shell window")?;
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (cwd, workspace_root, banner);
        bail!("interactive shell unsupported on this platform");
    }
}

#[cfg(windows)]
fn escape_for_cmd(s: &str) -> String {
    // Escape characters that would otherwise be interpreted by cmd when echoed.
    s.replace('^', "^^")
        .replace('&', "^&")
        .replace('|', "^|")
        .replace('>', "^>")
        .replace('<', "^<")
}

#[cfg(windows)]
fn windows_banner_command(banner: &str, cwd: &GuardedPath) -> String {
    let mut parts: Vec<String> = banner
        .lines()
        .map(|line| format!("echo {}", escape_for_cmd(line)))
        .collect();
    let cwd_path = oxdock_fs::command_path(cwd);
    parts.push(format!(
        "cd /d {}",
        escape_for_cmd(&cwd_path.as_ref().display().to_string())
    ));
    parts.join(" && ")
}

#[cfg(test)]
type ShellCmdHook = dyn FnMut(&crate::CommandSnapshot) -> Result<()> + Send;

#[cfg(test)]
thread_local! {
    static SHELL_CMD_HOOK: std::cell::RefCell<Option<Box<ShellCmdHook>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn set_shell_command_hook<F>(hook: F)
where
    F: FnMut(&crate::CommandSnapshot) -> Result<()> + Send + 'static,
{
    SHELL_CMD_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
fn clear_shell_command_hook() {
    SHELL_CMD_HOOK.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

#[cfg(test)]
fn try_shell_command_hook(cmd: &mut CommandBuilder) -> Result<bool> {
    SHELL_CMD_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            let snap = cmd.snapshot();
            hook(&snap)?;
            return Ok(true);
        }
        Ok(false)
    })
}

#[cfg(not(test))]
fn try_shell_command_hook(cmd: &mut CommandBuilder) -> Result<bool> {
    let _ = cmd;
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::{ShellLauncher, shell_cmd, shell_program};
    use crate::TestEnvGuard;

    use std::ffi::OsStr;
    use std::sync::Mutex;

    // For Windows, fixing COMSPEC override test race condition
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn shell_program_prefers_env_override() {
        let _lock = ENV_LOCK.lock().expect("env lock");
        #[cfg(windows)]
        let _guard = TestEnvGuard::set("COMSPEC", "custom-cmd");
        #[cfg(not(windows))]
        let _guard = TestEnvGuard::set("SHELL", "custom-sh");
        let program = shell_program();
        #[cfg(windows)]
        assert_eq!(program, "custom-cmd");
        #[cfg(not(windows))]
        assert_eq!(program, "custom-sh");
    }

    #[cfg_attr(
        miri,
        ignore = "spawns shell command; Miri does not support process execution"
    )]
    #[test]
    fn shell_launcher_run_with_output_captures_stdout() {
        let _lock = ENV_LOCK.lock().expect("env lock");
        let launcher = ShellLauncher;
        let mut cmd = shell_cmd("echo hello");
        let (status, stdout, _stderr) = launcher.run_with_output(&mut cmd).expect("run output");
        assert!(status.success());
        let out = String::from_utf8_lossy(&stdout);
        assert!(out.contains("hello"));
    }

    #[test]
    fn shell_launcher_program_arg_tracks_program() {
        let launcher = ShellLauncher;
        let cmd = launcher.program_arg("echo");
        assert_eq!(cmd.get_program(), OsStr::new("echo"));
    }

    #[test]
    fn shell_program_falls_back_to_default_without_env() {
        let _lock = ENV_LOCK.lock().expect("env lock");
        #[cfg(windows)]
        {
            let _guard = TestEnvGuard::remove("COMSPEC");
            assert_eq!(shell_program(), "cmd");
        }
        #[cfg(not(windows))]
        {
            let _guard = TestEnvGuard::remove("SHELL");
            assert_eq!(shell_program(), "sh");
        }
    }

    #[test]
    fn shell_cmd_applies_platform_flag_and_script() {
        let cmd = shell_cmd("echo hi");
        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        #[cfg(windows)]
        assert_eq!(args, vec!["/C".to_string(), "echo hi".to_string()]);
        #[cfg(not(windows))]
        assert_eq!(args, vec!["-c".to_string(), "echo hi".to_string()]);
    }

    #[cfg_attr(
        miri,
        ignore = "spawns shell command; Miri does not support process execution"
    )]
    #[test]
    fn shell_launcher_run_succeeds_on_zero_exit() {
        let launcher = ShellLauncher;
        let mut cmd = shell_cmd("exit 0");
        launcher.run(&mut cmd).expect("zero exit should succeed");
    }

    #[cfg_attr(
        miri,
        ignore = "spawns shell command; Miri does not support process execution"
    )]
    #[test]
    fn shell_launcher_run_reports_nonzero_status() {
        let launcher = ShellLauncher;
        let mut cmd = shell_cmd("exit 3");
        let err = launcher.run(&mut cmd).expect_err("nonzero exit must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("failed with status"),
            "unexpected error message: {msg}"
        );
    }

    #[cfg_attr(
        miri,
        ignore = "spawns shell command; Miri does not support process execution"
    )]
    #[test]
    fn shell_launcher_spawn_smoke() {
        let launcher = ShellLauncher;
        let mut cmd = shell_cmd("exit 0");
        launcher.spawn(&mut cmd).expect("spawn should succeed");
    }

    #[test]
    fn shell_launcher_with_stdins_none_passthrough_keeps_builder_usable() {
        let launcher = ShellLauncher;
        let mut cmd = launcher.program_arg("echo");
        let same = launcher.with_stdins(&mut cmd, None);
        assert_eq!(same.get_program(), OsStr::new("echo"));
    }
}

#[cfg(test)]
mod interactive_shell_tests {
    use super::{clear_shell_command_hook, set_shell_command_hook, spawn_interactive_shell};
    use crate::CommandSnapshot;
    use anyhow::Result;
    use oxdock_fs::GuardedPath;
    use std::sync::{Arc, Mutex};

    #[cfg(any(unix, windows))]
    #[test]
    fn spawn_interactive_shell_builds_command_for_platform() -> Result<()> {
        let workspace = GuardedPath::tempdir()?;
        let workspace_root = workspace.as_guarded_path().clone();
        let cwd = workspace_root.join("subdir")?;
        #[cfg(not(miri))]
        {
            let resolver =
                oxdock_fs::PathResolver::new(workspace_root.as_path(), workspace_root.as_path())?;
            resolver.create_dir_all(&cwd)?;
        }

        let captured = Arc::new(Mutex::new(None::<CommandSnapshot>));
        let guard = captured.clone();
        set_shell_command_hook(move |cmd| {
            *guard.lock().unwrap() = Some(cmd.clone());
            Ok(())
        });
        spawn_interactive_shell(&cwd, &workspace_root, "test banner")?;
        clear_shell_command_hook();

        let snap = captured
            .lock()
            .unwrap()
            .clone()
            .expect("hook should capture snapshot");
        let cwd_path = snap.cwd.expect("cwd should be set");
        assert!(
            cwd_path.ends_with("subdir"),
            "expected cwd to include subdir, got {}",
            cwd_path.display()
        );
        assert!(
            snap.envs
                .iter()
                .any(|(k, v)| k == "OXDOCK_BANNER" && v == "test banner"),
            "expected OXDOCK_BANNER env injection, got {:?}",
            snap.envs
        );

        #[cfg(unix)]
        {
            let program = snap.program.to_string_lossy();
            assert_eq!(
                program,
                super::shell_program(),
                "expected shell program name"
            );
            let args: Vec<_> = snap
                .args
                .iter()
                .map(|s| s.to_string_lossy().to_string())
                .collect();
            assert_eq!(
                args.len(),
                4,
                "expected four args (-c script sh shell), got {:?}",
                args
            );
            assert_eq!(args[0], "-c");
            assert!(
                args[1].contains("$OXDOCK_BANNER"),
                "expected env-based banner reference, got {:?}",
                args[1]
            );
            assert!(
                args[1].contains("exec \"$1\""),
                "expected positional shell exec, got {:?}",
                args[1]
            );
            assert!(
                !args[1].contains("test banner"),
                "banner must not be interpolated into the script, got {:?}",
                args[1]
            );
            assert_eq!(args[2], "sh", "expected $0 placeholder");
            assert_eq!(args[3], super::shell_program(), "expected shell path as $1");
        }

        #[cfg(windows)]
        {
            use super::windows_banner_command;
            let program = snap.program.to_string_lossy().to_string();
            assert_eq!(program, "cmd", "expected cmd.exe launcher");
            let args: Vec<_> = snap
                .args
                .iter()
                .map(|s| s.to_string_lossy().to_string())
                .collect();
            let banner_cmd = windows_banner_command("test banner", &cwd);
            let expected = vec![
                "/C".to_string(),
                "start".to_string(),
                "oxdock shell".to_string(),
                "cmd".to_string(),
                "/K".to_string(),
                banner_cmd,
            ];
            assert_eq!(args, expected, "expected exact windows shell argv");
        }

        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_banner_command_emits_all_lines() {
        let banner = "line1\nline2\nline3";
        let workspace = GuardedPath::tempdir().expect("tempdir");
        let cwd = workspace.as_guarded_path().clone();
        let cmd = super::windows_banner_command(banner, &cwd);
        assert!(cmd.contains("line1"));
        assert!(cmd.contains("line2"));
        assert!(cmd.contains("line3"));
        assert!(cmd.contains("cd /d "));
    }
}
