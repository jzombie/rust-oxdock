//! Unified `LET` capture (#111): `LET $x: STRING = <command>` and `LET $o: STRING = AWAIT $t`.

use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver};
use std::sync::{Arc, Mutex};

fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
    temp.as_guarded_path().clone()
}

fn run_script(root: &GuardedPath, script: &str) -> Result<(), anyhow::Error> {
    let steps = oxdock_core::parse_script(script).expect("parse script");
    run_steps_with_context_result_with_io(root, root, &steps, ExecIo::new()).map(|_| ())
}

fn read_file(path: &GuardedPath) -> String {
    let resolver = PathResolver::new(path.root(), path.root()).unwrap();
    resolver.read_to_string(path).unwrap()
}

#[test]
fn let_capture_echo_binds_exact_bytes() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $x: STRING = ECHO hi
        ASSERT_EQ $x "hi\n"
    "#};
    run_script(&root, script).expect("capture ECHO");
}

#[test]
fn let_capture_empty_stdout_binds_empty_string() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $x: STRING = MKDIR emptydir
        ASSERT_EQ $x ""
    "#};
    run_script(&root, script).expect("capture MKDIR");
    assert!(
        root.join("emptydir").unwrap().exists(),
        "inner command must still execute"
    );
}

#[test]
fn let_capture_hash_sha256() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        WRITE data.txt payload
        LET $sha: STRING = HASH_SHA256 data.txt
        WRITE sha.txt "{{ $sha }}"
    "#};
    run_script(&root, script).expect("capture HASH_SHA256");
    let sha = read_file(&root.join("sha.txt").unwrap());
    // HASH_SHA256 prints "<hex>\n"; capture keeps exact bytes.
    assert!(
        sha.ends_with('\n'),
        "capture keeps trailing newline, got {sha:?}"
    );
    let sha = sha.trim_end();
    assert_eq!(sha.len(), 64, "sha256 hex digest, got {sha:?}");
    assert!(sha.chars().all(|c| c.is_ascii_hexdigit()), "{sha:?}");
}

#[test]
fn let_capture_ls() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        MKDIR sub
        WRITE sub/only.txt x
        LET $listing: STRING = LS sub
        WRITE out.txt "{{ $listing }}"
    "#};
    run_script(&root, script).expect("capture LS");
    let listing = read_file(&root.join("out.txt").unwrap());
    assert!(
        listing.contains("only.txt"),
        "LS capture must list the file, got {listing:?}"
    );
    assert!(
        listing.ends_with('\n'),
        "LS capture keeps trailing newline, got {listing:?}"
    );
}

#[test]
fn let_capture_read_file() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        WRITE data.txt "file-bytes\n"
        LET $x: STRING = READ data.txt
        ASSERT_EQ $x "file-bytes\n"
    "#};
    run_script(&root, script).expect("capture READ file");
}

#[test]
fn let_capture_expand() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        WRITE tmpl.txt "hello \{{ env:WHO }}"
        LET $x: STRING = EXPAND tmpl.txt WHO=World
        ASSERT_EQ $x "hello World"
    "#};
    run_script(&root, script).expect("capture EXPAND");
}

#[test]
fn let_capture_write_binds_empty() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $x: STRING = WRITE empty.txt content
        ASSERT_EQ $x ""
        LET $e: STRING = READ empty.txt
        ASSERT_EQ $e "content"
    "#};
    run_script(&root, script).expect("capture WRITE");
}

#[test]
fn let_capture_run() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // Windows needs the `cmd /c` prefix (and its echo appends a stray
    // trailing quote), so containment — not exact bytes — is asserted here.
    // Exact-byte capture is pinned by `let_capture_echo_binds_exact_bytes`.
    #[cfg(unix)]
    let shell_cmd = "echo run-cap";
    #[cfg(windows)]
    let shell_cmd = "cmd /c echo run-cap";
    let script = indoc! {r#"
        LET $x: STRING = RUN "{CMD}"
        ASSERT_CONTAINS $x "run-cap"
    "#}
    .replace("{CMD}", shell_cmd);
    run_script(&root, &script).expect("capture RUN");
}

#[test]
fn let_capture_command_failure_binds_nothing() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $x: STRING = READ "missing.txt"
    "#};
    let err = run_script(&root, script).expect_err("failing capture must fail");
    assert!(
        err.to_string().contains("missing.txt"),
        "unexpected error: {err}"
    );
}

#[test]
fn let_capture_with_stdin_pipe() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $relay: PIPE
        WITH_IO [stdout=$relay] ECHO piped
        LET $x: STRING = WITH_IO [stdin=$relay] READ
        ASSERT_EQ $x "piped\n"
    "#};
    run_script(&root, script).expect("capture with stdin pipe");
}

