//! Unified parser backend: [`Tok`] stream to [`Step`].
//!
//! Built parallel to `parser.rs` (pest `Pair` based) per the cutover
//! plan. The old path stays live until this backend passes the corpus
//! and fuzz suites natively; then pest goes away.
//!
//! Statement boundaries resolve here from grammar state, never in the
//! lexers: groups arrive structurally atomic, so at each nesting level
//! a `Newline`/`Semicolon` ends the statement. Continuation exceptions
//! (trailing `=`, `IN`, trailing `ASYNC`, bare wrapper heads) land with
//! their structural slices; the decision point is
//! [`boundary_ends_statement`].
//!
//! Slice 1 covers generic commands only (`ECHO`, `WRITE`, `CWD`,
//! `ENV`, `RUN`, ...): head word plus gap-split args assembled exactly
//! like `parse_fragments` (single quoted string keeps `quoted=true`,
//! multi-fragment joins with space-or-empty by the `joint` flag).
//! Deferred with explicit errors, not silence: guards, structural
//! keywords (`LET`/`IF`/`FOR`/...), wrappers (`WITH_IO`/`ASYNC`/
//! `TIMEOUT`/`ISOLATED`), and groups inside args (calls, lists,
//! templates, blocks).

use crate::ast::{Arg, Step, StepKind};
use crate::error::ParseResult;
use crate::tok::{GroupDelim, Tok, TokKind};
use std::fmt;

/// Backend error. Carries the statement index and message; mapped to
/// [`crate::ParseError`] at cutover. Lowering failures from the shared
/// `lower` dispatcher stringify through here so success paths compare
/// exactly while v2 error text is still its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2Error {
    pub stmt: usize,
    pub msg: String,
}

impl fmt::Display for V2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "parse_v2 error in statement {}: {}",
            self.stmt + 1,
            self.msg
        )
    }
}

impl std::error::Error for V2Error {}

/// One split statement: leading bracket groups (guard region, slice 2)
/// plus the body tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stmt {
    guard: Vec<Tok>,
    body: Vec<Tok>,
}

/// Grammar-state boundary decision: does this `Newline`/`Semicolon`
/// end the statement? Groups are atomic, so every statement fits one
/// logical token line; structural heads error as unsupported (their
/// continuation rules arrive with them) and generic commands always
/// end here. `RUN` payloads are single-line by grammar, so they split
/// too: the gluing bugs of the text rebuild cannot reappear structurally.
fn boundary_ends_statement(body: &[Tok]) -> Result<bool, String> {
    let head = match body.first() {
        None => return Ok(true),
        Some(t) => t,
    };
    let name = match &head.kind {
        TokKind::Word(n) => n.clone(),
        _ => return Err("statement must open with a command word".to_string()),
    };
    if crate::STRUCTURAL_KEYWORDS.contains(&name.as_str()) {
        return Err(format!("unsupported in parse_v2 slice 1: `{name}`"));
    }
    match crate::Command::parse(&name) {
        None => Ok(true),
        Some(cmd) if cmd.expects_inner_command() => {
            Err(format!("unsupported in parse_v2 slice 1: wrapper `{name}`"))
        }
        Some(_) => Ok(true),
    }
}

