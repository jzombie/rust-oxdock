use oxdock_parser::ast::*;
use oxdock_parser::parse_braced_tokens;
use proptest::prelude::*;
use proptest::strategy::BoxedStrategy;
use std::str::FromStr;

// Strategies

fn arb_guard() -> impl Strategy<Value = Guard> {
    prop_oneof![
        ("macos|linux|windows").prop_map(|val| Guard::Attr {
            ns: Ns::Os,
            key: None,
            val: Some(val),
        }),
        prop::sample::select(oxdock_parser::ARCH_VALUES.to_vec()).prop_map(|val| Guard::Attr {
            ns: Ns::Arch,
            key: None,
            val: Some(val.to_string()),
        }),
        "[a-zA-Z_][a-zA-Z0-9_]*".prop_map(|key| Guard::Attr {
            ns: Ns::Env,
            key: Some(key),
            val: None,
        }),
        ("[a-zA-Z_][a-zA-Z0-9_]*", "[a-zA-Z_][a-zA-Z0-9_]*",).prop_map(|(key, value)| {
            Guard::Attr {
                ns: Ns::Env,
                key: Some(key),
                val: Some(value),
            }
        }),
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

/// Scalar literal expressions: non-negative ints, bools, finite
/// fraction-carrying floats, and safe strings. Negatives would lower
/// through unary negation instead of folding to a literal; integral
/// floats reparse as INT. Both would diverge from the generated AST.
fn arb_scalar_literal() -> impl Strategy<Value = Expr> {
    prop_oneof![
        (0u32..999_999).prop_map(|i| Expr::Literal(Value::int(i as i64))),
        any::<bool>().prop_map(|b| Expr::Literal(Value::bool(b))),
        (0u32..999_999, 1u32..999).prop_map(|(i, f)| Expr::Literal(Value::float(
            format!("{i}.{f}").parse::<f64>().unwrap_or(f64::NAN)
        ))),
        safe_string().prop_map(|s| Expr::Literal(Value::string(s))),
    ]
}

/// Leaf expressions plus one-level lists: everything here Displays
/// and reparses without registry lookups or scope state.
/// Deliberately ungenerated: `Arithmetic`/`Compare`/`Logical`/`Not`
/// (lowering folds constants and compiles dynamic subtrees, so AST
/// parity is undefined there by design — value behavior is covered by
/// the arithmetic suites), `Call` (needs a module table the token path
/// cannot carry; covered through the real macro in the corpus),
/// `Block` (renders `{{ }}` template shape, not reparseable as a
/// block), `FreshPipe`/`UnsignedIntBoundary` (lowering intermediates,
/// except bare `PIPE` declarations, which round-trip as `FreshPipe`
/// and are generated with `Assign` below).
fn arb_leaf_expr() -> impl Strategy<Value = Expr> {
    prop_oneof![
        arb_scalar_literal(),
        "[a-z][a-zA-Z0-9_]*".prop_map(Expr::Var),
        "[A-Z][A-Z0-9_]*".prop_map(Expr::Env),
        "[a-z][a-zA-Z0-9_]*".prop_map(Expr::Inspect),
        (
            "[a-z][a-zA-Z0-9_]*",
            prop::collection::vec("[a-z][a-zA-Z0-9_]*", 1..=2),
        )
            .prop_map(|(base, keys)| Expr::KeyPath { base, keys }),
        prop::collection::vec(safe_string(), 0..=3).prop_map(|items| {
            Expr::List(
                items
                    .into_iter()
                    .map(Value::string)
                    .map(Expr::Literal)
                    .collect(),
            )
        }),
        prop::collection::vec(("[a-z][a-zA-Z0-9_]*", safe_string()), 0..=2).prop_map(|entries| {
            Expr::Map(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, Expr::Literal(Value::string(v))))
                    .collect(),
            )
        },),
    ]
}

/// Message alphabet that exercises quoting and `"""` blocks: embedded
/// newlines render as blocks, quotes escape inline, and leading spaces
/// probe dedent baselines. Always quoted-or-block on Display (never a
/// bare word), so spacing survives both pathways. Comment markers,
/// templates, and digit leads stay excluded: unrelated edges with
/// their own coverage.
fn rich_line() -> impl Strategy<Value = String> {
    // No double quotes: no Rust token stream can carry a raw `"` (the
    // lexer ends the literal there), so `"`-holding blocks are
    // string-pathway-only by construction. `\"`-in-block stays covered
    // by parser unit tests at the verbatim level instead.
    prop::collection::vec(
        prop_oneof![
            "[a-zA-Z0-9]",
            Just(" ".to_string()),
            Just("'".to_string()),
            Just(".".to_string()),
            Just(",".to_string()),
            Just("-".to_string()),
            Just("_".to_string()),
            Just("/".to_string()),
            Just(":".to_string()),
            Just("!".to_string()),
            Just("?".to_string()),
        ],
        1..=12,
    )
    .prop_map(|cs| cs.concat())
    .prop_filter("No comment markers", |s| {
        !s.contains("//") && !s.contains("/*")
    })
    .prop_filter("No templates", |s| !s.contains("{{") && !s.contains("}}"))
    .prop_filter("No digit lead", |s| {
        !s.starts_with(|c: char| c.is_ascii_digit())
    })
    .prop_filter("No invalid numeric prefixes", |s| {
        !has_invalid_prefixed_literal(s)
    })
}