#[test]
fn let_capture_does_not_leak_into_parent_assert_stdout() {
    // Captured bytes must not tee into the parent stdout windows.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $x: STRING = ECHO hi
        ASSERT_CONTAINS stdout "hi"
    "#};
    let err = run_script(&root, script).expect_err("parent stdout must not see captured bytes");
    assert!(
        err.to_string().contains("did not contain"),
        "unexpected error: {err}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "AWAIT joins real background threads")]
fn let_await_capture_binds_task_output() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $t: HANDLE = ASYNC { ECHO "logged"; RETURN "returned" }
        LET $o: STRING = AWAIT $t
        ASSERT_EQ $o "returned"
    "#};
    run_script(&root, script).expect("LET $o: STRING = AWAIT $t");
}

#[test]
#[cfg_attr(miri, ignore = "AWAIT joins real background threads")]
fn bare_await_task_output_streams_live_to_parent() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $t: HANDLE = ASYNC ECHO fwd-hi
        AWAIT $t
    "#};
    let steps = oxdock_core::parse_script(script).expect("parse script");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut io_cfg = ExecIo::new();
    io_cfg.set_stdout(Some(captured.clone()));
    run_steps_with_context_result_with_io(&root, &root, &steps, io_cfg).expect("bare AWAIT");
    let contents = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert_eq!(contents, "fwd-hi\n");
}

#[test]
#[cfg_attr(miri, ignore = "AWAIT joins real background threads")]
fn await_capture_without_return_binds_zero() {
    // No stdout sniffing, no error either: a task that succeeded without
    // RETURN yields INT 0, like a process exit status.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $t: HANDLE = ASYNC ECHO hi
        LET $o: INT = AWAIT $t
        ASSERT_EQ $o 0
    "#};
    run_script(&root, script).expect("capture without RETURN binds 0");
}

#[test]
#[cfg_attr(miri, ignore = "AWAIT joins real background threads")]
fn await_capture_missing_fallback_branch_errors() {
    // A body that can RETURN on some path but falls off the end on the
    // path taken fails loudly: a missing fallback must never silently
    // bind the void-task zero.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $flag: BOOL = false
        LET $t: HANDLE = ASYNC {
            IF $flag {
                RETURN "yes"
            }
        }
        LET $o: STRING = AWAIT $t
    "#};
    let err = run_script(&root, script).expect_err("missing fallback must fail");
    assert!(
        err.to_string().contains("fell off the end without RETURN"),
        "unexpected error: {err}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "AWAIT joins real background threads")]
fn await_capture_exhaustive_branches_bind() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $flag: BOOL = false
        LET $t: HANDLE = ASYNC {
            IF $flag {
                RETURN "yes"
            }
            RETURN "fallback"
        }
        LET $o: STRING = AWAIT $t
        ASSERT_EQ $o "fallback"
    "#};
    run_script(&root, script).expect("fallthrough RETURN binds");
}

#[test]
#[cfg_attr(miri, ignore = "AWAIT joins real background threads")]
fn await_capture_then_bare_await_still_double_await_errors() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $t: HANDLE = ASYNC { ECHO "logged"; RETURN "returned" }
        LET $o: STRING = AWAIT $t
        AWAIT $t
    "#};
    let err = run_script(&root, script).expect_err("second AWAIT must fail");
    assert!(
        err.to_string().contains("already been awaited"),
        "unexpected error: {err}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "AWAIT joins real background threads")]
fn bare_await_then_capture_still_double_await_errors() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $t: HANDLE = ASYNC ECHO hi
        AWAIT $t
        LET $o: STRING = AWAIT $t
    "#};
    let err = run_script(&root, script).expect_err("second AWAIT must fail");
    assert!(
        err.to_string().contains("already been awaited"),
        "unexpected error: {err}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "AWAIT joins real background threads")]
fn await_capture_of_failing_task_propagates_error() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $t: HANDLE = ASYNC READ "missing.txt"
        LET $o: STRING = AWAIT $t
    "#};
    let err = run_script(&root, script).expect_err("capture of failing task must fail");
    assert!(
        err.to_string().contains("missing.txt"),
        "unexpected error: {err}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "CANCEL joins real background threads")]
fn await_capture_after_cancel_reports_cancelled() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $t: HANDLE = ASYNC SLEEP 30s
        CANCEL $t
        LET $o: STRING = AWAIT $t
    "#};
    let err = run_script(&root, script).expect_err("AWAIT after CANCEL must fail");
    assert!(err.to_string().contains("cancelled"), "{err}");
}
