use oxdock_fs::{GuardedPath, PathResolver};

fn make_stale_dir() -> std::path::PathBuf {
    let base = std::env::temp_dir();
    let stale = base.join("oxdock-stale-fixture");
    let _ = std::fs::remove_dir_all(&stale);
    std::fs::create_dir_all(&stale).expect("create stale dir");
    std::fs::write(stale.join(".oxdock-tempdir"), b"oxdock-tempdir").expect("marker");
    std::fs::write(stale.join(".oxdock-tempdir.lock"), b"999999").expect("lock");
    stale
}

#[test]
fn cleanup_runs_on_pathresolver_startup() {
    let root = std::env::temp_dir().join("oxdock-cleanup-root");
    let _ = std::fs::create_dir_all(&root);

    // Create a stale tempdir (dead pid) and a live tempdir (current pid) to ensure
    // startup cleanup removes only the stale one.
    let stale = make_stale_dir();
    assert!(stale.exists());

    let live = GuardedPath::tempdir().expect("live tempdir");
    let live_path = live.as_guarded_path().to_path_buf();
    assert!(live_path.exists());

    // First PathResolver creation in this process should trigger cleanup_once.
    let _resolver = PathResolver::new(&root, &root).expect("resolver");

    assert!(!stale.exists(), "stale dir should be removed on startup cleanup");
    assert!(live_path.exists(), "live dir must survive cleanup while pid is alive");

    drop(live);
    assert!(!live_path.exists(), "live dir should clean up on drop");

    let _ = std::fs::remove_dir_all(&root);
}
