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
        // the next command), so joining would hang `wait()` forever — the
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
            // Process already reaped — return cached exit status
            Ok(Some(
                guard
                    .exit_status
                    .unwrap_or_else(|| exit_status_from_code(0)),
            ))
        } else {
            // wait() is executing on another thread — process is still running
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
            // Zero PID to prevent killing recycled PIDs
            self.pid.store(0, Ordering::SeqCst);
            // Re-acquire lock to store result and join IO threads.
            // Output pumps only: the stdin feeder is detached in `new`
            // since its producer may outlive the child.
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            for thread in guard.io_threads.drain(..) {
                let _ = thread.join();
            }
            guard.reaped = true;
            guard.exit_status = Some(status);
            Ok(status)
        } else {
            // Already reaped — return cached exit status
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
        // Signal the process directly via OS PID. This does NOT need
        // &mut Child — no aliasing, no UB.
        #[cfg(unix)]
        {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        {
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
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .killed
            .store(true, Ordering::SeqCst);
        Ok(())
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
        if let Some(ref mut child) = guard.child
            && matches!(child.try_wait(), Ok(None))
        {
            let _ = child.kill();
            let _ = child.wait();
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
