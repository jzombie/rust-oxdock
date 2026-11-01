#[cfg(feature = "proc-macro-api")]
mod tests {
    use indoc::indoc;
    use oxdock_parser::script_from_braced_tokens;
    use proc_macro2::{Delimiter, Group, Ident, Punct, Spacing, Span, TokenStream, TokenTree};

    #[test]
    fn script_from_braced_tokens_handles_paren_group() {
        let mut ts = TokenStream::new();
        ts.extend([
            TokenTree::Ident(Ident::new("RUN", Span::call_site())),
            TokenTree::Ident(Ident::new("echo", Span::call_site())),
            TokenTree::Group(Group::new(
                Delimiter::Parenthesis,
                TokenStream::from(TokenTree::Ident(Ident::new("foo", Span::call_site()))),
            )),
        ]);

        let script = script_from_braced_tokens(&ts).expect("failed to render parentheses group");
        assert!(
            script.contains("( foo )"),
            "expected parentheses group in script: {script}"
        );
    }

    #[test]
    fn script_from_braced_tokens_handles_none_group() {
        let mut ts = TokenStream::new();
        ts.extend([
            TokenTree::Ident(Ident::new("RUN", Span::call_site())),
            TokenTree::Ident(Ident::new("echo", Span::call_site())),
            TokenTree::Group(Group::new(
                Delimiter::None,
                TokenStream::from(TokenTree::Ident(Ident::new("BAR", Span::call_site()))),
            )),
        ]);

        let script = script_from_braced_tokens(&ts).expect("failed to render none-delimiter group");
        assert!(
            script.contains("BAR"),
            "expected none-delimiter content in script: {script}"
        );
    }

    #[test]
    fn script_from_braced_tokens_preserves_dollar_brace_interpolation() {
        let mut ts = TokenStream::new();
        ts.extend([
            TokenTree::Ident(Ident::new("ECHO", Span::call_site())),
            TokenTree::Punct(Punct::new('$', Spacing::Alone)),
            TokenTree::Group(Group::new(
                Delimiter::Brace,
                TokenStream::from(TokenTree::Ident(Ident::new("RUN", Span::call_site()))),
            )),
        ]);

        let script = script_from_braced_tokens(&ts).expect("failed to render interpolation group");
        assert!(
            script.contains("${RUN}"),
            "expected interpolation to be preserved, got: {script}"
        );
    }

