use anyhow::Result;
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::contract::BackgroundHandle;

/// Shared inner state for `ChildHandle`, enabling safe cloning.
/// The OS PID is stored separately so `kill()` can signal
/// the process without needing `&mut Child`, avoiding undefined behavior.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
struct ChildInner {
    child: Option<Child>,
    io_threads: Vec<std::thread::JoinHandle<()>>,
    reaped: bool,
    exit_status: Option<ExitStatus>,
    killed: AtomicBool,
}

#[derive(Clone)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub struct ChildHandle {
    inner: Arc<Mutex<ChildInner>>,
    /// OS process ID for signal-based kill on Unix and `OpenProcess`-based
    /// terminate on Windows.
    pid: Arc<AtomicU32>,
}

impl ChildHandle {
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub(crate) fn new(
        child: Child,
        stdin_thread: Option<std::thread::JoinHandle<()>>,
        io_threads: Vec<std::thread::JoinHandle<()>>,
    ) -> Self {
        // The stdin feeder is deliberately detached, never joined (see
        // below): it blocks reading the producer pipe, whose writers may
        // legitimately outlive a short-lived child (a session pump feeding
        // the next command), so joining would hang `wait()` forever : the
        // same reason `Drop` below never joins pump threads. The detached
        // thread ends on pipe EOF or write failure and releases its
        // handles then; it holds no lock anyone else needs (fresh reader
        // handle per spawn).
        let _ = stdin_thread;
        #[cfg(unix)]
        let pid = child.id();
        #[cfg(windows)]
        let pid = child.id();
        Self {
            inner: Arc::new(Mutex::new(ChildInner {
                child: Some(child),
                io_threads,
                reaped: false,
                exit_status: None,
                killed: AtomicBool::new(false),
            })),
            pid: Arc::new(AtomicU32::new(pid)),
        }
    }
}

impl BackgroundHandle for ChildHandle {
    fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref mut child) = guard.child {
            match child.try_wait()? {
                Some(status) => {
                    // Same post-reap sweep as `wait`: an escapee that
                    // outlived the kill keeps the group (and the pump
                    // EOF it holds open) alive until this fires.
                    #[cfg(unix)]
                    if guard.killed.load(Ordering::SeqCst) {
                        sweep_process_group(self.pid.load(Ordering::SeqCst));
                    }
                    guard.reaped = true;
                    guard.exit_status = Some(status);
                    // Zero PID to prevent killing recycled PIDs
                    self.pid.store(0, Ordering::SeqCst);
                    // Join output pumps only: their EOF is guaranteed by
                    // child exit, while the stdin feeder may legitimately
                    // outlive it (the stdin feeder is detached in `new`).
                    for thread in guard.io_threads.drain(..) {
                        let _ = thread.join();
                    }
                    guard.child = None;
                    Ok(Some(status))
                }
                None => Ok(None),
            }
        } else if guard.reaped {
            // Process already reaped : return cached exit status
            Ok(Some(
                guard
                    .exit_status
                    .unwrap_or_else(|| exit_status_from_code(0)),
            ))
        } else {
            // wait() is executing on another thread : process is still running
            Ok(None)
        }
    }

    fn wait(&mut self) -> Result<ExitStatus> {
        // Take the child out of the mutex. This ensures only one thread
        // performs the blocking OS wait, preventing ECHILD from dual waitpid.
        let child_opt = {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.child.take()
        };

        if let Some(mut child) = child_opt {
            let status = child.wait()?;
            // Re-acquire lock to store result and join IO threads.
            // Output pumps only: the stdin feeder is detached in `new`
            // since its producer may outlive the child.
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            // Sweep after the leader is reaped: a grandchild forked
            // between the kill's group enumeration and the leader's
            // death escaped that signal, and the group stays
            // addressable at the same pgid while any such member
            // lives. Without the sweep the pump join below stalls on
            // the escapee's inherited pipe write ends until it exits.
            #[cfg(unix)]
            if guard.killed.load(Ordering::SeqCst) {
                sweep_process_group(self.pid.load(Ordering::SeqCst));
            }
            // Zero PID to prevent killing recycled PIDs
            self.pid.store(0, Ordering::SeqCst);
            for thread in guard.io_threads.drain(..) {
                let _ = thread.join();
            }
            guard.reaped = true;
            guard.exit_status = Some(status);
            Ok(status)
        } else {
            // Already reaped : return cached exit status
            let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            Ok(guard
                .exit_status
                .unwrap_or_else(|| exit_status_from_code(0)))
        }
    }

    fn kill(&mut self) -> Result<()> {
        let pid = self.pid.load(Ordering::SeqCst);
        if pid == 0 {
            return Ok(());
        }
        // Flag before signaling: the reaper reads `killed` after the
        // leader dies, and must see the post-reap sweep as required even
        // if it wins the inner lock before this thread stores the flag.
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .killed
            .store(true, Ordering::SeqCst);
        kill_tree(pid);
        Ok(())
    }
}