fn rich_msg() -> impl Strategy<Value = String> {
    prop::collection::vec(rich_line(), 1..=3)
        .prop_map(|lines| lines.join("\n"))
        .prop_filter("No digit lead", |s| {
            !s.starts_with(|c: char| c.is_ascii_digit())
        })
        // Bare `true`/`false` reparse as bools in ASSERT position via
        // `lower_assert_operand`, diverging from the generated string.
        // Every other bare-safe shape round-trips verbatim.
        .prop_filter("No bool literals", |s| s != "true" && s != "false")
}

/// Either plain or rich message text: keeps the established population
/// while routing half the samples through quoting and `"""` blocks.
fn any_msg() -> impl Strategy<Value = String> {
    prop_oneof![safe_msg(), rich_msg()]
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
            Just(WorkspaceTarget::Local),
            Just(WorkspaceTarget::Cache { local: false }),
            Just(WorkspaceTarget::Cache { local: true }),
            Just(WorkspaceTarget::System)
        ]
        .prop_map(StepKind::Workspace),
        (safe_string(), any_msg(),).prop_map(|(key, value)| StepKind::Env {
            key,
            value: value.into()
        }),
        safe_msg().prop_map(|s| StepKind::Run(s.into())),
        prop::collection::vec(safe_string(), 1..3).prop_map(|items| StepKind::RunExec {
            argv: items
                .into_iter()
                .map(|s| Arg::Expr(Expr::Literal(Value::string(s))))
                .collect(),
        }),
        any_msg().prop_map(|s| StepKind::Echo(s.into())),
        (safe_string(), safe_string()).prop_map(|(from, to)| StepKind::Copy {
            from_workspace: None,
            from_host: false,
            to_host: false,
            from: from.into(),
            to: to.into()
        }),
        (safe_string(), safe_string()).prop_map(|(from, to)| StepKind::Symlink {
            from_workspace: None,
            from: from.into(),
            to: to.into()
        }),
        safe_string().prop_map(|s| StepKind::Mkdir(s.into())),
        (safe_string(), safe_string()).prop_map(|(from, to)| StepKind::Install {
            from: from.into(),
            to: to.into()
        }),
        (safe_string(), safe_string()).prop_map(|(from, to)| StepKind::Archive {
            src: from.into(),
            dst: to.into(),
            raw: false,
        }),
        (safe_string(), safe_string()).prop_map(|(from, to)| StepKind::Archive {
            src: from.into(),
            dst: to.into(),
            raw: true,
        }),
        (safe_string(), safe_string()).prop_map(|(from, to)| StepKind::Unarchive {
            src: from.into(),
            dst: to.into(),
            max_bytes: None,
        }),
        (
            safe_string(),
            safe_string(),
            // Static flag values reparse through lower-time validation,
            // so the generator only emits spellings that validate (the
            // same reason other flagged variants generate defaults).
            prop::option::of(prop_oneof![
                Just("1".to_string()),
                Just("500B".to_string()),
                Just("100KB".to_string()),
                Just("2MIB".to_string()),
                Just("1GB".to_string()),
                Just("1GIB".to_string()),
                Just("1_000_000".to_string()),
            ]),
        )
            .prop_map(|(from, to, cap)| StepKind::Unarchive {
                src: from.into(),
                dst: to.into(),
                max_bytes: cap.map(Into::into),
            }),
        prop::option::of(safe_string()).prop_map(|s| StepKind::Ls(s.map(Into::into))),
        Just(StepKind::Cwd),
        prop::option::of(safe_string()).prop_map(|s| StepKind::Read(s.map(Into::into))),
        "[a-zA-Z_][a-zA-Z0-9_]*".prop_map(|v| StepKind::ReadLine { var: v }),
        (
            "[a-zA-Z_][a-zA-Z0-9_]*",
            any_msg(),
            prop::option::of(any_msg()),
        )
            .prop_map(|(var, message, default)| StepKind::Prompt {
                var,
                message: message.into(),
                default: default.map(Into::into),
            }),
        safe_string().prop_map(|path| StepKind::Remove {
            path: path.into(),
            recursive: false,
        }),
        (safe_string(), any_msg()).prop_map(|(path, contents)| StepKind::Write {
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
        (any_msg(), any_msg()).prop_map(|(actual, expected)| StepKind::AssertEq {
            hash: None,
            actual: AssertTarget::Value(actual.into()),
            expected: Some(expected.into()),
        }),
        (safe_string(), any_msg()).prop_map(|(haystack, needle)| StepKind::AssertContains {
            haystack: AssertTarget::Value(haystack.into()),
            needle: needle.into(),
        }),
        (0i32..255).prop_map(|i| StepKind::Exit(Arg::String(i.to_string(), false))),
        (
            "[A-Z][A-Z0-9_]*",
            prop_oneof![
                Just("MAP".to_string()),
                Just("LIST<MAP>".to_string()),
                Just("MAP<name: STRING>".to_string()),
                Just("LIST<PERSON>".to_string()),
            ]
        )
            .prop_map(|(name, target)| StepKind::TypeAlias { name, target }),
        // Assignment pairs decl tags with matching literal shapes. The
        // parser does not enforce the pairing, but keeping it preserves
        // the option of tightening validation without churning the fuzz.
        prop_oneof![
            ("[a-z][a-zA-Z0-9_]*", safe_string()).prop_map(|(var, s)| {
                StepKind::Assign {
                    var,
                    decl_type: "STRING".to_string(),
                    expr: Expr::Literal(Value::string(s)),
                }
            }),
            ("[a-z][a-zA-Z0-9_]*", 0u32..999_999).prop_map(|(var, i)| {
                StepKind::Assign {
                    var,
                    decl_type: "INT".to_string(),
                    expr: Expr::Literal(Value::int(i as i64)),
                }
            }),
            ("[a-z][a-zA-Z0-9_]*", any::<bool>()).prop_map(|(var, b)| {
                StepKind::Assign {
                    var,
                    decl_type: "BOOL".to_string(),
                    expr: Expr::Literal(Value::bool(b)),
                }
            }),
            (
                "[a-z][a-zA-Z0-9_]*",
                prop::collection::vec(safe_string(), 0..=2)
            )
                .prop_map(|(var, items)| StepKind::Assign {
                    var,
                    decl_type: "LIST<STRING>".to_string(),
                    expr: Expr::List(
                        items
                            .into_iter()
                            .map(Value::string)
                            .map(Expr::Literal)
                            .collect(),
                    ),
                },),
            "[a-z][a-zA-Z0-9_]*".prop_map(|var| StepKind::Assign {
                var,
                decl_type: "PIPE".to_string(),
                expr: Expr::FreshPipe,
            }),
        ],
        ("[a-z][a-zA-Z0-9_]*", arb_leaf_expr()).prop_map(|(var, expr)| StepKind::Set { var, expr }),
        arb_leaf_expr().prop_map(|expr| StepKind::Return {
            expr: Box::new(expr)
        }),
        Just(StepKind::Break),
        Just(StepKind::Continue),
        prop_oneof![
            Just("10s".to_string()),
            Just("500ms".to_string()),
            Just("2m".to_string()),
        ]
        .prop_map(|d| StepKind::Sleep {
            duration: Arg::String(d, false),
        }),
        ("[a-z][a-zA-Z0-9_]*", "[a-zA-Z_][a-zA-Z0-9_]*").prop_map(|(list, item)| {
            StepKind::ListAppend {
                list,
                item: Arg::String(item, false),
            }
        }),
        "[a-z][a-zA-Z0-9_]*".prop_map(|var| StepKind::Await { var: var.clone() }),
        "[a-z][a-zA-Z0-9_]*".prop_map(|var| StepKind::Cancel { var: var.clone() }),
        (safe_string(), any_msg()).prop_map(|(path, contents)| StepKind::Append {
            path: path.into(),
            contents: Some(contents.into()),
        }),
        (
            prop::option::of(safe_string()),
            prop::collection::vec((safe_string(), any_msg()), 0..=2),
        )
            .prop_map(|(path, overrides)| StepKind::Expand {
                path: path.map(Into::into),
                overrides: overrides.into_iter().map(|(k, v)| (k, v.into())).collect(),
            }),
        // Bodies nest flat kinds exactly one level deep; scope markers
        // normalize recursively in comparison.
        (any::<bool>(), arb_body(), arb_body()).prop_map(|(flag, then_body, else_body)| {
            StepKind::If {
                cond: Box::new(Expr::Literal(Value::bool(flag))),
                then_body,
                else_ifs: vec![],
                else_body: Some(else_body),
            }
        }),
        (any::<bool>(), arb_body()).prop_map(|(flag, body)| StepKind::While {
            cond: Box::new(Expr::Literal(Value::bool(flag))),
            body,
        }),
        ("FUZZ[A-Z0-9_]*", arb_body()).prop_map(|(name, body)| StepKind::FuncDef {
            name,
            params: vec![("coverage".to_string(), "STRING".to_string())],
            body,
        }),
        arb_command_body().prop_map(|body| StepKind::Timeout {
            duration: Arg::String("10s".to_string(), false),
            body,
        }),
        arb_command_body().prop_map(|body| StepKind::Isolated { body }),
        arb_body().prop_map(|body| StepKind::AsyncBlock { body }),
        ("[a-z][a-zA-Z0-9_]*", arb_body()).prop_map(|(var, body)| StepKind::AssignAsync {
            var,
            decl_type: "HANDLE".to_string(),
            body,
        }),
        arb_body().prop_map(|body| StepKind::RemoteBlock {
            target: "coveragetarget".to_string(),
            vars: vec![],
            env: vec![],
            body,
        }),
        ("[a-z][a-zA-Z0-9_]*",).prop_map(|(var,)| StepKind::AssignCapture {
            var,
            decl_type: "STRING".to_string(),
            cmd: Box::new(StepKind::Echo("coverage echo".into())),
        }),
        ("[a-z][a-zA-Z0-9_]*",).prop_map(|(var,)| StepKind::AwaitCapture {
            out_var: var.clone(),
            out_type: "STRING".to_string(),
            task_var: var,
        }),
        ("[a-z][a-zA-Z0-9_]*", "[a-z][a-zA-Z0-9_]*").prop_map(|(var, item)| {
            StepKind::For {
                key_var: None,
                key_type: None,
                var,
                var_type: "STRING".to_string(),
                in_expr: Expr::List(vec![Expr::Literal(Value::string(item))]),
                body: vec![],
            }
        }),
        Just(StepKind::WithIo {
            bindings: vec![IoBinding {
                stream: IoStream::Stdout,
                pipe: Some(PipeTarget::Var("coverage".to_string())),
            }],
            cmd: Box::new(StepKind::Echo("coverage echo".into())),
        }),
    ]
}

