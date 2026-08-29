use oxdock_parser::ast::*;
use oxdock_parser::{parse_braced_tokens, parse_script};
use proptest::prelude::*;
use proptest::strategy::BoxedStrategy;
use std::str::FromStr;

// Strategies

fn arb_platform_guard() -> impl Strategy<Value = PlatformGuard> {
    prop_oneof![
        Just(PlatformGuard::Unix),
        Just(PlatformGuard::Windows),
        Just(PlatformGuard::Macos),
        Just(PlatformGuard::Linux),
    ]
}

fn arb_guard() -> impl Strategy<Value = Guard> {
    prop_oneof![
        (arb_platform_guard(), any::<bool>())
            .prop_map(|(target, invert)| Guard::Platform { target, invert }),
        ("[a-zA-Z_][a-zA-Z0-9_]*", any::<bool>())
            .prop_map(|(key, invert)| Guard::EnvExists { key, invert }),
        (
            "[a-zA-Z_][a-zA-Z0-9_]*",
            "[a-zA-Z_][a-zA-Z0-9_]*",
            any::<bool>(),
        )
            .prop_map(|(key, value, invert)| Guard::EnvEquals { key, value, invert }),
    ]
}

fn arb_guard_expr_with_depth(depth: u32) -> BoxedStrategy<GuardExpr> {
    let leaf = arb_guard().prop_map(GuardExpr::from).boxed();
    if depth >= 3 {
        return leaf;
    }
    let deeper = arb_guard_expr_with_depth(depth + 1);
    prop_oneof![
        leaf,
        prop::collection::vec(deeper.clone(), 2..=3)
            .prop_map(GuardExpr::all)
            .boxed(),
        prop::collection::vec(deeper.clone(), 2..=3)
            .prop_map(GuardExpr::or)
            .boxed(),
        deeper.clone().prop_map(canonical_not).boxed(),
    ]
    .boxed()
}

fn arb_guard_expr() -> impl Strategy<Value = GuardExpr> {
    arb_guard_expr_with_depth(0)
}

fn canonical_not(expr: GuardExpr) -> GuardExpr {
    match expr {
        GuardExpr::Predicate(guard) => GuardExpr::Predicate(invert_guard_predicate(guard)),
        GuardExpr::Not(inner) => *inner,
        other => !other,
    }
}

fn invert_guard_predicate(guard: Guard) -> Guard {
    match guard {
        Guard::Platform { target, invert } => Guard::Platform {
            target,
            invert: !invert,
        },
        Guard::EnvExists { key, invert } => Guard::EnvExists {
            key,
            invert: !invert,
        },
        Guard::EnvEquals { key, value, invert } => Guard::EnvEquals {
            key,
            value,
            invert: !invert,
        },
    }
}

fn safe_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_./-]+"
        .prop_filter("Avoids comments", |s| !s.contains("//"))
        .prop_filter("Avoids invalid numeric prefixes", |s| {
            !has_invalid_prefixed_literal(s)
        })
}

fn safe_msg() -> impl Strategy<Value = String> {
    // Allow spaces and some punctuation, but avoid things that break the simple parser
    "[a-zA-Z0-9_./-][a-zA-Z0-9_./ -]*"
        .prop_map(|s| s.trim().to_string())
        .prop_filter("Avoids comments", |s| {
            !s.contains("//") && !s.contains("/*")
        })
        // Avoid hyphenated words without whitespace (ambiguous in TokenStream).
        .prop_filter("Avoids ambiguous hyphens", |s| {
            let chars: Vec<char> = s.chars().collect();
            for i in 0..chars.len() {
                if chars[i] != '-' {
                    continue;
                }
                let prev = i.checked_sub(1).and_then(|idx| chars.get(idx)).copied();
                let next = chars.get(i + 1).copied();
                if prev.is_some_and(|c| !c.is_whitespace())
                    && next.is_some_and(|c| !c.is_whitespace())
                {
                    return false;
                }
            }
            true
        })
        // Avoid sticky characters next to whitespace, as TokenStream loses this distinction
        // and macro_input.rs cannot perfectly reconstruct it without quotes.
        // Sticky chars: / . - : =
        .prop_filter("Avoids ambiguous spacing", |s| {
            // TokenStream collapses multiple spaces into one, so we can't round-trip them
            // without quoting, but quoting changes the AST (preserves quotes).
            if s.contains("  ") {
                return false;
            }
            let sticky = |c: char| matches!(c, '/' | '.' | '-' | ':' | '=');
            let chars: Vec<char> = s.chars().collect();
            for i in 0..chars.len() - 1 {
                let a = chars[i];
                let b = chars[i + 1];
                if (sticky(a) && b.is_whitespace()) || (a.is_whitespace() && sticky(b)) {
                    return false;
                }
            }
            true
        })
        .prop_filter("Avoids invalid numeric prefixes", |s| {
            !has_invalid_prefixed_literal(s)
        })
}

