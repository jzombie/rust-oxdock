//! Resize proof: a child reporting its window must observe a
//! mid-run resize applied through the shared size cell.
//!
//! Unix only: discovering the window needs `stty`, and Windows proves
//! the same path through the ConPTY legs of the smoke matrix instead.
//! Miri cannot allocate a real pty, so this stays on native runners.

#![cfg(unix)]

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
/// opener size, then the bumped size after `set`, all before
/// teardown. A 20s failsafe cancels come what may, so a broken
/// resize path fails the assert instead of hanging the suite.
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
            // stty prints through the pty with CR LF endings; the
            // substrings hold either way.
            let rendered = String::from_utf8_lossy(&out_watcher.out.lock().unwrap()).into_owned();
            if !resized && rendered.contains("37 97") {
                size_watcher.set(41, 121);
                resized = true;
            }
            if resized && rendered.contains("41 121") {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        cancel_watcher.store(true, Ordering::SeqCst);
    });

    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
    let outcome = run_pty_session(PtyRequest {
        argv: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "while true; do stty size; sleep 0.2; done".to_string(),
        ],
        env: vec![
            ("PATH".to_string(), path),
            ("TERM".to_string(), "xterm".to_string()),
        ],
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
        rendered.contains("37 97"),
        "child never reported the opener size: {rendered:?}"
    );
    assert!(
        rendered.contains("41 121"),
        "child never reported the resized window: {rendered:?}"
    );
}
