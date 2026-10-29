//! `WITH_IO [tty]` end-to-end runs over real pseudo-terminals.
//!
//! Every test here allocates a real pty, pumps threads, and spawns real
//! subprocesses, so all of them skip under Miri with a reason. Scripts
//! run through the native manager in hermetic tempdirs; each run carries
//! a bounded wait so a terminal regression fails loudly instead of
//! hanging the suite.

use std::time::Duration;

use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver};
use oxdock_parser::Step;

fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
    temp.as_guarded_path().clone()
}

fn read_text(root: &GuardedPath, rel: &str) -> String {
    let path = root.join(rel).unwrap();
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    resolver.read_to_string(&path).expect("output file reads")
}

fn parse(script: &str) -> Vec<Step> {
    oxdock_core::parse_script(script).expect("parse script")
}

/// Run a script to completion with a bounded wait: a stranded pty
/// session fails naming the timeout instead of hanging the suite.
fn run_bounded(root: &GuardedPath, steps: Vec<Step>, limit: Duration) -> anyhow::Result<()> {
    let owned_root = root.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(
            run_steps_with_context_result_with_io(&owned_root, &owned_root, &steps, ExecIo::new())
                .map(|_| ()),
        );
    });
    match done_rx.recv_timeout(limit) {
        Ok(outcome) => outcome.map_err(|err| anyhow::anyhow!("{err:#}")),
        Err(_) => anyhow::bail!("script did not complete within {limit:?}"),
    }
}

fn run_script(root: &GuardedPath, script: &str) -> anyhow::Result<()> {
    let steps = parse(script);
    run_bounded(root, steps, Duration::from_secs(60))
}

#[test]
#[cfg_attr(miri, ignore = "allocates a real pty, which dies under Miri isolation")]
fn tty_echo_roundtrip() {
    // Bare terminal run: the child sees a terminal, its output lands
    // in the stdout pipe, and assertions observe it like any pipe.
    // Shell form keeps this portable: `sh -c` on Unix, `cmd /C` on
    // Windows, both spelling `echo` the same way.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $p: PIPE
        WITH_IO [tty, stdout=$p] RUN "echo hello-tty"
        WITH_IO [stdin=$p] READ_LINE $line
        ASSERT_CONTAINS $line "hello-tty"
    "#};
    run_script(&root, script).expect("pty echo round-trips");
}

#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "allocates a real pty, which dies under Miri isolation")]
fn tty_translates_newlines() {
    // Pty line discipline translates `\n` to `\r\n`: authors must
    // expect the translation instead of byte-identical pipes. Unix
    // only: the assertion pins `onlcr` behavior, and Windows proves
    // the same path through the ConPTY legs of the smoke matrix.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $p: PIPE
        WITH_IO [tty, stdout=$p] RUN "echo hello-tty"
        WITH_IO [stdin=$p] WRITE captured.txt
    "#};
    run_script(&root, script).expect("pty run completes");
    let captured = read_text(&root, "captured.txt");
    assert_eq!(
        captured.as_bytes(),
        b"hello-tty\r\n",
        "pty output must carry CR LF translation, got {captured:?}"
    );
}

#[test]
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "spawns a real subprocess, which dies under Miri isolation"
)]
fn bare_run_has_no_pty_translation() {
    // No `tty` flag means hermetic pipes: the byte stream stays
    // byte-identical, so `\r` never appears without the flag.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $p: PIPE
        WITH_IO [stdout=$p] RUN "echo plain"
        WITH_IO [stdin=$p] WRITE captured.txt
    "#};
    run_script(&root, script).expect("piped run completes");
    let captured = read_text(&root, "captured.txt");
    assert_eq!(
        captured.as_bytes(),
        b"plain\n",
        "piped output must stay byte-identical, got {captured:?}"
    );
}

