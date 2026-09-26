use crate::common::mock_lower;

use indoc::indoc;
use oxdock_parser::StepKind;
use oxdock_parser::parse_script;

fn remote_case(script: &str) -> (String, Vec<String>, Vec<String>, Vec<oxdock_parser::Step>) {
    let steps = parse_script(script, mock_lower).expect("parse REMOTE");
    assert_eq!(steps.len(), 1, "expected a single REMOTE step");
    match &steps[0].kind {
        StepKind::RemoteBlock {
            target,
            vars,
            env,
            body,
        } => (target.clone(), vars.clone(), env.clone(), body.clone()),
        other => panic!("expected RemoteBlock, got {other:?}"),
    }
}

fn parse_err(script: &str) -> String {
    parse_script(script, mock_lower)
        .expect_err("script must fail to parse")
        .to_string()
}

#[test]
fn remote_block_with_header_list() {
    let script = indoc! {r#"
        REMOTE prod [$version, env:DEPLOY_ENV] {
            RUN ./deploy.sh $version
        }
    "#};
    let (target, vars, env, body) = remote_case(script);
    assert_eq!(target, "prod");
    assert_eq!(vars, vec!["version".to_string()]);
    assert_eq!(env, vec!["DEPLOY_ENV".to_string()]);
    assert_eq!(body.len(), 1);
    assert!(matches!(body[0].kind, StepKind::Run(_)));
}

#[test]
fn remote_block_without_header_is_sealed() {
    let (target, vars, env, body) = remote_case(indoc! {r#"
        REMOTE prod {
            ECHO "do something on remote"
        }
    "#});
    assert_eq!(target, "prod");
    assert!(vars.is_empty());
    assert!(env.is_empty());
    assert_eq!(body.len(), 1);
}

#[test]
fn remote_display_round_trips() {
    let script = indoc! {r#"
        REMOTE prod [$version, env:DEPLOY_ENV] {
            ECHO $version
        }
    "#};
    let steps = parse_script(script, mock_lower).expect("parse REMOTE");
    let rendered = steps
        .iter()
        .map(|step| step.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let reparsed = parse_script(&rendered, mock_lower).expect("reparse REMOTE");
    assert_eq!(steps, reparsed, "round-trip failed for {rendered}");
}

#[test]
fn remote_display_round_trips_sealed_form() {
    let script = indoc! {r#"
        REMOTE win-arm {
            ECHO hi
        }
    "#};
    let steps = parse_script(script, mock_lower).expect("parse REMOTE");
    let rendered = steps
        .iter()
        .map(|step| step.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert!(rendered.contains("REMOTE win-arm {"), "{rendered}");
    let reparsed = parse_script(&rendered, mock_lower).expect("reparse REMOTE");
    assert_eq!(steps, reparsed);
}

#[test]
fn remote_guarded_inner_steps_round_trip() {
    let script = indoc! {r#"
        REMOTE prod {
            [bool:true] ECHO guarded
            TIMEOUT 30s {
                ECHO nested
            }
        }
    "#};
    let steps = parse_script(script, mock_lower).expect("parse REMOTE");
    let body = match &steps[0].kind {
        StepKind::RemoteBlock { body, .. } => body.clone(),
        other => panic!("expected RemoteBlock, got {other:?}"),
    };
    assert_eq!(body.len(), 2);
    assert!(
        body[0].guard.is_some(),
        "bare guard line inside REMOTE body must survive lowering"
    );
    let rendered = steps
        .iter()
        .map(|step| step.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let reparsed = parse_script(&rendered, mock_lower).expect("reparse REMOTE");
    assert_eq!(steps, reparsed);
}

#[test]
fn remote_rejects_bad_targets() {
    // Grammar-level failures: the target cannot even lex, so any parse
    // error proves rejection (the message names the fallback instead).
    for target in ["", "has space"] {
        parse_script(
            &format!("REMOTE {target} {{\n ECHO hi\n}}\n"),
            mock_lower,
        )
        .expect_err("bad target must fail");
    }
    // Lowering-level failures: lexed fine, rejected with a named message.
    for target in ["-lead", "_lead", "has.dot", "a:b", "LOCAL", "REMOTE"] {
        let err = parse_err(&format!("REMOTE {target} {{\n ECHO hi\n}}\n"));
        assert!(err.contains("invalid REMOTE target"), "{target}: {err}");
    }
}

#[test]
fn remote_rejects_inline_form() {
    let err = parse_err("REMOTE prod ECHO hi\n");
    assert!(
        err.contains("REMOTE requires a braced block") || err.contains("REMOTE"),
        "{err}"
    );
}

#[test]
fn remote_rejects_empty_body() {
    let err = parse_err("REMOTE prod {\n}\n");
    assert!(err.contains("non-empty"), "{err}");
}

#[test]
fn remote_rejects_nesting() {
    let err = parse_err(indoc! {r#"
        REMOTE outer {
            REMOTE inner {
                ECHO hi
            }
        }
    "#});
    assert!(err.contains("cannot nest"), "{err}");
}

#[test]
fn remote_rejects_nesting_through_if() {
    let err = parse_err(indoc! {r#"
        REMOTE outer {
            IF true {
                REMOTE inner {
                    ECHO hi
                }
            }
        }
    "#});
    assert!(err.contains("cannot nest"), "{err}");
}

#[test]
fn remote_rejects_inherit_env_inside() {
    let err = parse_err(indoc! {r#"
        REMOTE prod {
            INHERIT_ENV [HOME]
            ECHO hi
        }
    "#});
    assert!(err.contains("INHERIT_ENV"), "{err}");
}

#[test]
fn remote_rejects_outer_func_calls() {
    let err = parse_err(indoc! {r#"
        FUNC BUILD($x: STRING) {
            RETURN $x
        }
        REMOTE prod {
            BUILD("hi")
        }
    "#});
    assert!(err.contains("calls outer function 'BUILD'"), "{err}");
}

#[test]
fn remote_allows_inner_func_defs() {
    let script = indoc! {r#"
        FUNC BUILD($x: STRING) {
            RETURN $x
        }
        REMOTE prod {
            FUNC BUILD($x: STRING) {
                RETURN $x
            }
            LET $out: STRING = BUILD("hi")
            ECHO $out
        }
    "#};
    let steps = parse_script(script, mock_lower).expect("parse REMOTE");
    let remote = steps
        .iter()
        .find_map(|step| match &step.kind {
            StepKind::RemoteBlock {
                target,
                vars,
                env,
                body,
            } => Some((target.clone(), vars.clone(), env.clone(), body.clone())),
            _ => None,
        })
        .expect("one REMOTE step");
    let (target, _, _, body) = remote;
    assert_eq!(target, "prod");
    assert_eq!(body.len(), 3);
}

#[test]
fn remote_allows_builtin_calls() {
    // mock_lower knows ECHO, WRITE, ENV, WORKDIR, WORKSPACE: all pass the
    // self-containment check because none is an outer user FUNC.
    let script = indoc! {r#"
        REMOTE prod {
            ECHO hi
            WRITE out.txt data
            ENV NOTE=hello
        }
    "#};
    let (_, _, _, body) = remote_case(script);
    assert_eq!(body.len(), 3);
}

#[test]
fn remote_header_rejects_malformed_entries() {
    let err = parse_err(indoc! {r#"
        REMOTE prod [version] {
            ECHO hi
        }
    "#});
    assert!(err.contains("REMOTE"), "{err}");
    assert!(!err.contains("cannot nest"), "{err}");
}

#[test]
fn remote_single_line_with_io_prefix_parses() {
    // WITH_IO treats REMOTE like any other command: the block rides along.
    let script = indoc! {r#"
        LET $out: PIPE
        WITH_IO [stdout=$out] REMOTE prod {
            ECHO hi
        }
    "#};
    let steps = parse_script(script, mock_lower).expect("parse WITH_IO REMOTE");
    assert_eq!(steps.len(), 2);
    match &steps[1].kind {
        StepKind::WithIo { cmd, .. } => {
            assert!(
                matches!(cmd.as_ref(), StepKind::RemoteBlock { .. }),
                "WITH_IO must wrap RemoteBlock, got {:?}",
                cmd
            );
        }
        other => panic!("expected WithIo, got {other:?}"),
    }
}

#[test]
fn remote_single_line_timeout_prefix_parses() {
    let script = indoc! {r#"
        TIMEOUT 30s REMOTE prod {
            ECHO hi
        }
    "#};
    let steps = parse_script(script, mock_lower).expect("parse TIMEOUT REMOTE");
    assert_eq!(steps.len(), 1);
    match &steps[0].kind {
        StepKind::Timeout { body, .. } => {
            assert_eq!(body.len(), 1);
            assert!(matches!(body[0].kind, StepKind::RemoteBlock { .. }));
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
}

#[test]
fn remote_single_line_async_prefix_parses() {
    let script = indoc! {r#"
        ASYNC REMOTE prod {
            ECHO hi
        }
    "#};
    let steps = parse_script(script, mock_lower).expect("parse ASYNC REMOTE");
    assert_eq!(steps.len(), 1);
    match &steps[0].kind {
        StepKind::AsyncBlock { body } => {
            assert_eq!(body.len(), 1);
            assert!(matches!(body[0].kind, StepKind::RemoteBlock { .. }));
        }
        other => panic!("expected AsyncBlock, got {other:?}"),
    }
}

#[test]
fn remote_wraps_in_with_io_block() {
    let script = indoc! {r#"
        LET $out: PIPE
        WITH_IO [stdout=$out] {
            REMOTE prod {
                ECHO hi
            }
        }
    "#};
    let steps = parse_script(script, mock_lower).expect("parse WITH_IO REMOTE");
    assert_eq!(steps.len(), 2);
    match &steps[1].kind {
        StepKind::WithIo { cmd, .. } => {
            assert!(
                matches!(cmd.as_ref(), StepKind::RemoteBlock { .. }),
                "WITH_IO must wrap RemoteBlock, got {:?}",
                cmd
            );
        }
        other => panic!("expected WithIo, got {other:?}"),
    }
}

#[test]
fn remote_target_charset_edges() {
    for target in ["a", "win-arm", "mac_mini_01", "A0-9_z"] {
        let (_, _, _, _) = remote_case(&format!("REMOTE {target} {{\n ECHO hi\n}}\n"));
    }
    let long = "a".repeat(64);
    let (_, _, _, _) = remote_case(&format!("REMOTE {long} {{\n ECHO hi\n}}\n"));
    let too_long = "a".repeat(65);
    let err = parse_err(&format!("REMOTE {too_long} {{\n ECHO hi\n}}\n"));
    assert!(err.contains("invalid REMOTE target"), "{err}");
}