fn has_invalid_prefixed_literal(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'0' {
            let next = bytes[i + 1];
            let after = bytes.get(i + 2).copied();
            let valid = match next {
                b'b' | b'B' => after.is_some_and(|c| c == b'0' || c == b'1'),
                b'o' | b'O' => after.is_some_and(|c| matches!(c, b'0'..=b'7')),
                b'x' | b'X' => after.is_some_and(|c| c.is_ascii_hexdigit()),
                _ => {
                    i += 1;
                    continue;
                }
            };
            if !valid {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn arb_step_kind() -> impl Strategy<Value = StepKind> {
    prop_oneof![
        prop::collection::vec("[A-Z_][A-Z0-9_]*", 1..3).prop_map(|keys| StepKind::InheritEnv {
            keys: keys.into_iter().map(|k| k.to_string()).collect(),
        }),
        safe_string().prop_map(|s| StepKind::Workdir(s.into())),
        prop_oneof![
            Just(WorkspaceTarget::Snapshot),
            Just(WorkspaceTarget::Local)
        ]
        .prop_map(StepKind::Workspace),
        (safe_string(), safe_string()).prop_map(|(key, value)| StepKind::Env {
            key,
            value: value.into()
        }),
        safe_msg().prop_map(|s| StepKind::Run(s.into())),
        safe_msg().prop_map(|s| StepKind::Echo(s.into())),
        safe_msg().prop_map(|s| StepKind::RunBg(s.into())),
        (safe_string(), safe_string()).prop_map(|(from, to)| StepKind::Copy {
            from_current_workspace: false,
            from: from.into(),
            to: to.into()
        }),
        (safe_string(), safe_string()).prop_map(|(from, to)| StepKind::Symlink {
            from: from.into(),
            to: to.into()
        }),
        safe_string().prop_map(|s| StepKind::Mkdir(s.into())),
        prop::option::of(safe_string()).prop_map(|s| StepKind::Ls(s.map(Into::into))),
        Just(StepKind::Cwd),
        prop::option::of(safe_string()).prop_map(|s| StepKind::Read(s.map(Into::into))),
        (safe_string(), safe_msg()).prop_map(|(path, contents)| StepKind::Write {
            path: path.into(),
            contents: Some(contents.into())
        }),
        (safe_string(), safe_string()).prop_map(|(path, contents)| StepKind::RawWrite {
            path: path.into(),
            contents,
        }),
        (safe_string(), safe_string(), safe_string()).prop_map(|(rev, from, to)| {
            StepKind::CopyGit {
                rev: rev.into(),
                from: from.into(),
                to: to.into(),
                include_dirty: false,
            }
        }),
        safe_string().prop_map(|path| StepKind::HashSha256 { path: path.into() }),
        // The grammar defines ASSERT_FILE's hash form without trailing
        // contents, so generators must preserve that invariant for
        // Display round-trips.
        ("[0-9a-f]{64}", safe_string()).prop_map(|(digest, path)| StepKind::AssertFile {
            hash: Some(digest),
            path: path.into(),
            contents: None,
        }),
        (safe_string(), prop::option::of(safe_msg())).prop_map(|(path, contents)| {
            StepKind::AssertFile {
                hash: None,
                path: path.into(),
                contents: contents.map(Into::into),
            }
        }),
        safe_string().prop_map(|s| StepKind::AssertDir(s.into())),
        safe_string().prop_map(|s| StepKind::AssertAbsent(s.into())),
        safe_msg().prop_map(|s| StepKind::AssertStdout(s.into())),
        (0i32..255).prop_map(StepKind::Exit),
    ]
}

fn arb_step() -> impl Strategy<Value = Step> {
    (prop::option::of(arb_guard_expr()), arb_step_kind())
        .prop_map(|(guard, kind)| Step {
            guard,
            kind,
            scope_enter: 0,
            scope_exit: 0,
        })
        .prop_filter("Reject guarded INHERIT_ENV", |step| match &step.kind {
            StepKind::InheritEnv { .. } => step.guard.is_none(),
            _ => true,
        })
        .prop_filter("Reject strings that fail proc_macro2 lexing", |step| {
            let s = step.to_string();
            proc_macro2::TokenStream::from_str(&s).is_ok()
        })
}

fn assert_steps_eq(left: &Step, right: &Step, msg: &str) {
    assert_eq!(left.guard, right.guard, "Guards mismatch: {}", msg);
    assert_eq!(
        left.scope_enter, right.scope_enter,
        "Scope enter mismatch: {}",
        msg
    );
    assert_eq!(
        left.scope_exit, right.scope_exit,
        "Scope exit mismatch: {}",
        msg
    );

    match (&left.kind, &right.kind) {
        (StepKind::Run(l), StepKind::Run(r)) => {
            assert_eq!(l, r, "Run cmd mismatch: {}", msg)
        }
        (StepKind::RunBg(l), StepKind::RunBg(r)) => {
            assert_eq!(l, r, "RunBg cmd mismatch: {}", msg)
        }
        _ => assert_eq!(left.kind, right.kind, "Kind mismatch: {}", msg),
    }
}

proptest! {
    #[test]
    #[cfg_attr(
        miri,
        ignore = "requires real TokenStream/proc-macro parsing to validate API parity"
    )]
    fn fuzz_parity(step in arb_step()) {
        let s = step.to_string();

        // 1. Parse string
        let parsed_steps = parse_script(&s).expect("failed to parse generated string");
        assert_eq!(parsed_steps.len(), 1);
        let mut parsed_step = parsed_steps[0].clone();
        parsed_step.scope_enter = 0;
        parsed_step.scope_exit = 0;

        assert_steps_eq(&parsed_step, &step, &format!("String parse mismatch: {}", s));

        // 2. Parse tokens (if feature enabled)
        let ts: proc_macro2::TokenStream = s.parse().expect("failed to tokenize string");
        let token_steps = parse_braced_tokens(&ts).expect("failed to parse tokens");

        assert_eq!(token_steps.len(), 1);
        let mut token_step = token_steps[0].clone();
        token_step.scope_enter = 0;
        token_step.scope_exit = 0;

        assert_steps_eq(&token_step, &step, &format!("Token parse mismatch: {}", s));
    }
}