/// Zero scope markers recursively: generated steps carry 0/0 while
/// parsed bodies earn real markers from structure. Both pathways parse
/// the same text, so markers agree after normalization.
fn zero_markers(steps: &mut [Step]) {
    for step in steps.iter_mut() {
        step.scope_enter = 0;
        step.scope_exit = 0;
        zero_kind_markers(&mut step.kind);
    }
}

fn zero_kind_markers(kind: &mut StepKind) {
    match kind {
        StepKind::If {
            then_body,
            else_ifs,
            else_body,
            ..
        } => {
            zero_markers(then_body);
            for (_, body) in else_ifs.iter_mut() {
                zero_markers(body);
            }
            if let Some(body) = else_body {
                zero_markers(body);
            }
        }
        StepKind::For { body, .. }
        | StepKind::While { body, .. }
        | StepKind::FuncDef { body, .. }
        | StepKind::Timeout { body, .. }
        | StepKind::Isolated { body, .. }
        | StepKind::AsyncBlock { body, .. }
        | StepKind::AssignAsync { body, .. }
        | StepKind::RemoteBlock { body, .. } => zero_markers(body),
        StepKind::AssignCapture { cmd, .. } | StepKind::WithIo { cmd, .. } => {
            zero_kind_markers(cmd)
        }
        _ => {}
    }
}

