//! Helpers that let proc-macro inputs reuse the regular string parser.
//!
//! The macros ultimately want everything to flow through `parse_script`, since
//! that code already does the heavy lifting of guard handling, scope tracking,
//! and AST construction.  Unfortunately `TokenStream` values do not retain
//! whitespace or “line” structure, so we first have to rebuild a textual DSL
//! representation that the parser understands.  The `sticky`/`needs_space`
//! helpers below exist solely to recreate enough spacing for commands such as
//! `ENV FOO=bar` or `RUN echo && ls` to look exactly like the string DSL,
//! keeping both pathways unified.

use super::{Command, Step, parse_script};
use anyhow::Result;
use proc_macro2::{Delimiter, LineColumn, Spacing, TokenStream as TokenStream2, TokenTree};
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitStr, Token};

/// Parsed macro arguments for `embed!` and `prepare!`.
pub struct DslMacroInput {
    pub name: Ident,
    pub script: ScriptSource,
    pub out_dir: LitStr,
}

/// The script payload, either as a literal string or a braced token stream.
pub enum ScriptSource {
    Literal(LitStr),
    Braced(TokenStream2),
}

impl Parse for DslMacroInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let name_label: Ident = input.parse()?;
        if name_label != "name" {
            return Err(syn::Error::new(name_label.span(), "expected `name` label"));
        }
        input.parse::<Token![:]>()?;
        let name: Ident = input.parse()?;
        let _ = input.parse::<Token![,]>().ok();

        let script_label: Ident = input.parse()?;
        if script_label != "script" {
            return Err(syn::Error::new(
                script_label.span(),
                "expected `script` label",
            ));
        }
        input.parse::<Token![:]>()?;
        let script = if input.peek(LitStr) {
            let s: LitStr = input.parse()?;
            ScriptSource::Literal(s)
        } else if input.peek(syn::token::Brace) {
            let content;
            syn::braced!(content in input);
            let ts: TokenStream2 = content.parse()?;
            ScriptSource::Braced(ts)
        } else {
            return Err(syn::Error::new(
                input.span(),
                "expected string literal or braced script block",
            ));
        };
        let _ = input.parse::<Token![,]>().ok();

        let out_dir_label: Ident = input.parse()?;
        if out_dir_label != "out_dir" {
            return Err(syn::Error::new(
                out_dir_label.span(),
                "expected `out_dir` label",
            ));
        }
        input.parse::<Token![:]>()?;
        let out_dir: LitStr = input.parse()?;
        let _ = input.parse::<Token![,]>().ok();

        Ok(Self {
            name,
            script,
            out_dir,
        })
    }
}

fn finalize_line(lines: &mut Vec<String>, line: &mut String, capture_has_inner: &mut bool) {
    let trimmed = line.trim();
    if !trimmed.is_empty() {
        lines.push(trimmed.to_string());
    }
    line.clear();
    *capture_has_inner = false;
}

fn sticky(c: char) -> bool {
    matches!(c, '/' | '.' | '-' | ':' | '=' | '$' | '{' | '}')
}

fn needs_space(prev: char, next: char) -> bool {
    if next == ';' {
        return false;
    }
    if prev.is_whitespace() || next.is_whitespace() {
        return false;
    }
    if sticky(prev) || sticky(next) {
        return false;
    }
    if (prev == '&' && next == '&') || (prev == '|' && next == '|') {
        return false;
    }
    true
}

fn push_fragment(buf: &mut String, frag: &str, force_space: bool) {
    if frag.is_empty() {
        return;
    }
    let next_char = frag.chars().next().unwrap_or(' ');
    if let Some(prev) = buf.chars().rev().find(|c| !c.is_whitespace())
        && ((force_space && !prev.is_whitespace()) || needs_space(prev, next_char))
    {
        buf.push(' ');
    }
    buf.push_str(frag);
}

fn span_gap_requires_space(prev: LineColumn, next: LineColumn) -> bool {
    prev.line == next.line && next.column > prev.column
}

fn delim_pair(delim: Delimiter) -> Option<(char, char)> {
    match delim {
        Delimiter::Parenthesis => Some(('(', ')')),
        Delimiter::Brace => Some(('{', '}')),
        Delimiter::Bracket => Some(('[', ']')),
        Delimiter::None => None,
    }
}

fn current_line_command(line: &str) -> Option<Command> {
    let trimmed = line.trim_start();
    let head = trimmed.split_whitespace().next()?;
    Command::parse(head)
}

fn line_expects_inner_command(line: &str) -> bool {
    matches!(
        current_line_command(line),
        Some(cmd) if cmd.expects_inner_command()
    )
}

fn line_is_run_context(line: &str) -> bool {
    matches!(
        current_line_command(line),
        Some(Command::Run | Command::RunBg)
    )
}