#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "allocates a real pty, which dies under Miri isolation")]
fn tty_prompt_loop_end_to_end() {
    // The isolated-leg scenario hermetically: an answer piped into the
    // master feeds an interactive prompt, and the prompted directory
    // lands. Unix only: the prompt stanza needs `sh`/`read`, and
    // Windows proves the same path through the ConPTY legs of the
    // smoke matrix instead.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let dir = root
        .as_path()
        .join("prompted")
        .to_string_lossy()
        .into_owned();
    let script = indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $answer: PIPE
        WITH_IO [stdout=$answer] ECHO "@DIR@"
        LET $out: PIPE
        WITH_IO [tty, stdin=$answer, stdout=$out] RUN ["sh", "-c", "read dest && mkdir -p \"$dest/sub\" && echo done \"$dest\""]
        # The master echoes the answer before the child output lands:
        # consume the echo line, then the completion line.
        WITH_IO [stdin=$out] READ_LINE $echoed
        ASSERT_CONTAINS $echoed "prompted"
        WITH_IO [stdin=$out] READ_LINE $line
        ASSERT_CONTAINS $line "done"
        LET $kind: STRING = PATH_TYPE("@DIR@/sub")
        ASSERT_EQ $kind "dir"
    "#}
    .replace("@DIR@", &dir);
    run_script(&root, &script).expect("prompt loop installs the prompted dir");
}

#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "allocates a real pty, which dies under Miri isolation")]
fn tty_stdin_starts_closed() {
    // Bare `[tty]` feeds nothing into the master: a child polling
    // stdin observes no bytes (in particular no EOF-nudge bytes, which
    // only follow a real input worker). `read -t 0` polls without
    // blocking, so an open stdin could never hang this either way.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $p: PIPE
        WITH_IO [tty, stdout=$p] RUN ["bash", "-c", "if read -t 0 line; then echo HASINPUT; else echo NOINPUT; fi"]
        WITH_IO [stdin=$p] READ_LINE $line
        ASSERT_CONTAINS $line "NOINPUT"
    "#};
    run_script(&root, script).expect("closed stdin reports no input");
}

#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "allocates a real pty, which dies under Miri isolation")]
fn tty_partial_line_input_still_terminates() {
    // Input ending mid-line (no trailing newline): the release nudge
    // submits the pending partial line before signalling EOF, so a
    // read-until-EOF child terminates instead of stranding the
    // session. `cat` mirrors stdin to stdout, making the partial
    // bytes observable through the same master they arrived on.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $answer: PIPE
        WITH_IO [stdout=$answer] RUN "printf 'partial'"
        LET $out: PIPE
        WITH_IO [tty, stdin=$answer, stdout=$out] RUN ["sh", "-c", "cat"]
        WITH_IO [stdin=$out] READ_LINE $line
        ASSERT_CONTAINS $line "partial"
    "#};
    run_script(&root, script).expect("partial-line input terminates the child");
}

#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "allocates a real pty, which dies under Miri isolation")]
fn tty_empty_input_injects_no_line() {
    // An input pipe whose producers complete with zero bytes is EOF
    // with nothing fed: the nudge must not inject a newline of its
    // own, so a read-until-EOF child observes immediate EOF and the
    // master carries no phantom empty line. The assertion is
    // line-shaped, not byte-exact: platforms with terminal ECHOCTL
    // echo the EOT itself as a `^D` marker, which CONTAINS-style
    // assertions sail past. (A never-written pipe is a different
    // shape: script pipes without writers block like everywhere else
    // in the DSL, so only completed-empty proves the nudge clean.)
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $answer: PIPE
        WITH_IO [stdout=$answer] RUN "printf ''"
        LET $out: PIPE
        WITH_IO [tty, stdin=$answer, stdout=$out] RUN ["sh", "-c", "cat"]
        WITH_IO [stdin=$out] WRITE captured.txt
    "#};
    run_script(&root, script).expect("empty input terminates the child");
    let captured = read_text(&root, "captured.txt");
    assert!(
        !captured.contains("\r\n"),
        "no input must mean no output lines, got {captured:?}"
    );
}

