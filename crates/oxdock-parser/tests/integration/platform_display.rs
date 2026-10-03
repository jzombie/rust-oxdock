use crate::common::mock_lower;

use oxdock_parser::ast::{Guard, Ns, Step, StepKind};
use oxdock_parser::parse_script;

#[test]
fn platform_guard_display_uses_namespaces() {
    let guard = Guard::Attr {
        ns: Ns::Os,
        key: None,
        val: Some("windows".to_string()),
    };

    assert_eq!(guard.to_string(), "os:windows");

    let step = Step {
        guard: Some(guard.into()),
        kind: StepKind::Workdir("a".into()),
        scope_enter: 0,
        scope_exit: 0,
    };

    let rendered = step.to_string();
    assert_eq!(rendered, "[os:windows] WORKDIR a");

    let parsed = parse_script(&rendered, mock_lower).expect("round-trip parse");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].guard, step.guard);
    assert_eq!(parsed[0].kind, step.kind);
}

#[test]
fn family_alias_lowers_to_os_disjunction() {
    // `family:` is an accepted alias, never a stored namespace: it
    // lowers at parse to `os:` expressions.
    let parsed = parse_script("[family:unix] WORKDIR a", mock_lower).expect("alias parses");
    assert_eq!(parsed.len(), 1);
    let guard = parsed[0].guard.clone().expect("guard present");
    assert_eq!(guard.to_string(), "any(os:macos, os:linux)");

    let parsed = parse_script("[family:windows] WORKDIR a", mock_lower).expect("alias parses");
    let guard = parsed[0].guard.clone().expect("guard present");
    assert_eq!(guard.to_string(), "os:windows");
}
