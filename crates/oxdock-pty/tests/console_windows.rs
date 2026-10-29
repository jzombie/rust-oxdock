//! Windows console probe proof: a sacrificial child detaches any inherited
//! console, allocates its own, injects a key event, and proves the acquired
//! host-stdin reader delivers it. Child-local detach keeps process-global
//! console state out of the test harness entirely. The probe reports
//! environment skips itself; anything else failing is a real regression.
//!
//! Windows only: consoles do not exist elsewhere. Miri never runs here:
//! no headless emulator can host a real console.

#![cfg(windows)]

use std::time::{Duration, Instant};

use oxdock_process::{BackgroundHandle, CommandBuilder};

/// How long the probe may run before it is killed and failed: detach plus
/// injection plus one pump read completes in milliseconds; anything longer
/// means the reader parked and the test would strand CI instead of failing.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// Poll cadence for the probe wait loop.
const PROBE_POLL: Duration = Duration::from_millis(100);
/// Automake skip convention, mirrored from the probe binary.
const EXIT_ENV_SKIP: i32 = 77;

#[test]
fn console_input_probe_passes_in_sacrificial_child() {
    let probe = env!("CARGO_BIN_EXE_pty_console_probe");
    let mut child = CommandBuilder::new(probe)
        .spawn()
        .expect("probe binary spawns");
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("probe wait observes exit") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("probe parked past {PROBE_TIMEOUT:?}; reader must not block");
        }
        std::thread::sleep(PROBE_POLL);
    };
    match status.code() {
        Some(0) => {}
        Some(EXIT_ENV_SKIP) => {
            eprintln!("SKIP: environment cannot host a console");
        }
        other => panic!("probe failed with status {other:?}"),
    }
}