#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "allocates a real pty, which dies under Miri isolation")]
fn tty_explicit_geometry_reaches_child() {
    // Named geometry wins over any host window: the child observes
    // exactly the requested size. Unix only: discovering the window
    // needs `stty`, and Windows proves explicit sizing through the
    // ConPTY legs of the smoke matrix instead.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $out: PIPE
        WITH_IO [tty, rows=40, cols=100, stdout=$out] RUN ["sh", "-c", "stty size"]
        WITH_IO [stdin=$out] READ_LINE $line
        ASSERT_CONTAINS $line "40 100"
    "#};
    run_script(&root, script).expect("explicit geometry reaches the child");
}

#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "allocates a real pty, which dies under Miri isolation")]
fn tty_unnamed_geometry_is_a_real_window() {
    // Unnamed sides read the host terminal when one sits behind our
    // own stdio, else the 24x80 fallback: either way the child sees
    // a positive, sane window, never zeros. Unix only, same `stty`
    // rationale as the explicit test above.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $out: PIPE
        WITH_IO [tty, stdout=$out] RUN ["sh", "-c", "stty size"]
        WITH_IO [stdin=$out] READ_LINE $line
        WRITE size.txt "{{ $line }}"
    "#};
    run_script(&root, script).expect("host geometry reaches the child");
    let size = read_text(&root, "size.txt");
    let mut dims = size.split_whitespace();
    let rows: u64 = dims.next().unwrap_or("0").parse().unwrap_or(0);
    let cols: u64 = dims.next().unwrap_or("0").parse().unwrap_or(0);
    assert!(
        rows > 0 && cols > 0,
        "child must observe a positive window, got {size:?}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "spawns worker threads; thread scheduling under Miri isolation is nondeterministic"
)]
fn tty_stdio_never_promotes_to_os_pipe() {
    // The promotion walk skips terminal-bound descriptors: inside an
    // `ASYNC` single-`RUN` body (the exact shape that promotes task
    // pipes to OS kernel pairs) a `tty` pipe inspects as `script`.
    // Without the skip this reads `os`, so the assertion is the proof.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $t: HANDLE = ASYNC {
            LET $p: PIPE
            LET $q: PIPE
            WITH_IO [tty, stdin=$p, stdout=$q] RUN "echo hi"
            LET $info: MAP<ANY> = INSPECT($q)
            WRITE kind.txt "{{ $info.pipe_kind }}"
        }
        AWAIT $t
    "#};
    run_script(&root, script).expect("terminal run completes under ASYNC");
    let kind = read_text(&root, "kind.txt");
    assert_eq!(
        kind, "script",
        "terminal stdio must stay script-backed, got {kind:?}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "allocates a real pty, which dies under Miri isolation")]
fn tty_block_backstop_rejects_non_run() {
    // Direct misuse fails at parse; block-expanded defaults reach the
    // same rules at runtime, so every construction shares one story.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        WITH_IO [tty] {
            ECHO hi
        }
    "#};
    let err = run_script(&root, script).expect_err("tty outside RUN must fail");
    assert!(
        format!("{err:#}").contains("only wraps a RUN step"),
        "unexpected error: {err:#}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "allocates a real pty, which dies under Miri isolation")]
fn tty_block_backstop_rejects_stderr() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $e: PIPE
        WITH_IO [tty] {
            WITH_IO [stderr=$e] RUN "echo hi"
        }
    "#};
    let err = run_script(&root, script).expect_err("tty with stderr must fail");
    assert!(
        format!("{err:#}").contains("demultiplex"),
        "unexpected error: {err:#}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "allocates a real pty, which dies under Miri isolation")]
fn tty_duplicate_bindings_fail() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    for bindings in [
        "tty, tty",
        "tty, rows=24, rows=40",
        "tty, cols=80, cols=100",
    ] {
        let script =
            format!("IMPORT [STD]\nINHERIT_ENV [PATH]\nWITH_IO [{bindings}] RUN \"echo hi\"\n");
        let err = run_script(&root, &script).expect_err("duplicate bindings must fail");
        assert!(
            format!("{err:#}").contains("more than once"),
            "unexpected error for [{bindings}]: {err:#}"
        );
    }
}
