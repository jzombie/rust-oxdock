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

use super::constants::MODULE_SEPARATOR;
use super::{Command, Step, parse_script};
use anyhow::Result;
use proc_macro2::{Delimiter, LineColumn, Spacing, TokenStream as TokenStream2, TokenTree};
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitStr, Token};

/// Parsed macro arguments for `oxdock_embed!` and `oxdock_prepare!`.
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

/// True for UPPERCASE function heads (`GREET`, `LOAD_TOML`): the token-level
/// mirror of the `func_call_head` grammar rule.
fn is_upper_func_head(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_ascii_uppercase() => (),
        _ => return false,
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// True for a call-head token, plain (`GREET`) or module-qualified
/// (`MOCK::READ_CSV`): every `::` segment is non-empty, leading segments
/// are UPPERCASE, and the tail is identifier-shaped. The tail stays
/// case-open so `STD::glob(` attaches contiguously and fails at lowering
/// with a span-accurate UPPERCASE error; the grammar still rejects it.
/// Lets the `(` attach contiguously so the string grammar routes `M::F(...)`
/// to `call_statement`; deeper paths (`A::B::F`) still fail at parse
/// time.
fn is_call_head_token(token: &str) -> bool {
    match token.rsplit_once(MODULE_SEPARATOR) {
        // Qualified: every leading segment is a non-empty UPPERCASE
        // identifier and the tail is identifier-shaped (case checked at
        // lowering, so `STD::glob(` still attaches and fails with a span).
        Some((head, tail)) => {
            !tail.is_empty()
                && tail.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !head.is_empty()
                && head
                    .split(MODULE_SEPARATOR)
                    .all(|part| !part.is_empty() && is_upper_func_head(part))
        }
        // Plain: exactly the old rule, so bare lowercase heads keep falling
        // through to the unknown-command path with its did-you-mean hint.
        None => is_upper_func_head(token),
    }
}

/// True when the current line ends with a bare-call head: an uppercase
/// identifier that is neither a known command nor a statement keyword.
/// Used to attach `(` contiguously (see the parenthesis group handling).
fn trailing_call_head(line: &str) -> bool {
    let Some(token) = line.split_whitespace().last() else {
        return false;
    };
    is_call_head_token(token)
        && Command::parse(token).is_none()
        && !Command::is_statement_keyword(token)
}

/// Check if a brace group is a `{{ ... }}` template placeholder.
/// Rust lexes `{{ env:KEY }}` as a brace group containing a single nested brace group.
fn is_template_group(g: &proc_macro2::Group) -> bool {
    let mut inner_tokens = g.stream().into_iter();
    matches!(
        inner_tokens.next(),
        Some(TokenTree::Group(inner))
            if inner.delimiter() == Delimiter::Brace && inner_tokens.next().is_none()
    )
}

/// Emit a `{{ ... }}` template placeholder on the current line,
/// reconstructing interior spacing from span positions.
fn emit_template_placeholder(
    g: &proc_macro2::Group,
    line: &mut String,
    span: proc_macro2::Span,
    gap_space: bool,
    last_span_end: &mut Option<LineColumn>,
) {
    let Some(TokenTree::Group(inner)) = g.stream().into_iter().next() else {
        unreachable!("is_template_group checked above")
    };
    push_fragment(line, "{{", gap_space);

    let mut inner_tokens = inner.stream().into_iter();
    let leading_space = inner_tokens
        .next()
        .map(|tt| {
            let start = tt.span().start();
            start.line == span.start().line && start.column > span.start().column + 2
        })
        .unwrap_or(false);
    if leading_space {
        line.push(' ');
    }
    let mut inner_span_end = None;
    let mut last_was_command = false;
    let mut capture_has_inner = false;
    walk(
        inner.stream(),
        line,
        &mut Vec::new(),
        &mut last_was_command,
        false,
        &mut capture_has_inner,
        &mut inner_span_end,
    )
    .ok();
    let trailing_space = inner_span_end
        .map(|end| span.end().line == end.line && span.end().column > end.column + 2)
        .unwrap_or(false);
    let close_text = if trailing_space { " }}" } else { "}}" };
    push_fragment(line, close_text, false);
    *last_span_end = Some(span.end());
}

fn line_expects_inner_command(line: &str) -> bool {
    matches!(
        current_line_command(line),
        Some(cmd) if cmd.expects_inner_command()
    )
}

fn line_is_run_context(line: &str) -> bool {
    matches!(current_line_command(line), Some(Command::Run))
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
        // A RUN step consumes the rest of its source line as shell text
        // (`RUN echo && ls` stays one step), but any token opening on a later
        // line starts a new statement: without this, `RUN echo hi` followed by
        // `WRITE x` : or by a punctuation-led statement like `$count = 1` :
        // would glue into a single shell command. Punctuation must participate
        // too: `$`/`#` would otherwise advance `last_span_end` and blind the
        // check for the tokens that follow them on the same line.
        if last_span_end.is_some_and(|prev| span.start().line > prev.line)
            && !line.trim().is_empty()
            && line_is_run_context(line.trim())
        {
            finalize_line(lines, line, capture_has_inner);
        }
        match tt {
            TokenTree::Group(g) => {
                if let Some((open, close)) = delim_pair(g.delimiter()) {
                    match g.delimiter() {
                        Delimiter::Brace => {
                            let trimmed = line.trim_end();

                            // 1. Interpolation context: ${var} or #{expr}
                            //    Keep the entire expression on one line.
                            if trimmed.ends_with('$') || trimmed.ends_with('#') {
                                push_fragment(line, &open.to_string(), false);
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
                            }
                            // 2. Template placeholder: {{ env:KEY }}
                            //    Rust lexes this as nested brace groups.
                            else if is_template_group(&g) {
                                emit_template_placeholder(&g, line, span, gap_space, last_span_end);
                            }
                            // 3. DSL statement block: WITH_IO [...] {, FOR ..., LET ...
                            //    Attach opening brace to the current line, then walk body.
                            else if !trimmed.is_empty() {
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
                            }
                            // 4. Standalone / top-level block
                            else {
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
                            // A `[` group continues the current statement when it
                            // reads as an expression: argv/bindings right after a
                            // command (`RUN [...]`, `WITH_IO [...]`), or a list
                            // literal after `=`, `IN`, `(`, `[`, `,`. Otherwise
                            // it starts a guard on a new line. This is purely
                            // syntactic so it also holds for synthetic spans
                            // (e.g. `quote!`), where line numbers carry no signal.
                            let trimmed_here = line.trim_end();
                            let continues_expr = trimmed_here.ends_with('=')
                                || trimmed_here.ends_with(':')
                                || trimmed_here.ends_with('(')
                                || trimmed_here.ends_with('[')
                                || trimmed_here.ends_with(',')
                                || trimmed_here.split_whitespace().last() == Some("IN");
                            if *last_was_command || continues_expr {
                                // First bracket group after a command: attach to the command
                                // (e.g., INHERIT_ENV [keys], WITH_IO [bindings]).
                                // Same-line groups elsewhere attach as expressions.
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
                                // Reset so the next bracket group (if any) is recognized as a guard.
                                *last_was_command = false;
                            } else {
                                // Guard bracket (e.g., [#flag], [env:KEY])
                                // or second bracket group after a command.
                                // Finalize previous line and start guard on a new line.
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
                            // A `(` group after an uppercase non-command head
                            // is a bare call: push it contiguously so the
                            // string grammar routes `FOO(...)` to
                            // `call_statement`, except when source spans
                            // show a real gap (`FOO (` stays an instruction).
                            // Commands keep existing spacing (`ECHO (1 + 2)`).
                            // Note: this bypasses `push_fragment`, whose
                            // `needs_space` heuristic would re-insert the
                            // space we are deliberately dropping.
                            let attach_call = open == '('
                                && trailing_call_head(line)
                                && last_span_end
                                    .map(|prev| prev == span.start())
                                    .unwrap_or(true);
                            if attach_call {
                                line.push(open);
                            } else {
                                push_fragment(
                                    line,
                                    &open.to_string(),
                                    *last_was_command || gap_space,
                                );
                            }
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
                // A RUN step consumes the rest of its source line as shell
                // text, but an ident opening on a later line starts a new
                // statement: the hoisted check at the top of the loop already
                // finalized the RUN line, so statement detection below sees a
                // fresh line.
                let is_command = super::Command::parse(&ident_text).is_some();
                // LET and FOR introduce new statements but aren't in the Command enum.
                // They must still trigger line finalization so they start on a new line.
                // The same holds for the other structural statements parsed by PEG
                // rules rather than plain-command lowering (AWAIT, CANCEL, FUNC,
                // RETURN, WHILE, BREAK, CONTINUE): without this, `FUNC`
                // after `MKDIR dist` would glue onto the same line.
                let is_new_statement = super::Command::is_statement_keyword(&ident_text);
                // A bare `NAME(...)` call opens a statement when it starts a
                // new source line with an empty continuation state. This
                // mirrors the string grammar, where statement calls begin
                // lines: `GREET("ada")` after `WRITE a.txt hi` must split,
                // while `ECHO GREET("ada")` (same line, argument position)
                // and `LET $r: STRING = GREET("ada")` (after `=`) stay glued.
                // `FOO (` with a space is not a call (see `dsl.pest`), so a
                // gap between the head and the paren group opts out.
                let is_call_statement_start = !is_command
                    && is_upper_func_head(&ident_text)
                    && matches!(
                        next,
                        Some(TokenTree::Group(g))
                            if g.delimiter() == Delimiter::Parenthesis
                    )
                    && {
                        let head_end = span.end();
                        match next {
                            Some(TokenTree::Group(g)) => {
                                let paren_start = g.span().start();
                                paren_start.line == head_end.line
                                    && paren_start.column == head_end.column
                            }
                            _ => false,
                        }
                    };
                // A qualified `MODULE::NAME(...)` call opens a statement
                // under the same conditions: the head starts at the module
                // identifier, so look ahead over `::`, the name, and the
                // paren group. Without this, `MOCK::READ_CSV(..)` after a
                // complete statement glues onto its line (the module ident
                // alone matches neither the keyword nor the bare-call rule).
                let is_qualified_call_start = is_upper_func_head(&ident_text)
                    && matches!(next, Some(TokenTree::Punct(p)) if p.as_char() == ':')
                    && matches!(tokens.get(idx + 2), Some(TokenTree::Punct(p)) if p.as_char() == ':')
                    && matches!(tokens.get(idx + 3), Some(TokenTree::Ident(_)))
                    && matches!(
                        tokens.get(idx + 4),
                        Some(TokenTree::Group(g))
                            if g.delimiter() == Delimiter::Parenthesis
                    );
                let trimmed = line.trim();
                let trimmed_empty = trimmed.is_empty();
                let guard_prefix = trimmed.starts_with('[');
                let line_requires_inner = line_expects_inner_command(trimmed);
                // A line ending in `=` (or the `IN` of a FOR header) expects an
                // expression next: `LET $o: STRING = ECHO hi`,
                // `LET $r: STRING = F()`,
                // `LET $t: HANDLE = ASYNC ...`, `FOR $x: STRING IN [...]`.
                // A statement keyword there continues the line instead of
                // starting a new one.
                let trimmed_end = trimmed.trim_end();
                let expects_expr = trimmed_end.ends_with('=')
                    || trimmed_end.split_whitespace().last() == Some("IN");
                let mut should_finalize = false;
                // ELSE always appends to current line : grammar handles } \n ELSE via blank*
                // IF after ELSE stays on same line (ELSE IF clause)
                if ident_text == "ELSE" || (ident_text == "IF" && trimmed.ends_with("ELSE")) {
                    should_finalize = false;
                } else if is_new_statement && !trimmed_empty && !guard_prefix && !expects_expr {
                    let current_expects_inner = line_expects_inner_command(trimmed);
                    should_finalize = !line_is_run_context(trimmed) && !current_expects_inner;
                }
                // Bare and qualified calls split like keywords when they open
                // a new source line after a complete statement. Same-line
                // occurrences (command arguments, parenthesized groups)
                // attach instead. A head continuing a `MODULE::` qualifier
                // never splits: `MOCK::` plus `READ_CSV(` is one call head
                // (the split, if any, already happened at the module ident).
                if (is_call_statement_start || is_qualified_call_start)
                    && !trimmed_empty
                    && !guard_prefix
                    && !expects_expr
                    && !line_is_run_context(trimmed)
                    && !trimmed_end.ends_with(MODULE_SEPARATOR)
                    && last_span_end.is_some_and(|prev| span.start().line > prev.line)
                {
                    should_finalize = true;
                }
                if is_command
                    && !trimmed_empty
                    && !guard_prefix
                    && *capture_has_inner
                    && line_requires_inner
                    && ident_text != "ASYNC"
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
                *last_was_command = is_new_statement;
                *last_span_end = Some(span.end());
            }
        }
        idx += 1;
    }
    Ok(())
}

/// Split an optional leading `modules: [A, B]` prefix off a braced macro
/// token stream. The names declare opaque modules (membership unknown at
/// compile time) for the `oxdock!` parse; the remainder walks as DSL.
/// Returns no names when the stream opens with anything else, so plain
/// scripts pass through untouched.
pub fn split_modules_prefix(ts: &TokenStream2) -> Result<(Vec<String>, TokenStream2)> {
    use std::iter::FromIterator;
    let mut tokens: Vec<TokenTree> = ts.clone().into_iter().collect();
    let prefix = match tokens.as_slice() {
        [
            TokenTree::Ident(head),
            TokenTree::Punct(colon),
            TokenTree::Group(list),
            ..,
        ] if head == "modules"
            && colon.as_char() == ':'
            && list.delimiter() == Delimiter::Bracket =>
        {
            let mut names = Vec::new();
            for item in list.stream().into_iter() {
                match item {
                    TokenTree::Ident(name) => names.push(name.to_string()),
                    TokenTree::Punct(comma) if comma.as_char() == ',' => {}
                    other => {
                        anyhow::bail!("modules: prefix holds module names, found `{other}`");
                    }
                }
            }
            names
        }
        _ => return Ok((Vec::new(), ts.clone())),
    };
    tokens.drain(..3);
    // An optional comma separates the prefix from the script body.
    if let Some(TokenTree::Punct(comma)) = tokens.first()
        && comma.as_char() == ','
    {
        tokens.drain(..1);
    }
    Ok((prefix, TokenStream2::from_iter(tokens)))
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
/// Requires a lowering function : callers must provide it.
pub fn parse_braced_tokens(
    ts: &TokenStream2,
    lower: impl Fn(&str, Vec<crate::Arg>) -> crate::ParseResult<crate::StepKind>,
) -> Result<Vec<Step>> {
    let script = script_from_braced_tokens(ts)?;
    // The string rebuild loses per token positions. Forward the head
    // token coordinates so failures still carry a macro position; the
    // caller keeps its own span handle for `syn::Error` squiggles.
    let head_ctx = ts.clone().into_iter().next().map(|t| {
        let start = t.span().start();
        crate::SpanContext::from_compiler_span(
            start.line.max(1),
            Some(start.column.saturating_add(1)),
            Some(start.column.saturating_add(1)),
            t.span(),
        )
    });
    parse_script(&script, lower).map_err(|e| {
        let e = match &head_ctx {
            Some(ctx) => e.with_span(ctx),
            None => e,
        };
        anyhow::Error::from(e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StepKind;
    use indoc::indoc;
    use quote::quote;

    /// Mock lowering for macro_input tests.
    fn mock_lower(name: &str, args: Vec<crate::Arg>) -> crate::ParseResult<StepKind> {
        crate::test_lower_mock::lower(name, args)
    }

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
        let steps = parse_braced_tokens(&ts, mock_lower).expect("parse guarded block");
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

        let steps = parse_braced_tokens(&ts, mock_lower).expect("parse templated script");
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
        let braced = parse_braced_tokens(&ts, mock_lower).expect("braced parse");
        let string = parse_script(text, mock_lower).expect("string parse");
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

            let steps = parse_braced_tokens(&ts, mock_lower).expect("parse");
            match &steps[0].kind {
                StepKind::Write { path, contents } => {
                    assert_eq!(path.as_ref(), "f.txt", "path must match for {source}");
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

    #[test]
    fn braced_structural_statements_start_new_lines() {
        // FUNC, bare calls, WHILE (and friends) are parsed by PEG rules
        // rather than plain-command lowering, so the token walker must still
        // recognize them as statement starters instead of gluing them onto
        // the previous line.
        let ts: proc_macro2::TokenStream = indoc! {r#"
            WRITE a.txt hi
            FUNC GREET($name: STRING) {
                RETURN $name
            }
            GREET("ada")
            WHILE $flag {
                BREAK
            }
        "#}
        .parse()
        .expect("tokens");
        let steps = parse_braced_tokens(&ts, mock_lower).expect("structural statements parse");
        assert_eq!(steps.len(), 4, "got: {steps:?}");
        assert!(matches!(steps[0].kind, StepKind::Write { .. }));
        assert!(matches!(steps[1].kind, StepKind::FuncDef { .. }));
        assert!(matches!(steps[2].kind, StepKind::Call { .. }));
        assert!(matches!(steps[3].kind, StepKind::While { .. }));
    }

    #[test]
    fn braced_expression_continuations_stay_on_one_line() {
        // A statement keyword after `=` or `IN` continues the line: LET-capture
        // (`= ECHO ...`, `= F(...)`) and list literals (`= [...]`,
        // `IN [...]`) must not split.
        let ts: proc_macro2::TokenStream = indoc! {r#"
            LET $names: LIST = ["alpha", "beta"]
            FOR $n: STRING IN ["alpha", "beta"] {
                WRITE out.txt hi
            }
            LET $echo: STRING = ECHO hi
        "#}
        .parse()
        .expect("tokens");
        let steps = parse_braced_tokens(&ts, mock_lower).expect("continuations parse");
        assert_eq!(steps.len(), 3, "got: {steps:?}");
        assert!(matches!(steps[0].kind, StepKind::Assign { .. }));
        assert!(matches!(steps[1].kind, StepKind::For { .. }));
        assert!(matches!(steps[2].kind, StepKind::AssignCapture { .. }));
    }

    #[test]
    fn braced_map_literal_with_list_value_parses() {
        // A list literal after a map entry colon (`{ key: [...] }`) is an
        // expression fragment, not a guard: it must not split onto a new line.
        let ts: proc_macro2::TokenStream = indoc! {r#"
            LET $m: MAP = {"key": ["a", "b"]}
        "#}
        .parse()
        .expect("tokens");
        let steps = parse_braced_tokens(&ts, mock_lower).expect("map literal parses");
        assert_eq!(steps.len(), 1, "got: {steps:?}");
        assert!(matches!(steps[0].kind, StepKind::Assign { .. }));
    }

    #[test]
    fn braced_run_ends_at_source_line_break() {
        // Shell text stays on one step (`RUN echo && ls`), but a step opening
        // on a later source line must not glue onto the RUN command.
        let ts: proc_macro2::TokenStream = indoc! {r#"
            RUN echo hi
            WRITE out.txt hi
            RUN echo again
        "#}
        .parse()
        .expect("tokens");
        let steps = parse_braced_tokens(&ts, mock_lower).expect("run lines parse");
        assert_eq!(steps.len(), 3, "got: {steps:?}");
    }

    #[test]
    fn braced_run_followed_by_mutation_or_interpolation_ends_line() {
        // Punctuation-led statements (`$count = 1`, `#cmd`) open with `$`/`#`,
        // not an Ident: the RUN line break must fire for any token type, or the
        // `$` would merely advance the span cursor and blind the check for the
        // tokens that follow it on the same line. (`#cmd` is a DSL comment, so
        // only the RUN and the mutation survive as steps.)
        let ts: proc_macro2::TokenStream = indoc! {r#"
            RUN echo hi
            $count = 1
            #cmd
        "#}
        .parse()
        .expect("tokens");
        let steps = parse_braced_tokens(&ts, mock_lower).expect("post-RUN lines parse");
        assert_eq!(steps.len(), 2, "got: {steps:?}");
        assert!(matches!(steps[0].kind, StepKind::Run(_)));
        assert!(matches!(steps[1].kind, StepKind::Set { .. }));
    }

    #[test]
    fn braced_guards_still_start_new_lines() {
        // A `[` group on a fresh line is a guard, even though same-line
        // brackets attach as expressions.
        let ts: proc_macro2::TokenStream = indoc! {r#"
            WRITE a.txt hi
            [bool:true] WRITE b.txt yo
        "#}
        .parse()
        .expect("tokens");
        let steps = parse_braced_tokens(&ts, mock_lower).expect("guarded lines parse");
        assert_eq!(steps.len(), 2, "got: {steps:?}");
        assert!(matches!(steps[0].kind, StepKind::Write { .. }));
        assert!(matches!(steps[1].kind, StepKind::Write { .. }));
    }

    #[test]
    fn qualified_call_never_splits_at_double_colon() {
        // `MOCK::READ_CSV(...)` lexes as Ident Punct Punct Ident Group:
        // the statement split happens at the module ident (own line), and
        // the walker must not split again between `::` and the name.
        // Intra-line spacing is cosmetic; line structure is what's pinned.
        // Real spans required: `quote!` stamps every token line 1, which
        // disables line-driven splitting by design.
        let ts: proc_macro2::TokenStream = indoc! {r#"
            WRITE a.txt hi
            MOCK::READ_CSV("a")
        "#}
        .parse()
        .expect("tokens");
        let script = script_from_braced_tokens(&ts).expect("render braced script");
        let lines: Vec<&str> = script.lines().collect();
        assert_eq!(lines.len(), 2, "got: {script}");
        assert_eq!(lines[0], "WRITE a.txt hi", "got: {script}");
        assert_eq!(
            lines[1].replace(' ', ""),
            "MOCK::READ_CSV(\"a\")",
            "got: {script}"
        );
    }

    #[test]
    fn qualified_call_in_expression_position_stays_glued() {
        let ts = quote! {
            LET $x: STRING = STD::INT("2")
        };
        let script = script_from_braced_tokens(&ts).expect("render braced script");
        assert_eq!(
            script.replace(' ', ""),
            "LET$x:STRING=STD::INT(\"2\")",
            "got: {script}"
        );
    }

    #[test]
    fn modules_prefix_splits_off_and_leaves_script() {
        let ts = quote! {
            modules: [DOCS]
            IMPORT [STD, DOCS]
        };
        let (names, rest) = split_modules_prefix(&ts).expect("split prefix");
        assert_eq!(names, vec!["DOCS".to_string()]);
        let script = script_from_braced_tokens(&rest).expect("render rest");
        assert_eq!(script.replace(' ', ""), "IMPORT[STD,DOCS]", "got: {script}");
    }

    #[test]
    fn modules_prefix_absent_passes_through() {
        let ts = quote! {
            WRITE a.txt hi
        };
        let (names, rest) = split_modules_prefix(&ts).expect("split prefix");
        assert!(names.is_empty());
        let script = script_from_braced_tokens(&rest).expect("render rest");
        assert_eq!(script.trim(), "WRITE a.txt hi", "got: {script}");
    }

    #[test]
    fn modules_prefix_rejects_non_ident_entries() {
        let ts = quote! {
            modules: [42]
        };
        assert!(split_modules_prefix(&ts).is_err());
    }
}
