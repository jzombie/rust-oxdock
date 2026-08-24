//! Pins the once-per-process semantics of the startup stale-tempdir sweep:
//! every `PathResolver` constructor funnels through a single `OnceLock`, so
//! stale dirs planted *after* the first construction must survive further
//! constructor calls (the direct cleanup API remains available).
//!
//! This file runs in its own test binary, guaranteeing an untouched
//! process-wide sweep state.
use oxdock_fs::{GuardedPath, PathResolver};

// Mirrors the private consts in workspace_fs/path.rs; kept in sync by the
// contract test `tempdir_with_custom_configurator_writes_marker_and_pid_lock`.
const MARKER: &str = ".oxdock-tempdir";
const LOCK: &str = ".oxdock-tempdir.lock";

#[cfg_attr(
    miri,
    ignore = "relies on host tempdirs and the startup filesystem sweep"
)]
#[test]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn startup_sweep_runs_only_once_per_process() {
    let scratch = GuardedPath::tempdir().expect("scratch tempdir");
    let root = scratch.as_guarded_path().clone();

    // Consume this process' single startup sweep via a constructor.
    PathResolver::new(root.as_path(), root.as_path()).expect("first resolver");

    // Plant a reclaimable stale dir (marker present, no lock) AFTER that pass.
    let stale = std::env::temp_dir().join(format!("oxdock-once-semantics-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stale);
    std::fs::create_dir_all(&stale).expect("plant dir");
    std::fs::write(stale.join(MARKER), b"oxdock-tempdir").expect("marker");

    // A second constructor must NOT rescan.
    PathResolver::new(root.as_path(), root.as_path()).expect("second resolver");
    assert!(
        stale.exists(),
        "startup sweep must run at most once per process"
    );

    // The direct API still reclaims it, proving the fixture was sweepable.
    GuardedPath::cleanup_stale_tempdirs().expect("direct cleanup");
    assert!(!stale.exists(), "direct cleanup must reclaim stale dirs");

    // And a third constructor still performs no additional sweeping.
    PathResolver::new(root.as_path(), root.as_path()).expect("third resolver");

    let lock_probe =
        std::env::temp_dir().join(format!("oxdock-once-semantics-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&lock_probe);
    std::fs::create_dir_all(&lock_probe).expect("plant locked dir");
    std::fs::write(lock_probe.join(MARKER), b"oxdock-tempdir").expect("marker");
    std::fs::write(lock_probe.join(LOCK), format!("{}\n", std::process::id()))
        .expect("lock with live pid");

    PathResolver::new(root.as_path(), root.as_path()).expect("fourth resolver");
    assert!(
        lock_probe.exists(),
        "live-pid dir survives constructor call"
    );

    GuardedPath::cleanup_stale_tempdirs().expect("final direct cleanup keeps live pid dir");
    assert!(lock_probe.exists());

    let _ = std::fs::remove_dir_all(&lock_probe);
}
