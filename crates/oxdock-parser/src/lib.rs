pub mod ast;
mod lexer;
#[cfg(feature = "proc-macro-api")]
mod macro_input;
pub mod parser;

pub use ast::*;
pub use lexer::LANGUAGE_SPEC;
#[cfg(feature = "proc-macro-api")]
pub use macro_input::{
    DslMacroInput, ScriptSource, parse_braced_tokens, script_from_braced_tokens,
};
pub use parser::parse_script;

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    #[cfg(feature = "proc-macro-api")]
    use quote::quote;
    use std::collections::HashMap;

    fn guard_text(step: &Step) -> Option<String> {
        step.guard.as_ref().map(|g| g.to_string())
    }

    #[test]
    fn commands_are_case_sensitive() {
        for bad in ["run echo hi", "Run echo hi", "rUn echo hi", "write foo bar"] {
            parse_script(bad).expect_err("mixed/lowercase commands must fail");
        }
    }

    #[test]
    fn string_dsl_supports_rust_style_comments() {
        let script = indoc! {r#"
            // leading comment line
            WORKDIR /tmp // inline comment
            RUN echo "keep // literal"
            /* block comment
               WORKDIR ignored
               /* nested inner */
               RUN ignored as well
            */
            RUN echo final
            RUN echo 'literal /* stay */ value'
        "#};
        let steps = parse_script(script).expect("parse ok");
        assert_eq!(steps.len(), 4, "expected 4 executable steps");
        match &steps[0].kind {
            StepKind::Workdir(path) => assert_eq!(path, "/tmp"),
            other => panic!("expected WORKDIR, saw {:?}", other),
        }
        match &steps[1].kind {
            StepKind::Run(cmd) => assert_eq!(cmd, "echo \"keep // literal\""),
            other => panic!("expected RUN, saw {:?}", other),
        }
        match &steps[2].kind {
            StepKind::Run(cmd) => assert_eq!(cmd, "echo final"),
            other => panic!("expected RUN, saw {:?}", other),
        }
        match &steps[3].kind {
            StepKind::Run(cmd) => assert_eq!(cmd, "echo 'literal /* stay */ value'"),
            other => panic!("expected RUN, saw {:?}", other),
        }
    }

    #[test]
    fn string_dsl_errors_on_unclosed_block_comment() {
        let script = indoc! {r#"
            RUN echo hi
            /* unclosed
        "#};
        parse_script(script).expect_err("should fail");
    }

    #[test]
    fn semicolon_attached_to_command_splits_instructions() {
        let script = "RUN echo hi; RUN echo bye";
        let steps = parse_script(script).expect("parse ok");
        assert_eq!(steps.len(), 2);
        match &steps[0].kind {
            StepKind::Run(cmd) => assert_eq!(cmd, "echo hi"),
            other => panic!("expected RUN, saw {:?}", other),
        }
        match &steps[1].kind {
            StepKind::Run(cmd) => assert_eq!(cmd, "echo bye"),
            other => panic!("expected RUN, saw {:?}", other),
        }
    }

    #[test]
    fn guard_supports_colon_separator() {
        let script = "[env:FOO] RUN echo hi";
        let steps = parse_script(script).expect("parse ok");
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
        let steps = parse_script(script).expect("parse ok");
        assert_eq!(steps.len(), 1);
        assert_eq!(guard_text(&steps[0]).as_deref(), Some("env:A, env:B"));
    }

    #[test]
    fn guard_block_must_contain_command() {
        let script = indoc! {r#"
            [env.A] {
            }
        "#};
        parse_script(script).expect_err("empty block should fail");
    }

    #[test]
    fn with_io_supports_named_pipes() {
        let script = "WITH_IO [stdin, stdout=pipe:setup, stderr=pipe:errors] RUN echo hi";
        let steps = parse_script(script).expect("parse ok");
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
                assert_eq!(cmd.as_ref(), &StepKind::Run("echo hi".into()));
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
        parse_script(script).expect_err("unguarded block should fail");
    }

    #[test]
    fn multi_line_guard_blocks_apply_to_next_command() {
        let script = indoc! {r#"
            [
                env:A,
                env:B
            ]
            RUN echo guarded
        "#};
        let steps = parse_script(script).expect("parse ok");
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
        let steps = parse_script(script).expect("parse ok");
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
        let steps = parse_script(script).expect("parse ok");
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
        let steps = parse_script(script).expect("parse ok");
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
            [or(env:B, env:C)]
            RUN echo complex
        "#};
        let steps = parse_script(script).expect("parse ok");
        assert_eq!(steps.len(), 1);
        let guard = steps[0].guard.as_ref().expect("missing guard");
        assert_eq!(guard.to_string(), "env:A, or(env:B, env:C)");

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
                invert: false,
            }
            .into(),
            Guard::EnvExists {
                key: "ALSO_MISSING".into(),
                invert: false,
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
        let script = "[or(env:A, linux), mac] RUN echo hi";
        let steps = parse_script(script).expect("parse ok");
        assert_eq!(steps.len(), 1);
        let guard = steps[0].guard.as_ref().expect("missing guard");
        assert_eq!(guard.to_string(), "or(env:A, linux), macos");
        let GuardExpr::All(children) = guard else {
            panic!("expected ALL guard");
        };
        assert!(matches!(children[0], GuardExpr::Or(_)));
        match &children[1] {
            GuardExpr::Predicate(Guard::Platform {
                target: PlatformGuard::Macos,
                invert: false,
            }) => {}
            other => panic!("unexpected trailing guard: {other:?}"),
        }
    }

    #[test]
    fn guard_or_guard_line_parses() {
        use crate::lexer::{LanguageParser, Rule};
        use pest::Parser;
        LanguageParser::parse(Rule::guard_line, "[or(linux, env:FOO)]")
            .expect("guard guard line should parse");
    }

    #[test]
    fn env_equals_guard_respects_inversion() {
        let g = Guard::EnvEquals {
            key: "A".into(),
            value: "1".into(),
            invert: true,
        };
        let mut env = HashMap::new();
        env.insert("A".into(), "1".into());
        assert!(!guard_allows(&g, &env));
        env.insert("A".into(), "2".into());
        assert!(guard_allows(&g, &env));
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
        let steps = parse_script(script).expect("parse ok");
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[1].scope_enter, 1);
        assert_eq!(steps[1].scope_exit, 0);
        assert_eq!(steps[2].scope_enter, 0);
        assert_eq!(steps[2].scope_exit, 1);
        assert_eq!(steps[3].scope_enter, 0);
        assert_eq!(steps[3].scope_exit, 0);
    }

    #[test]
    fn run_args_single_quoted_unwraps() {
        let script = "RUN \"echo hi\"";
        let steps = parse_script(script).expect("parse ok");
        assert_eq!(steps.len(), 1, "expected single step");
        match &steps[0].kind {
            StepKind::Run(cmd) => assert_eq!(cmd, "echo hi"),
            other => panic!("expected RUN command, saw {:?}", other),
        }
    }

    #[test]
    fn run_args_preserve_quotes_for_problematic_tokens() {
        let script = "RUN echo \"a; b\"";
        let steps = parse_script(script).expect("parse ok");
        match &steps[0].kind {
            StepKind::Run(cmd) => {
                assert_eq!(cmd, "echo \"a; b\"");
            }
            other => panic!("expected RUN command, saw {:?}", other),
        }
    }

    #[test]
    fn workdir_allows_templated_argument_with_spaces() {
        let script = "WORKDIR {{ env:OXBOOK_RUNNER_DIR }}";
        let steps = parse_script(script).expect("parse ok");
        assert_eq!(steps.len(), 1);
        match &steps[0].kind {
            StepKind::Workdir(path) => {
                assert_eq!(path, "{{ env:OXBOOK_RUNNER_DIR }}");
            }
            other => panic!("expected WORKDIR, saw {:?}", other),
        }
    }

    #[test]
    #[cfg(feature = "proc-macro-api")]
    fn string_and_braced_scripts_produce_identical_ast() {
        let mut cases = Vec::new();

        cases.push((
            indoc! {r#"
                WORKDIR /tmp
                RUN echo hello
            "#}
            .trim()
            .to_string(),
            quote! {
                WORKDIR /tmp
                RUN echo hello
            },
        ));

        cases.push((
            indoc! {r#"
                [!env:SKIP]
                [windows] RUN echo win
                [env:MODE==beta, linux] RUN echo combo
            "#}
            .trim()
            .to_string(),
            quote! {
                [!env:SKIP]
                [windows] RUN echo win
                [env:MODE==beta, linux] RUN echo combo
            },
        ));

        cases.push((
            indoc! {r#"
                [env:OUTER] {
                    WORKDIR nested
                    [env:INNER] RUN echo deep
                }
            "#}
            .trim()
            .to_string(),
            quote! {
                [env:OUTER] {
                    WORKDIR nested
                    [env:INNER] RUN echo deep
                }
            },
        ));

        cases.push((
            indoc! {r#"
                [env:TEST==1]
                WITH_IO [stdout=pipe:capture_case] RUN echo hi
                WITH_IO [stdin=pipe:capture_case] WRITE out.txt
            "#}
            .trim()
            .to_string(),
            quote! {
                [env:TEST==1]
                WITH_IO [stdout=pipe:capture_case] RUN echo hi
                WITH_IO [stdin=pipe:capture_case] WRITE out.txt
            },
        ));

        for (idx, (literal, tokens)) in cases.iter().enumerate() {
            let text = literal.trim();
            let string_steps = parse_script(text)
                .unwrap_or_else(|e| panic!("string parse failed for case {idx}: {e}"));
            let braced_steps = parse_braced_tokens(tokens)
                .unwrap_or_else(|e| panic!("token parse failed for case {idx}: {e}"));
            assert_eq!(
                string_steps, braced_steps,
                "AST mismatch for case {idx} literal:\n{text}"
            );
        }
    }
}
