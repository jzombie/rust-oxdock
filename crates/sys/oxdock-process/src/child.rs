use anyhow::Result;
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::process::{Child, ExitStatus};

use crate::contract::BackgroundHandle;

#[derive(Debug)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub struct ChildHandle {
    pub(crate) child: Child,
    pub(crate) io_threads: Vec<std::thread::JoinHandle<()>>,
    reaped: bool,
}

impl ChildHandle {
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub(crate) fn new(child: Child, io_threads: Vec<std::thread::JoinHandle<()>>) -> Self {
        Self {
            child,
            io_threads,
            reaped: false,
        }
    }
}

impl BackgroundHandle for ChildHandle {
    fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        let res = self.child.try_wait()?;
        // Observing `Some(_)` means the OS handle has been reaped; record it
        // so `Drop` short-circuits instead of issuing redundant queries.
        if res.is_some() {
            self.reaped = true;
        }
        Ok(res)
    }

    fn wait(&mut self) -> Result<ExitStatus> {
        let status = self.child.wait()?;
        // Wait for IO threads to finish to ensure all output is captured
        for thread in self.io_threads.drain(..) {
            let _ = thread.join();
        }
        self.reaped = true;
        Ok(status)
    }

    fn kill(&mut self) -> Result<()> {
        // Deliberately does NOT set `reaped`: killing is not reaping, so
        // `Drop` must still wait afterwards to avoid zombies.
        if self.child.try_wait()?.is_none() {
            let _ = self.child.kill();
        }
        Ok(())
    }
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        // Safety net for error/panic paths that abandon live children between
        // spawn and the next poll. The executor's explicit teardown paths
        // remain the primary mechanism; this only bounds leakage.
        if self.reaped {
            return;
        }
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            // Bounded after SIGKILL/TerminateProcess; best-effort reap.
            let _ = self.child.wait();
        }
        // `io_threads` are deliberately NOT joined: a grandchild inheriting
        // the pipe can keep pump threads alive indefinitely. They terminate
        // on pipe EOF after the kill and only ever write into Arc'd buffers.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{
        CommandContext, CommandMode, CommandOptions, CommandResult, CommandStdout, ProcessManager,
    };
    use crate::shell_manager::ShellProcessManager;
    use oxdock_fs::{GuardedPath, PolicyPath};
    use std::collections::HashMap;

    fn make_ctx() -> (oxdock_fs::GuardedTempDir, CommandContext) {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let guard = temp.as_guarded_path().clone();
        let cwd: PolicyPath = guard.clone().into();
        let map: HashMap<String, String> = HashMap::new();
        let ctx = CommandContext::from_map(&cwd, &map, &guard, &guard, &guard);
        (temp, ctx)
    }

    #[cfg(unix)]
    #[cfg_attr(
        miri,
        ignore = "spawns processes; Miri does not support process execution"
    )]
    #[test]
    fn drop_kills_spawned_child_before_it_can_finish() {
        use crate::builder::CommandBuilder;
        use oxdock_fs::PathResolver;

        let temp = oxdock_fs::GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let marker = root.join("late.txt").expect("marker path");
        let marker_display = marker.display().to_string();

        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");
        let mut builder = CommandBuilder::new("sh");
        builder.args(["-c", &format!("sleep 1; echo done > {marker_display}")]);
        let handle = builder.spawn().expect("spawn");
        // Drop while the child is still sleeping; the safety net must kill it
        // before the delayed write can happen.
        drop(handle);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            assert!(
                resolver.read_file(&marker).is_err(),
                "child must be killed by Drop before writing {marker_display}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn long_running_script() -> &'static str {
        #[cfg(windows)]
        {
            "ping -n 30 127.0.0.1 >NUL"
        }
        #[cfg(not(windows))]
        {
            "sleep 30"
        }
    }

    #[cfg_attr(
        miri,
        ignore = "spawns processes; Miri does not support process execution"
    )]
    #[test]
    fn child_handle_background_lifecycle_polls_then_waits() {
        let (_temp, ctx) = make_ctx();
        let mut pm = ShellProcessManager;
        let options = CommandOptions {
            mode: CommandMode::Background,
            ..Default::default()
        };
        let mut handle = match pm.run_command(&ctx, "exit 0", options).expect("run") {
            CommandResult::Background(handle) => handle,
            CommandResult::Completed => panic!("expected Background, got Completed"),
            CommandResult::Captured(_) => panic!("expected Background, got Captured"),
        };

        let mut status = None;
        for _ in 0..500 {
            if let Some(done) = handle.try_wait().expect("try_wait") {
                status = Some(done);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let status = status.expect("child should exit within polling window");
        assert!(status.success());

        let waited = handle.wait().expect("wait");
        assert!(waited.success());
    }

    #[cfg_attr(
        miri,
        ignore = "spawns processes; Miri does not support process execution"
    )]
    #[test]
    fn child_handle_kill_is_idempotent_and_wait_joins_threads() {
        let (_temp, ctx) = make_ctx();
        let mut pm = ShellProcessManager;
        let options = CommandOptions {
            mode: CommandMode::Background,
            stdout: CommandStdout::Stream(std::sync::Arc::new(std::sync::Mutex::new(
                Vec::<u8>::new(),
            ))),
            ..Default::default()
        };
        let mut handle = match pm
            .run_command(&ctx, long_running_script(), options)
            .expect("run")
        {
            CommandResult::Background(handle) => handle,
            CommandResult::Completed => panic!("expected Background, got Completed"),
            CommandResult::Captured(_) => panic!("expected Background, got Captured"),
        };

        handle.kill().expect("first kill");
        handle.kill().expect("second kill must be idempotent");
        let _status = handle.wait().expect("wait after kill");
    }
}
