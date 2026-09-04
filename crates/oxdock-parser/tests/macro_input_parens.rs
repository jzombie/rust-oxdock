#[cfg(feature = "proc-macro-api")]
mod tests {
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
            script.contains("WRITE out_txt\nWORKDIR dist"),
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
        assert_eq!(script, "RUN echo;\nLS;\nRUN echo && ls");
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
}