/// Kill the whole tree rooted at `pid`: the process group on Unix (a
/// background child owns its group, see `run_prepared`), `taskkill /T`
/// plus a direct terminate on Windows. Best-effort: a fully exited tree
/// reports ESRCH / a missing target, which is the already-dead outcome
/// teardown wants.
#[cfg(unix)]
fn kill_tree(pid: u32) {
    // Negative PID signals the whole process group, so orphaned
    // grandchildren die with the direct child instead of holding pipe
    // write ends open past the kill and stalling pump EOF.
    let group = -(pid as i32);
    let rc = unsafe { libc::kill(group, libc::SIGKILL) };
    if rc != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
fn kill_tree(pid: u32) {
    // Tree kill first: `taskkill /T` ends child processes the direct
    // terminate below would orphan, and orphaned grandchildren holding
    // pipe write ends open stall pump EOF exactly like orphaned process
    // groups do on Unix. A missing target (already exited tree) is the
    // desired end state, so only a failure to launch taskkill itself
    // falls back to the direct terminate. Ambient env (never the script
    // map) keeps teardown independent of whatever the script set.
    let tree_killed = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();
    if !tree_killed {
        // Open the process by PID and terminate it. This works even when
        // wait() has taken the `Child` out of `ChildInner`, and avoids
        // storing a raw HANDLE (which is neither Send nor Sync and would
        // break the `BackgroundHandle: Send` bound).
        // `pid` is zeroed after successful wait, so recycled PIDs are safe.
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !handle.is_null() {
                TerminateProcess(handle, 1);
                CloseHandle(handle);
            }
        }
    }
}

/// Signal a killed child's process group once more after its leader has
/// been reaped. Signal delivery to the leader is deferred to its next
/// kernel transition, so a grandchild the leader forked after the first
/// kill's group enumeration escapes that signal while the leader still
/// dies. The group persists at the same pgid as long as any such member
/// lives, so this reaches the escapees that would otherwise stall pump
/// EOF. Group only, never the raw pid: the leader's pid may already be
/// recycled. Probes first with signal 0, so an already-empty group is
/// left alone instead of risking a recycled pgid.
#[cfg(unix)]
fn sweep_process_group(pid: u32) {
    if pid == 0 {
        return;
    }
    let group = -(pid as i32);
    if unsafe { libc::kill(group, 0) } != 0 {
        return;
    }
    unsafe {
        libc::kill(group, libc::SIGKILL);
    }
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        // Only the last clone (when Arc refcount is 1) runs the actual cleanup.
        if Arc::strong_count(&self.inner) > 1 {
            return;
        }
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.reaped {
            return;
        }
        let running = guard
            .child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
        if running {
            // Tree kill, not `child.kill()`: a direct-child-only kill
            // leaves orphaned grandchildren holding inherited pipe write
            // ends open, and whoever joins the pumps then stalls until
            // they exit on their own.
            let pid = self.pid.load(Ordering::SeqCst);
            guard.killed.store(true, Ordering::SeqCst);
            kill_tree(pid);
            if let Some(child) = guard.child.as_mut() {
                let _ = child.wait();
            }
            // Same post-reap sweep as `wait`/`try_wait`: reach a
            // grandchild that forked after the kill's enumeration.
            #[cfg(unix)]
            sweep_process_group(pid);
        }
        guard.reaped = true;
        // `io_threads` are deliberately NOT joined: a grandchild inheriting
        // the pipe can keep pump threads alive indefinitely. They terminate
        // on pipe EOF after the kill and only ever write into Arc'd buffers.
    }
}

/// Helper to create an exit status from a raw code. Used for synthetic statuses.
fn exit_status_from_code(code: i32) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::sweep_process_group;
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    /// The post-escape state: the leader is reaped, but a grandchild it
    /// forked after the first kill's group enumeration still lives and
    /// keeps the group addressable. The sweep must end that group, which
    /// is what unblocks pump EOF in `wait`/`try_wait`.
    #[cfg_attr(miri, ignore = "spawns a real child process, unsupported under Miri")]
    #[test]
    fn sweep_process_group_kills_escapee_that_outlives_the_leader() {
        #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
        let mut leader = std::process::Command::new("sh")
            .arg("-c")
            // Background the sleeper so the shell can announce it: once
            // "started" is read the grandchild is guaranteed to exist in
            // the leader's group (no job control under `sh -c`, so `&`
            // keeps the same group). `wait` holds the shell alive.
            .arg("sleep 30 & echo started; wait")
            .process_group(0)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn leader");
        let pid = leader.id();
        {
            use std::io::BufRead;
            let mut line = String::new();
            let stdout = leader.stdout.as_mut().expect("leader stdout");
            std::io::BufReader::new(stdout)
                .read_line(&mut line)
                .expect("read readiness line");
            assert_eq!(line.trim(), "started");
        }
        // Leader-only signal: the sleeper escapes, exactly as it would
        // when the first group kill's enumeration raced the shell's
        // fork of it.
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        let _ = leader.wait();
        assert_eq!(
            unsafe { libc::kill(-(pid as i32), 0) },
            0,
            "orphaned sleeper should keep the group alive"
        );
        sweep_process_group(pid);
        let deadline = Instant::now() + Duration::from_secs(5);
        while unsafe { libc::kill(-(pid as i32), 0) } == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            unsafe { libc::kill(-(pid as i32), 0) },
            -1,
            "sweep must end the escapee's process group"
        );
    }
}
