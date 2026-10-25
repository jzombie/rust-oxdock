use indoc::indoc;
use oxdock_parser::ast::{IoStream, PipeTarget, StepKind};
use oxdock_parser::{lower_command as mock_lower, parse_script};

#[test]
fn with_io_tty_wraps_run() {
    let steps = parse_script("WITH_IO [tty] RUN \"echo hi\"", mock_lower).expect("parse ok");
    assert_eq!(steps.len(), 1);
    match &steps[0].kind {
        StepKind::WithIo { bindings, cmd } => {
            assert_eq!(bindings.len(), 1);
            assert_eq!(bindings[0].stream, IoStream::Tty);
            assert_eq!(bindings[0].pipe, None);
            assert!(matches!(cmd.as_ref(), StepKind::Run(_)));
        }
        other => panic!("expected WITH_IO, saw {other:?}"),
    }
    assert_eq!(steps[0].kind.to_string(), "WITH_IO [tty] RUN echo hi");
}

#[test]
fn with_io_tty_full_binding_shape() {
    let steps = parse_script(
        "WITH_IO [stdin=$answer, tty, rows=40, cols=100] RUN [\"bash\", \"./install.sh\"]",
        mock_lower,
    )
    .expect("parse ok");
    assert_eq!(steps.len(), 1);
    match &steps[0].kind {
        StepKind::WithIo { bindings, cmd } => {
            assert_eq!(bindings.len(), 4);
            assert!(bindings.iter().any(|b| matches!(b.stream, IoStream::Stdin)
                && b.pipe == Some(PipeTarget::Var("answer".to_string()))));
            assert!(
                bindings
                    .iter()
                    .any(|b| matches!(b.stream, IoStream::Tty) && b.pipe.is_none())
            );
            assert!(
                bindings
                    .iter()
                    .any(|b| matches!(b.stream, IoStream::Rows(40)))
            );
            assert!(
                bindings
                    .iter()
                    .any(|b| matches!(b.stream, IoStream::Cols(100)))
            );
            assert!(matches!(cmd.as_ref(), StepKind::RunExec { .. }));
        }
        other => panic!("expected WITH_IO, saw {other:?}"),
    }
    assert_eq!(
        steps[0].kind.to_string(),
        "WITH_IO [stdin=$answer, tty, rows=40, cols=100] RUN [\"bash\", \"./install.sh\"]"
    );
}

#[test]
fn with_io_tty_block_wraps_each_step() {
    let script = indoc! {r#"
        WITH_IO [tty] {
            RUN "echo hi"
        }
    "#};
    let steps = parse_script(script, mock_lower).expect("parse ok");
    assert_eq!(steps.len(), 1);
    match &steps[0].kind {
        StepKind::WithIo { bindings, cmd } => {
            assert!(bindings.iter().any(|b| matches!(b.stream, IoStream::Tty)));
            assert!(matches!(cmd.as_ref(), StepKind::Run(_)));
        }
        other => panic!("expected WITH_IO, saw {other:?}"),
    }
}

#[test]
fn with_io_tty_rejects_value() {
    // `tty` is a bare flag: the name stays reserved so `tty = $x`
    // fails at parse naming the valid shapes.
    let err = parse_script("WITH_IO [tty=$p] RUN \"echo hi\"", mock_lower)
        .expect_err("tty with a value must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("takes no value"), "unexpected error: {msg}");
    assert!(msg.contains("rows="), "unexpected error: {msg}");
}

#[test]
fn with_io_tty_rejects_non_run() {
    let err =
        parse_script("WITH_IO [tty] ECHO hi", mock_lower).expect_err("tty outside RUN must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("only wraps a RUN step"),
        "unexpected error: {msg}"
    );
}

#[test]
fn with_io_tty_rejects_stderr() {
    // One interleaved master stream: stdout and stderr cannot demultiplex.
    let err = parse_script("WITH_IO [tty, stderr=$e] RUN \"echo hi\"", mock_lower)
        .expect_err("tty with stderr must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("demultiplex"), "unexpected error: {msg}");
}

#[test]
fn with_io_geometry_needs_tty() {
    let err = parse_script("WITH_IO [rows=40] RUN \"echo hi\"", mock_lower)
        .expect_err("geometry without tty must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("need [tty]"), "unexpected error: {msg}");
}

#[test]
fn with_io_geometry_rejects_non_positive() {
    for bad in ["rows=0", "cols=0", "rows=65536"] {
        let script = format!("WITH_IO [tty, {bad}] RUN \"echo hi\"");
        let err = parse_script(&script, mock_lower).expect_err("out-of-range geometry must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("positive integer 1-65535"),
            "unexpected error for {bad}: {msg}"
        );
    }
}

#[test]
fn with_io_geometry_rejects_non_integer() {
    // Non-numeric geometry never reaches the grammar rule: the
    // fallback hint names the integer shape instead.
    let err = parse_script("WITH_IO [tty, rows=many] RUN \"echo hi\"", mock_lower)
        .expect_err("non-integer geometry must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("rows=40"), "unexpected error: {msg}");
}

#[test]
fn with_io_unknown_stream_names_tty_shapes() {
    for bad in ["term", "row", "col"] {
        let script = format!("WITH_IO [{bad}] RUN \"echo hi\"");
        let err = parse_script(&script, mock_lower).expect_err("unknown stream must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("tty"), "unexpected error for {bad}: {msg}");
    }
}