/// Split top-level tokens into statements. Leading `[..]` groups attach
/// to the following statement as its guard region; `Newline`/`Semicolon`
/// end statements per [`boundary_ends_statement`]. Empty statements
/// (blank lines, stray `;`) drop, matching the `blank | semicolon`
/// pest elements.
fn split_statements(toks: &[Tok]) -> Result<Vec<Stmt>, V2Error> {
    let mut stmts = Vec::new();
    let mut guard = Vec::new();
    let mut body: Vec<Tok> = Vec::new();
    let mut stmt_idx = 0;
    let mut flush = |guard: &mut Vec<Tok>, body: &mut Vec<Tok>, stmt_idx: &mut usize| {
        if body.is_empty() {
            guard.clear();
            return;
        }
        stmts.push(Stmt {
            guard: std::mem::take(guard),
            body: std::mem::take(body),
        });
        *stmt_idx += 1;
    };
    for tok in toks {
        match &tok.kind {
            TokKind::Newline | TokKind::Semicolon => {
                if body.is_empty() {
                    continue;
                }
                match boundary_ends_statement(&body) {
                    Ok(true) => flush(&mut guard, &mut body, &mut stmt_idx),
                    Ok(false) => {}
                    Err(msg) => {
                        return Err(V2Error {
                            stmt: stmt_idx,
                            msg,
                        });
                    }
                }
            }
            TokKind::Group { delim, .. } if body.is_empty() && *delim == GroupDelim::Bracket => {
                guard.push(tok.clone());
            }
            _ => body.push(tok.clone()),
        }
    }
    if !body.is_empty() {
        match boundary_ends_statement(&body) {
            Ok(_) => flush(&mut guard, &mut body, &mut stmt_idx),
            Err(msg) => {
                return Err(V2Error {
                    stmt: stmt_idx,
                    msg,
                });
            }
        }
    }
    Ok(stmts)
}

/// Render one arg run (gap-delimited token run) mirroring
/// `parse_fragments` + the `Arg::String` quoting rule: a lone quoted
/// string keeps `quoted=true`; anything else concatenates (gap gives a
/// space, joint glues) as unquoted. Contents stay verbatim for runtime
/// `expand_string`; `"""` bodies lower through the shared
/// `lower_block_string`.
fn assemble_arg(run: &[Tok], stmt: usize) -> Result<Arg, V2Error> {
    let unsupported = |what: &str| V2Error {
        stmt,
        msg: format!("unsupported in parse_v2 slice 1: {what}"),
    };
    if run.len() == 1 {
        match &run[0].kind {
            TokKind::Str(s) => return Ok(Arg::String(s.clone(), true)),
            TokKind::BlockStr(full) => {
                return Ok(Arg::String(crate::command::lower_block_string(full), true));
            }
            _ => {}
        }
    }
    // Lone `$var` is a variable expression, matching the pest arg rule;
    // longer `$`-led runs (paths, templates) arrive with later slices.
    if run.len() == 2
        && let (TokKind::Punct('$'), TokKind::Word(name)) = (&run[0].kind, &run[1].kind)
    {
        return Ok(Arg::Expr(crate::Expr::Var(name.clone())));
    }
    let mut out = String::new();
    for tok in run {
        if !tok.joint && !out.is_empty() {
            out.push(' ');
        }
        match &tok.kind {
            TokKind::Word(w) => out.push_str(w),
            TokKind::Punct(c) => {
                if *c == '$' {
                    // A `$` surviving to the render path opens a template
                    // or path the expression slices own: loud, not lossy.
                    return Err(unsupported("dollar-led fragment in args"));
                }
                out.push(*c);
            }
            TokKind::Str(s) => out.push_str(s),
            TokKind::BlockStr(full) => {
                out.push_str(&crate::command::lower_block_string(full));
            }
            TokKind::Group { .. } => return Err(unsupported("group inside args")),
            TokKind::Newline | TokKind::Semicolon => {
                return Err(unsupported("line break inside args"));
            }
        }
    }
    Ok(Arg::String(out, false))
}

