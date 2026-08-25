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
    use quote::quote;

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
}
