use oxdock_parser::ast::*;
use oxdock_parser::parse_braced_tokens;
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
        arb_platform_guard().prop_map(|target| Guard::Platform { target }),
        "[a-zA-Z_][a-zA-Z0-9_]*".prop_map(|key| Guard::EnvExists { key }),
        ("[a-zA-Z_][a-zA-Z0-9_]*", "[a-zA-Z_][a-zA-Z0-9_]*",)
            .prop_map(|(key, value)| Guard::EnvEquals { key, value }),
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
        GuardExpr::Not(inner) => *inner,
        other => GuardExpr::Not(Box::new(other)),
    }
}

fn safe_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_./:-]+"
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
        prop::collection::vec(safe_string(), 1..3).prop_map(|items| StepKind::RunExec {
            argv: items
                .into_iter()
                .map(|s| Arg::Expr(Expr::Literal(Value::String(s))))
                .collect(),
        }),
        safe_msg().prop_map(|s| StepKind::Echo(s.into())),
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
        "[a-zA-Z_][a-zA-Z0-9_]*".prop_map(|v| StepKind::ReadLine { var: v }),
        (safe_string(), safe_msg()).prop_map(|(path, contents)| StepKind::Write {
            path: path.into(),
            contents: Some(contents.into())
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
        // The grammar defines ASSERT_EQ's hash form with a value actual,
        // so generators must preserve that shape for Display round-trips.
        ("[0-9a-f]{64}", safe_string()).prop_map(|(digest, actual)| {
            StepKind::AssertEq {
                hash: Some(digest),
                actual: AssertTarget::Value(actual.into()),
                expected: None,
            }
        }),
        (safe_string(), safe_msg()).prop_map(|(actual, expected)| StepKind::AssertEq {
            hash: None,
            actual: AssertTarget::Value(actual.into()),
            expected: Some(expected.into()),
        }),
        (safe_string(), safe_msg()).prop_map(|(haystack, needle)| StepKind::AssertContains {
            haystack: AssertTarget::Value(haystack.into()),
            needle: needle.into(),
        }),
        (0i32..255).prop_map(|i| StepKind::Exit(Arg::String(i.to_string(), false))),
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

fn arg_content_eq(a: &Arg, b: &Arg) -> bool {
    a.as_str() == b.as_str()
}

fn assert_target_eq(l: &AssertTarget, r: &AssertTarget, what: &str, msg: &str) {
    match (l, r) {
        (AssertTarget::Value(la), AssertTarget::Value(ra)) => {
            assert!(arg_content_eq(la, ra), "{what}: {msg}")
        }
        _ => assert_eq!(l, r, "{what}: {msg}"),
    }
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
            assert_eq!(l.as_str(), r.as_str(), "Run cmd mismatch: {}", msg)
        }
        (StepKind::RunExec { argv: l }, StepKind::RunExec { argv: r }) => {
            assert_eq!(l, r, "RunExec argv mismatch: {}", msg);
        }
        (StepKind::Workdir(l), StepKind::Workdir(r)) => {
            assert!(arg_content_eq(l, r), "Workdir mismatch: {}", msg)
        }
        (StepKind::Echo(l), StepKind::Echo(r)) => {
            assert!(arg_content_eq(l, r), "Echo mismatch: {}", msg)
        }
        (StepKind::Env { key: lk, value: lv }, StepKind::Env { key: rk, value: rv }) => {
            // Quoted-ness is parse provenance (Display normalizes quoting),
            // so compare content like every other string-carrying variant.
            assert_eq!(lk, rk, "Env key mismatch: {}", msg);
            assert!(arg_content_eq(lv, rv), "Env value mismatch: {}", msg)
        }
        (StepKind::Mkdir(l), StepKind::Mkdir(r)) => {
            assert!(arg_content_eq(l, r), "Mkdir mismatch: {}", msg)
        }
        (StepKind::Ls(l), StepKind::Ls(r)) => assert_eq!(
            l.as_ref().map(|a| a.as_str()),
            r.as_ref().map(|a| a.as_str()),
            "Ls mismatch: {}",
            msg
        ),
        (StepKind::Read(l), StepKind::Read(r)) => assert_eq!(
            l.as_ref().map(|a| a.as_str()),
            r.as_ref().map(|a| a.as_str()),
            "Read mismatch: {}",
            msg
        ),
        (
            StepKind::Write {
                path: lp,
                contents: lc,
            },
            StepKind::Write {
                path: rp,
                contents: rc,
            },
        ) => {
            assert!(arg_content_eq(lp, rp), "Write path mismatch: {}", msg);
            assert_eq!(
                lc.as_ref().map(|a| a.as_str()),
                rc.as_ref().map(|a| a.as_str()),
                "Write contents mismatch: {}",
                msg
            );
        }
        (
            StepKind::Append {
                path: lp,
                contents: lc,
            },
            StepKind::Append {
                path: rp,
                contents: rc,
            },
        ) => {
            assert!(arg_content_eq(lp, rp), "Append path mismatch: {}", msg);
            assert_eq!(
                lc.as_ref().map(|a| a.as_str()),
                rc.as_ref().map(|a| a.as_str()),
                "Append contents mismatch: {}",
                msg
            );
        }
        (
            StepKind::Copy {
                from: lf, to: lt, ..
            },
            StepKind::Copy {
                from: rf, to: rt, ..
            },
        ) => {
            assert!(arg_content_eq(lf, rf), "Copy from mismatch: {}", msg);
            assert!(arg_content_eq(lt, rt), "Copy to mismatch: {}", msg);
        }
        (
            StepKind::CopyGit {
                rev: lr,
                from: lf,
                to: lt,
                ..
            },
            StepKind::CopyGit {
                rev: rr,
                from: rf,
                to: rt,
                ..
            },
        ) => {
            assert!(arg_content_eq(lr, rr), "CopyGit rev mismatch: {}", msg);
            assert!(arg_content_eq(lf, rf), "CopyGit from mismatch: {}", msg);
            assert!(arg_content_eq(lt, rt), "CopyGit to mismatch: {}", msg);
        }
        (StepKind::Symlink { from: lf, to: lt }, StepKind::Symlink { from: rf, to: rt }) => {
            assert!(arg_content_eq(lf, rf), "Symlink from mismatch: {}", msg);
            assert!(arg_content_eq(lt, rt), "Symlink to mismatch: {}", msg);
        }
        (
            StepKind::AssertEq {
                hash: lh,
                actual: la,
                expected: le,
            },
            StepKind::AssertEq {
                hash: rh,
                actual: ra,
                expected: re,
            },
        ) => {
            assert_eq!(lh, rh, "AssertEq hash mismatch: {}", msg);
            assert_target_eq(la, ra, "AssertEq actual mismatch", msg);
            assert_eq!(
                le.as_ref().map(|a| a.as_str()),
                re.as_ref().map(|a| a.as_str()),
                "AssertEq expected mismatch: {}",
                msg
            );
        }
        (
            StepKind::AssertContains {
                haystack: lh,
                needle: ln,
            },
            StepKind::AssertContains {
                haystack: rh,
                needle: rn,
            },
        ) => {
            assert_target_eq(lh, rh, "AssertContains haystack mismatch", msg);
            assert!(
                arg_content_eq(ln, rn),
                "AssertContains needle mismatch: {}",
                msg
            );
        }
        (StepKind::HashSha256 { path: lp }, StepKind::HashSha256 { path: rp }) => {
            assert!(arg_content_eq(lp, rp), "HashSha256 mismatch: {}", msg)
        }
        (StepKind::ReadLine { var: lv }, StepKind::ReadLine { var: rv }) => {
            assert_eq!(lv, rv, "ReadLine mismatch: {}", msg)
        }
        (
            StepKind::Expand {
                path: lp,
                overrides: lo,
            },
            StepKind::Expand {
                path: rp,
                overrides: ro,
            },
        ) => {
            assert_eq!(
                lp.as_ref().map(|a| a.as_str()),
                rp.as_ref().map(|a| a.as_str()),
                "Expand path mismatch: {}",
                msg
            );
            assert_eq!(
                lo.len(),
                ro.len(),
                "Expand overrides length mismatch: {}",
                msg
            );
            for ((lk, lv), (rk, rv)) in lo.iter().zip(ro.iter()) {
                assert_eq!(lk, rk, "Expand override key mismatch: {}", msg);
                assert!(
                    arg_content_eq(lv, rv),
                    "Expand override value mismatch: {}",
                    msg
                );
            }
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
        let parsed_steps = oxdock_core::parse_script(&s).expect("failed to parse generated string");
        assert_eq!(parsed_steps.len(), 1);
        let mut parsed_step = parsed_steps[0].clone();
        parsed_step.scope_enter = 0;
        parsed_step.scope_exit = 0;

        assert_steps_eq(&parsed_step, &step, &format!("String parse mismatch: {}", s));

        // 2. Parse tokens (if feature enabled)
        let ts: proc_macro2::TokenStream = s.parse().expect("failed to tokenize string");
        let token_steps = parse_braced_tokens(&ts, oxdock_core::lower_command).expect("failed to parse tokens");

        assert_eq!(token_steps.len(), 1);
        let mut token_step = token_steps[0].clone();
        token_step.scope_enter = 0;
        token_step.scope_exit = 0;

        assert_steps_eq(&token_step, &step, &format!("Token parse mismatch: {}", s));
    }
}

/// Regression test for the CI failure with minimal input
/// `Write { path: "a", contents: "IF" }`: bare statement keywords as arg
/// values must render quoted so both parse pathways round-trip them as
/// values instead of splitting them into new statements. Iterates the
/// canonical [`STRUCTURAL_KEYWORDS`] registry plus every registered command
/// name so coverage cannot rot when keywords are added.
#[test]
#[cfg_attr(
    miri,
    ignore = "requires real TokenStream/proc-macro parsing to validate API parity"
)]
fn keyword_args_round_trip_through_both_pathways() {
    let mut keywords: Vec<&str> = STRUCTURAL_KEYWORDS.to_vec();
    keywords.extend(oxdock_parser::all_metadata().iter().map(|m| m.name));
    keywords.sort_unstable();
    keywords.dedup();
    for kw in keywords {
        let cases = [
            StepKind::Write {
                path: "a".into(),
                contents: Some(kw.into()),
            },
            StepKind::Write {
                path: kw.into(),
                contents: Some("v".into()),
            },
            StepKind::Echo(kw.into()),
        ];
        for kind in cases {
            let step = Step {
                guard: None,
                kind,
                scope_enter: 0,
                scope_exit: 0,
            };
            let s = step.to_string();

            let parsed_steps = oxdock_core::parse_script(&s).expect("string parse must round-trip");
            assert_eq!(parsed_steps.len(), 1, "string split lines: {s}");
            assert_steps_eq(&parsed_steps[0], &step, &format!("String mismatch: {s}"));

            let ts: proc_macro2::TokenStream = s.parse().expect("rendered step must tokenize");
            let token_steps = parse_braced_tokens(&ts, oxdock_core::lower_command)
                .expect("token parse must round-trip");
            assert_eq!(token_steps.len(), 1, "token walk split lines: {s}");
            assert_steps_eq(&token_steps[0], &step, &format!("Token mismatch: {s}"));
        }
    }
}
