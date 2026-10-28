//! Resize proof for Windows: a child reporting its console window must
//! observe a mid-run resize applied through the shared size cell.
//!
//! Windows only: the child loops `[Console]::WindowWidth/Height`
//! under PowerShell, which reads the ConPTY-hosted console. Unix
//! proves the same path in `resize_unix.rs` (`stty size` loop). Miri
//! cannot allocate a real pty, so this stays on native runners.

#![cfg(windows)]

use std::io::Write;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use oxdock_pty::{PtyOutcome, PtyRequest, PtySize, WatchSize, run_pty_session};

/// Shared sink the test reads back: appends everything the child
/// prints, so the watcher can wait for size lines to appear.
#[derive(Clone, Default)]
struct Tee {
    out: Arc<Mutex<Vec<u8>>>,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.out.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The pump applies a mid-run resize: the child first reports the
/// opener size (width height), then the bumped size after `set`,
/// all before teardown. A 20s failsafe cancels come what may, so a
/// broken resize path fails the assert instead of hanging the suite.
#[test]
#[cfg_attr(miri, ignore = "allocates a real pty, which dies under Miri isolation")]
fn resize_reaches_child_mid_run() {
    let tee = Tee::default();
    let output: Arc<Mutex<dyn Write + Send>> = Arc::new(Mutex::new(tee.clone()));
    let size = WatchSize::new(37, 97);
    let cancel = Arc::new(AtomicBool::new(false));

    let out_watcher = tee.clone();
    let size_watcher = Arc::clone(&size);
    let cancel_watcher = Arc::clone(&cancel);
    let watcher = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut resized = false;
        loop {
            if std::time::Instant::now() > deadline {
                break;
            }
            // ConPTY renders CR LF endings; the substrings hold
            // either way. PowerShell prints width before height,
            // the reverse of `stty size`.
            let rendered = String::from_utf8_lossy(&out_watcher.out.lock().unwrap()).into_owned();
            if !resized && rendered.contains("97 37") {
                size_watcher.set(41, 121);
                resized = true;
            }
            if resized && rendered.contains("121 41") {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        cancel_watcher.store(true, Ordering::SeqCst);
    });

    let path = std::env::var("PATH").unwrap_or_else(|_| "C:\\Windows\\System32".to_string());
    let outcome = run_pty_session(PtyRequest {
        argv: vec![
            "powershell".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "while ($true) { Write-Output \"$([Console]::WindowWidth) $([Console]::WindowHeight)\"; Start-Sleep -Milliseconds 200 }".to_string(),
        ],
        env: vec![("PATH".to_string(), path)],
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        initial_size: PtySize {
            rows: 37,
            cols: 97,
            seq: 0,
        },
        input: None,
        output,
        size_source: size,
        should_cancel: Box::new({
            let cancel = Arc::clone(&cancel);
            move || cancel.load(Ordering::SeqCst)
        }),
        on_exit: None,
        on_output_drained: None,
        func: "pty resize proof",
    });

    let _ = watcher.join();
    // Cancellation is the only exit: the child loops forever.
    assert!(
        matches!(outcome, Ok(PtyOutcome::Cancelled)),
        "session must end via the failsafe cancel, got {outcome:?}"
    );
    let rendered = String::from_utf8_lossy(&tee.out.lock().unwrap()).into_owned();
    assert!(
        rendered.contains("97 37"),
        "child never reported the opener size: {rendered:?}"
    );
    assert!(
        rendered.contains("121 41"),
        "child never reported the resized window: {rendered:?}"
    );
}
