mod common;
use common::mock_lower;

use oxdock_parser::ast::{Guard, PlatformGuard, Step, StepKind};
use oxdock_parser::parse_script;

#[test]
fn platform_guard_display_uses_equals() {
    let guard = Guard::Platform {
        target: PlatformGuard::Unix,
        invert: false,
    };

    assert_eq!(guard.to_string(), "unix");

    let step = Step {
        guard: Some(guard.into()),
        kind: StepKind::Workdir("a".into()),
        scope_enter: 0,
        scope_exit: 0,
    };

    let rendered = step.to_string();
    assert_eq!(rendered, "[unix] WORKDIR a");

    let parsed = parse_script(&rendered, mock_lower).expect("round-trip parse");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].guard, step.guard);
    assert_eq!(parsed[0].kind, step.kind);
}