fn walk(
    ts: TokenStream2,
    line: &mut String,
    lines: &mut Vec<String>,
    last_was_command: &mut bool,
    in_interpolation: bool,
    capture_has_inner: &mut bool,
    last_span_end: &mut Option<LineColumn>,
) -> Result<()> {
    let tokens: Vec<TokenTree> = ts.into_iter().collect();
    let mut idx = 0;
    while idx < tokens.len() {
        let tt = tokens[idx].clone();
        let next = tokens.get(idx + 1);
        let span = tt.span();
        let gap_space = last_span_end
            .map(|prev| span_gap_requires_space(prev, span.start()))
            .unwrap_or(false);
        match tt {
            TokenTree::Group(g) => {
                if let Some((open, close)) = delim_pair(g.delimiter()) {
                    match g.delimiter() {
                        Delimiter::Brace => {
                            if line.trim_end().ends_with('$') {
                                push_fragment(line, &open.to_string(), gap_space);
                                *last_was_command = false;
                                let mut inner_span_end = None;
                                walk(
                                    g.stream(),
                                    line,
                                    lines,
                                    last_was_command,
                                    true,
                                    capture_has_inner,
                                    &mut inner_span_end,
                                )?;
                                push_fragment(line, &close.to_string(), false);
                            } else if *last_was_command {
                                // Keep the opening brace attached to commands that expect an inner block
                                // (e.g., WITH_IO block form), then break to a new line for the body.
                                push_fragment(line, &open.to_string(), gap_space);
                                finalize_line(lines, line, capture_has_inner);
                                *last_was_command = false;
                                let mut inner_span_end = None;
                                walk(
                                    g.stream(),
                                    line,
                                    lines,
                                    last_was_command,
                                    false,
                                    capture_has_inner,
                                    &mut inner_span_end,
                                )?;
                                finalize_line(lines, line, capture_has_inner);
                                push_fragment(line, &close.to_string(), false);
                                finalize_line(lines, line, capture_has_inner);
                            } else {
                                // `{{ ... }}` template placeholder: Rust lexes
                                // this as a brace group whose stream is a single
                                // nested brace group. Emit both braces literally
                                // on the current line so the string DSL sees the
                                // templated_arg shape it expects.
                                let mut inner_tokens = g.stream().into_iter();
                                let nested_is_template = matches!(
                                    inner_tokens.next(),
                                    Some(TokenTree::Group(inner))
                                        if inner.delimiter() == Delimiter::Brace
                                            && inner_tokens.next().is_none()
                                );
                                if nested_is_template {
                                    let Some(TokenTree::Group(inner)) =
                                        g.stream().into_iter().next()
                                    else {
                                        unreachable!("nested_is_template checked above")
                                    };
                                    push_fragment(line, "{{", gap_space);

                                    // Interior spacing is reconstructed
                                    // explicitly on both sides: exactly two
                                    // braces separate real content from the
                                    // outer delimiters, so a larger offset on
                                    // either side means source whitespace.
                                    let mut inner_tokens = inner.stream().into_iter();
                                    let leading_space = inner_tokens
                                        .next()
                                        .map(|tt| {
                                            let start = tt.span().start();
                                            start.line == span.start().line
                                                && start.column > span.start().column + 2
                                        })
                                        .unwrap_or(false);
                                    if leading_space {
                                        line.push(' ');
                                    }
                                    let mut inner_span_end = None;
                                    walk(
                                        inner.stream(),
                                        line,
                                        lines,
                                        last_was_command,
                                        in_interpolation,
                                        capture_has_inner,
                                        &mut inner_span_end,
                                    )?;
                                    let trailing_space = inner_span_end
                                        .map(|end| {
                                            span.end().line == end.line
                                                && span.end().column > end.column + 2
                                        })
                                        .unwrap_or(false);
                                    let close_text = if trailing_space { " }}" } else { "}}" };
                                    push_fragment(line, close_text, false);
                                    *last_span_end = Some(span.end());
                                } else {
                                    finalize_line(lines, line, capture_has_inner);
                                    line.push(open);
                                    finalize_line(lines, line, capture_has_inner);
                                    *last_was_command = false;
                                    let mut inner_span_end = None;
                                    walk(
                                        g.stream(),
                                        line,
                                        lines,
                                        last_was_command,
                                        false,
                                        capture_has_inner,
                                        &mut inner_span_end,
                                    )?;
                                    finalize_line(lines, line, capture_has_inner);
                                    line.push(close);
                                    finalize_line(lines, line, capture_has_inner);
                                    *last_was_command = false;
                                }
                            }
                        }
                        Delimiter::Bracket => {
                            if *last_was_command {
                                // Keep bracketed flags (e.g., WITH_IO [...] or guard lists) on the same line
                                // when they immediately follow a command token. This avoids rendering a
                                // newline between the command and its bracket payload, which the string DSL
                                // parser would reject.
                                push_fragment(line, &open.to_string(), gap_space);
                                let mut inner_span_end = None;
                                walk(
                                    g.stream(),
                                    line,
                                    lines,
                                    last_was_command,
                                    false,
                                    capture_has_inner,
                                    &mut inner_span_end,
                                )?;
                                push_fragment(line, &close.to_string(), false);
                                *last_was_command = true;
                            } else {
                                finalize_line(lines, line, capture_has_inner);
                                push_fragment(line, &open.to_string(), gap_space);
                                finalize_line(lines, line, capture_has_inner);
                                let mut inner_span_end = None;
                                walk(
                                    g.stream(),
                                    line,
                                    lines,
                                    last_was_command,
                                    false,
                                    capture_has_inner,
                                    &mut inner_span_end,
                                )?;
                                finalize_line(lines, line, capture_has_inner);
                                push_fragment(line, &close.to_string(), false);
                                finalize_line(lines, line, capture_has_inner);
                            }
                        }
                        _ => {
                            push_fragment(line, &open.to_string(), *last_was_command || gap_space);
                            *last_was_command = false;
                            let mut inner_span_end = None;
                            walk(
                                g.stream(),
                                line,
                                lines,
                                last_was_command,
                                in_interpolation,
                                capture_has_inner,
                                &mut inner_span_end,
                            )?;
                            push_fragment(line, &close.to_string(), *last_was_command);
                        }
                    }
                } else {
                    let mut inner_span_end = None;
                    walk(
                        g.stream(),
                        line,
                        lines,
                        last_was_command,
                        in_interpolation,
                        capture_has_inner,
                        &mut inner_span_end,
                    )?;
                }
                *last_span_end = Some(span.end());
            }
            TokenTree::Literal(lit) => {
                push_fragment(line, &lit.to_string(), *last_was_command || gap_space);
                *last_was_command = false;
                *last_span_end = Some(span.end());
            }
            TokenTree::Punct(p) => {
                let ch = p.as_char();
                let mut force_space = gap_space || (*last_was_command && ch != ';');
                if ch == '-'
                    && p.spacing() == Spacing::Alone
                    && line_is_run_context(line)
                    && matches!(next, Some(TokenTree::Ident(_) | TokenTree::Literal(_)))
                    && let Some(prev) = line.chars().rev().find(|c| !c.is_whitespace())
                    && (prev.is_ascii_alphanumeric() || matches!(prev, ')' | ']' | '"' | '\''))
                {
                    force_space = true;
                }
                push_fragment(line, &ch.to_string(), force_space);
                *last_was_command = false;
                *last_span_end = Some(span.end());
                if ch == ';' {
                    finalize_line(lines, line, capture_has_inner);
                }
            }
            TokenTree::Ident(ident) => {
                let ident_text = ident.to_string();
                if in_interpolation {
                    push_fragment(line, &ident_text, false);
                    *last_was_command = false;
                    idx += 1;
                    continue;
                }
                let is_command = super::Command::parse(&ident_text).is_some();
                let trimmed = line.trim();
                let trimmed_empty = trimmed.is_empty();
                let guard_prefix = trimmed.starts_with('[');
                let line_requires_inner = line_expects_inner_command(trimmed);
                let mut should_finalize = false;
                if is_command && !trimmed_empty && !guard_prefix {
                    let current_expects_inner = line_expects_inner_command(trimmed);
                    should_finalize = !line_is_run_context(trimmed) && !current_expects_inner;
                }
                if is_command
                    && !trimmed_empty
                    && !guard_prefix
                    && *capture_has_inner
                    && line_requires_inner
                {
                    finalize_line(lines, line, capture_has_inner);
                }
                if should_finalize {
                    finalize_line(lines, line, capture_has_inner);
                }
                push_fragment(line, &ident_text, *last_was_command || gap_space);
                if is_command
                    && line_expects_inner_command(line)
                    && !matches!(
                        Command::parse(&ident_text),
                        Some(cmd) if cmd.expects_inner_command()
                    )
                {
                    *capture_has_inner = true;
                }
                *last_was_command = is_command;
                *last_span_end = Some(span.end());
            }
        }
        idx += 1;
    }
    Ok(())
}