/// Command-subset bodies for single-form wrappers (`TIMEOUT`,
/// `ISOLATED`): one-step bodies render as `WRAPPER <command>`, where the
/// grammar takes commands but never `LET`, so bodies draw from the
/// command subset. Multi-step bodies render block form either way.
fn arb_command_body() -> impl Strategy<Value = Vec<Step>> {
    prop::collection::vec(
        (prop::option::of(arb_guard_expr()), arb_command_body_kind()).prop_map(|(guard, kind)| {
            Step {
                guard,
                kind,
                scope_enter: 0,
                scope_exit: 0,
            }
        }),
        1..=2,
    )
}

fn arb_command_body_kind() -> impl Strategy<Value = StepKind> {
    prop_oneof![
        Just(StepKind::Echo("coverage echo".into())),
        Just(StepKind::Write {
            path: "coverage.txt".into(),
            contents: Some("covered".into()),
        }),
        Just(StepKind::Env {
            key: "COVERAGE".to_string(),
            value: "covered".into(),
        }),
        Just(StepKind::AssertEq {
            hash: None,
            actual: AssertTarget::Value("coverage-actual".into()),
            expected: Some("coverage-expected".into()),
        }),
    ]
}
fn arb_body() -> impl Strategy<Value = Vec<Step>> {
    prop::collection::vec(
        (prop::option::of(arb_guard_expr()), arb_body_step_kind()).prop_map(|(guard, kind)| Step {
            guard,
            kind,
            scope_enter: 0,
            scope_exit: 0,
        }),
        1..=2,
    )
}

