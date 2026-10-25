use crate::common::parse_with_math;

// Release-script shell lines that must parse under small-stack
// runners (musl static binary). LET-capture used to re-parse the
// single instruction line nested inside the whole-file parse,
// doubling native stack usage and tripping pest's stack guard;
// the pair now lowers directly, so Docker-style bare shell forms
// (`&&`, `||`, `>`, quoted paths) parse at any stack size.
const PROBE: &str = r#"LET $probe: STRING = RUN cd "{{ $probedir }}" && cargo info "{{ $pname }}@{{ $pver }}" >/dev/null 2>&1 || printf OXDOCK_PUBLISH_MISSING"#;
const DIAG: &str = r#"LET $diag: STRING = RUN cat "{{ $probedir }}/publish-round.log""#;
const REV_PARSE: &str = r#"LET $head: STRING = RUN git rev-parse HEAD"#;
const PUBLISH: &str = r#"LET $round_out: STRING = RUN cargo publish --workspace{{ $excludes }} >"{{ $probedir }}/publish-round.log" 2>&1 || printf OXDOCK_PUBLISH_ROUND_FAILED"#;

fn parse_prod(script: &str) {
    parse_with_math(script, oxdock_parser::lower_command).expect("parse shell RUN");
}

#[test]
fn shell_run_lines_parse_on_small_stack() {
    for stack in [128 * 1024, 256 * 1024, 512 * 1024, 1024 * 1024] {
        let child = std::thread::Builder::new()
            .stack_size(stack)
            .spawn(|| {
                parse_prod(PROBE);
                parse_prod(DIAG);
                parse_prod(REV_PARSE);
                parse_prod(PUBLISH);
            })
            .expect("spawn parse thread");
        child.join().expect("shell RUN must parse on a small stack");
    }
}