/// Convert a braced Rust token stream into textual DSL lines.
pub fn script_from_braced_tokens(ts: &TokenStream2) -> Result<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut last_was_command = false;
    let mut capture_has_inner = false;
    let mut last_span_end = None;
    walk(
        ts.clone(),
        &mut current,
        &mut lines,
        &mut last_was_command,
        false,
        &mut capture_has_inner,
        &mut last_span_end,
    )?;
    finalize_line(&mut lines, &mut current, &mut capture_has_inner);
    Ok(lines.join("\n"))
}

/// Parse a braced token stream directly into DSL steps.
pub fn parse_braced_tokens(ts: &TokenStream2) -> Result<Vec<Step>> {
    let script = script_from_braced_tokens(ts)?;
    parse_script(&script)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StepKind;
    use indoc::indoc;
    use quote::quote;

    #[test]
    fn parse_dsl_macro_input_literal_script() {
        let input: DslMacroInput =
            syn::parse_str("name: foo, script: \"RUN echo hi\", out_dir: \"target/out\"")
                .expect("parse literal script");
        assert!(matches!(input.script, ScriptSource::Literal(_)));
        assert_eq!(input.name.to_string(), "foo");
        assert_eq!(input.out_dir.value(), "target/out");
    }

    #[test]
    fn parse_dsl_macro_input_braced_script() {
        let input: DslMacroInput =
            syn::parse_str("name: foo, script: { RUN echo hi }, out_dir: \"out\"")
                .expect("parse braced script");
        assert!(matches!(input.script, ScriptSource::Braced(_)));
    }

    #[test]
    fn braced_script_preserves_dot_path_spacing() {
        // Parsed from real text so span-column gaps drive spacing decisions,
        // exactly like the historical proc-macro input pathway.
        let ts: proc_macro2::TokenStream = "SYMLINK ./client ./client".parse().expect("tokens");
        let script = script_from_braced_tokens(&ts).expect("render braced script");
        assert!(
            script.contains("SYMLINK ./client ./client"),
            "expected dot paths separated, got: {script}"
        );
    }

    #[test]
    fn braced_script_splits_semicolon_commands() {
        let ts = quote! { RUN echo; LS; RUN echo && ls };
        let script = script_from_braced_tokens(&ts).expect("render braced script");
        assert!(script.lines().count() >= 3, "got: {script}");
    }

    #[test]
    fn braced_script_with_guard_block_parses() {
        let ts = quote! {
            [env:TEST_SCOPE] {
                WRITE inner.txt inside
            }
        };
        let steps = parse_braced_tokens(&ts).expect("parse guarded block");
        assert_eq!(steps.len(), 1);
    }

    #[test]
    fn braced_script_preserves_template_placeholders() {
        // `{{ env:X }}` nests as brace-within-brace in the token stream; the
        // normalizer must re-emit it verbatim instead of exploding it across
        // lines.
        let ts: proc_macro2::TokenStream = "WRITE dist/hello.txt Built with {{ env:PROJECT }}"
            .parse()
            .expect("tokens");
        let script = script_from_braced_tokens(&ts).expect("render braced script");
        assert_eq!(
            script, "WRITE dist/hello.txt Built with {{ env:PROJECT }}",
            "template placeholder must round-trip"
        );

        let steps = parse_braced_tokens(&ts).expect("parse templated script");
        match &steps[0].kind {
            StepKind::Write { path, contents } => {
                assert_eq!(path.as_ref(), "dist/hello.txt");
                assert_eq!(
                    contents.as_ref().map(AsRef::as_ref),
                    Some("Built with {{ env:PROJECT }}")
                );
            }
            other => panic!("expected WRITE, saw {:?}", other),
        }
    }

    #[test]
    fn braced_and_string_forms_agree_on_templates() {
        let text = indoc! {r#"
            ENV GREETING=hello
            ECHO <{{ env:GREETING }}>
        "#}
        .trim();
        let ts: proc_macro2::TokenStream = text.parse().expect("tokens");
        let braced = parse_braced_tokens(&ts).expect("braced parse");
        let string = parse_script(text).expect("string parse");
        assert_eq!(braced, string, "template AST parity between forms");
    }

    #[test]
    fn braced_template_spacing_round_trips_both_variants() {
        for source in [
            "WRITE f.txt Built with {{ env:P }}",
            "WRITE f.txt Built with {{env:P}}",
        ] {
            let ts: proc_macro2::TokenStream = source.parse().expect("tokens");
            let script = script_from_braced_tokens(&ts)
                .unwrap_or_else(|e| panic!("render failed for {source}: {e}"));
            assert_eq!(script, source, "spacing must round-trip verbatim");

            let steps = parse_braced_tokens(&ts).expect("parse");
            match &steps[0].kind {
                StepKind::Write { contents, .. } => {
                    assert_eq!(
                        contents.as_ref().map(AsRef::as_ref),
                        Some(source.strip_prefix("WRITE f.txt ").expect("prefix")),
                        "AST interior must match source for {source}"
                    );
                }
                other => panic!("expected WRITE, saw {:?}", other),
            }
        }
    }
}
