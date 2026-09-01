use indoc::indoc;
use oxdock_cli::{Options, ScriptSource, Step, StepKind};
use oxdock_core::parse_script;
use oxdock_fs::{GuardedPath, GuardedTempDir};
use oxdock_parser::Arg;

fn workspace_root() -> GuardedTempDir {
    GuardedPath::tempdir().expect("tmpdir")
}

#[test]
fn parse_basic_script() {
    let script = indoc! {
        r#"
        # build and publish
        WORKDIR "client"
        RUN "npm run build"
        WORKDIR "/"
        COPY "client/dist" "client/dist"
        SYMLINK "server/dist" "../client/dist"
        RUN "cargo publish --workspace --locked"
        MKDIR "tmp"
        LS "tmp"
        WRITE "tmp/file.txt" "hello"
        "#
    };

    let steps = parse_script(script).expect("parse should succeed");
    assert_eq!(steps.len(), 9);
    if let Step {
        kind: StepKind::Workdir(ref p),
        ..
    } = steps[0]
    {
        assert_eq!(p, "client");
    } else {
        panic!();
    }
    if let Step {
        kind: StepKind::Run(ref p),
        ..
    } = steps[1]
    {
        assert_eq!(p, "npm run build");
    } else {
        panic!();
    }
}

#[test]
fn parse_mkdir_ls_write() {
    let script = indoc! {
        "
        MKDIR \"a/b\"
        LS \"a\"
        WRITE \"a/b/file.txt\" \"hi\"
        "
    };
    let steps = parse_script(script).expect("parse should succeed");
    assert_eq!(steps.len(), 3);
    if let Step {
        kind: StepKind::Mkdir(ref p),
        ..
    } = steps[0]
    {
        assert_eq!(p, "a/b");
    } else {
        panic!();
    }
}

#[test]
fn parse_cat() {
    let script = "READ \"path/to/file.txt\"";
    let steps = parse_script(script).expect("parse should succeed");
    assert_eq!(steps.len(), 1);
    if let Step {
        kind: StepKind::Read(Some(ref p)),
        ..
    } = steps[0]
    {
        assert_eq!(p, "path/to/file.txt");
    } else {
        panic!();
    }
}

#[test]
fn parse_options_accepts_stdin_dash() {
    let mut args = "--script -".split_whitespace().map(String::from);
    let root = workspace_root();
    let opts = Options::parse(&mut args, &root).expect("options parse should succeed");
    match opts.script {
        ScriptSource::Stdin => {}
        _ => panic!("expected stdin source when passing '-'"),
    }
    assert!(!opts.shell);
}

#[test]
fn parse_options_defaults_to_stdin() {
    let mut args = std::iter::empty();
    let root = workspace_root();
    let opts = Options::parse(&mut args, &root).expect("options parse should succeed");
    match opts.script {
        ScriptSource::Stdin => {}
        _ => panic!("expected stdin source by default"),
    }
    assert!(!opts.shell);
}

#[test]
fn parse_options_accepts_shell_flag() {
    let mut args = "--shell".split_whitespace().map(String::from);
    let root = workspace_root();
    let opts = Options::parse(&mut args, &root).expect("options parse should succeed");
    assert!(opts.shell);
    match opts.script {
        ScriptSource::Stdin => {}
        _ => panic!("expected stdin default even when shell is requested"),
    }
}

#[test]
fn parse_semicolon_keeps_shell_payload_together() {
    let script = "RUN \"echo one; echo two\"";
    let steps = parse_script(script).expect("parse should succeed");
    assert_eq!(steps.len(), 1);
    if let Step {
        kind: StepKind::Run(ref cmd),
        ..
    } = steps[0]
    {
        assert_eq!(cmd, "echo one; echo two");
    } else {
        panic!("expected RUN step");
    }
}

#[test]
fn parse_semicolon_splits_multiple_instructions() {
    let script = "WRITE \"one.txt\" \"1\"; WRITE \"two.txt\" \"2\"";
    let steps = parse_script(script).expect("parse should succeed");
    assert_eq!(steps.len(), 2);
    match &steps[0].kind {
        StepKind::Write { path, contents } => {
            assert_eq!(path, "one.txt");
            assert_eq!(contents.as_ref().map(Arg::as_str), Some("1"));
        }
        _ => panic!("expected first WRITE"),
    }
    match &steps[1].kind {
        StepKind::Write { path, contents } => {
            assert_eq!(path, "two.txt");
            assert_eq!(contents.as_ref().map(Arg::as_str), Some("2"));
        }
        _ => panic!("expected second WRITE"),
    }
}

#[test]
fn parse_multi_line_guard_block() {
    let script = indoc! {r#"
        [ eq(env:MODE, debug),
          linux
        ]
        WRITE "guarded.txt" "ok"
    "#};
    let steps = parse_script(script).expect("parse should succeed");
    assert_eq!(steps.len(), 1);
    assert!(steps[0].guard.is_some());
    match &steps[0].kind {
        StepKind::Write { path, .. } => assert_eq!(path, "guarded.txt"),
        _ => panic!("expected WRITE"),
    }
}

#[test]
fn parse_guarded_block_applies_to_all_commands() {
    let script = indoc! {r#"
        [eq(env:TEST, 1)] {
            WRITE "one.txt" "1"
            WRITE "two.txt" "2"
        }
        WRITE "three.txt" "3"
    "#};
    let steps = parse_script(script).expect("parse should succeed");
    assert_eq!(steps.len(), 3);
    assert!(steps[0].guard.is_some());
    assert!(steps[1].guard.is_some());
    assert!(steps[2].guard.is_none());
}
