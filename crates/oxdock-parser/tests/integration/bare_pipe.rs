use crate::common::mock_lower;

use indoc::indoc;
use oxdock_parser::ast::{Expr, StepKind};
use oxdock_parser::{lower_command, parse_script};

// Bare `LET $p: PIPE` mints a fresh backend per declaration: the shape
// lowers through the mock exactly like an initialized LET, with no
// registry or runtime involved.
#[test]
fn bare_let_pipe_shape_and_round_trip() {
    let script = indoc! {r#"
        LET $p: PIPE
        WITH_IO [stdout=$p] WRITE "out.txt"
    "#};
    let steps = parse_script(script, mock_lower).expect("bare LET plus variable binding parses");
    assert_eq!(steps.len(), 2);
    match &steps[0].kind {
        StepKind::Assign {
            var,
            decl_type,
            expr,
        } => {
            assert_eq!(var, "p");
            assert_eq!(decl_type, "PIPE");
            assert!(matches!(expr, Expr::FreshPipe));
        }
        other => panic!("expected Assign, got {other:?}"),
    }
    assert_eq!(steps[0].kind.to_string(), "LET $p: PIPE");
}

// Section 5 acceptance (reproduction-first): the exact nested bridge
// composition from `scripts/proto.echo-listen.oxdock` parses with the
// real command set. A bare mock lower cannot see CONNECT/LISTEN, so this
// pins the valid nested `WITH_IO`+`ASYNC` forms end to end: four
// top-level steps, with the bridge lines nested inside the loop body
// alongside per-iteration pipe declarations.
#[test]
fn proto_echo_listen_bridge_composition_parses() {
    let script = indoc! {r#"
        WORKSPACE LOCAL

        // Dummy ports: this test only parses and never executes, so no
        // socket is ever opened and nothing connects anywhere. Do not
        // copy real environment ports (e.g. container forwards) in here.
        LET $listen_port: INT = 19191
        LET $orb_port: INT = 29292

        WHILE true {
            ECHO "proxy listening on 127.0.0.1:{{ $listen_port }}"

            LET $req: PIPE
            LET $resp: PIPE
            LET $ls: HANDLE = ASYNC {
                WITH_IO [stdin=$req, stdout=$resp] LISTEN 127.0.0.1:{{ $listen_port }} --no-half-close
            }

            WITH_IO [stdin=$resp, stdout=$req] ASYNC CONNECT 127.0.0.1:{{ $orb_port }}

            AWAIT $ls
            ECHO "client disconnected"

            SLEEP 2
        }
    "#};
    let steps = parse_script(script, lower_command).expect("proto bridge composition parses");
    assert_eq!(steps.len(), 4, "expected 4 top-level steps, got {steps:?}");
    match &steps[3].kind {
        StepKind::While { body, .. } => {
            assert_eq!(body.len(), 8, "expected 8 loop-body steps, got {body:?}");
        }
        other => panic!("expected WHILE, got {other:?}"),
    }
}
