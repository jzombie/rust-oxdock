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
        WRITE captured.txt "{{ $x }}"
        ASSERT_FILE captured.txt "hi\n"
    "#};
    run_script(&root, script).expect("capture ECHO");
    assert_eq!(
        read_file(&root.join("captured.txt").unwrap()),
        "hi\n",
        "capture must preserve the trailing newline exactly"
    );
}

#[test]
fn let_capture_empty_stdout_binds_empty_string() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $x: STRING = MKDIR emptydir
        WRITE marker.txt "done-{{ $x }}!"
        ASSERT_FILE marker.txt "done-!"
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
        WRITE out.txt "{{ $x }}"
        ASSERT_FILE out.txt "file-bytes\n"
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
        WRITE out.txt "{{ $x }}"
        ASSERT_FILE out.txt "hello World"
    "#};
    run_script(&root, script).expect("capture EXPAND");
}

#[test]
fn let_capture_write_binds_empty() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $x: STRING = WRITE empty.txt content
        WRITE marker.txt "done-{{ $x }}!"
        ASSERT_FILE marker.txt "done-!"
        ASSERT_FILE empty.txt content
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
        WRITE out.txt "{{ $x }}"
    "#}
    .replace("{CMD}", shell_cmd);
    run_script(&root, &script).expect("capture RUN");
    let contents = read_file(&root.join("out.txt").unwrap());
    assert!(
        contents.contains("run-cap"),
        "RUN capture must contain shell output, got {contents:?}"
    );
}

#[test]
fn let_capture_command_failure_binds_nothing() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $x: STRING = ASSERT_FILE missing.txt
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
        WITH_IO [stdout=pipe:relay] ECHO piped
        LET $x: STRING = WITH_IO [stdin=pipe:relay] READ
        WRITE out.txt "{{ $x }}"
        ASSERT_FILE out.txt "piped\n"
    "#};
    run_script(&root, script).expect("capture with stdin pipe");
}

#[test]
fn let_capture_does_not_leak_into_parent_assert_stdout() {
    // Captured bytes must not tee into the parent ASSERT_STDOUT windows.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $x: STRING = ECHO hi
        ASSERT_STDOUT hi
    "#};
    let err =
        run_script(&root, script).expect_err("parent ASSERT_STDOUT must not see captured bytes");
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
        LET $t: HANDLE = ASYNC ECHO "task-hi"
        LET $o: STRING = AWAIT $t
        WRITE out.txt "{{ $o }}"
        ASSERT_FILE out.txt "task-hi\n"
    "#};
    run_script(&root, script).expect("LET $o: STRING = AWAIT $t");
}

#[test]
#[cfg_attr(miri, ignore = "AWAIT joins real background threads")]
fn bare_await_forwards_task_output_to_parent() {
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
fn await_capture_then_bare_await_still_double_await_errors() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $t: HANDLE = ASYNC ECHO hi
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
        LET $t: HANDLE = ASYNC ASSERT_FILE missing.txt
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