/// Lower one statement through the shared command dispatcher.
fn lower_stmt(
    stmt: &Stmt,
    idx: usize,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> Result<Step, V2Error> {
    if !stmt.guard.is_empty() {
        return Err(V2Error {
            stmt: idx,
            msg: "unsupported in parse_v2 slice 1: guards".to_string(),
        });
    }
    let name = match stmt.body.first().map(|t| &t.kind) {
        Some(TokKind::Word(n)) => n.clone(),
        _ => {
            return Err(V2Error {
                stmt: idx,
                msg: "statement must open with a command word".to_string(),
            });
        }
    };
    if crate::STRUCTURAL_KEYWORDS.contains(&name.as_str()) {
        return Err(V2Error {
            stmt: idx,
            msg: format!("unsupported in parse_v2 slice 1: `{name}`"),
        });
    }
    match crate::Command::parse(&name) {
        Some(cmd) if cmd.expects_inner_command() => {
            return Err(V2Error {
                stmt: idx,
                msg: format!("unsupported in parse_v2 slice 1: wrapper `{name}`"),
            });
        }
        _ => {}
    }
    // Gap-split args, exactly like pest fragment grouping: a run breaks
    // wherever a gap precedes a token.
    let mut args = Vec::new();
    let mut run: Vec<Tok> = Vec::new();
    for tok in &stmt.body[1..] {
        if !tok.joint && !run.is_empty() {
            args.push(assemble_arg(&run, idx)?);
            run.clear();
        }
        run.push(tok.clone());
    }
    if !run.is_empty() {
        args.push(assemble_arg(&run, idx)?);
    }
    let kind = lower(&name, args).map_err(|e| V2Error {
        stmt: idx,
        msg: e.to_string(),
    })?;
    Ok(Step {
        guard: None,
        kind,
        scope_enter: 0,
        scope_exit: 0,
    })
}

/// Parse a token stream (either lexer) into steps. Markers normalize to
/// 0/0 like the fuzz harness; scopes arrive with block slices.
pub fn parse_v2(
    toks: &[Tok],
    lower: impl Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> Result<Vec<Step>, V2Error> {
    split_statements(toks)?
        .iter()
        .enumerate()
        .map(|(i, s)| lower_stmt(s, i, &lower))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_lower_mock;

    fn lower(name: &str, args: Vec<Arg>) -> ParseResult<StepKind> {
        test_lower_mock::lower(name, args)
    }

    /// Differential oracle: v2 must equal pest on slice-1 scripts,
    /// through both lexers.
    fn assert_parity(text: &str) {
        let old = crate::parse_script(text, lower).expect("pest must parse parity script");
        let toks = crate::tok::lex_str(text).expect("string lex must succeed");
        let new = parse_v2(&toks, lower).expect("v2 must parse parity script");
        assert_eq!(new, old, "v2/string-lex mismatch for {text:?}");
        #[cfg(feature = "proc-macro-api")]
        {
            let ts: proc_macro2::TokenStream = text.parse().expect("must tokenize");
            let toks = crate::tok::lex_tokens(&ts).expect("token lex must succeed");
            let new = parse_v2(&toks, lower).expect("v2 must parse token script");
            assert_eq!(new, old, "v2/token-lex mismatch for {text:?}");
        }
    }

    #[test]
    fn v2_matches_pest_on_flat_commands() {
        for text in [
            "ECHO hi",
            "ECHO hi\nWRITE x out",
            "CWD",
            "ECHO \"quoted\" unquoted",
            "ENV COVERAGE=covered",
            "ECHO a; ECHO b",
            "// comment\nECHO hi",
            "ECHO 1.0.0 --flag a/b",
            "ECHO $x",
            "\n\nECHO hi\n\n",
        ] {
            assert_parity(text);
        }
    }

    #[cfg(feature = "proc-macro-api")]
    #[test]
    fn backslash_outside_strings_is_string_only() {
        // `rustc` rejects a bare `\`, so unquoted backslash escapes
        // cannot cross `TokenStream` (same exclusion class as raw-`"`
        // blocks): the string lexer keeps them, the token path fails to
        // tokenize before v2 is ever reached.
        let toks = crate::tok::lex_str("ECHO a\\nb").expect("string lex keeps escapes");
        let old = crate::parse_script("ECHO a\\nb", lower).expect("pest parses it");
        assert_eq!(parse_v2(&toks, lower).expect("v2 parses it"), old);
        assert!("ECHO a\\nb".parse::<proc_macro2::TokenStream>().is_err());
    }

    #[test]
    fn v2_rejects_slices_not_yet_built() {
        for text in [
            "LET $x: INT = 1",
            "[env:X] ECHO hi",
            "WITH_IO [stdout=$p] ECHO hi",
            "IF true { ECHO hi }",
        ] {
            let toks = crate::tok::lex_str(text).expect("lex must succeed");
            assert!(
                parse_v2(&toks, lower).is_err(),
                "v2 must explicitly reject (not silently drift): {text:?}"
            );
        }
    }
}
