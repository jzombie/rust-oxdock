pub mod ast;
pub mod command;
pub mod commands;
mod lexer;
#[cfg(feature = "proc-macro-api")]
mod macro_input;
pub mod markdown;
pub mod parser;
pub mod strip_flags;

pub use ast::*;
pub use command::{
    ArgSpec, CommandMeta, CommandSpec, Example, FlagSpec, FlagValueType, IoDirection, Stream,
};
pub use commands::{all_metadata, lower_command};
pub use lexer::LANGUAGE_SPEC;
#[cfg(feature = "proc-macro-api")]
pub use macro_input::{
    DslMacroInput, ScriptSource, parse_braced_tokens, script_from_braced_tokens,
};
pub use markdown::{BlockMetadata, FencedBlock, extract_fenced_blocks};
pub use parser::{parse_guard_expr_str, parse_script};
pub use strip_flags::strip_flags;

/// Shared mock lowering for parser tests.
/// Centralizes AST lowering so unit tests, integration tests, and macro_input tests
/// all exercise the same command set against the same grammar.
pub mod test_lower_mock {
    use crate::{Arg, StepKind, WorkspaceTarget};
    use anyhow::{anyhow, bail};

    pub fn lower(name: &str, args: Vec<Arg>) -> anyhow::Result<StepKind> {
        match name {
            "CWD" => Ok(StepKind::Cwd),
            "WRITE" => {
                let mut it = args.into_iter();
                let path = it.next().ok_or_else(|| anyhow!("WRITE requires path"))?;
                let remaining: Vec<_> = it.collect();
                let contents = if remaining.is_empty() {
                    None
                } else {
                    let joined = remaining
                        .iter()
                        .map(|a| a.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    Some(Arg::String(joined, false))
                };
                Ok(StepKind::Write { path, contents })
            }
            "HASH_SHA256" => {
                let mut a = args;
                if a.first().map(|a| a.as_str()) == Some("--hash") {
                    a.remove(0);
                    let hash = a
                        .first()
                        .ok_or_else(|| anyhow!("--hash requires value"))?
                        .as_str()
                        .to_string();
                    a.remove(0);
                    let path = a
                        .first()
                        .ok_or_else(|| anyhow!("HASH_SHA256 requires path"))?
                        .clone();
                    Ok(StepKind::AssertFile {
                        hash: Some(hash),
                        path,
                        contents: None,
                    })
                } else {
                    let path = a
                        .first()
                        .ok_or_else(|| anyhow!("HASH_SHA256 requires path"))?
                        .clone();
                    let contents = a.get(1).cloned();
                    Ok(StepKind::AssertFile {
                        hash: None,
                        path,
                        contents,
                    })
                }
            }
            "ENV" => {
                let arg = args
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow!("ENV requires key=val"))?;
                let (k, v) = arg
                    .as_str()
                    .split_once('=')
                    .ok_or_else(|| anyhow!("ENV requires key=val"))?;
                Ok(StepKind::Env {
                    key: k.to_string(),
                    value: Arg::String(v.to_string(), false),
                })
            }
            "WORKSPACE" => {
                let target = args
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow!("requires target"))?;
                match target.as_str() {
                    "SNAPSHOT" | "snapshot" | "A" => {
                        Ok(StepKind::Workspace(WorkspaceTarget::Snapshot))
                    }
                    "LOCAL" | "local" | "B" => Ok(StepKind::Workspace(WorkspaceTarget::Local)),
                    _ => bail!("unknown workspace target"),
                }
            }
            "INHERIT_ENV" => {
                let keys = args.into_iter().map(|a| a.as_str().to_string()).collect();
                Ok(StepKind::InheritEnv { keys })
            }
            "ECHO" => {
                let msg = args
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow!("ECHO requires arg"))?;
                Ok(StepKind::Echo(msg))
            }
            "RUN" => {
                let cmd = args
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow!("RUN requires arg"))?;
                Ok(StepKind::Run(cmd))
            }
            "WORKDIR" => {
                let path = args
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow!("requires path"))?;
                Ok(StepKind::Workdir(path))
            }
            _ => bail!("unknown command: {name}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    #[cfg(feature = "proc-macro-api")]
    use quote::quote;
    use std::collections::HashMap;

    /// Mock lowering — tests grammar mechanics, not domain commands.
    fn test_lower(name: &str, args: Vec<Arg>) -> anyhow::Result<StepKind> {
        crate::test_lower_mock::lower(name, args)
    }

    fn guard_text(step: &Step) -> Option<String> {
        step.guard.as_ref().map(|g| g.to_string())
    }

    #[test]
    fn commands_are_case_sensitive() {
        for bad in ["cwd hi", "Cwd hi", "cwd foo"] {
            parse_script(bad, test_lower).expect_err("mixed/lowercase commands must fail");
        }
    }

    #[test]
    fn string_dsl_supports_rust_style_comments() {
        let script = indoc! {r#"
            // leading comment line
            CWD // inline comment
            WRITE 'echo "keep // literal"'
            /* block comment
               CWD ignored
               /* nested inner */
               WRITE ignored as well
            */
            WRITE "echo final"
            WRITE "echo 'literal /* stay */ value'"
        "#};
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 4, "expected 4 executable steps");
        assert!(matches!(&steps[0].kind, StepKind::Cwd));
        assert!(matches!(&steps[1].kind, StepKind::Write { .. }));
        assert!(matches!(&steps[2].kind, StepKind::Write { .. }));
        assert!(matches!(&steps[3].kind, StepKind::Write { .. }));
    }

    #[test]
    fn string_dsl_errors_on_unclosed_block_comment() {
        let script = indoc! {r#"
            WRITE echo hi
            /* unclosed
        "#};
        parse_script(script, test_lower).expect_err("should fail");
    }

    #[test]
    fn semicolon_splits_instructions() {
        let script = "WRITE \"echo hi\"; WRITE \"echo bye\"";
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 2);
    }

    #[test]
    fn guard_supports_colon_separator() {
        let script = "[env:FOO] WRITE \"echo hi\"";
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 1);
        assert_eq!(guard_text(&steps[0]).as_deref(), Some("env:FOO"));
    }

    #[test]
    fn guard_lines_chain_before_block() {
        let script = indoc! {r#"
            [env:A]
            [env:B]
            {
                WRITE ok.txt hi
            }
        "#};
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 1);
        assert_eq!(guard_text(&steps[0]).as_deref(), Some("env:A, env:B"));
    }

    #[test]
    fn guard_block_must_contain_command() {
        let script = indoc! {r#"
            [env.A] {
            }
        "#};
        parse_script(script, test_lower).expect_err("empty block should fail");
    }

    #[test]
    fn with_io_supports_named_pipes() {
        let script = "WITH_IO [stdin, stdout=pipe:setup, stderr=pipe:errors] WRITE \"echo hi\"";
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 1);
        match &steps[0].kind {
            StepKind::WithIo { bindings, cmd } => {
                assert_eq!(bindings.len(), 3);
                assert!(
                    bindings
                        .iter()
                        .any(|b| matches!(b.stream, IoStream::Stdin) && b.pipe.is_none())
                );
                assert!(
                    bindings.iter().any(|b| matches!(b.stream, IoStream::Stdout)
                        && b.pipe.as_deref() == Some("setup"))
                );
                assert!(
                    bindings.iter().any(|b| matches!(b.stream, IoStream::Stderr)
                        && b.pipe.as_deref() == Some("errors"))
                );
                assert!(matches!(cmd.as_ref(), StepKind::Write { .. }));
            }
            other => panic!("expected WITH_IO, saw {:?}", other),
        }
    }

    #[test]
    fn brace_blocks_require_guard() {
        let script = indoc! {r#"
            {
                WRITE nope.txt hi
            }
        "#};
        parse_script(script, test_lower).expect_err("unguarded block should fail");
    }

    #[test]
    fn multi_line_guard_blocks_apply_to_next_command() {
        let script = indoc! {r#"
            [
                env:A,
                env:B
            ]
            WRITE "echo guarded"
        "#};
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 1);
        assert_eq!(guard_text(&steps[0]).as_deref(), Some("env:A, env:B"));
    }

    #[test]
    fn guarded_brace_blocks_apply_to_all_inner_steps() {
        let script = indoc! {r#"
            [env:A] {
                WRITE one.txt 1
                WRITE two.txt 2
            }
        "#};
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 2);
        assert!(steps.iter().all(|s| s.guard.is_some()));
    }

    #[test]
    fn nested_guard_blocks_stack() {
        let script = indoc! {r#"
            [env:A] {
                WRITE outer.txt no
                [env:B] {
                    WRITE nested.txt yes
                }
            }
        "#};
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 2);
        assert_eq!(guard_text(&steps[0]).as_deref(), Some("env:A"));
        assert_eq!(guard_text(&steps[1]).as_deref(), Some("env:A, env:B"));
    }

    #[test]
    fn nested_guard_block_scopes_stack_counts() {
        let script = indoc! {r#"
            [env:A] {
                WRITE outer.txt ok
                [env:B] {
                    WRITE deep.txt ok
                }
                WRITE outer_again.txt ok
            }
        "#};
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].scope_enter, 1);
        assert_eq!(steps[0].scope_exit, 0);
        assert_eq!(steps[1].scope_enter, 1);
        assert_eq!(steps[1].scope_exit, 1);
        assert_eq!(steps[2].scope_enter, 0);
        assert_eq!(steps[2].scope_exit, 1);
    }

    #[test]
    fn guard_or_and_and_compose_as_expected() {
        let script = indoc! {r#"
            [env:A]
            [any(env:B, env:C)]
            WRITE "echo complex"
        "#};
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 1);
        let guard = steps[0].guard.as_ref().expect("missing guard");
        assert_eq!(guard.to_string(), "env:A, any(env:B, env:C)");

        let mut env = HashMap::new();
        env.insert("A".into(), "1".into());
        env.insert("B".into(), "1".into());
        assert!(guard_expr_allows(guard, &env), "A && B should pass");

        env.remove("B");
        env.insert("C".into(), "1".into());
        assert!(guard_expr_allows(guard, &env), "A && C should pass");

        env.remove("C");
        assert!(!guard_expr_allows(guard, &env), "A without B/C should fail");
    }

    #[test]
    fn guard_or_requires_at_least_one_branch() {
        let expr = GuardExpr::or(vec![
            Guard::EnvExists {
                key: "MISSING".into(),
            }
            .into(),
            Guard::EnvExists {
                key: "ALSO_MISSING".into(),
            }
            .into(),
        ]);
        assert!(!guard_expr_allows(&expr, &HashMap::new()));
        let mut env = HashMap::new();
        env.insert("MISSING".into(), "1".into());
        assert!(guard_expr_allows(&expr, &env));
    }

    #[test]
    fn guard_or_can_chain_with_additional_predicates() {
        let script = "[any(env:A, linux), mac] WRITE \"echo hi\"";
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 1);
        let guard = steps[0].guard.as_ref().expect("missing guard");
        assert_eq!(guard.to_string(), "any(env:A, linux), macos");
        let GuardExpr::All(children) = guard else {
            panic!("expected ALL guard");
        };
        assert!(matches!(children[0], GuardExpr::Or(_)));
        match &children[1] {
            GuardExpr::Predicate(Guard::Platform {
                target: PlatformGuard::Macos,
            }) => {}
            other => panic!("unexpected trailing guard: {other:?}"),
        }
    }

    #[test]
    fn guard_or_guard_line_parses() {
        use crate::lexer::{LanguageParser, Rule};
        use pest::Parser;
        LanguageParser::parse(Rule::guard_line, "[any(linux, env:FOO)]")
            .expect("guard guard line should parse");
    }

    #[test]
    fn env_equals_guard_with_not_wrapper() {
        let g = GuardExpr::Not(Box::new(GuardExpr::Predicate(Guard::EnvEquals {
            key: "A".into(),
            value: "1".into(),
        })));
        let mut env = HashMap::new();
        env.insert("A".into(), "1".into());
        assert!(!guard_expr_allows(&g, &env));
        env.insert("A".into(), "2".into());
        assert!(guard_expr_allows(&g, &env));
    }

    #[test]
    fn guard_block_emits_scope_markers() {
        let script = indoc! {r#"
            ENV RUN=1
            [env:RUN] {
                WRITE one.txt 1
                WRITE two.txt 2
            }
            WRITE three.txt 3
        "#};
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[1].scope_enter, 1);
        assert_eq!(steps[1].scope_exit, 0);
        assert_eq!(steps[2].scope_enter, 0);
        assert_eq!(steps[2].scope_exit, 1);
        assert_eq!(steps[3].scope_enter, 0);
        assert_eq!(steps[3].scope_exit, 0);
    }

    #[test]
    fn mock_hash_form_parses() {
        let script = "HASH_SHA256 --hash aabb path.txt";
        let steps = parse_script(script, test_lower).expect("parse ok");
        match &steps[0].kind {
            StepKind::AssertFile {
                hash,
                path,
                contents,
            } => {
                assert_eq!(hash.as_deref(), Some("aabb"));
                assert_eq!(path.as_ref(), "path.txt");
                assert!(contents.is_none());
            }
            other => panic!("expected AssertFile, saw {:?}", other),
        }
    }

    #[test]
    fn mock_commands_parse_and_round_trip() {
        let script = indoc! {r#"
            WRITE "dist/hello.txt" "Built with OxDock"
            WRITE "deeply/nested/tree"
            WRITE "chained.txt"
            WRITE "visible-after-comments"
        "#};
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 4);

        // Verify each step's kind matches what we expect
        for step in &steps {
            assert!(
                matches!(&step.kind, StepKind::Write { .. }),
                "expected Write variant"
            );
        }
    }

    #[test]
    fn quoted_string_content_preserved() {
        let script = "WRITE 'echo \"a; b\"'";
        let steps = parse_script(script, test_lower).expect("parse ok");
        match &steps[0].kind {
            StepKind::Write { path, .. } => assert_eq!(path, "echo \"a; b\""),
            other => panic!("expected Write, saw {:?}", other),
        }
    }

    #[test]
    fn templated_argument_with_spaces() {
        let script = "WRITE {{ env:OXBOOK_RUNNER_DIR }}";
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 1);
        match &steps[0].kind {
            StepKind::Write { path, .. } => assert_eq!(path, "{{ env:OXBOOK_RUNNER_DIR }}"),
            other => panic!("expected Write, saw {:?}", other),
        }
    }

    #[test]
    #[cfg(feature = "proc-macro-api")]
    fn string_and_braced_scripts_produce_identical_ast() {
        let mut cases = Vec::new();

        cases.push((
            indoc! {r#"
                WRITE /tmp
                WRITE hello
            "#}
            .trim()
            .to_string(),
            quote! {
                WRITE /tmp
                WRITE hello
            },
        ));

        cases.push((
            indoc! {r#"
                [not(env:SKIP)]
                [windows] WRITE win
                [eq(env:MODE, beta), linux] WRITE combo
            "#}
            .trim()
            .to_string(),
            quote! {
                [not(env:SKIP)]
                [windows] WRITE win
                [eq(env:MODE, beta), linux] WRITE combo
            },
        ));

        cases.push((
            indoc! {r#"
                [env:OUTER] {
                    WRITE nested
                    [env:INNER] WRITE deep
                }
            "#}
            .trim()
            .to_string(),
            quote! {
                [env:OUTER] {
                    WRITE nested
                    [env:INNER] WRITE deep
                }
            },
        ));

        cases.push((
            indoc! {r#"
                [eq(env:TEST, 1)]
                WITH_IO [stdout=pipe:capture_case] WRITE hi
                WITH_IO [stdin=pipe:capture_case] WRITE out.txt
            "#}
            .trim()
            .to_string(),
            quote! {
                [eq(env:TEST, 1)]
                WITH_IO [stdout=pipe:capture_case] WRITE hi
                WITH_IO [stdin=pipe:capture_case] WRITE out.txt
            },
        ));

        for (idx, (literal, tokens)) in cases.iter().enumerate() {
            let text = literal.trim();
            let string_steps = parse_script(text, test_lower)
                .unwrap_or_else(|e| panic!("string parse failed for case {idx}: {e}"));
            let braced_steps = parse_braced_tokens(tokens, test_lower)
                .unwrap_or_else(|e| panic!("token parse failed for case {idx}: {e}"));
            assert_eq!(
                string_steps, braced_steps,
                "AST mismatch for case {idx} literal:\n{text}"
            );
        }
    }

    #[test]
    fn let_assign_with_bare_word() {
        let script = r#"LET $x = hello"#;
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 1);
        match &steps[0].kind {
            StepKind::Assign { var, expr } => {
                assert_eq!(var, "x");
                assert_eq!(expr, &Expr::Literal(Value::String("hello".to_string())));
            }
            other => panic!("expected Assign, got {:?}", other),
        }
    }

    #[test]
    fn let_assign_with_quoted_string() {
        let script = r#"LET $x = "hello world""#;
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 1);
        match &steps[0].kind {
            StepKind::Assign { var, expr } => {
                assert_eq!(var, "x");
                assert_eq!(
                    expr,
                    &Expr::Literal(Value::String("hello world".to_string()))
                );
            }
            other => panic!("expected Assign, got {:?}", other),
        }
    }

    #[test]
    fn let_assign_with_list_literal() {
        let script = r#"LET $x = ["a", "b", "c"]"#;
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 1);
        match &steps[0].kind {
            StepKind::Assign { var, expr } => {
                assert_eq!(var, "x");
                assert_eq!(
                    expr,
                    &Expr::List(vec![
                        Expr::Literal(Value::String("a".to_string())),
                        Expr::Literal(Value::String("b".to_string())),
                        Expr::Literal(Value::String("c".to_string()))
                    ])
                );
            }
            other => panic!("expected Assign, got {:?}", other),
        }
    }

    #[test]
    fn let_assign_with_variable_ref() {
        let script = r#"LET $x = $y"#;
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 1);
        match &steps[0].kind {
            StepKind::Assign { var, expr } => {
                assert_eq!(var, "x");
                assert_eq!(expr, &Expr::Var("y".to_string()));
            }
            other => panic!("expected Assign, got {:?}", other),
        }
    }

    #[test]
    fn for_loop_parses() {
        let script = indoc! {r#"
            FOR $f IN ["x", "y"] {
                WRITE $f
            }
        "#};
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 1);
        match &steps[0].kind {
            StepKind::For {
                key_var,
                var,
                in_expr,
                body,
            } => {
                assert!(key_var.is_none());
                assert_eq!(var, "f");
                assert_eq!(
                    in_expr,
                    &Expr::List(vec![
                        Expr::Literal(Value::String("x".to_string())),
                        Expr::Literal(Value::String("y".to_string()))
                    ])
                );
                assert_eq!(body.len(), 1);
            }
            other => panic!("expected For, got {:?}", other),
        }
    }

    #[test]
    fn for_map_iteration_parses() {
        let script = indoc! {r#"
            FOR $k, $v IN $map {
                WRITE $k
            }
        "#};
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 1);
        match &steps[0].kind {
            StepKind::For {
                key_var,
                var,
                in_expr,
                body,
            } => {
                assert_eq!(key_var.as_deref(), Some("k"));
                assert_eq!(var, "v");
                assert_eq!(in_expr, &Expr::Var("map".to_string()));
                assert_eq!(body.len(), 1);
            }
            other => panic!("expected For, got {:?}", other),
        }
    }

    #[test]
    fn if_statement_parses() {
        let script = "IF true { WRITE yes }\n";
        let steps = parse_script(script, test_lower).expect("parse ok");
        assert_eq!(steps.len(), 1);
        match &steps[0].kind {
            StepKind::If { .. } => {}
            other => panic!("expected If, got {:?}", other),
        }
    }

    #[test]
    fn if_keyword_matches_directly() {
        use crate::lexer::{LanguageParser, Rule};
        use pest::Parser;
        let result = LanguageParser::parse(Rule::if_keyword, "IF ");
        assert!(
            result.is_ok(),
            "if_keyword should match 'IF ': {:?}",
            result.err()
        );
    }

    #[test]
    fn if_statement_pest_matches() {
        use crate::lexer::{LanguageParser, Rule};
        use pest::Parser;
        let result = LanguageParser::parse(Rule::if_statement, "IF true {\n    WRITE yes\n}");
        assert!(
            result.is_ok(),
            "if_statement should match: {:?}",
            result.err()
        );
    }

    #[test]
    fn guard_block_with_mock_command() {
        let script = "CWD\n[env:GATE] {\n    WRITE gated\n}\n[eq(env:A, 1)] WRITE eq\n";
        let steps = parse_script(script, test_lower).expect("parse should succeed");
        assert!(
            steps.len() >= 2,
            "expected at least 2 steps, got {}",
            steps.len()
        );
    }

    #[test]
    fn single_line_blocks() {
        let test_cases = [
            "IF true { WRITE \"hello\" }",
            "IF true { WRITE \"cargo test\" }",
            "IF true { WRITE \"/app\" }",
        ];
        for script in test_cases {
            assert!(
                parse_script(script, test_lower).is_ok(),
                "Failed to parse: {}",
                script
            );
        }
    }

    #[test]
    fn async_run_parses() {
        let script = "ASYNC RUN \"echo hello\"";
        let steps = parse_script(script, test_lower).expect("parse should succeed");
        assert_eq!(steps.len(), 1);
        assert!(matches!(&steps[0].kind, StepKind::AsyncBlock { .. }));
    }

    #[test]
    fn async_block_parses() {
        let script = indoc! {r#"
            ASYNC {
                RUN "echo one"
                RUN "echo two"
            }
        "#};
        let steps = parse_script(script, test_lower).expect("parse should succeed");
        assert_eq!(steps.len(), 1);
        match &steps[0].kind {
            StepKind::AsyncBlock { body } => {
                assert_eq!(body.len(), 2);
            }
            other => panic!("expected AsyncBlock, got {:?}", other),
        }
    }

    #[test]
    fn nested_async_parses() {
        let script = "ASYNC ASYNC RUN \"echo nested\"";
        let steps = parse_script(script, test_lower).expect("parse should succeed");
        assert_eq!(steps.len(), 1);
        match &steps[0].kind {
            StepKind::AsyncBlock { body } => {
                assert_eq!(body.len(), 1);
                match &body[0].kind {
                    StepKind::AsyncBlock { body } => {
                        assert_eq!(body.len(), 1);
                        assert!(matches!(&body[0].kind, StepKind::Run(_)));
                    }
                    other => panic!("expected inner AsyncBlock, got {:?}", other),
                }
            }
            other => panic!("expected outer AsyncBlock, got {:?}", other),
        }
    }

    #[test]
    fn nested_async_block_form_parses() {
        let script = indoc! {r#"
            ASYNC {
                ASYNC {
                    RUN "echo nested"
                }
            }
        "#};
        let steps = parse_script(script, test_lower).expect("parse should succeed");
        assert_eq!(steps.len(), 1);
        match &steps[0].kind {
            StepKind::AsyncBlock { body } => {
                assert_eq!(body.len(), 1);
                match &body[0].kind {
                    StepKind::AsyncBlock { body } => {
                        assert_eq!(body.len(), 1);
                        assert!(matches!(&body[0].kind, StepKind::Run(_)));
                    }
                    other => panic!("expected inner AsyncBlock, got {:?}", other),
                }
            }
            other => panic!("expected outer AsyncBlock, got {:?}", other),
        }
    }

    #[test]
    fn with_io_wrapping_async_parses() {
        let script = "WITH_IO [stdout] ASYNC RUN \"echo test\"";
        let steps = parse_script(script, test_lower).expect("parse should succeed");
        assert_eq!(steps.len(), 1);
        match &steps[0].kind {
            StepKind::WithIo { cmd, .. } => {
                assert!(matches!(cmd.as_ref(), StepKind::AsyncBlock { .. }));
            }
            other => panic!("expected WithIo, got {:?}", other),
        }
    }
}