    #[test]
    fn script_from_braced_tokens_terminates_write_instruction() {
        let mut ts = TokenStream::new();
        ts.extend([
            TokenTree::Ident(Ident::new("WRITE", Span::call_site())),
            TokenTree::Ident(Ident::new("out_txt", Span::call_site())),
            TokenTree::Ident(Ident::new("WORKDIR", Span::call_site())),
            TokenTree::Ident(Ident::new("dist", Span::call_site())),
        ]);

        let script = script_from_braced_tokens(&ts).expect("failed to render write instruction");
        assert!(
            script.contains(indoc! {r#"WRITE out_txt
            WORKDIR dist"#}),
            "expected WRITE to terminate before next command, got: {script}"
        );
    }

    #[test]
    fn script_from_braced_tokens_preserves_flag_spacing() {
        let mut ts = TokenStream::new();
        ts.extend([
            TokenTree::Ident(Ident::new("RUN", Span::call_site())),
            TokenTree::Ident(Ident::new("ls", Span::call_site())),
            TokenTree::Punct(Punct::new('-', Spacing::Alone)),
            TokenTree::Ident(Ident::new("lsa", Span::call_site())),
        ]);

        let script = script_from_braced_tokens(&ts).expect("failed to render run args");
        assert!(
            script.contains("RUN ls -lsa"),
            "expected flag spacing, got: {script}"
        );
    }

    #[test]
    fn script_from_braced_tokens_keeps_joint_hyphenated_words() {
        let mut ts = TokenStream::new();
        ts.extend([
            TokenTree::Ident(Ident::new("RUN", Span::call_site())),
            TokenTree::Ident(Ident::new("foo", Span::call_site())),
            TokenTree::Punct(Punct::new('-', Spacing::Joint)),
            TokenTree::Ident(Ident::new("bar", Span::call_site())),
        ]);

        let script = script_from_braced_tokens(&ts).expect("failed to render hyphenated args");
        assert!(
            script.contains("RUN foo-bar"),
            "expected hyphenated word, got: {script}"
        );
    }

    #[test]
    fn script_from_braced_tokens_preserves_quoted_run_args() {
        let mut ts = TokenStream::new();
        ts.extend([
            TokenTree::Ident(Ident::new("RUN", Span::call_site())),
            TokenTree::Literal(proc_macro2::Literal::string("ls -lsa")),
        ]);

        let script = script_from_braced_tokens(&ts).expect("failed to render quoted run args");
        assert!(
            script.contains("RUN \"ls -lsa\""),
            "expected quoted args preserved, got: {script}"
        );
    }

    #[test]
    fn script_from_braced_tokens_splits_semicolons_into_lines() {
        let mut ts = TokenStream::new();
        ts.extend([
            TokenTree::Ident(Ident::new("RUN", Span::call_site())),
            TokenTree::Ident(Ident::new("echo", Span::call_site())),
            TokenTree::Punct(Punct::new(';', Spacing::Alone)),
            TokenTree::Ident(Ident::new("LS", Span::call_site())),
            TokenTree::Punct(Punct::new(';', Spacing::Alone)),
            TokenTree::Ident(Ident::new("RUN", Span::call_site())),
            TokenTree::Ident(Ident::new("echo", Span::call_site())),
            TokenTree::Punct(Punct::new('&', Spacing::Joint)),
            TokenTree::Punct(Punct::new('&', Spacing::Alone)),
            TokenTree::Ident(Ident::new("ls", Span::call_site())),
        ]);

        let script = script_from_braced_tokens(&ts).expect("failed to render semicolon script");
        assert_eq!(
            script,
            indoc! {r#"RUN echo;
            LS;
            RUN echo && ls"#}
        );
    }

    #[test]
    fn script_from_braced_tokens_allows_command_like_ident_in_async() {
        let mut ts = TokenStream::new();
        ts.extend([
            TokenTree::Ident(Ident::new("ASYNC", Span::call_site())),
            TokenTree::Ident(Ident::new("LS", Span::call_site())),
            TokenTree::Ident(Ident::new("a", Span::call_site())),
        ]);

        let script = script_from_braced_tokens(&ts).expect("failed to render ASYNC script");
        assert_eq!(script, "ASYNC LS a");
    }

    #[test]
    fn func_docs_associate_with_the_correct_function_on_both_paths() {
        // Association proof across three scenarios: distinct docs per
        // function, blank-line orphans attaching to nothing, and nested
        // definitions staying undocumented. String and braced paths must
        // agree exactly, on the map and on the steps.
        let text = indoc! {r#"
            # First does A.
            FUNC AAA() {
                RETURN "a"
            }
            # Second does B.
            # With two lines.
            FUNC BBB() {
                RETURN "b"
            }
            # Orphaned by a blank line.

            FUNC CCC() {
                RETURN "c"
            }
            IF true {
                # Looks like docs but nested.
                FUNC NESTED() {
                    RETURN "n"
                }
            }
        "#};
        let ts: TokenStream = text.parse().unwrap();
        let parse_string = || {
            oxdock_parser::parse_script_with_modules_and_exports(
                text,
                crate::common::mock_lower,
                std::collections::HashSet::new(),
                oxdock_parser::ModuleTable::default(),
            )
            .expect("string path parses")
        };
        let parse_braced = || {
            oxdock_parser::parse_braced_tokens_with_modules_and_exports(
                &ts,
                crate::common::mock_lower,
                std::collections::HashSet::new(),
                oxdock_parser::ModuleTable::default(),
            )
            .expect("braced path parses")
        };
        let string = parse_string();
        let braced = parse_braced();
        assert_eq!(braced.func_docs, string.func_docs);
        assert_eq!(braced.steps, string.steps);
        for parsed in [&string, &braced] {
            let doc_of = |want: &str| {
                parsed
                    .steps
                    .iter()
                    .flat_map(|s| match &s.kind {
                        oxdock_parser::StepKind::FuncDef { name, docs, .. } if name == want => {
                            Some(docs.clone())
                        }
                        oxdock_parser::StepKind::If { then_body, .. } => {
                            then_body.iter().find_map(|s| match &s.kind {
                                oxdock_parser::StepKind::FuncDef { name, docs, .. }
                                    if name == want =>
                                {
                                    Some(docs.clone())
                                }
                                _ => None,
                            })
                        }
                        _ => None,
                    })
                    .next()
                    .unwrap_or(None)
            };
            assert_eq!(
                doc_of("AAA"),
                Some(("First does A.".to_string(), "First does A.".to_string())),
                "AAA owns its docs"
            );
            assert_eq!(
                doc_of("BBB"),
                Some((
                    "Second does B.".to_string(),
                    "Second does B.\nWith two lines.".to_string()
                )),
                "BBB owns its two lines"
            );
            assert_eq!(doc_of("CCC"), None, "blank line orphans the docs");
            assert_eq!(doc_of("NESTED"), None, "nested defs stay undocumented");
        }
        assert_eq!(string.func_docs.len(), 2, "map holds AAA and BBB only");
    }

    #[test]
    fn braced_and_string_agree_on_func_docs() {
        // Capture proof, both paths: the same `#`-commented source must
        // yield identical docs, exports, and steps through the string
        // entry point and the braced `TokenStream` entry point. Docs are
        // read back out of the product here, not just tolerated.
        let text = indoc! {r#"
            # Adds one.
            # Second line.
            FUNC FOO() {
                RETURN "x"
            }
            EXPORT FOO
        "#};
        let ts: TokenStream = text.parse().unwrap();
        let string = oxdock_parser::parse_script_with_modules_and_exports(
            text,
            crate::common::mock_lower,
            std::collections::HashSet::new(),
            oxdock_parser::ModuleTable::default(),
        )
        .expect("string path parses");
        let braced = oxdock_parser::parse_braced_tokens_with_modules_and_exports(
            &ts,
            crate::common::mock_lower,
            std::collections::HashSet::new(),
            oxdock_parser::ModuleTable::default(),
        )
        .expect("braced path parses");
        assert_eq!(braced.func_docs, string.func_docs);
        assert_eq!(braced.exports, string.exports);
        assert_eq!(braced.steps, string.steps);
        let (summary, docs) = braced.func_docs.get("FOO").expect("docs captured");
        assert_eq!(summary, "Adds one.");
        assert!(docs.contains("Second line"), "{docs}");
        assert_eq!(braced.exports, vec!["FOO".to_string()]);
    }

    #[test]
    fn guard_value_named_like_command_and_env_key_stay_glued() {
        // Regression: guard idents that match command names (`LS`) or
        // follow `env:` must glue inside guard brackets. Splitting them
        // across lines strands `]` and fails the reparse (found by
        // multistep fuzz through `ASYNC { [..] WHILE .. }`).
        let guard: TokenStream = "[(env:LS)] WHILE false { ECHO hi }".parse().unwrap();
        let script = script_from_braced_tokens(&guard).expect("failed to render guard script");
        assert!(
            !script.contains("env:\n"),
            "guard key must stay glued to `env:`, got: {script:?}"
        );
        let steps = oxdock_parser::parse_script(&script, crate::common::mock_lower)
            .expect("rebuilt guard must parse");
        assert_eq!(steps.len(), 1, "got: {steps:?}");
    }

    #[test]
    fn glued_dash_in_run_shell_stays_glued() {
        // Regression: a dash glued to a word in RUN shell text (`W-XYZ`)
        // must rebuild glued. The flag-spacing heuristic only applies
        // when spans carry no adjacency signal (found by multistep fuzz).
        let run: TokenStream = "RUN W-XYZ".parse().unwrap();
        let script = script_from_braced_tokens(&run).expect("failed to render RUN script");
        assert_eq!(script, "RUN W-XYZ", "got: {script:?}");
    }

    #[test]
    fn braced_func_docs_and_exports_survive() {
        // The braced path must carry the same `ParsedScript` metadata the
        // string path provides: `#` doc blocks above top-level `FUNC`s
        // plus `EXPORT` names. Future MCP hosts reading braced input
        // need both to describe and expose tools.
        let ts: TokenStream = indoc! {r#"
            # Adds one.
            # Second line.
            FUNC FOO() {
                RETURN "x"
            }
            EXPORT FOO
        "#}
        .parse()
        .unwrap();
        let parsed = oxdock_parser::parse_braced_tokens_with_modules_and_exports(
            &ts,
            crate::common::mock_lower,
            std::collections::HashSet::new(),
            oxdock_parser::ModuleTable::default(),
        )
        .expect("braced export with docs parses");
        let (summary, docs) = parsed.func_docs.get("FOO").expect("docs collected");
        assert_eq!(summary, "Adds one.");
        assert!(docs.contains("Second line"), "{docs}");
        assert_eq!(parsed.exports, vec!["FOO".to_string()]);
        // The comment describes the function inside the product, not
        // just the sidecar: step-level docs match the map.
        let func_docs = parsed.steps.iter().find_map(|s| match &s.kind {
            oxdock_parser::StepKind::FuncDef { name, docs, .. } if name == "FOO" => docs.clone(),
            _ => None,
        });
        assert_eq!(
            func_docs,
            Some((
                "Adds one.".to_string(),
                "Adds one.\nSecond line.".to_string()
            )),
            "step docs must match the map"
        );
        assert!(
            parsed
                .steps
                .iter()
                .any(|s| matches!(s.kind, oxdock_parser::StepKind::FuncDef { .. })),
            "got: {:?}",
            parsed.steps
        );
        // The plain braced entry points keep their signatures and agree
        // on steps.
        let steps = oxdock_parser::parse_braced_tokens(&ts, crate::common::mock_lower)
            .expect("plain braced parse agrees");
        assert_eq!(steps.len(), parsed.steps.len());
    }
}