/// Body-safe flat kinds: prelude directives (`INHERIT_ENV`, `TYPE`)
/// are top-level-only and fail inside blocks, so bodies draw from the
/// executable subset.
fn arb_body_step_kind() -> impl Strategy<Value = StepKind> {
    prop_oneof![
        Just(StepKind::Echo("coverage echo".into())),
        Just(StepKind::Write {
            path: "coverage.txt".into(),
            contents: Some("covered".into()),
        }),
        Just(StepKind::Env {
            key: "COVERAGE".to_string(),
            value: "covered".into(),
        }),
        Just(StepKind::Assign {
            var: "coverage".to_string(),
            decl_type: "STRING".to_string(),
            expr: Expr::Literal(Value::string("covered".to_string())),
        }),
        Just(StepKind::AssertEq {
            hash: None,
            actual: AssertTarget::Value("coverage-actual".into()),
            expected: Some("coverage-expected".into()),
        }),
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
        .prop_filter("Reject guarded TYPE aliases", |step| match &step.kind {
            StepKind::TypeAlias { .. } => step.guard.is_none(),
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
        (StepKind::Install { from: lf, to: lt }, StepKind::Install { from: rf, to: rt }) => {
            assert!(arg_content_eq(lf, rf), "Install from mismatch: {}", msg);
            assert!(arg_content_eq(lt, rt), "Install to mismatch: {}", msg);
        }
        (
            StepKind::Archive {
                src: ls,
                dst: ld,
                raw: lr,
            },
            StepKind::Archive {
                src: rs,
                dst: rd,
                raw: rr,
            },
        ) => {
            assert!(arg_content_eq(ls, rs), "Archive src mismatch: {}", msg);
            assert!(arg_content_eq(ld, rd), "Archive dst mismatch: {}", msg);
            assert_eq!(lr, rr, "Archive raw mismatch: {}", msg);
        }
        (
            StepKind::Unarchive {
                src: ls,
                dst: ld,
                max_bytes: lm,
            },
            StepKind::Unarchive {
                src: rs,
                dst: rd,
                max_bytes: rm,
            },
        ) => {
            assert!(arg_content_eq(ls, rs), "Unarchive src mismatch: {}", msg);
            assert!(arg_content_eq(ld, rd), "Unarchive dst mismatch: {}", msg);
            assert_eq!(
                lm.as_ref().map(|a| a.as_str()),
                rm.as_ref().map(|a| a.as_str()),
                "Unarchive max_bytes mismatch: {}",
                msg
            );
        }
        (
            StepKind::Remove {
                path: lp,
                recursive: lr,
            },
            StepKind::Remove {
                path: rp,
                recursive: rr,
            },
        ) => {
            assert!(arg_content_eq(lp, rp), "Remove path mismatch: {}", msg);
            assert_eq!(lr, rr, "Remove recursive mismatch: {}", msg);
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
        (
            StepKind::Symlink {
                from: lf, to: lt, ..
            },
            StepKind::Symlink {
                from: rf, to: rt, ..
            },
        ) => {
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
            StepKind::Prompt {
                var: lv,
                message: lm,
                default: ld,
            },
            StepKind::Prompt {
                var: rv,
                message: rm,
                default: rd,
            },
        ) => {
            assert_eq!(lv, rv, "Prompt var mismatch: {}", msg);
            assert!(arg_content_eq(lm, rm), "Prompt message mismatch: {}", msg);
            assert_eq!(
                ld.as_ref().map(|a| a.as_str()),
                rd.as_ref().map(|a| a.as_str()),
                "Prompt default mismatch: {}",
                msg
            );
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
        (
            StepKind::Assign {
                var: lv,
                decl_type: lt,
                expr: le,
            },
            StepKind::Assign {
                var: rv,
                decl_type: rt,
                expr: re,
            },
        ) => {
            assert_eq!(lv, rv, "Assign var mismatch: {}", msg);
            assert_eq!(lt, rt, "Assign type mismatch: {}", msg);
            assert_eq!(le, re, "Assign expr mismatch: {}", msg);
        }
        (StepKind::Set { var: lv, expr: le }, StepKind::Set { var: rv, expr: re }) => {
            assert_eq!(lv, rv, "Set var mismatch: {}", msg);
            assert_eq!(le, re, "Set expr mismatch: {}", msg);
        }
        (StepKind::Return { expr: le }, StepKind::Return { expr: re }) => {
            assert_eq!(le, re, "Return expr mismatch: {}", msg);
        }
        (StepKind::Break, StepKind::Break) => {}
        (StepKind::Continue, StepKind::Continue) => {}
        (StepKind::Sleep { duration: l }, StepKind::Sleep { duration: r }) => {
            assert!(arg_content_eq(l, r), "Sleep duration mismatch: {}", msg)
        }
        (
            StepKind::ListAppend { list: ll, item: li },
            StepKind::ListAppend { list: rl, item: ri },
        ) => {
            assert_eq!(ll, rl, "ListAppend list mismatch: {}", msg);
            assert!(arg_content_eq(li, ri), "ListAppend item mismatch: {}", msg);
        }
        (StepKind::Await { var: l }, StepKind::Await { var: r }) => {
            assert_eq!(l, r, "Await var mismatch: {}", msg)
        }
        (StepKind::Cancel { var: l }, StepKind::Cancel { var: r }) => {
            assert_eq!(l, r, "Cancel var mismatch: {}", msg)
        }
        (
            StepKind::If {
                cond: lc,
                then_body: lt,
                else_ifs: le,
                else_body: lb,
            },
            StepKind::If {
                cond: rc,
                then_body: rt,
                else_ifs: re,
                else_body: rb,
            },
        ) => {
            assert_eq!(lc, rc, "If cond mismatch: {}", msg);
            assert_bodies_eq(lt, rt, msg);
            assert_eq!(le.len(), re.len(), "If else-if mismatch: {}", msg);
            for ((c1, b1), (c2, b2)) in le.iter().zip(re.iter()) {
                assert_eq!(c1, c2, "If else-if cond mismatch: {}", msg);
                assert_bodies_eq(b1, b2, msg);
            }
            match (lb, rb) {
                (None, None) => {}
                (Some(x), Some(y)) => assert_bodies_eq(x, y, msg),
                _ => panic!("If else-body mismatch: {}", msg),
            }
        }
        (StepKind::While { cond: lc, body: lb }, StepKind::While { cond: rc, body: rb }) => {
            assert_eq!(lc, rc, "While cond mismatch: {}", msg);
            assert_bodies_eq(lb, rb, msg);
        }
        (
            StepKind::FuncDef {
                name: ln,
                params: lp,
                body: lb,
            },
            StepKind::FuncDef {
                name: rn,
                params: rp,
                body: rb,
            },
        ) => {
            assert_eq!(ln, rn, "FuncDef name mismatch: {}", msg);
            assert_eq!(lp, rp, "FuncDef params mismatch: {}", msg);
            assert_bodies_eq(lb, rb, msg);
        }
        (
            StepKind::Timeout {
                duration: ld,
                body: lb,
            },
            StepKind::Timeout {
                duration: rd,
                body: rb,
            },
        ) => {
            assert!(arg_content_eq(ld, rd), "Timeout duration mismatch: {}", msg);
            assert_bodies_eq(lb, rb, msg);
        }
        (StepKind::Isolated { body: lb }, StepKind::Isolated { body: rb }) => {
            assert_bodies_eq(lb, rb, msg)
        }
        (StepKind::AsyncBlock { body: lb }, StepKind::AsyncBlock { body: rb }) => {
            assert_bodies_eq(lb, rb, msg)
        }
        (
            StepKind::AssignAsync {
                var: lv,
                decl_type: lt,
                body: lb,
            },
            StepKind::AssignAsync {
                var: rv,
                decl_type: rt,
                body: rb,
            },
        ) => {
            assert_eq!(lv, rv, "AssignAsync var mismatch: {}", msg);
            assert_eq!(lt, rt, "AssignAsync type mismatch: {}", msg);
            assert_bodies_eq(lb, rb, msg);
        }
        (
            StepKind::RemoteBlock {
                target: lt,
                vars: lv,
                env: le,
                body: lb,
            },
            StepKind::RemoteBlock {
                target: rt,
                vars: rv,
                env: re,
                body: rb,
            },
        ) => {
            assert_eq!(lt, rt, "RemoteBlock target mismatch: {}", msg);
            assert_eq!(lv, rv, "RemoteBlock vars mismatch: {}", msg);
            assert_eq!(le, re, "RemoteBlock env mismatch: {}", msg);
            assert_bodies_eq(lb, rb, msg);
        }
        (
            StepKind::AssignCapture {
                var: lv,
                decl_type: lt,
                cmd: lc,
            },
            StepKind::AssignCapture {
                var: rv,
                decl_type: rt,
                cmd: rc,
            },
        ) => {
            assert_eq!(lv, rv, "AssignCapture var mismatch: {}", msg);
            assert_eq!(lt, rt, "AssignCapture type mismatch: {}", msg);
            assert_bodies_eq(&[shell_of_kind(lc)], &[shell_of_kind(rc)], msg);
        }
        (
            StepKind::AwaitCapture {
                out_var: lo,
                out_type: lt,
                task_var: lv,
            },
            StepKind::AwaitCapture {
                out_var: ro,
                out_type: rt,
                task_var: rv,
            },
        ) => {
            assert_eq!(lo, ro, "AwaitCapture out mismatch: {}", msg);
            assert_eq!(lt, rt, "AwaitCapture type mismatch: {}", msg);
            assert_eq!(lv, rv, "AwaitCapture task mismatch: {}", msg);
        }
        (
            StepKind::WithIo {
                bindings: lb,
                cmd: lc,
            },
            StepKind::WithIo {
                bindings: rb,
                cmd: rc,
            },
        ) => {
            assert_eq!(lb, rb, "WithIo bindings mismatch: {}", msg);
            assert_bodies_eq(&[shell_of_kind(lc)], &[shell_of_kind(rc)], msg);
        }
        (
            StepKind::For {
                key_var: lk,
                key_type: lkt,
                var: lv,
                var_type: lvt,
                in_expr: li,
                body: lb,
            },
            StepKind::For {
                key_var: rk,
                key_type: rkt,
                var: rv,
                var_type: rvt,
                in_expr: ri,
                body: rb,
            },
        ) => {
            assert_eq!(lk, rk, "For key var mismatch: {}", msg);
            assert_eq!(lkt, rkt, "For key type mismatch: {}", msg);
            assert_eq!(lv, rv, "For var mismatch: {}", msg);
            assert_eq!(lvt, rvt, "For var type mismatch: {}", msg);
            assert_eq!(li, ri, "For iterable mismatch: {}", msg);
            assert_bodies_eq(lb, rb, msg);
        }
        (StepKind::Call { name: ln, args: la }, StepKind::Call { name: rn, args: ra }) => {
            assert_eq!(ln, rn, "Call name mismatch: {}", msg);
            assert_eq!(la, ra, "Call args mismatch: {}", msg);
        }
        _ => assert_eq!(left.kind, right.kind, "Kind mismatch: {}", msg),
    }
}

/// Compare two bodies after normalizing scope markers: parsed bodies
/// earn real markers from structure while generated ones carry 0/0.
fn assert_bodies_eq(l: &[Step], r: &[Step], msg: &str) {
    let mut l = l.to_vec();
    let mut r = r.to_vec();
    zero_markers(&mut l);
    zero_markers(&mut r);
    assert_eq!(l.len(), r.len(), "Body length mismatch: {}", msg);
    for (a, b) in l.iter().zip(r.iter()) {
        assert_steps_eq(a, b, msg);
    }
}

/// Wrap a boxed command kind in a marker-free shell step for body
/// comparison.
fn shell_of_kind(kind: &StepKind) -> Step {
    Step {
        guard: None,
        kind: kind.clone(),
        scope_enter: 0,
        scope_exit: 0,
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
        let mut parsed = parsed_steps;
        zero_markers(&mut parsed);
        let parsed_step = &parsed[0];

        assert_steps_eq(parsed_step, &step, &format!("String parse mismatch: {}", s));

        // 2. Parse tokens (if feature enabled)
        let ts: proc_macro2::TokenStream = s.parse().expect("failed to tokenize string");
        let token_steps = parse_braced_tokens(&ts, oxdock_core::lower_command).expect("failed to parse tokens");

        assert_eq!(token_steps.len(), 1);
        let mut tokenized = token_steps;
        zero_markers(&mut tokenized);
        let token_step = &tokenized[0];

        assert_steps_eq(token_step, &step, &format!("Token parse mismatch: {}", s));
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

/// One canonical instance per generated variant. The fuzzer proves the
/// alphabet (quoting, blocks, escapes) across these shapes; the samples
/// below prove every generated shape survives both pathways at least
/// once, deterministically.
fn sample_covered_kinds() -> Vec<StepKind> {
    vec![
        StepKind::InheritEnv {
            keys: vec!["COVERAGE".to_string()],
        },
        StepKind::Workdir("coverage".into()),
        StepKind::Workspace(WorkspaceTarget::Local),
        StepKind::Env {
            key: "COVERAGE".to_string(),
            value: "covered".into(),
        },
        StepKind::Run("coveragerun".into()),
        StepKind::RunExec {
            argv: vec![Arg::Expr(Expr::Literal(Value::string(
                "coverage".to_string(),
            )))],
        },
        StepKind::Echo("coverage echo".into()),
        StepKind::Copy {
            from_workspace: None,
            from_host: false,
            to_host: false,
            from: "coverage-from".into(),
            to: "coverage-to".into(),
        },
        StepKind::Symlink {
            from_workspace: None,
            from: "coverage-from".into(),
            to: "coverage-to".into(),
        },
        StepKind::Mkdir("coverage-dir".into()),
        StepKind::Install {
            from: "coverage-from".into(),
            to: "coverage-to".into(),
        },
        StepKind::Archive {
            src: "coverage-src".into(),
            dst: "coverage-dst".into(),
            raw: false,
        },
        StepKind::Unarchive {
            src: "coverage-src".into(),
            dst: "coverage-dst".into(),
            max_bytes: None,
        },
        StepKind::Ls(Some("coverage".into())),
        StepKind::Cwd,
        StepKind::Read(Some("coverage".into())),
        StepKind::ReadLine {
            var: "coverage".to_string(),
        },
        StepKind::Prompt {
            var: "coverage".to_string(),
            message: "coverage message".into(),
            default: Some("coverage-default".into()),
        },
        StepKind::Remove {
            path: "coverage".into(),
            recursive: false,
        },
        StepKind::Write {
            path: "coverage.txt".into(),
            contents: Some("covered".into()),
        },
        StepKind::CopyGit {
            rev: "coverage-rev".into(),
            from: "coverage-from".into(),
            to: "coverage-to".into(),
            include_dirty: false,
        },
        StepKind::HashSha256 {
            path: "coverage".into(),
        },
        StepKind::AssertEq {
            hash: None,
            actual: AssertTarget::Value("coverage-actual".into()),
            expected: Some("coverage-expected".into()),
        },
        StepKind::AssertContains {
            haystack: AssertTarget::Value("coverage haystack".into()),
            needle: "coverage".into(),
        },
        StepKind::Exit(Arg::String("3".to_string(), false)),
        StepKind::TypeAlias {
            name: "COVERAGE".to_string(),
            target: "MAP".to_string(),
        },
        StepKind::Assign {
            var: "coverage".to_string(),
            decl_type: "STRING".to_string(),
            expr: Expr::Literal(Value::string("covered".to_string())),
        },
        StepKind::Set {
            var: "coverage".to_string(),
            expr: Expr::Literal(Value::int(7)),
        },
        StepKind::Return {
            expr: Box::new(Expr::Literal(Value::bool(true))),
        },
        StepKind::Break,
        StepKind::Continue,
        StepKind::Sleep {
            duration: Arg::String("10s".to_string(), false),
        },
        StepKind::ListAppend {
            list: "coverage".to_string(),
            item: Arg::String("covered".to_string(), false),
        },
        StepKind::Await {
            var: "coverage".to_string(),
        },
        StepKind::Cancel {
            var: "coverage".to_string(),
        },
        StepKind::Append {
            path: "coverage.txt".into(),
            contents: Some("covered".into()),
        },
        StepKind::Expand {
            path: None,
            overrides: vec![(
                "COVERAGE".to_string(),
                Arg::String("covered".to_string(), false),
            )],
        },
        StepKind::If {
            cond: Box::new(Expr::Literal(Value::bool(true))),
            then_body: vec![Step {
                guard: None,
                kind: StepKind::Echo("coverage echo".into()),
                scope_enter: 0,
                scope_exit: 0,
            }],
            else_ifs: vec![],
            else_body: None,
        },
        StepKind::While {
            cond: Box::new(Expr::Literal(Value::bool(true))),
            body: vec![Step {
                guard: None,
                kind: StepKind::Echo("coverage echo".into()),
                scope_enter: 0,
                scope_exit: 0,
            }],
        },
        StepKind::FuncDef {
            name: "COVERAGEFUNC".to_string(),
            params: vec![("coverage".to_string(), "STRING".to_string())],
            body: vec![Step {
                guard: None,
                kind: StepKind::Echo("coverage echo".into()),
                scope_enter: 0,
                scope_exit: 0,
            }],
        },
        StepKind::Timeout {
            duration: Arg::String("10s".to_string(), false),
            body: vec![Step {
                guard: None,
                kind: StepKind::Echo("coverage echo".into()),
                scope_enter: 0,
                scope_exit: 0,
            }],
        },
        StepKind::Isolated {
            body: vec![Step {
                guard: None,
                kind: StepKind::Echo("coverage echo".into()),
                scope_enter: 0,
                scope_exit: 0,
            }],
        },
        StepKind::AsyncBlock {
            body: vec![Step {
                guard: None,
                kind: StepKind::Echo("coverage echo".into()),
                scope_enter: 0,
                scope_exit: 0,
            }],
        },
        StepKind::AssignAsync {
            var: "coverage".to_string(),
            decl_type: "HANDLE".to_string(),
            body: vec![Step {
                guard: None,
                kind: StepKind::Echo("coverage echo".into()),
                scope_enter: 0,
                scope_exit: 0,
            }],
        },
        StepKind::RemoteBlock {
            target: "coveragetarget".to_string(),
            vars: vec![],
            env: vec![],
            body: vec![Step {
                guard: None,
                kind: StepKind::Echo("coverage echo".into()),
                scope_enter: 0,
                scope_exit: 0,
            }],
        },
        StepKind::AssignCapture {
            var: "coverage".to_string(),
            decl_type: "STRING".to_string(),
            cmd: Box::new(StepKind::Echo("coverage echo".into())),
        },
        StepKind::AwaitCapture {
            out_var: "coverage".to_string(),
            out_type: "STRING".to_string(),
            task_var: "coverage".to_string(),
        },
        StepKind::For {
            key_var: None,
            key_type: None,
            var: "coverage".to_string(),
            var_type: "STRING".to_string(),
            in_expr: Expr::List(vec![Expr::Literal(Value::string("covered".to_string()))]),
            body: vec![Step {
                guard: None,
                kind: StepKind::Echo("coverage echo".into()),
                scope_enter: 0,
                scope_exit: 0,
            }],
        },
        StepKind::WithIo {
            bindings: vec![IoBinding {
                stream: IoStream::Stdout,
                pipe: Some(PipeTarget::Var("coverage".to_string())),
            }],
            cmd: Box::new(StepKind::Echo("coverage echo".into())),
        },
    ]
}

/// Fuzzer coverage inventory: every `StepKind` variant appears here
/// explicitly with no wildcard, so adding a variant fails compilation
/// until it is classified as generated (`true`) or out of scope
/// (`false`, reason beside it). String/macro parity cannot rot silently
/// as the language grows.
fn fuzz_covers_kind(kind: &StepKind) -> bool {
    match kind {
        StepKind::Workdir(..)
        | StepKind::Workspace(..)
        | StepKind::Env { .. }
        | StepKind::InheritEnv { .. }
        | StepKind::Echo(..)
        | StepKind::Run(..)
        | StepKind::RunExec { .. }
        | StepKind::Copy { .. }
        | StepKind::CopyGit { .. }
        | StepKind::Symlink { .. }
        | StepKind::Mkdir(..)
        | StepKind::Install { .. }
        | StepKind::Archive { .. }
        | StepKind::Unarchive { .. }
        | StepKind::Ls(..)
        | StepKind::Cwd
        | StepKind::Read(..)
        | StepKind::ReadLine { .. }
        | StepKind::Prompt { .. }
        | StepKind::Remove { .. }
        | StepKind::Write { .. }
        | StepKind::HashSha256 { .. }
        | StepKind::AssertEq { .. }
        | StepKind::AssertContains { .. }
        | StepKind::Exit(..)
        | StepKind::TypeAlias { .. } => true,
        // Expression bodies need Expr strategies (literals, math,
        // calls, keypaths) before generation is sound.
        StepKind::Assign { .. } => true,
        StepKind::Set { .. } => true,
        StepKind::AssignCapture { .. } => true,
        StepKind::AwaitCapture { .. } => true,
        StepKind::AsyncBlock { .. } => true,
        StepKind::AssignAsync { .. } => true,
        StepKind::Await { .. } => true,
        StepKind::Cancel { .. } => true,
        StepKind::Timeout { .. } => true,
        StepKind::Isolated { .. } => true,
        StepKind::IsolatedInheritEnv { .. } => false,
        StepKind::RemoteBlock { .. } => true,
        StepKind::FuncDef { .. } => true,
        // Token path carries no module table, so qualified calls cannot
        // resolve there; single-step text cannot carry IMPORT either.
        // Covered through the real macro in the corpus instead.
        StepKind::Call { .. } => false,
        StepKind::Return { .. } => true,
        StepKind::While { .. } => true,
        StepKind::Break => true,
        StepKind::Continue => true,
        // Bodies (step vectors, bindings) need recursive generation.
        StepKind::WithIo { .. } => true,
        StepKind::WithIoBlock { .. } => false,
        StepKind::For { .. } => true,
        StepKind::If { .. } => true,
        // Durations and list items need value-shaped strategies.
        StepKind::Sleep { .. } => true,
        StepKind::ListAppend { .. } => true,
        // Overrides render `KEY=value` lines needing assignment-aware
        // comparison, not plain content equality.
        StepKind::Append { .. } => true,
        StepKind::Expand { .. } => true,
    }
}

/// Every claimed-covered variant round-trips both pathways, and every
/// sample claims coverage: the inventory and the generator cannot drift
/// apart without failing here.
#[test]
#[cfg_attr(
    miri,
    ignore = "requires real TokenStream/proc-macro parsing to validate API parity"
)]
fn fuzz_inventory_covers_claimed_kinds() {
    let samples = sample_covered_kinds();
    assert!(
        !samples.is_empty(),
        "coverage inventory must hold at least one sample"
    );
    for step_kind in &samples {
        assert!(
            fuzz_covers_kind(step_kind),
            "sampled {step_kind:?} must claim coverage"
        );
        let step = Step {
            guard: None,
            kind: step_kind.clone(),
            scope_enter: 0,
            scope_exit: 0,
        };
        let s = step.to_string();
        let parsed_steps = oxdock_core::parse_script(&s).expect("sample must parse from string");
        assert_eq!(parsed_steps.len(), 1, "string split lines: {s}");
        let ts: proc_macro2::TokenStream = s.parse().expect("sample must tokenize");
        let token_steps = parse_braced_tokens(&ts, oxdock_core::lower_command)
            .expect("sample must parse from tokens");
        assert_eq!(token_steps.len(), 1, "token walk split lines: {s}");
        let mut parsed = parsed_steps;
        zero_markers(&mut parsed);
        let mut tokenized = token_steps;
        zero_markers(&mut tokenized);
        assert_steps_eq(&parsed[0], &step, &format!("String mismatch: {s}"));
        assert_steps_eq(&tokenized[0], &step, &format!("Token mismatch: {s}"));
    }
}
