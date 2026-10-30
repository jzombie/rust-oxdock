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
//! Deferred with explicit errors, not silence: structural keywords
//! (`LET`/`IF`/`FOR`/...) land in slice 3, wrappers (`WITH_IO`/`ASYNC`/
//! `TIMEOUT`/`ISOLATED`) and calls in slice 4, blocks in slice 5.
//!
//! Slice 2 implements guards: leading `[..]` groups parse to
//! [`GuardExpr`] (predicates, `not`/`any`/`all`, groups, comma seqs,
//! stacked brackets combining with `All`), and in-body bracket/paren
//! groups splice to raw text for generic args. In-body brackets are
//! args (probed: `ECHO [env:X]` keeps it as an argument); a leading
//! bracket that is not a guard (`[a] ECHO hi`) errors exactly like
//! pest.
//!
//! Slice 3 implements `LET`/`SET`/`RETURN` plus the expression tiers
//! (or/and/compare/ordering/add/mul/unary/atom with lists and maps).
//! Lowering runs through the shared pest constructors, so folding,
//! RPN, and overflow outcomes are identical; only the tier walkers are
//! mirrored. Deferred: calls (slice 4), inline blocks (slice 5).
//!
//! Slice 4a implements calls, RUN-exec, and instruction exactness:
//! static resolution runs through the shared `resolve_call_name`
//! (extracted verbatim from `LowerCtx`, which now delegates); generic
//! arguments classify like `assignment | argument` (assignments merge
//! across gaps to the next boundary, `$`/`env:`-led runs extend as
//! expressions, brace-led runs try templates then maps/blocks,
//! everything else renders raw with quote-accurate splices); and
//! `ENV`/`EXPAND` lower through the shared direct cores instead of
//! the injected dispatcher. `LET` right-hand sides dispatch like the
//! grammar order (wrappers defer, `AWAIT $v` captures, RUN-exec and
//! known commands capture, unknown leads parse as expressions).
//! Deferred: single-command wrappers (slice 4b), blocks (slice 5).
//!
//! Slice 4b implements single-command wrappers, io-blocks,
//! `AWAIT`/`CANCEL`, and multi-element statements: `WITH_IO`
//! bindings (streams, pipes, geometry) with the shared tty check,
//! `TIMEOUT` static/dynamic durations validated through the shared
//! Duration check, `ISOLATED` with the shared inherit rewrite,
//! `ASYNC` with the shared `WITH_IO`-inside scan, all through the
//! shared capture/nesting validators; `LET` wrapper forms
//! (`ASYNC`/`WITH_IO`/`TIMEOUT` right-hand sides) mirror
//! `parse_let_async_statement_from_pair` branch for branch; and
//! statements lower to step vectors (structural pairs end mid-line,
//! trailing elements loop on, fixing two slice-4a divergences:
//! calls and RUN-exec with trailing commands). Bare `WITH_IO [..]`
//! waits for its `{...}` across separators like `pending_io_block`.
//! Deferred: block statements, directives, `REMOTE` (slice 5).
//!
//! TODO(cutover): duplication inventory to unify when pest retires.
//! Each item below mirrors a pest-side rule with identical outcomes
//! pinned by the differential oracle; at cutover the pest twin goes
//! away and the marker goes with it.
//! - `assemble_arg` mirrors `parse_fragments` + the `Arg::String`
//!   quoting rule.
//! - `key_char`/`bare_run`/`value_run` mirror `guard_key_char` and
//!   `bare_guard_value`.
//! - `ns_guard` mirrors `parse_ns_guard` (messages included).
//! - Guard validation strings (`any(...)/all(...)/eq(...)` shapes)
//!   mirror their pest counterparts.
//! - `parse_guard_groups` folds stacked brackets with `All` exactly
//!   like the pest path's pending-guard stash.
//! - The expression tiers mirror the pest precedence chain; only the
//!   lowering half is truly shared (`make_arith`, `make_compare`,
//!   `apply_unary_neg`, `reject_boundary`, `classify_numeric_text`,
//!   `canonicalize_spelling`, `lower_block_string`).
//! - Static call resolution is truly shared (`resolve_call_name`,
//!   `check_func_ident` in `resolve.rs`; `LowerCtx` delegates).
//! - Instruction tokens are truly shared (`InsToken`,
//!   `lower_env_tokens`, `lower_expand_tokens`,
//!   `canonical_assignment_arg` in `commands.rs`); only the
//!   token-level classifiers (key runs, boundaries, call shapes,
//!   expression extension) are v2-native mirrors.
//! - Numeric `_`-separator shape gating mirrors the `numeric_literal`
//!   trailing guard; bare-word charsets mirror `bare_word`;
//!   `neg_op`/`bare` `-` dispatch mirrors the unary rule.
//! - Wrapper validators are truly shared (`check_tty_wrapping`,
//!   `reject_async_in_capture`, `reject_pipe_stdout_in_capture`,
//!   `rewrite_isolated_inherit`, `contains_inherit_env`,
//!   `reject_nested_inherit_env`, `merge_bindings`, `IoBindingSet`,
//!   `and_guard_exprs`, `reject_with_io_in_async` in `commands.rs`;
//!   the pest backend delegates); only the token-level shape
//!   classifiers (duration gate, binding splitter, await/cancel
//!   shapes, block-vs-template, inherit keys) are v2-native mirrors.
//! - The element loop mirrors the grammar's `element*`: structural
//!   pairs end mid-line and trailing elements form following steps
//!   (guards attach to the first only); the `WITH_IO` pending wait
//!   mirrors `pending_io_block`.

use crate::ast::{
    Arg, ArithOp, CompareOp, Expr, Guard, GuardExpr, IoBinding, IoStream, LogicalOp, ModuleTable,
    Ns, Step, StepKind, Value,
};
use crate::command::ArgType;
use crate::commands::{
    InsToken, canonical_assignment_arg, check_tty_wrapping, is_known_command, lower_env_tokens,
    lower_expand_tokens,
};
use crate::error::{ParseResult, SpanContext};
use crate::parser::{
    apply_unary_neg, classify_numeric_text, make_arith, make_compare, reject_boundary,
};
use crate::resolve::resolve_call_name;
use crate::tok::{GroupDelim, Tok, TokKind};
use std::collections::HashSet;
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
/// logical token line; single-command wrappers and `AWAIT`/`CANCEL`
/// always end here (their brace bodies ride inside group tokens).
/// Generic commands always end here. `RUN` payloads are single-line by
/// grammar, so they split too: the gluing bugs of the text rebuild
/// cannot reappear structurally.
fn boundary_ends_statement(body: &[Tok]) -> Result<bool, String> {
    let head = match body.first() {
        None => return Ok(true),
        Some(t) => t,
    };
    // Slice 3 owns `LET`/`RETURN`/`$var =` heads; slice 4b owns the
    // single-command wrappers plus `AWAIT`/`CANCEL`; anything else
    // structural stays deferred, never guessed.
    match &head.kind {
        TokKind::Word(name)
            if name == "LET" || name == "RETURN" || name == "AWAIT" || name == "CANCEL" =>
        {
            Ok(true)
        }
        TokKind::Punct('$') => Ok(true),
        // A lone leading `{...}` ends here too: the splitter either
        // merges it into a pending `WITH_IO` wait or emits it for
        // the slice-5 block deferral below, never splitting inside.
        TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        } => Ok(true),
        TokKind::Word(name) => {
            // Wrappers end at the newline (brace bodies ride inside
            // groups); `ASYNC` matches here despite also living in
            // `STRUCTURAL_KEYWORDS`. Anything else structural stays
            // deferred, never guessed. The predicate is shared.
            if crate::Command::parse(name).is_some_and(|c| c.expects_inner_command()) {
                return Ok(true);
            }
            if crate::STRUCTURAL_KEYWORDS.contains(&name.as_str()) {
                return Err(format!("unsupported in parse_v2: `{name}` (later slice)"));
            }
            Ok(true)
        }
        _ => Err("statement must open with a command word".to_string()),
    }
}

/// Bare `WITH_IO [..]` with no inner command: holds the line for a
/// following `{...}` block (the pest `pending_io_block` mechanism),
/// which merges into one io-block statement. Anything else (or EOF)
/// fails exactly like the grammar's `must be followed by '{'`.
fn is_bare_with_io(body: &[Tok]) -> bool {
    matches!(body.first().map(|t| &t.kind), Some(TokKind::Word(w)) if w == "WITH_IO")
        && matches!(
            body.get(1).map(|t| (&t.kind, t.joint)),
            Some((
                TokKind::Group {
                    delim: GroupDelim::Bracket,
                    ..
                },
                false
            ))
        )
        && body.len() == 2
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
    // Bare `WITH_IO [..]` awaiting its `{...}` block (see
    // `is_bare_with_io`): the guard and body hold across the
    // separator until the brace arrives or the wait fails.
    let mut pending_io: Option<(Vec<Tok>, Vec<Tok>)> = None;
    // Flush a statement, holding bare `WITH_IO [..]` for its block
    // instead of emitting it (the pest `pending_io_block` wait).
    let mut flush_or_hold = |guard: &mut Vec<Tok>,
                             body: &mut Vec<Tok>,
                             stmt_idx: &mut usize,
                             pending_io: &mut Option<(Vec<Tok>, Vec<Tok>)>|
     -> Result<(), V2Error> {
        if body.is_empty() {
            guard.clear();
            return Ok(());
        }
        if is_bare_with_io(body) {
            if pending_io.is_some() {
                return Err(V2Error {
                    stmt: *stmt_idx,
                    msg: "previous WITH_IO block is still waiting for '{'".to_string(),
                });
            }
            *pending_io = Some((std::mem::take(guard), std::mem::take(body)));
            return Ok(());
        }
        if let Some((pending_guard, mut pending_body)) = pending_io.take() {
            // Only a lone guard-less `{...}` group satisfies the wait;
            // anything else (commands, guarded braces) fails like the
            // grammar's `must be followed by '{'`.
            let lone_brace = guard.is_empty()
                && body.len() == 1
                && matches!(
                    body.first().map(|t| &t.kind),
                    Some(TokKind::Group {
                        delim: GroupDelim::Brace,
                        ..
                    })
                );
            if !lone_brace {
                return Err(V2Error {
                    stmt: *stmt_idx,
                    msg: "WITH_IO block must be followed by '{'".to_string(),
                });
            }
            pending_body.extend(std::mem::take(body));
            guard.clear();
            stmts.push(Stmt {
                guard: pending_guard,
                body: pending_body,
            });
            *stmt_idx += 1;
            return Ok(());
        }
        // Ordinary statement: emit it.
        stmts.push(Stmt {
            guard: std::mem::take(guard),
            body: std::mem::take(body),
        });
        *stmt_idx += 1;
        Ok(())
    };
    for tok in toks {
        match &tok.kind {
            TokKind::Newline | TokKind::Semicolon => {
                if body.is_empty() {
                    continue;
                }
                match boundary_ends_statement(&body) {
                    Ok(true) => {
                        flush_or_hold(&mut guard, &mut body, &mut stmt_idx, &mut pending_io)?
                    }
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
            Ok(_) => flush_or_hold(&mut guard, &mut body, &mut stmt_idx, &mut pending_io)?,
            Err(msg) => {
                return Err(V2Error {
                    stmt: stmt_idx,
                    msg,
                });
            }
        }
    } else if !guard.is_empty() {
        // Trailing guards with no following command error (probed:
        // pest rejects a lone `[env:X]`), never drop silently.
        return Err(V2Error {
            stmt: stmt_idx,
            msg: "guard declared without a following command".to_string(),
        });
    }
    if pending_io.is_some() {
        return Err(V2Error {
            stmt: stmt_idx,
            msg: "WITH_IO block must be followed by '{'".to_string(),
        });
    }
    Ok(stmts)
}

/// Splice in-body bracket/paren groups to flat raw text: pest reads
/// arg-position brackets as ordinary characters split purely on
/// whitespace (`ECHO [a, b]` is args `"[a,"` + `"b]"`), so v2 expands
/// each group to open-punct + inner + close-punct and lets the normal
/// gap-split/render reproduce the bytes exactly (probed parity).
/// Recursion handles nesting; edge joints ride along untouched. Brace
/// groups stay put (blocks, never args). Newlines/semicolons spliced
/// from multi-line groups fail loudly in assembly, like pest.
fn splice_arg_groups(body: &[Tok], stmt: usize) -> Result<Vec<Tok>, V2Error> {
    let _ = stmt;
    let mut out = Vec::new();
    for tok in body.iter() {
        match &tok.kind {
            TokKind::Group {
                delim: GroupDelim::Brace,
                ..
            } => out.push(tok.clone()),
            TokKind::Group {
                delim,
                inner,
                close_joint,
                ..
            } => {
                let (open, close) = match delim {
                    GroupDelim::Bracket => ('[', ']'),
                    GroupDelim::Paren => ('(', ')'),
                    GroupDelim::Brace => unreachable!(),
                };
                out.push(Tok::fresh(TokKind::Punct(open), tok.joint));
                // Inside brackets pest reads raw text: quotes are
                // characters, blocks never dedent, and whitespace splits
                // runs (`['a b']` is args `"['a"` + `"b']"`). Convert
                // spliced strings back to word text (splitting it like
                // raw text); top-level strings stay structural.
                for t in splice_arg_groups(inner, stmt)? {
                    match &t.kind {
                        TokKind::Str { text, quote } => {
                            out.extend(split_word_text(&format!("{quote}{text}{quote}"), t.joint));
                        }
                        TokKind::BlockStr(full) => {
                            out.extend(split_word_text(full, t.joint));
                        }
                        _ => out.push(t),
                    }
                }
                out.push(Tok::fresh(TokKind::Punct(close), *close_joint));
            }
            _ => out.push(tok.clone()),
        }
    }
    Ok(out)
}

/// Split re-quoted raw text the way pest splits it: on whitespace
/// runs. The first piece keeps the source joint flag; the rest break
/// (whitespace separates args). Multi-space gaps normalize to one
/// (documented edge, same class as group rendering).
fn split_word_text(text: &str, joint: bool) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut first = true;
    for piece in text.split_whitespace() {
        out.push(Tok::fresh(
            TokKind::Word(piece.to_string()),
            if first { joint } else { false },
        ));
        first = false;
    }
    if out.is_empty() {
        out.push(Tok::fresh(TokKind::Word(text.to_string()), joint));
    }
    out
}

/// Assignment key charset mirror (`assign_key`, minus the `=` the
/// classifier handles separately). Words are pre-filtered by the same
/// set; `$`, quotes, and groups never start a key.
fn is_assign_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':')
}

/// Match an assignment key run at an argument start: joint
/// word/punct run in the key charset, non-empty, not `--`-led
/// (flags keep their literal reading). Returns the key and end index.
fn match_assign_key(toks: &[Tok], pos: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut p = pos;
    loop {
        let t = toks.get(p)?;
        if !out.is_empty() && !t.joint {
            break;
        }
        match &t.kind {
            TokKind::Word(w) => out.push_str(w),
            TokKind::Punct(c) if is_assign_key_char(*c) => out.push(*c),
            _ => break,
        }
        p += 1;
    }
    if out.is_empty() || out.starts_with("--") {
        return None;
    }
    if !out
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':'))
    {
        return None;
    }
    Some((out, p))
}

/// Assignment boundary ahead (for value scanning): an argument start
/// holding `KEY=` (key run plus `=` at any gap, mirroring `gap`).
/// Groups and strings are opaque: a `=` inside them never counts.
fn at_assign_boundary(toks: &[Tok], pos: usize) -> bool {
    match match_assign_key(toks, pos) {
        Some((_, end)) => matches!(toks.get(end).map(|t| &t.kind), Some(TokKind::Punct('='))),
        None => false,
    }
}

/// Raw-text render of one gap-run (or value span): words/puncts
/// literally (`$`+word literally, `$` elsewhere errors), strings by
/// their inner bytes, blocks lowered, groups spliced. Gaps collapse
/// to one space like `collapse_ws`; newlines/semicolons fail (the
/// grammar's raw span excludes them).
fn render_raw_text(toks: &[Tok], stmt: usize) -> Result<String, V2Error> {
    let mut out = String::new();
    let mut i = 0;
    while i < toks.len() {
        let tok = &toks[i];
        if !tok.joint && !out.is_empty() {
            out.push(' ');
        }
        match &tok.kind {
            TokKind::Word(w) => out.push_str(w),
            TokKind::Punct(c) => {
                if *c == '$' {
                    // Lone-`$` runs never reach here (expression
                    // extension claims them first and fails loudly).
                    // Elsewhere `$` renders literally (probed `a$(x)`
                    // reads raw) except before a brace group, where
                    // the grammar strands it (probed `K=${x}` fails).
                    let next = toks.get(i + 1);
                    let brace_next = next.is_some_and(|t| {
                        matches!(
                            &t.kind,
                            TokKind::Group {
                                delim: GroupDelim::Brace,
                                ..
                            }
                        )
                    });
                    if brace_next {
                        return Err(V2Error {
                            stmt,
                            msg: "unexpected `$` in argument".to_string(),
                        });
                    }
                    out.push('$');
                } else {
                    out.push(*c);
                }
            }
            TokKind::Str { text: s, .. } => out.push_str(s),
            TokKind::BlockStr(full) => {
                out.push_str(&crate::command::lower_block_string(full));
            }
            TokKind::Group {
                delim: GroupDelim::Brace,
                inner,
                open_joint,
                close_joint,
                ..
            } => out.push_str(&render_brace_run_text(inner, *open_joint, *close_joint)),
            TokKind::Group {
                delim: GroupDelim::Bracket,
                inner,
                open_joint,
                close_joint,
                ..
            } => out.push_str(&render_group('[', ']', inner, *open_joint, *close_joint)),
            TokKind::Group {
                delim: GroupDelim::Paren,
                inner,
                open_joint,
                close_joint,
                ..
            } => out.push_str(&render_group('(', ')', inner, *open_joint, *close_joint)),
            TokKind::Newline | TokKind::Semicolon => {
                return Err(V2Error {
                    stmt,
                    msg: "unexpected line break in argument".to_string(),
                });
            }
        }
        i += 1;
    }
    Ok(out)
}

/// Build an assignment value `Arg` over a region that runs to the next
/// boundary or end. Single quoted/block strings keep `quoted=true`; a
/// region that is exactly one lone `$var`/keypath/`env:`/call shape
/// stays typed (calls resolve through the shared resolver, matching
/// `assign_expr`); everything else renders raw. A quoted unit followed
/// by joint content errors (probed: `K="a"b` fails), like the grammar.
fn build_value_arg(region: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<Arg, V2Error> {
    if region.is_empty() {
        return Ok(Arg::String(String::new(), false));
    }
    if region.len() == 1 {
        match &region[0].kind {
            TokKind::Str { text: s, .. } => return Ok(Arg::String(s.clone(), true)),
            TokKind::BlockStr(full) => {
                return Ok(Arg::String(crate::command::lower_block_string(full), true));
            }
            _ => {}
        }
    }
    if region.len() > 1
        && matches!(&region[0].kind, TokKind::Str { .. } | TokKind::BlockStr(_))
        && region[1].joint
    {
        return Err(V2Error {
            stmt,
            msg: "unexpected text after quoted value".to_string(),
        });
    }
    if let Some(expr) = try_lone_value_expr(region, stmt, ctx)? {
        return Ok(expr);
    }
    Ok(Arg::String(render_raw_text(region, stmt)?, false))
}

/// Lone assignable shapes (`assign_expr` without continuation): `$var`,
/// key paths, `env:KEY`, and full-region calls (resolved). Anything
/// longer renders raw; the caller decides strictness.
fn try_lone_value_expr(region: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<Option<Arg>, V2Error> {
    // `$` shapes: exactly Punct+Word or dotted runs, all joint past
    // the first (the region starts at an argument start by construction).
    if matches!(region.first().map(|t| &t.kind), Some(TokKind::Punct('$'))) {
        let mut cur = Cursor::expr(region, stmt);
        cur.pos += 1;
        let base = match cur.next() {
            Some(t) if t.joint && matches!(&t.kind, TokKind::Word(w) if is_ident_word(w)) => {
                match &t.kind {
                    TokKind::Word(w) => w.clone(),
                    _ => unreachable!(),
                }
            }
            _ => return Ok(None),
        };
        let mut keys = Vec::new();
        loop {
            let dot = matches!(
                cur.toks.get(cur.pos).map(|t| &t.kind),
                Some(TokKind::Punct('.'))
            ) && cur.toks.get(cur.pos).is_some_and(|t| t.joint);
            let seg = cur.toks.get(cur.pos + 1);
            let seg_ok = dot
                && matches!(seg.map(|t| &t.kind), Some(TokKind::Word(w)) if is_key_segment(w))
                && seg.is_some_and(|t| t.joint);
            if !seg_ok {
                break;
            }
            if let Some(TokKind::Word(w)) = seg.map(|t| &t.kind) {
                keys.push(w.clone());
            }
            cur.pos += 2;
        }
        if cur.pos != region.len() {
            return Ok(None);
        }
        return Ok(Some(Arg::Expr(if keys.is_empty() {
            Expr::Var(base)
        } else {
            Expr::KeyPath { base, keys }
        })));
    }
    // `env:KEY` read, gapless and exact.
    if region.len() == 3
        && matches!(&region[0].kind, TokKind::Word(w) if w == "env")
        && matches!(&region[1].kind, TokKind::Punct(':'))
        && region[1].joint
        && let TokKind::Word(k) = &region[2].kind
        && is_env_key(k)
        && region[2].joint
    {
        return Ok(Some(Arg::Expr(Expr::Env(k.clone()))));
    }
    // Full-region call: resolve and keep typed (canonical rendering
    // happens per command: verbatim for ENV/EXPAND, `K=`-joined text
    // elsewhere, all from the same `Arg::Expr`).
    if let Some((name, paren)) = call_shape_at(region, 0, false)
        && paren + 1 == region.len()
        && let TokKind::Group { inner, .. } = &region[paren].kind
    {
        let inner = inner.clone();
        let args = parse_call_args(&inner, stmt, ctx)?;
        if name == "INSPECT" {
            let [arg] = args.as_slice() else {
                return Err(V2Error {
                    stmt,
                    msg: "INSPECT requires exactly one argument: INSPECT($var)".to_string(),
                });
            };
            if let Expr::Var(var) = arg {
                return Ok(Some(Arg::Expr(Expr::Inspect(var.clone()))));
            }
            return Err(V2Error {
                stmt,
                msg: format!("INSPECT requires a $variable argument, found {arg:?}"),
            });
        }
        let qualified = v2_resolve(ctx, stmt, &name)?;
        return Ok(Some(Arg::Expr(Expr::Call {
            name: qualified,
            args,
        })));
    }
    Ok(None)
}

/// Expression extension for `$`/`env:`-led argument starts: parse a
/// full expression (operators reach across gaps exactly like the
/// grammar's inner `gap`s). Ending at a gap/end yields a typed single
/// group; ending mid-run falls back to raw text of the run (`$x=K`);
/// parse failure is fatal (every fragment alternative failed, like
/// pest). Returns the arg and tokens consumed.
fn try_expr_arg(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<Option<(Arg, usize)>, V2Error> {
    let extend = matches!(toks.get(pos).map(|t| &t.kind), Some(TokKind::Punct('$')))
        || matches!(toks.get(pos).map(|t| &t.kind), Some(TokKind::Word(w)) if w == "env");
    if !extend {
        return Ok(None);
    }
    let mut cur = Cursor::expr(&toks[pos..], stmt);
    let expr = parse_expr_or(&mut cur, ctx)?;
    let end = pos + cur.pos;
    // Mid-run stop (joint follower): the whole run renders raw
    // (`$x=K`, `$a$b`); the run always ends the fallback span here
    // because extension starts groups.
    if toks.get(end).is_some_and(|t| t.joint) {
        let mut run_end = end;
        while toks.get(run_end).is_some_and(|t| t.joint) {
            run_end += 1;
        }
        let text = render_raw_text(&toks[pos..run_end], stmt)?;
        return Ok(Some((Arg::String(text, false), run_end - pos)));
    }
    Ok(Some((Arg::Expr(expr), end - pos)))
}

/// Render a brace group inside raw text (`{{..}}` templates and
/// mid-run `{..}` shapes): verbatim bytes, single-space-normalized
/// gaps. Run-start non-template braces never reach here (they parse
/// as maps/blocks or defer loudly in `build_arg_tokens`).
fn render_brace_run_text(inner: &[Tok], open_joint: bool, close_joint: bool) -> String {
    render_group('{', '}', inner, open_joint, close_joint)
}

/// Inner bytes of a fully-quoted word (`"hi"`, `'a'`): the re-quoted
/// standalone strings spliced from groups strip exactly like
/// structural ones. Triple-quote pieces never qualify (block lowering
/// owns them); glued quotes (`['a`) never reach here as sole runs.
fn quoted_word_inner(w: &str) -> Option<String> {
    if w.len() >= 2 && !w.starts_with("\"\"\"") {
        let q = w.as_bytes()[0] as char;
        if (q == '"' || q == '\'') && w.ends_with(q) {
            return Some(w[1..w.len() - 1].to_string());
        }
    }
    None
}

/// Build instruction tokens over generic args, mirroring
/// `assignment | argument` order at every argument start: assignments
/// merge across gaps to the next boundary, `$`/`env:`-led runs extend
/// as expressions, brace-led runs try templates then maps/blocks, and
/// everything else renders raw. Single quoted strings keep their flag.
fn build_arg_tokens(toks: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<Vec<InsToken>, V2Error> {
    // Splice bracket/paren groups first: pest reads them as ordinary
    // characters (whitespace splits inside them), with strings
    // re-quoted back to word text. Brace groups stay structural
    // (templates render whole; blocks parse as expressions).
    let flat = splice_arg_groups(toks, stmt)?;
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < flat.len() {
        // 1. Assignment: key run plus `=` at any gap.
        if let Some((key, key_end)) = match_assign_key(&flat, pos)
            && let Some(tok) = flat.get(key_end)
            && matches!(&tok.kind, TokKind::Punct('='))
        {
            let mut vend = key_end + 1;
            while vend < flat.len() && !(!flat[vend].joint && at_assign_boundary(&flat, vend)) {
                vend += 1;
            }
            let value = build_value_arg(&flat[key_end + 1..vend], stmt, ctx)?;
            out.push(InsToken::Assign(key, value));
            pos = vend;
            continue;
        }
        // 2. Brace-led run: template text, else map/block expression.
        if matches!(
            flat.get(pos).map(|t| &t.kind),
            Some(TokKind::Group {
                delim: GroupDelim::Brace,
                ..
            })
        ) {
            let mut run_end = pos + 1;
            while flat.get(run_end).is_some_and(|t| t.joint) {
                run_end += 1;
            }
            let run = &flat[pos..run_end];
            let is_template = is_template_brace(&run[0]);
            if is_template {
                out.push(InsToken::Pos(Arg::String(
                    render_raw_text(run, stmt)?,
                    false,
                )));
            } else {
                let inner = match &run[0].kind {
                    TokKind::Group { inner, .. } => inner.clone(),
                    _ => unreachable!(),
                };
                // Map success alone stays typed; map success with a
                // joint tail renders raw (`{k:1}x`, like the grammar's
                // mixed fragments); map failure defers/errors exactly
                // like the expression path.
                match parse_map_or_block(&inner, stmt, ctx) {
                    Ok(expr) if run.len() == 1 => {
                        out.push(InsToken::Pos(Arg::Expr(expr)));
                    }
                    Ok(_) => out.push(InsToken::Pos(Arg::String(
                        render_raw_text(run, stmt)?,
                        false,
                    ))),
                    Err(e) => return Err(e),
                }
            }
            pos = run_end;
            continue;
        }
        // 3. `$`/`env:`-led expression extension.
        if let Some((arg, used)) = try_expr_arg(&flat, pos, stmt, ctx)? {
            // A lone expression group stays typed; the fallback
            // already rendered text above.
            out.push(InsToken::Pos(arg));
            pos += used;
            continue;
        }
        // 4. Raw text run to the next gap.
        let mut run_end = pos + 1;
        while flat.get(run_end).is_some_and(|t| t.joint) {
            run_end += 1;
        }
        let run = &flat[pos..run_end];
        if run.len() == 1 {
            match &run[0].kind {
                TokKind::Str { text: s, .. } => {
                    out.push(InsToken::Pos(Arg::String(s.clone(), true)));
                    pos = run_end;
                    continue;
                }
                TokKind::BlockStr(full) => {
                    out.push(InsToken::Pos(Arg::String(
                        crate::command::lower_block_string(full),
                        true,
                    )));
                    pos = run_end;
                    continue;
                }
                // Re-quoted standalone strings (spliced from groups)
                // strip exactly like structural ones (`( "hi" )` gives
                // `hi`); glued quotes stay literal (`['a` keeps its
                // mark). Triple-quote pieces never qualify.
                TokKind::Word(w) if quoted_word_inner(w).is_some() => {
                    out.push(InsToken::Pos(Arg::String(
                        quoted_word_inner(w).unwrap(),
                        true,
                    )));
                    pos = run_end;
                    continue;
                }
                _ => {}
            }
        }
        out.push(InsToken::Pos(Arg::String(
            render_raw_text(run, stmt)?,
            false,
        )));
        pos = run_end;
    }
    Ok(out)
}

/// Lower generic args through the shared cores: `ENV`/`EXPAND` bypass
/// the injected dispatcher exactly like pest; everything else
/// canonicalizes assignments and calls it.
fn lower_generic_args(
    name: &str,
    tokens: Vec<InsToken>,
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<StepKind, V2Error> {
    if name == "ENV" {
        return lower_env_tokens(&lowering_ctx(stmt), tokens).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
        });
    }
    if name == "EXPAND" {
        return lower_expand_tokens(&lowering_ctx(stmt), tokens).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
        });
    }
    let args = tokens
        .into_iter()
        .map(|token| match token {
            InsToken::Pos(arg) => arg,
            InsToken::Assign(key, value) => canonical_assignment_arg(&key, &value),
        })
        .collect();
    (ctx.lower)(name, args).map_err(|e| V2Error {
        stmt,
        msg: e.to_string(),
    })
}

/// Render a bracket/paren/brace group back to source text for raw-text
/// positions (in-body groups are args, template braces render whole).
/// Fragments join with a space on gaps and glue on `joint`, so both
/// lexers render the same source identically; quote styles round-trip
/// exactly. Known normalization (documented, not silent): multi-space
/// gaps collapse to one.
fn render_group(
    open: char,
    close: char,
    inner: &[Tok],
    open_joint: bool,
    close_joint: bool,
) -> String {
    let mut out = String::from(open);
    if !open_joint && !inner.is_empty() {
        out.push(' ');
    }
    let mut first = true;
    for tok in inner {
        match &tok.kind {
            TokKind::Newline => out.push('\n'),
            TokKind::Semicolon => out.push(';'),
            _ => {
                if !first && !tok.joint {
                    out.push(' ');
                }
                match &tok.kind {
                    TokKind::Word(w) => out.push_str(w),
                    TokKind::Punct(c) => out.push(*c),
                    TokKind::Str { text, quote } => {
                        out.push(*quote);
                        out.push_str(text);
                        out.push(*quote);
                    }
                    TokKind::BlockStr(full) => out.push_str(full),
                    TokKind::Group {
                        delim: GroupDelim::Bracket,
                        inner,
                        open_joint,
                        close_joint,
                        ..
                    } => out.push_str(&render_group('[', ']', inner, *open_joint, *close_joint)),
                    TokKind::Group {
                        delim: GroupDelim::Paren,
                        inner,
                        open_joint,
                        close_joint,
                        ..
                    } => out.push_str(&render_group('(', ')', inner, *open_joint, *close_joint)),
                    TokKind::Group {
                        delim: GroupDelim::Brace,
                        inner,
                        open_joint,
                        close_joint,
                        ..
                    } => out.push_str(&render_group('{', '}', inner, *open_joint, *close_joint)),
                    TokKind::Newline | TokKind::Semicolon => unreachable!(),
                }
            }
        }
        first = false;
    }
    if !close_joint && !inner.is_empty() {
        out.push(' ');
    }
    out.push(close);
    out
}

/// Positional cursor over guard/group token slices. Newlines skip
/// freely between tokens (`ws` admits linebreaks in the grammar);
/// bare key/value runs never cross them (the grammar excludes
/// linebreaks there), so runs read raw.
///
/// Expression parsing reuses this cursor with `nl_skip = false`:
/// expressions are single-line (`gap`, never `gap_nl`, between
/// operands), so a newline terminates instead of vanishing. Newlines
/// inside group inners (call args, lists, maps) skip via explicit
/// `skip_newlines` calls at the grammar's `gap_nl` junctures.
struct Cursor<'t> {
    toks: &'t [Tok],
    pos: usize,
    stmt: usize,
    nl_skip: bool,
}

impl<'t> Cursor<'t> {
    fn new(toks: &'t [Tok], stmt: usize) -> Self {
        Self {
            toks,
            pos: 0,
            stmt,
            nl_skip: true,
        }
    }

    /// Expression-mode cursor: newlines stop, never skip.
    fn expr(toks: &'t [Tok], stmt: usize) -> Self {
        Self {
            toks,
            pos: 0,
            stmt,
            nl_skip: false,
        }
    }

    fn err(&self, msg: impl Into<String>) -> V2Error {
        V2Error {
            stmt: self.stmt,
            msg: msg.into(),
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(
            self.toks.get(self.pos).map(|t| &t.kind),
            Some(TokKind::Newline)
        ) {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<&'t Tok> {
        if self.nl_skip {
            self.skip_newlines();
        }
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<&'t Tok> {
        if self.nl_skip {
            self.skip_newlines();
        }
        let t = self.toks.get(self.pos)?;
        self.pos += 1;
        Some(t)
    }

    fn done(&mut self) -> bool {
        if self.nl_skip {
            self.skip_newlines();
        }
        self.pos >= self.toks.len()
    }

    fn eat_punct(&mut self, c: char) -> bool {
        if self.nl_skip {
            self.skip_newlines();
        }
        match self.peek() {
            Some(t) if matches!(&t.kind, TokKind::Punct(p) if *p == c) => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    /// Consume the parenthesized group following a call word (`not`,
    /// `any`, `all`, `eq`, `ne`), ignoring gap width exactly like the
    /// grammar's implicit whitespace.
    fn eat_paren_group(&mut self) -> Option<Vec<Tok>> {
        match self.peek() {
            Some(t) => match &t.kind {
                TokKind::Group {
                    delim: GroupDelim::Paren,
                    inner,
                    ..
                } => {
                    let inner = inner.clone();
                    self.pos += 1;
                    Some(inner)
                }
                _ => None,
            },
            None => None,
        }
    }
}

/// Split a flat token slice on top-level commas (nesting lives inside
/// atomic groups, so a flat scan is exact).
fn split_top_commas(toks: &[Tok]) -> Vec<&[Tok]> {
    let mut parts = Vec::new();
    let mut start = 0;
    for (i, t) in toks.iter().enumerate() {
        if matches!(&t.kind, TokKind::Punct(',')) {
            parts.push(&toks[start..i]);
            start = i + 1;
        }
    }
    parts.push(&toks[start..]);
    parts
}

/// Punctuation admissible inside bare guard key runs.
/// TODO(cutover): mirrors `guard_key_char` minus the terminators the
/// parser enforces separately. `;` arrives as its own token kind; the
/// grammar's key charset admits it, so runs accept it literally.
fn key_char(c: char) -> bool {
    !matches!(c, ':' | '=' | '!' | ',' | '|' | '(' | ')' | ']' | '$')
}

/// Consume a joint run of words/punctuation as one bare key or value.
/// Gaps end the run; terminators and groups end or fail it per `extra`.
fn bare_run(cur: &mut Cursor, extra: fn(char) -> bool, what: &str) -> Result<String, V2Error> {
    let mut out = String::new();
    loop {
        let tok = match cur.toks.get(cur.pos) {
            Some(t) if matches!(&t.kind, TokKind::Newline) => break,
            Some(t) => t,
            None => break,
        };
        // First element may follow a gap; continuations must be gapless.
        if !out.is_empty() && !tok.joint {
            break;
        }
        match &tok.kind {
            TokKind::Word(w) => out.push_str(w),
            TokKind::Punct(c) if key_char(*c) && extra(*c) => out.push(*c),
            TokKind::Semicolon if extra(';') => out.push(';'),
            _ => break,
        }
        cur.pos += 1;
    }
    if out.is_empty() {
        return Err(cur.err(format!("missing {what}")));
    }
    Ok(out)
}

/// The `guard_term` negative lookahead: a complete term followed by
/// `(`, `:`, `=`, or `!` is malformed, never a longer form.
fn check_term_end(cur: &mut Cursor) -> Result<(), V2Error> {
    cur.skip_newlines();
    match cur.toks.get(cur.pos) {
        Some(t) => match &t.kind {
            TokKind::Group {
                delim: GroupDelim::Paren,
                ..
            } => Err(cur.err("unexpected `(` after guard term")),
            TokKind::Punct(c) if matches!(c, ':' | '=' | '!') => {
                Err(cur.err(format!("unexpected `{c}` after guard term")))
            }
            _ => Ok(()),
        },
        None => Ok(()),
    }
}

fn os_guard(value: &str) -> GuardExpr {
    GuardExpr::Predicate(Guard::Attr {
        ns: Ns::Os,
        key: None,
        val: Some(value.to_string()),
    })
}

/// TODO(cutover): unify with `parse_ns_guard` and retire that
/// function. Outcomes and messages are identical by construction until
/// then; the differential oracle pins them.
fn ns_guard(ns: &str, val: &str, cur: &Cursor) -> Result<GuardExpr, V2Error> {
    match ns {
        "family" => match val {
            "unix" => Ok(GuardExpr::or(vec![os_guard("macos"), os_guard("linux")])),
            "windows" => Ok(os_guard("windows")),
            _ => Err(cur.err(format!(
                "unknown family '{val}'; known values: unix, windows"
            ))),
        },
        "os" => match val {
            "macos" | "linux" | "windows" => Ok(os_guard(val)),
            _ => Err(cur.err(format!(
                "unknown os '{val}'; known values: macos, linux, windows"
            ))),
        },
        "arch" => {
            if crate::ARCH_VALUES.contains(&val) {
                Ok(GuardExpr::Predicate(Guard::Attr {
                    ns: Ns::Arch,
                    key: None,
                    val: Some(val.to_string()),
                }))
            } else {
                Err(cur.err(format!(
                    "unknown arch '{val}'; known values: {}",
                    crate::ARCH_VALUES.join(", ")
                )))
            }
        }
        _ => Err(cur.err(format!(
            "unknown guard namespace '{ns}'; known namespaces: family, os, arch, env, bool"
        ))),
    }
}

fn parse_guard_term(cur: &mut Cursor) -> Result<GuardExpr, V2Error> {
    let tok = cur
        .next()
        .ok_or_else(|| cur.err("missing guard predicate"))?;
    let word = match &tok.kind {
        TokKind::Word(w) => w.clone(),
        _ => return Err(cur.err("missing guard predicate")),
    };
    // `__OXDOCK_INJECT_N__` interpolation placeholder: env-key shape.
    if let Some(rest) = word.strip_prefix("__OXDOCK_INJECT_") {
        if !rest.is_empty()
            && rest
                .strip_suffix("__")
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        {
            return Ok(GuardExpr::Predicate(Guard::Attr {
                ns: Ns::Env,
                key: Some(word),
                val: None,
            }));
        }
        return Err(cur.err("missing guard predicate"));
    }
    match word.as_str() {
        "eq" | "ne" => {
            let inner = cur
                .eat_paren_group()
                .ok_or_else(|| cur.err(format!("`{word}` requires `(env:KEY, value)`")))?;
            let mut arg = Cursor::new(&inner, cur.stmt);
            if arg.next().map(|t| &t.kind) != Some(&TokKind::Word("env".to_string())) {
                return Err(arg.err(format!("`{word}` requires `(env:KEY, value)`")));
            }
            if !arg.eat_punct(':') {
                return Err(arg.err(format!("`{word}` requires `(env:KEY, value)`")));
            }
            let key = bare_run(&mut arg, |_| false, "guard key")?;
            if !arg.eat_punct(',') {
                return Err(arg.err(format!("`{word}` requires `(env:KEY, value)`")));
            }
            arg.skip_newlines();
            let value = match arg.next() {
                Some(t) => match &t.kind {
                    TokKind::Str { text, .. } => text.clone(),
                    TokKind::BlockStr(full) => crate::command::lower_block_string(full),
                    _ => {
                        // Bare value: runs to the region end (commas and
                        // groups terminate it with an error, matching the
                        // `bare_guard_value` exclusions).
                        arg.pos -= 1;
                        value_run(&mut arg)?
                    }
                },
                None => return Err(arg.err(format!("`{word}` requires `(env:KEY, value)`"))),
            };
            if !arg.done() {
                return Err(arg.err(format!("unexpected text after `{word}` value")));
            }
            let pred = GuardExpr::Predicate(Guard::Attr {
                ns: Ns::Env,
                key: Some(key),
                val: Some(value),
            });
            if word == "ne" {
                Ok(GuardExpr::Not(Box::new(pred)))
            } else {
                Ok(pred)
            }
        }
        "bool" => {
            if !cur.eat_punct(':') {
                return Err(cur.err("`bool` requires `:value`"));
            }
            let val = bare_run(cur, |_| false, "bool value")?;
            check_term_end(cur)?;
            Ok(GuardExpr::Predicate(Guard::Attr {
                ns: Ns::Bool,
                key: None,
                val: Some(val),
            }))
        }
        "env" => {
            if !cur.eat_punct(':') {
                return Err(cur.err("`env` requires `:KEY`"));
            }
            let key = bare_run(cur, |_| false, "guard key")?;
            check_term_end(cur)?;
            Ok(GuardExpr::Predicate(Guard::Attr {
                ns: Ns::Env,
                key: Some(key),
                val: None,
            }))
        }
        _ => {
            if !cur.eat_punct(':') {
                return Err(cur.err("missing guard predicate"));
            }
            let val = bare_run(cur, |_| false, "guard value")?;
            check_term_end(cur)?;
            ns_guard(&word, &val, cur)
        }
    }
}

/// Bare `eq`/`ne` value run: key charset plus `:`/`=`/`!` (the grammar
/// admits them in values, never in keys), with nested groups rendered
/// for the pathological `(`-in-value shape. `$`, `|`, `,` terminate
/// with an error downstream.
fn value_run(arg: &mut Cursor) -> Result<String, V2Error> {
    let mut out = String::new();
    loop {
        let tok = match arg.toks.get(arg.pos) {
            Some(t) if matches!(&t.kind, TokKind::Newline) => break,
            Some(t) => t,
            None => break,
        };
        if !out.is_empty() && !tok.joint {
            break;
        }
        match &tok.kind {
            TokKind::Word(w) => out.push_str(w),
            TokKind::Punct(c) if key_char(*c) || matches!(c, ':' | '=' | '!') => {
                out.push(*c);
            }
            TokKind::Semicolon => out.push(';'),
            TokKind::Str { text, .. } => {
                if !out.is_empty() {
                    break;
                }
                out.push_str(text);
            }
            TokKind::BlockStr(full) => {
                if !out.is_empty() {
                    break;
                }
                out.push_str(&crate::command::lower_block_string(full));
            }
            TokKind::Group {
                delim: GroupDelim::Bracket,
                inner,
                open_joint,
                close_joint,
                ..
            } => {
                if !out.is_empty() && !tok.joint {
                    break;
                }
                out.push_str(&render_group('[', ']', inner, *open_joint, *close_joint));
            }
            TokKind::Group {
                delim: GroupDelim::Paren,
                inner,
                open_joint,
                close_joint,
                ..
            } => {
                if !out.is_empty() && !tok.joint {
                    break;
                }
                out.push_str(&render_group('(', ')', inner, *open_joint, *close_joint));
            }
            TokKind::Group {
                delim: GroupDelim::Brace,
                ..
            } => break,
            // Newlines terminate values; `;` was pushed above (it is a
            // key-charset char inside groups).
            TokKind::Newline => break,
            _ => break,
        }
        arg.pos += 1;
    }
    if out.is_empty() {
        return Err(arg.err("missing guard value"));
    }
    Ok(out)
}

/// True when the cursor sits on `name` followed (over newlines,
/// like the grammar's implicit whitespace) by a paren group.
fn at_call(cur: &Cursor, name: &str) -> bool {
    let mut pos = cur.pos;
    match cur.toks.get(pos) {
        Some(t) if matches!(&t.kind, TokKind::Word(w) if w == name) => pos += 1,
        _ => return false,
    }
    while matches!(cur.toks.get(pos).map(|t| &t.kind), Some(TokKind::Newline)) {
        pos += 1;
    }
    matches!(
        cur.toks.get(pos).map(|t| &t.kind),
        Some(TokKind::Group {
            delim: GroupDelim::Paren,
            ..
        })
    )
}

fn parse_guard_primary(cur: &mut Cursor) -> Result<GuardExpr, V2Error> {
    // Call shapes dispatch on word + paren group regardless of gap
    // width (the grammar's implicit whitespace); anything else is a term.
    for name in ["not", "any", "all"] {
        if at_call(cur, name) {
            cur.next();
            let inner = cur
                .eat_paren_group()
                .ok_or_else(|| cur.err("unreachable"))?;
            let parts = split_top_commas(&inner);
            match name {
                "not" => {
                    if parts.len() != 1 {
                        return Err(cur.err("`not` takes exactly one guard expression"));
                    }
                    let mut sub = Cursor::new(parts[0], cur.stmt);
                    let expr = parse_guard_seq(&mut sub)?;
                    if !sub.done() {
                        return Err(sub.err("unexpected text in `not`"));
                    }
                    return Ok(GuardExpr::Not(Box::new(expr)));
                }
                "any" => {
                    if parts.len() < 2 {
                        return Err(cur.err("any(...) requires at least two guard expressions"));
                    }
                    let mut exprs = Vec::new();
                    for part in parts {
                        let mut sub = Cursor::new(part, cur.stmt);
                        exprs.push(parse_guard_seq(&mut sub)?);
                        if !sub.done() {
                            return Err(sub.err("unexpected text in `any`"));
                        }
                    }
                    return Ok(GuardExpr::or(exprs));
                }
                _ => {
                    if parts.is_empty() {
                        return Err(cur.err("all(...) requires at least one guard expression"));
                    }
                    let mut exprs = Vec::new();
                    for part in parts {
                        if part.iter().all(|t| matches!(&t.kind, TokKind::Newline)) {
                            return Err(cur.err("all(...) requires at least one guard expression"));
                        }
                        let mut sub = Cursor::new(part, cur.stmt);
                        exprs.push(parse_guard_seq(&mut sub)?);
                        if !sub.done() {
                            return Err(sub.err("unexpected text in `all`"));
                        }
                    }
                    return Ok(GuardExpr::all(exprs));
                }
            }
        }
    }
    match cur.peek() {
        Some(t) => match &t.kind {
            TokKind::Group {
                delim: GroupDelim::Paren,
                inner,
                ..
            } => {
                let inner = inner.clone();
                cur.next();
                let mut sub = Cursor::new(&inner, cur.stmt);
                let expr = parse_guard_seq(&mut sub)?;
                if !sub.done() {
                    return Err(sub.err("unexpected text in guard group"));
                }
                Ok(expr)
            }
            _ => parse_guard_term(cur),
        },
        None => Err(cur.err("missing guard expression")),
    }
}

fn parse_guard_seq(cur: &mut Cursor) -> Result<GuardExpr, V2Error> {
    let first = parse_guard_primary(cur)?;
    let mut rest = vec![first];
    while cur.eat_punct(',') {
        rest.push(parse_guard_primary(cur)?);
    }
    if rest.len() == 1 {
        Ok(rest.pop().unwrap())
    } else {
        Ok(GuardExpr::all(rest))
    }
}

/// Parse one bracket group's contents as a guard expression.
fn parse_guard_inner(inner: &[Tok], stmt: usize) -> Result<GuardExpr, V2Error> {
    let mut cur = Cursor::new(inner, stmt);
    let expr = parse_guard_seq(&mut cur)?;
    if !cur.done() {
        return Err(cur.err("unexpected text in guard"));
    }
    Ok(expr)
}

/// Combine a statement's leading guard groups exactly like the pest
/// path's pending-guard stash: stacked brackets `All` together, a
/// single bracket stands alone.
fn parse_guard_groups(guard: &[Tok], stmt: usize) -> Result<Option<GuardExpr>, V2Error> {
    let mut acc: Option<GuardExpr> = None;
    for tok in guard {
        let inner = match &tok.kind {
            TokKind::Group {
                delim: GroupDelim::Bracket,
                inner,
                ..
            } => inner.clone(),
            _ => {
                return Err(V2Error {
                    stmt,
                    msg: "unsupported in parse_v2 slice 2: non-bracket guard".to_string(),
                });
            }
        };
        let expr = parse_guard_inner(&inner, stmt)?;
        acc = Some(match acc {
            None => expr,
            Some(prev) => GuardExpr::all(vec![prev, expr]),
        });
    }
    Ok(acc)
}

// ---- Slice 4a: calls, RUN-exec, instruction exactness ----
// Static call resolution runs through the shared `resolve_call_name`
// (same module table, same messages); only the token-level call-shape
// detector is v2-native. Generic arguments classify like the
// `assignment | argument` grammar order (assignments merge across
// gaps, `$`/`env:`-led runs extend as expressions, everything else
// renders raw), and `ENV`/`EXPAND` lower through the shared direct
// cores instead of the injected dispatcher, exactly like pest.

/// Parse context: shared lowering dispatcher plus module table for
/// static call resolution. Func/import frames arrive in slice 5; until
/// then resolution sees modules only (identical to pest with empty
/// scopes, pinned by the oracle).
pub struct V2Ctx<'a> {
    pub lower: &'a dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
    pub modules: &'a ModuleTable,
}

fn v2_resolve(ctx: &V2Ctx, stmt: usize, name: &str) -> Result<String, V2Error> {
    let funcs = HashSet::new();
    let imports: Vec<String> = vec![];
    resolve_call_name(ctx.modules, &funcs, &imports, &lowering_ctx(stmt), name).map_err(|e| {
        V2Error {
            stmt,
            msg: e.to_string(),
        }
    })
}

/// Call shape at `pos`: UPPERCASE head plus joint `::` segments and a
/// paren group (joint iff required: statements forbid the gap via
/// `call_head_paren`; expressions allow it via `func_call`'s `gap`).
/// Peek-only; anything else falls through exactly like the grammar's
/// `func_call` mismatch. Returns the dotted name and paren index.
fn call_shape_at(toks: &[Tok], pos: usize, joint_paren: bool) -> Option<(String, usize)> {
    let head = match toks.get(pos).map(|t| &t.kind) {
        Some(TokKind::Word(w))
            if !w.is_empty()
                && w.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && w.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') =>
        {
            w.clone()
        }
        _ => return None,
    };
    let mut name = head;
    let mut p = pos + 1;
    loop {
        let colons = matches!(toks.get(p).map(|t| &t.kind), Some(TokKind::Punct(':')))
            && toks.get(p).is_some_and(|t| t.joint)
            && matches!(toks.get(p + 1).map(|t| &t.kind), Some(TokKind::Punct(':')))
            && toks.get(p + 1).is_some_and(|t| t.joint)
            && matches!(toks.get(p + 2).map(|t| &t.kind), Some(TokKind::Word(_)))
            && toks.get(p + 2).is_some_and(|t| t.joint);
        if !colons {
            break;
        }
        if let Some(TokKind::Word(seg)) = toks.get(p + 2).map(|t| &t.kind) {
            name.push_str("::");
            name.push_str(seg);
        }
        p += 3;
    }
    match toks.get(p).map(|t| &t.kind) {
        Some(TokKind::Group {
            delim: GroupDelim::Paren,
            ..
        }) => {
            if joint_paren && !toks[p].joint {
                return None;
            }
            Some((name, p))
        }
        _ => None,
    }
}

/// Call arguments over a paren group's contents: comma-separated full
/// expressions with `gap_nl` newlines at the edges, mirroring
/// `func_call` (each arg rejects a stray boundary like pest).
fn parse_call_args(inner: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<Vec<Expr>, V2Error> {
    let mut cur = Cursor::expr(inner, stmt);
    cur.skip_newlines();
    if cur.done() {
        return Ok(vec![]);
    }
    let mut args = Vec::new();
    loop {
        let arg = parse_expr_or(&mut cur, ctx)?;
        reject_boundary(&lowering_ctx(stmt), &arg).map_err(|e| cur.err(e.to_string()))?;
        args.push(arg);
        cur.skip_newlines();
        if cur.eat_punct(',') {
            cur.skip_newlines();
            continue;
        }
        break;
    }
    if !cur.done() {
        return Err(cur.err("unexpected text in call"));
    }
    Ok(args)
}

/// Lower a call-shape to `Expr`: `INSPECT` keeps its variable
/// unevaluated (same arity/name errors as pest); everything else
/// resolves statically through the shared resolver.
fn parse_expr_call(name: String, inner: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    let args = parse_call_args(inner, stmt, ctx)?;
    if name == "INSPECT" {
        let [arg] = args.as_slice() else {
            return Err(V2Error {
                stmt,
                msg: "INSPECT requires exactly one argument: INSPECT($var)".to_string(),
            });
        };
        if let Expr::Var(var) = arg {
            return Ok(Expr::Inspect(var.clone()));
        }
        return Err(V2Error {
            stmt,
            msg: format!("INSPECT requires a $variable argument, found {arg:?}"),
        });
    }
    let qualified = v2_resolve(ctx, stmt, &name)?;
    Ok(Expr::Call {
        name: qualified,
        args,
    })
}

/// RUN exec-form items: atoms only (mirroring `run_exec_arg`), except
/// parenthesized/calls/lists/maps which recurse fully like pest. Each
/// item rejects a stray boundary.
fn parse_run_exec_list(inner: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    let mut cur = Cursor::expr(inner, stmt);
    cur.skip_newlines();
    if cur.done() {
        return Ok(Expr::List(vec![]));
    }
    let mut items = Vec::new();
    loop {
        let item = parse_expr_atom(&mut cur, ctx)?;
        reject_boundary(&lowering_ctx(stmt), &item).map_err(|e| cur.err(e.to_string()))?;
        items.push(item);
        cur.skip_newlines();
        if cur.eat_punct(',') {
            cur.skip_newlines();
            continue;
        }
        break;
    }
    if !cur.done() {
        return Err(cur.err("unexpected text in RUN list"));
    }
    Ok(Expr::List(items))
}

// ---- Slice 4b: single-command wrappers + AWAIT/CANCEL ----
// Wrappers parse through the shared validators (`check_tty_wrapping`,
// capture rejects, isolated rewrite, nesting reject, binding merge,
// guard `and`) promoted verbatim to `commands.rs`; only the
// token-level shape classifiers (duration gate, binding splitter,
// await/cancel shapes, block-vs-template) are v2-native. Statements
// lower to step VECTORS: structural pairs (calls, RUN-exec,
// await/cancel, wrapper blocks) end mid-line and trailing elements
// form following steps, exactly like the grammar's `element*`.
//
// Deferred: block STATEMENTS (slice 5: top-level `IF`/`FOR`/`WHILE`
// bodies, guard blocks, `REMOTE`, directives).

/// Static duration shape mirror (`timeout_literal`, atomic): ASCII
/// digits plus one optional unit, full-match only. Anything else
/// (`10ss`, `banana`, `-5s`) misses the wrapper shape and falls back
/// to the generic instruction path, which fails in lowering exactly
/// like the grammar's committed `instruction`.
fn is_timeout_literal(text: &str) -> bool {
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return false;
    }
    matches!(&text[digits..], "" | "ms" | "s" | "m" | "h")
}

/// Template-brace test: a brace group whose first inner token opens
/// another brace (`{{...}}`); anything else braced is a block body.
/// Same shape the arg engine uses; single source from here.
fn is_template_brace(tok: &Tok) -> bool {
    matches!(
        &tok.kind,
        TokKind::Group {
            delim: GroupDelim::Brace,
            inner,
            ..
        } if matches!(
            inner.first().map(|t| &t.kind),
            Some(TokKind::Group {
                delim: GroupDelim::Brace,
                ..
            })
        )
    )
}

/// Parse one TIMEOUT duration unit at `pos` (the token after the
/// head, gapped like the grammar's `sep`). Returns the arg and
/// tokens consumed, or `None` on shape miss (caller falls back to
/// generic, which fails in lowering exactly like pest). Static
/// literals validate now through the shared Duration check
/// (`TIMEOUT 0s` fails here, probed); dynamics defer to runtime.
fn parse_timeout_duration_at(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
) -> Result<Option<(Arg, usize)>, V2Error> {
    let Some(head) = toks.get(pos) else {
        return Ok(None);
    };
    if head.joint {
        return Ok(None);
    }
    let arg = match &head.kind {
        TokKind::Word(w) if is_timeout_literal(w) => Arg::String(w.clone(), false),
        TokKind::Punct('$') => match toks.get(pos + 1) {
            Some(var) if var.joint && matches!(&var.kind, TokKind::Word(w) if is_ident_word(w)) => {
                match &var.kind {
                    TokKind::Word(w) => Arg::Expr(Expr::Var(w.clone())),
                    _ => unreachable!(),
                }
            }
            _ => return Ok(None),
        },
        TokKind::Str { text, .. } => Arg::String(text.clone(), true),
        TokKind::BlockStr(full) => Arg::String(crate::command::lower_block_string(full), true),
        TokKind::Group {
            delim: GroupDelim::Brace,
            inner,
            open_joint,
            close_joint,
        } if is_template_brace(&toks[pos]) => Arg::String(
            render_group('{', '}', inner, *open_joint, *close_joint),
            false,
        ),
        _ => return Ok(None),
    };
    let used = match &head.kind {
        TokKind::Punct('$') => 2,
        _ => 1,
    };
    ArgType::Duration.check_arg(&arg).map_err(|e| V2Error {
        stmt,
        msg: e.to_string(),
    })?;
    Ok(Some((arg, used)))
}

/// Parse `WITH_IO [...]` bindings over a bracket group's inner
/// tokens: comma-separated `stream[=$var]` / `rows|cols=int`
/// bindings, newlines skipping freely like `ws`. Returns `None` on
/// shape miss (empty flags, bad stream words, missing commas or
/// values: the caller falls back to generic, which fails in lowering
/// exactly like pest). Grammar-matched but invalid bindings (tty
/// values, out-of-range geometry) fail loudly, mirroring
/// `parse_io_binding`'s messages one by one.
fn parse_io_bindings(inner: &[Tok], stmt: usize) -> Result<Option<Vec<IoBinding>>, V2Error> {
    let mut parts: Vec<&[Tok]> = Vec::new();
    let mut start = 0;
    for (i, tok) in inner.iter().enumerate() {
        // Newlines skip freely like `ws` (filtered per part below);
        // commas split bindings; everything else is content.
        if matches!(&tok.kind, TokKind::Punct(',')) {
            parts.push(&inner[start..i]);
            start = i + 1;
        }
    }
    parts.push(&inner[start..]);
    // An empty flag list (`[]`, `[,]`) misses the shape.
    if parts.len() == 1 && parts[0].iter().all(|t| matches!(&t.kind, TokKind::Newline)) {
        return Ok(None);
    }
    let mut out = Vec::new();
    for part in parts {
        let part: Vec<&Tok> = part
            .iter()
            .filter(|t| !matches!(&t.kind, TokKind::Newline))
            .collect();
        match parse_io_binding_part(&part, stmt)? {
            Some(binding) => out.push(binding),
            None => return Ok(None),
        }
    }
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some(out))
}

/// One `stream[=$var]` / `rows|cols=int` binding. Gaps ride free
/// around `=`/`,`/names like `ws?`; only `$`+name stays gapless
/// (atomic `dollar_ident`). Streams and size names outside the
/// grammar's words miss the shape; a valued `tty` and out-of-range
/// geometry fail loudly with pest's exact messages.
fn parse_io_binding_part(part: &[&Tok], stmt: usize) -> Result<Option<IoBinding>, V2Error> {
    let fail = |msg: String| V2Error { stmt, msg };
    let Some(first) = part.first() else {
        return Ok(None);
    };
    let TokKind::Word(stream) = &first.kind else {
        return Ok(None);
    };
    let is_stream = matches!(stream.as_str(), "stdin" | "stdout" | "stderr" | "tty");
    let is_size = matches!(stream.as_str(), "rows" | "cols");
    if !is_stream && !is_size {
        return Ok(None);
    }
    let rest = &part[1..];
    if rest.is_empty() {
        if is_size {
            // Bare `rows` misses `io_size` (`=` + digits required).
            return Ok(None);
        }
        return Ok(Some(IoBinding {
            stream: io_stream_of(stream),
            pipe: None,
        }));
    }
    // `= value` at any gap.
    if !matches!(rest.first().map(|t| &t.kind), Some(TokKind::Punct('='))) {
        return Ok(None);
    }
    let value = &rest[1..];
    if is_size {
        // Geometry: one all-digit word, 1-65535 (mirrors `parse_io_size`).
        if value.len() != 1 {
            return Ok(None);
        }
        let TokKind::Word(digits) = &value[0].kind else {
            return Ok(None);
        };
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return Ok(None);
        }
        let dim = digits
            .parse::<u64>()
            .ok()
            .filter(|v| (1..=u64::from(u16::MAX)).contains(v))
            .ok_or_else(|| {
                fail(
                    "WITH_IO terminal geometry needs a positive integer 1-65535 (e.g. `rows=40`)"
                        .to_string(),
                )
            })?;
        let dim = dim as u16;
        return Ok(Some(IoBinding {
            stream: if stream == "rows" {
                IoStream::Rows(dim)
            } else {
                IoStream::Cols(dim)
            },
            pipe: None,
        }));
    }
    // Stream pipe: `= $var` with a gapless name.
    if value.len() != 2 {
        return Ok(None);
    }
    if !matches!(&value[0].kind, TokKind::Punct('$')) {
        return Ok(None);
    }
    let TokKind::Word(var) = &value[1].kind else {
        return Ok(None);
    };
    if !value[1].joint || !is_ident_word(var) {
        return Ok(None);
    }
    if stream == "tty" {
        // `tty` is a bare flag (mirrors `parse_io_binding`).
        return Err(fail(
            "WITH_IO [tty] takes no value: use bare `tty`, with `rows = <int>` / `cols = <int>` for geometry (e.g. `[tty, rows=40, cols=100]`)"
                .to_string(),
        ));
    }
    Ok(Some(IoBinding {
        stream: io_stream_of(stream),
        pipe: Some(crate::ast::PipeTarget::Var(var.clone())),
    }))
}

/// Stream word to [`IoStream`]: only grammar words reach here, so
/// the mapping is total (mirrors `parse_io_stream`).
fn io_stream_of(name: &str) -> IoStream {
    match name {
        "stdin" => IoStream::Stdin,
        "stdout" => IoStream::Stdout,
        "stderr" => IoStream::Stderr,
        _ => IoStream::Tty,
    }
}

/// `AWAIT`/`CANCEL $var` shape at `pos`. A head without `$` misses
/// (`None`: generic fails it in lowering like pest); once `$`
/// commits, the name must follow and parse errors stay loud, like
/// the grammar's committed `"$" ~ ident`. `var_joint` enforces the
/// atomic `$var` adjacency inside compound-atomic parents; element
/// level admits the gap (probed `AWAIT $ t`). The caller continues
/// its element loop past the shape.
fn parse_await_cancel_at(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
    var_joint: bool,
) -> Result<Option<(StepKind, usize)>, V2Error> {
    let head = match toks.get(pos).map(|t| &t.kind) {
        Some(TokKind::Word(w)) if w == "AWAIT" || w == "CANCEL" => w.clone(),
        _ => return Ok(None),
    };
    if !matches!(
        toks.get(pos + 1).map(|t| &t.kind),
        Some(TokKind::Punct('$'))
    ) {
        return Ok(None);
    }
    let var = match toks.get(pos + 2) {
        Some(t)
            if matches!(&t.kind, TokKind::Word(w) if is_ident_word(w))
                && (!var_joint || t.joint) =>
        {
            match &t.kind {
                TokKind::Word(w) => w.clone(),
                _ => unreachable!(),
            }
        }
        _ => {
            return Err(V2Error {
                stmt,
                msg: format!("{head} requires a variable"),
            });
        }
    };
    Ok(Some((
        if head == "AWAIT" {
            StepKind::Await { var }
        } else {
            StepKind::Cancel { var }
        },
        3,
    )))
}

/// Wrapper strictness for one single-command inner: `AWAIT` parses
/// dedicated in `TIMEOUT`/`ISOLATED` inners but falls to generic in
/// `WITH_IO`/`ASYNC` inners (where lowering fails it, probed), and
/// `WITH_IO` recurses everywhere but directly under `ASYNC` (where
/// lowering fails it, probed). Shared predicates, never restated.
#[derive(Clone, Copy)]
struct InnerRules {
    allow_await: bool,
    allow_with_io: bool,
}

/// Parse one single (non-block) inner command, returning the kind
/// and tokens consumed: dedicated await/cancel, deferred
/// while/remote (pest accepts; bodies arrive slice 5), calls,
/// RUN-exec, nested wrappers, strict `INHERIT_ENV` (the caller
/// rejects the nesting or rewrites it), or the generic instruction
/// path (which consumes everything and fails unknown heads in
/// lowering, exactly like the grammar's committed `instruction`).
fn parse_single_inner(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
    ctx: &V2Ctx,
    rules: InnerRules,
) -> Result<(StepKind, usize), V2Error> {
    let head_word = match toks.get(pos).map(|t| &t.kind) {
        Some(TokKind::Word(w)) => Some(w.clone()),
        _ => None,
    };
    // WHILE/REMOTE inners parse in pest; their bodies arrive slice
    // 5, so defer loudly instead of drifting.
    if matches!(head_word.as_deref(), Some("WHILE") | Some("REMOTE")) {
        let head = head_word.unwrap_or_default();
        return Err(V2Error {
            stmt,
            msg: format!("unsupported in parse_v2: `{head}` (later slice)"),
        });
    }
    // Dedicated await/cancel first (await gated by parent); anything
    // they decline falls through to generic below.
    if head_word.as_deref() == Some("AWAIT")
        && rules.allow_await
        && let Some(found) = parse_await_cancel_at(toks, pos, stmt, true)?
    {
        return Ok(found);
    }
    if head_word.as_deref() == Some("CANCEL")
        && let Some(found) = parse_await_cancel_at(toks, pos, stmt, true)?
    {
        return Ok(found);
    }
    // Calls and RUN-exec end mid-run; the caller continues past them.
    if let Some((call_name, paren)) = call_shape_at(toks, pos, true) {
        let inner = match &toks[paren].kind {
            TokKind::Group { inner, .. } => inner.clone(),
            _ => unreachable!(),
        };
        let args = parse_call_args(&inner, stmt, ctx)?;
        if !call_name.contains("::") {
            crate::resolve::check_func_ident(&lowering_ctx(stmt), &call_name).map_err(|e| {
                V2Error {
                    stmt,
                    msg: e.to_string(),
                }
            })?;
        }
        let qualified = v2_resolve(ctx, stmt, &call_name)?;
        return Ok((
            StepKind::Call {
                name: qualified,
                args,
            },
            paren + 1 - pos,
        ));
    }
    if head_word.as_deref() == Some("RUN")
        && matches!(
            toks.get(pos + 1).map(|t| &t.kind),
            Some(TokKind::Group {
                delim: GroupDelim::Bracket,
                ..
            })
        )
    {
        // Gapped like the grammar's `sep`; a joint bracket is a
        // hard error, never a spliced arg (probed `RUN["a"]`).
        if toks[pos + 1].joint {
            return Err(V2Error {
                stmt,
                msg: "RUN [...] requires a gap after RUN".to_string(),
            });
        }
        let inner = match &toks[pos + 1].kind {
            TokKind::Group { inner, .. } => inner.clone(),
            _ => unreachable!(),
        };
        let list = parse_run_exec_list(&inner, stmt, ctx)?;
        let kind = (ctx.lower)("RUN", vec![Arg::Expr(list)]).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
        })?;
        return Ok((kind, 2));
    }
    // Nested wrappers recurse structurally; a `WITH_IO` directly
    // under `ASYNC` goes generic instead (lowering fails it, probed).
    match head_word.as_deref() {
        Some("WITH_IO") if rules.allow_with_io => {
            match parse_with_io_single(toks, pos, stmt, ctx) {
                Ok(Some((kind, used))) => return Ok((kind, used)),
                Ok(None) => {}
                Err(e) => return Err(e),
            }
        }
        Some("TIMEOUT") => match parse_timeout_single(toks, pos, stmt, ctx) {
            Ok(Some((kind, used))) => return Ok((kind, used)),
            Ok(None) => {}
            Err(e) => return Err(e),
        },
        Some("ISOLATED") => match parse_isolated_single(toks, pos, stmt, ctx) {
            Ok(Some((kind, used))) => return Ok((kind, used)),
            Ok(None) => {}
            Err(e) => return Err(e),
        },
        Some("ASYNC") => match parse_async_single(toks, pos, stmt, ctx) {
            Ok(Some((kind, used))) => return Ok((kind, used)),
            Ok(None) => {}
            Err(e) => return Err(e),
        },
        // `INHERIT_ENV [...]` parses strictly here (shared with
        // element level); the caller either rejects the nesting or
        // rewrites it, exactly like the finish validator. Trailing
        // elements loop on outside (a bad follower fails there, like
        // the grammar's line end).
        Some("INHERIT_ENV") => {
            if let Some(found) = parse_inherit_env_at(toks, pos)? {
                return Ok(found);
            }
        }
        _ => {}
    }
    // Generic instruction: consumes the whole run; unknown heads
    // (LET/RETURN/FUNC/IF/...) fail in lowering exactly like pest.
    let (kind, used) = parse_inner_command(&toks[pos..], stmt, ctx)?;
    Ok((kind, used))
}

/// Parse an `INHERIT_ENV [...]` key list over a bracket group's
/// inner tokens, mirroring `inherit_list` strictly: comma-separated
/// keys, gaps/newlines only around keys and commas (never inside:
/// keys are atomic), each key a joint run validated against the
/// `guard_key_char` exclusions (`: = ! , | ( ) ] $`, plus the `//`
/// and `/*` comment opens). Groups and strings render raw (pest
/// reads them as ordinary characters here); anything else misses
/// the shape (`None`: generic fails it in lowering like pest).
/// Statement-level `INHERIT_ENV` stays generic (slice 5 directives);
/// this serves wrapper inners, where the nesting check (or the
/// isolated rewrite) consumes the result.
fn parse_inherit_keys(inner: &[Tok]) -> Result<Option<Vec<String>>, V2Error> {
    let mut parts: Vec<&[Tok]> = Vec::new();
    let mut start = 0;
    for (i, tok) in inner.iter().enumerate() {
        if matches!(&tok.kind, TokKind::Punct(',')) {
            parts.push(&inner[start..i]);
            start = i + 1;
        }
    }
    parts.push(&inner[start..]);
    let mut out = Vec::new();
    for part in parts {
        // Newlines skip at the edges only, like `ws?`.
        let mut part = part;
        while matches!(part.first().map(|t| &t.kind), Some(TokKind::Newline)) {
            part = &part[1..];
        }
        while matches!(part.last().map(|t| &t.kind), Some(TokKind::Newline)) {
            part = &part[..part.len() - 1];
        }
        let Some(key) = render_inherit_key(part)? else {
            return Ok(None);
        };
        out.push(key);
    }
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some(out))
}

/// Render one inherit key: a single joint run (first token rides
/// any gap; the rest gapless, like the atomic `env_key`), validated
/// against the grammar exclusions. `None` on shape miss.
fn render_inherit_key(part: &[Tok]) -> Result<Option<String>, V2Error> {
    let mut out = String::new();
    for (i, tok) in part.iter().enumerate() {
        if i > 0 && !tok.joint {
            return Ok(None);
        }
        match &tok.kind {
            TokKind::Word(w) => out.push_str(w),
            TokKind::Punct(c) => out.push(*c),
            TokKind::Semicolon => out.push(';'),
            TokKind::Str { text, quote } => {
                out.push(*quote);
                out.push_str(text);
                out.push(*quote);
            }
            TokKind::BlockStr(full) => out.push_str(full),
            TokKind::Group {
                delim,
                inner,
                open_joint,
                close_joint,
            } => {
                let (open, close) = match delim {
                    GroupDelim::Bracket => ('[', ']'),
                    GroupDelim::Paren => ('(', ')'),
                    GroupDelim::Brace => ('{', '}'),
                };
                out.push_str(&render_group(open, close, inner, *open_joint, *close_joint));
            }
            TokKind::Newline => return Ok(None),
        }
    }
    if out.is_empty() {
        return Ok(None);
    }
    // `guard_key_char` exclusions: the single-char set plus the
    // comment opens (`//`, `/*`); lone `/` stays legal, like pest.
    if out.contains("//") || out.contains("/*") {
        return Ok(None);
    }
    if out
        .chars()
        .any(|c| matches!(c, ':' | '=' | '!' | ',' | '|' | '(' | ')' | ']' | '$'))
    {
        return Ok(None);
    }
    Ok(Some(out))
}

/// Strict `INHERIT_ENV [...]` at an element or single-inner
/// position: bracket riding gapped with valid keys yields the
/// directive (trailing elements loop on outside); anything else
/// misses (`None`) and generic fails it in lowering like pest.
/// Serves both levels so keys stay exact everywhere; slice 5 adds
/// only the prelude-order validation.
fn parse_inherit_env_at(toks: &[Tok], pos: usize) -> Result<Option<(StepKind, usize)>, V2Error> {
    // The bracket rides gapped (`sep`); anything else misses and
    // generic fails it in lowering like pest.
    let bracket = match toks.get(pos + 1) {
        Some(t)
            if !t.joint
                && matches!(
                    &t.kind,
                    TokKind::Group {
                        delim: GroupDelim::Bracket,
                        ..
                    }
                ) =>
        {
            Some(t)
        }
        _ => None,
    };
    let Some(bracket) = bracket else {
        return Ok(None);
    };
    let inner = match &bracket.kind {
        TokKind::Group { inner, .. } => inner,
        _ => unreachable!(),
    };
    match parse_inherit_keys(inner)? {
        Some(keys) => Ok(Some((StepKind::InheritEnv { keys }, 2))),
        None => Ok(None),
    }
}

/// Parse a brace block body into steps (wrapper inline bodies):
/// statements split and lower recursively, each keeping its own
/// guard; scope markers stay 0/0 (only io-block scopes mark).
fn parse_body_steps(inner: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<Vec<Step>, V2Error> {
    let mut out = Vec::new();
    for s in split_statements(inner)? {
        out.extend(lower_stmt_steps(&s, stmt, ctx)?);
    }
    Ok(out)
}

/// Single (non-block) `WITH_IO`: bindings plus an optional inner
/// command. `None` on shape miss (no/gapped bracket: generic fails
/// it in lowering like pest). A brace in single-inner position is
/// the grammar's pending-guard error, never an io-block; a bare
/// bracket leaks the transient `WithIoBlock` into the body exactly
/// like pest (probed).
fn parse_with_io_single(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<Option<(StepKind, usize)>, V2Error> {
    let bracket = match toks.get(pos + 1) {
        // The bracket rides gapped (`sep`); anything else misses the
        // shape and generic fails it in lowering like pest.
        Some(t)
            if !t.joint
                && matches!(
                    &t.kind,
                    TokKind::Group {
                        delim: GroupDelim::Bracket,
                        ..
                    }
                ) =>
        {
            t
        }
        _ => return Ok(None),
    };
    let inner = match &bracket.kind {
        TokKind::Group { inner, .. } => inner.clone(),
        _ => unreachable!(),
    };
    let bindings = match parse_io_bindings(&inner, stmt)? {
        Some(bindings) => bindings,
        None => return Ok(None),
    };
    let rest = pos + 2;
    // A brace directly after the bracket is the pending-guard error
    // in single-inner position (probed); templates fall to generic.
    if let Some(next) = toks.get(rest)
        && matches!(
            &next.kind,
            TokKind::Group {
                delim: GroupDelim::Brace,
                ..
            }
        )
        && !is_template_brace(next)
    {
        return Err(V2Error {
            stmt,
            msg: "'{' without a pending guard".to_string(),
        });
    }
    if rest >= toks.len() {
        // Bare bracket: the transient block node leaks into wrapper
        // bodies exactly like pest (statement level never reaches
        // here; the splitter holds it for `{`).
        return Ok(Some((StepKind::WithIoBlock { bindings }, 2)));
    }
    // A joint follower misses the inner `sep`: the bracket stands
    // bare (probed `TIMEOUT 10s WITH_IO [..]ECHO hi` yields the
    // transient node) and trailing elements loop on outside.
    if toks[rest].joint {
        return Ok(Some((StepKind::WithIoBlock { bindings }, 2)));
    }
    let (cmd, used) = parse_single_inner(
        toks,
        rest,
        stmt,
        ctx,
        InnerRules {
            allow_await: false,
            allow_with_io: true,
        },
    )?;
    let span = lowering_ctx(stmt);
    check_tty_wrapping(&bindings, Some(&cmd), &span).map_err(|e| V2Error {
        stmt,
        msg: e.to_string(),
    })?;
    let kind = StepKind::WithIo {
        bindings,
        cmd: Box::new(cmd),
    };
    crate::commands::reject_nested_inherit_env(&span, &kind).map_err(|e| V2Error {
        stmt,
        msg: e.to_string(),
    })?;
    Ok(Some((kind, 2 + used)))
}

/// Single (non-block) `TIMEOUT`: duration plus one inner command.
/// `None` on shape miss (bad duration, missing/joint inner: generic
/// fails it in lowering like pest). Blocks parse inline; the body
/// holds one step for single inners.
fn parse_timeout_single(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<Option<(StepKind, usize)>, V2Error> {
    let Some((duration, dused)) = parse_timeout_duration_at(toks, pos + 1, stmt)? else {
        return Ok(None);
    };
    let rest = pos + 1 + dused;
    let Some(next) = toks.get(rest) else {
        // `TIMEOUT 10s` alone is a generic instruction (lowering
        // fails it, probed), never a bare wrapper.
        return Ok(None);
    };
    if next.joint {
        return Ok(None);
    }
    let body = if matches!(
        &next.kind,
        TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        }
    ) && !is_template_brace(next)
    {
        let inner = match &next.kind {
            TokKind::Group { inner, .. } => inner.clone(),
            _ => unreachable!(),
        };
        let steps = parse_body_steps(&inner, stmt, ctx)?;
        (steps, 1)
    } else {
        let (cmd, used) = parse_single_inner(
            toks,
            rest,
            stmt,
            ctx,
            InnerRules {
                allow_await: true,
                allow_with_io: true,
            },
        )?;
        (
            vec![Step {
                guard: None,
                kind: cmd,
                scope_enter: 0,
                scope_exit: 0,
            }],
            used,
        )
    };
    let kind = StepKind::Timeout {
        duration,
        body: body.0,
    };
    let span = lowering_ctx(stmt);
    crate::commands::reject_nested_inherit_env(&span, &kind).map_err(|e| V2Error {
        stmt,
        msg: e.to_string(),
    })?;
    Ok(Some((kind, 1 + dused + body.1)))
}

/// Single (non-block) `ISOLATED`: one inner command or block; the
/// body rewrites nested `INHERIT_ENV` through the shared pass.
/// `None` on shape miss (missing/joint inner: generic fails it like
/// pest).
fn parse_isolated_single(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<Option<(StepKind, usize)>, V2Error> {
    let Some(next) = toks.get(pos + 1) else {
        return Ok(None);
    };
    if next.joint {
        return Ok(None);
    }
    let (mut body, used) = if matches!(
        &next.kind,
        TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        }
    ) && !is_template_brace(next)
    {
        let inner = match &next.kind {
            TokKind::Group { inner, .. } => inner.clone(),
            _ => unreachable!(),
        };
        (parse_body_steps(&inner, stmt, ctx)?, 1)
    } else {
        let (cmd, used) = parse_single_inner(
            toks,
            pos + 1,
            stmt,
            ctx,
            InnerRules {
                allow_await: true,
                allow_with_io: true,
            },
        )?;
        (
            vec![Step {
                guard: None,
                kind: cmd,
                scope_enter: 0,
                scope_exit: 0,
            }],
            used,
        )
    };
    crate::commands::rewrite_isolated_inherit(&mut body);
    Ok(Some((StepKind::Isolated { body }, 1 + used)))
}

/// Single (non-block) `ASYNC`: one inner command or block (blocks
/// admit a joint brace, like `async_statement_block`'s `sep?`).
/// Top-level `WITH_IO` steps fail through the shared reject; the
/// `LET` path never scans (probed: it accepts). `None` on shape
/// miss (missing/joint-single inner: generic fails it like pest).
fn parse_async_single(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<Option<(StepKind, usize)>, V2Error> {
    let Some(next) = toks.get(pos + 1) else {
        return Ok(None);
    };
    // Block form admits a joint brace (`ASYNC{...}`, probed); the
    // single form needs its `sep`.
    if matches!(
        &next.kind,
        TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        }
    ) && !is_template_brace(next)
    {
        let inner = match &next.kind {
            TokKind::Group { inner, .. } => inner.clone(),
            _ => unreachable!(),
        };
        let body = parse_body_steps(&inner, stmt, ctx)?;
        let span = lowering_ctx(stmt);
        crate::commands::reject_with_io_in_async(&span, &body).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
        })?;
        let kind = StepKind::AsyncBlock { body };
        crate::commands::reject_nested_inherit_env(&span, &kind).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
        })?;
        return Ok(Some((kind, 2)));
    }
    if next.joint {
        return Ok(None);
    }
    let (cmd, used) = parse_single_inner(
        toks,
        pos + 1,
        stmt,
        ctx,
        InnerRules {
            allow_await: false,
            allow_with_io: false,
        },
    )?;
    let body = vec![Step {
        guard: None,
        kind: cmd,
        scope_enter: 0,
        scope_exit: 0,
    }];
    let span = lowering_ctx(stmt);
    crate::commands::reject_with_io_in_async(&span, &body).map_err(|e| V2Error {
        stmt,
        msg: e.to_string(),
    })?;
    let kind = StepKind::AsyncBlock { body };
    crate::commands::reject_nested_inherit_env(&span, &kind).map_err(|e| V2Error {
        stmt,
        msg: e.to_string(),
    })?;
    Ok(Some((kind, 1 + used)))
}

// ---- Slice 3: LET/SET/RETURN + expressions ----
// Expression tiers over [`Tok`] mirror the pest precedence chain
// (or > and > comparison > ordering > add > mul > unary > atom);
// arithmetic/comparison lowering runs through the shared constructors
// (`make_arith`, `make_compare`, `apply_unary_neg`), so folding, RPN
// compilation, and overflow behavior are identical by construction,
// never re-derived.

/// Eat a one- or two-char operator: single puncts match directly;
/// pairs (`==`, `!=`, `<=`, `>=`, `&&`, `||`, `::`) require a joint
/// second punct. Newlines never skip here (expression mode).
fn eat_op(cur: &mut Cursor, first: char, second: Option<char>) -> bool {
    let (a, b) = match (cur.toks.get(cur.pos), cur.toks.get(cur.pos + 1)) {
        (Some(a), b) => (a, b),
        _ => return false,
    };
    if !matches!(&a.kind, TokKind::Punct(c) if *c == first) {
        return false;
    }
    match second {
        None => {
            cur.pos += 1;
            true
        }
        Some(c2) => match b {
            Some(t) if t.joint && matches!(&t.kind, TokKind::Punct(c) if *c == c2) => {
                cur.pos += 2;
                true
            }
            _ => false,
        },
    }
}

/// Dummy span for the shared lowering constructors: v2 owns no source
/// positions yet, so only success values (span-free) participate in
/// parity; failures surface through [`V2Error`] text.
fn lowering_ctx(stmt: usize) -> SpanContext<'static> {
    let _ = stmt;
    SpanContext::line_only(0)
}

fn parse_expr_or(cur: &mut Cursor, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    let mut left = parse_expr_and(cur, ctx)?;
    while eat_op(cur, '|', Some('|')) {
        let right = parse_expr_and(cur, ctx)?;
        // No folding, no boundary checks: mirrors the pest tier quirk.
        left = Expr::Logical {
            op: LogicalOp::Or,
            left: Box::new(left),
            right: Box::new(right),
        };
    }
    Ok(left)
}

fn parse_expr_and(cur: &mut Cursor, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    let mut left = parse_expr_cmp(cur, ctx)?;
    while eat_op(cur, '&', Some('&')) {
        let right = parse_expr_cmp(cur, ctx)?;
        reject_boundary(&lowering_ctx(cur.stmt), &left).map_err(|e| cur.err(e.to_string()))?;
        reject_boundary(&lowering_ctx(cur.stmt), &right).map_err(|e| cur.err(e.to_string()))?;
        left = Expr::Logical {
            op: LogicalOp::And,
            left: Box::new(left),
            right: Box::new(right),
        };
    }
    Ok(left)
}

fn parse_expr_cmp(cur: &mut Cursor, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    let left = parse_expr_ord(cur, ctx)?;
    let op = if eat_op(cur, '=', Some('=')) {
        CompareOp::Eq
    } else if eat_op(cur, '!', Some('=')) {
        CompareOp::Ne
    } else {
        return Ok(left);
    };
    let right = parse_expr_ord(cur, ctx)?;
    make_compare(&lowering_ctx(cur.stmt), op, left, right).map_err(|e| cur.err(e.to_string()))
}

fn parse_expr_ord(cur: &mut Cursor, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    let left = parse_expr_add(cur, ctx)?;
    let op = if eat_op(cur, '<', Some('=')) {
        CompareOp::Le
    } else if eat_op(cur, '>', Some('=')) {
        CompareOp::Ge
    } else if eat_op(cur, '<', None) {
        CompareOp::Lt
    } else if eat_op(cur, '>', None) {
        CompareOp::Gt
    } else {
        return Ok(left);
    };
    let right = parse_expr_add(cur, ctx)?;
    make_compare(&lowering_ctx(cur.stmt), op, left, right).map_err(|e| cur.err(e.to_string()))
}

fn parse_expr_add(cur: &mut Cursor, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    let mut left = parse_expr_mul(cur, ctx)?;
    loop {
        let op = if eat_op(cur, '+', None) {
            ArithOp::Add
        } else if eat_op(cur, '-', None) {
            ArithOp::Sub
        } else {
            break;
        };
        let right = parse_expr_mul(cur, ctx)?;
        left = make_arith(&lowering_ctx(cur.stmt), op, left, right)
            .map_err(|e| cur.err(e.to_string()))?;
    }
    Ok(left)
}

fn parse_expr_mul(cur: &mut Cursor, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    let mut left = parse_expr_unary(cur, ctx)?;
    loop {
        let op = if eat_op(cur, '*', None) {
            ArithOp::Mul
        } else if eat_op(cur, '/', None) {
            ArithOp::Div
        } else {
            break;
        };
        let right = parse_expr_unary(cur, ctx)?;
        left = make_arith(&lowering_ctx(cur.stmt), op, left, right)
            .map_err(|e| cur.err(e.to_string()))?;
    }
    Ok(left)
}

/// A `-` opens a bare word (never negation) when gaplessly followed by
/// a letter, `_`, or `/` — the `neg_op` lookahead, read off `joint`.
fn dash_is_bare(cur: &Cursor) -> bool {
    let next = cur.toks.get(cur.pos + 1);
    match next {
        Some(t) if t.joint => match &t.kind {
            TokKind::Word(w) => w
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_'),
            TokKind::Punct('/') => true,
            _ => false,
        },
        _ => false,
    }
}

fn parse_expr_unary(cur: &mut Cursor, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    let mut prefixes = Vec::new();
    loop {
        match cur.toks.get(cur.pos).map(|t| &t.kind) {
            Some(TokKind::Punct('!')) => {
                cur.pos += 1;
                prefixes.push(false);
            }
            Some(TokKind::Punct('-')) if !dash_is_bare(cur) => {
                cur.pos += 1;
                prefixes.push(true);
            }
            _ => break,
        }
    }
    let mut expr = parse_expr_atom(cur, ctx)?;
    // Innermost prefix is closest to the atom: apply in reverse order.
    for is_neg in prefixes.into_iter().rev() {
        if is_neg {
            expr = apply_unary_neg(&lowering_ctx(cur.stmt), expr)
                .map_err(|e| cur.err(e.to_string()))?;
        } else {
            reject_boundary(&lowering_ctx(cur.stmt), &expr).map_err(|e| cur.err(e.to_string()))?;
            expr = Expr::Not(Box::new(expr));
        }
    }
    Ok(expr)
}

/// Identifier charset for `$var` and `.segment` parts. Segments admit
/// digits-only too (`$a.0`); anything else ends the key path and errors
/// downstream, like the grammar's backtrack to `variable`.
fn is_ident_word(w: &str) -> bool {
    !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_key_segment(w: &str) -> bool {
    !w.is_empty()
        && (w.chars().all(|c| c.is_ascii_digit())
            || (w
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')))
}

fn is_env_key(w: &str) -> bool {
    !w.is_empty()
        && w.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && w.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Maximal joint run of words and `-/.'/'` puncts: pest `bare_word`
/// shape. Callers classify (`true`/`false`/string); the numeric path
/// runs first and falls back here on shape/follower mismatch.
/// Backslash escapes are arg-position syntax: pest `bare_word` admits
/// no `\`, so expression runs containing one fail here (never drift
/// into a string the grammar rejects).
fn expr_bare_run(cur: &mut Cursor, leading_dash: bool) -> Result<String, V2Error> {
    let mut out = String::new();
    if leading_dash {
        match cur.next() {
            Some(t) if matches!(&t.kind, TokKind::Punct('-')) => out.push('-'),
            _ => return Err(cur.err("missing expression operand")),
        }
    }
    match cur.next() {
        Some(t) => match &t.kind {
            TokKind::Word(w) => out.push_str(w),
            _ => return Err(cur.err("missing expression operand")),
        },
        None => return Err(cur.err("missing expression operand")),
    }
    loop {
        let next = match cur.toks.get(cur.pos) {
            Some(t) if t.joint => t,
            _ => break,
        };
        match &next.kind {
            TokKind::Word(w) => {
                // Abutting words only continue through an operator gap
                // the tiers own (`a -b` never fuses); without a joint
                // punct between, stop. Since this token IS joint, the
                // previous token must have been a bare punct.
                out.push_str(w);
                cur.pos += 1;
            }
            TokKind::Punct(c) if matches!(c, '-' | '.' | '/') => {
                out.push(*c);
                cur.pos += 1;
            }
            _ => break,
        }
    }
    if out.contains('\\') {
        return Err(cur.err("unexpected escape in expression"));
    }
    Ok(out)
}

/// Numeric literal head: digit-led word plus joint `.digits` tails.
/// Callers apply the separator-shape gate and the follower rule, then
/// the shared classifier.
fn number_head(cur: &mut Cursor) -> Option<String> {
    let first = match cur.toks.get(cur.pos).map(|t| &t.kind) {
        Some(TokKind::Word(w)) if w.chars().next().is_some_and(|c| c.is_ascii_digit()) => w.clone(),
        _ => return None,
    };
    let mut text = first;
    let mut pos = cur.pos + 1;
    loop {
        let dot = matches!(
            cur.toks.get(pos).map(|t| &t.kind),
            Some(TokKind::Punct('.'))
        ) && cur.toks.get(pos).is_some_and(|t| t.joint);
        let digits = cur.toks.get(pos + 1);
        let digits_ok = dot
            && matches!(digits.map(|t| &t.kind), Some(TokKind::Word(w)) if w.chars().all(|c| c.is_ascii_digit() || c == '_'))
            && digits.is_some_and(|t| t.joint);
        if !digits_ok {
            break;
        }
        text.push('.');
        if let Some(TokKind::Word(w)) = digits.map(|t| &t.kind) {
            text.push_str(w);
        }
        pos += 2;
    }
    Some(text)
}

/// `_`-separator shape gate mirroring the `numeric_literal` rule:
/// digits with single-underscore separators, at most one fraction.
fn numeric_shape_ok(text: &str) -> bool {
    let mut parts = text.split('.');
    let int_part = parts.next().unwrap_or("");
    let frac_part = parts.next();
    if parts.next().is_some() {
        return false;
    }
    fn run_ok(s: &str) -> bool {
        !s.is_empty()
            && s.chars().all(|c| c.is_ascii_digit() || c == '_')
            && !s.starts_with('_')
            && !s.ends_with('_')
            && !s.contains("__")
    }
    run_ok(int_part) && frac_part.is_none_or(run_ok)
}

/// Follower rule mirroring the `numeric_literal` trailing guard
/// (`!(ALNUM | "_" | "." | "/")`): a joint word, `.`, or `/` continues
/// into a bare word (`30s`, `1.`, `4/2`); `-`/`+`/operators let the
/// number stand for the tiers (`1-2` is `Sub`, like the grammar).
fn number_falls_to_bare(cur: &Cursor, consumed: usize) -> bool {
    match cur.toks.get(cur.pos + consumed) {
        Some(t) if t.joint => matches!(
            &t.kind,
            TokKind::Word(_) | TokKind::Punct('.') | TokKind::Punct('/')
        ),
        _ => false,
    }
}

fn parse_expr_atom(cur: &mut Cursor, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    match cur.toks.get(cur.pos).map(|t| &t.kind) {
        Some(TokKind::Group {
            delim: GroupDelim::Paren,
            inner,
            ..
        }) => {
            let inner = inner.clone();
            cur.pos += 1;
            let mut sub = Cursor::expr(&inner, cur.stmt);
            sub.skip_newlines();
            let expr = parse_expr_or(&mut sub, ctx)?;
            sub.skip_newlines();
            if !sub.done() {
                return Err(sub.err("unexpected text in parentheses"));
            }
            Ok(expr)
        }
        Some(TokKind::Word(_)) if call_shape_at(cur.toks, cur.pos, false).is_some() => {
            let (name, paren) =
                call_shape_at(cur.toks, cur.pos, false).ok_or_else(|| cur.err("unreachable"))?;
            let inner = match &cur.toks[paren].kind {
                TokKind::Group { inner, .. } => inner.clone(),
                _ => unreachable!(),
            };
            cur.pos = paren + 1;
            parse_expr_call(name, &inner, cur.stmt, ctx)
        }
        Some(TokKind::Punct('$')) => {
            cur.pos += 1;
            // Atomic `variable`: `$` and the name ride gapless (the
            // enclosing rules are compound-atomic).
            let base = match cur.next() {
                Some(t) if t.joint && matches!(&t.kind, TokKind::Word(w) if is_ident_word(w)) => {
                    match &t.kind {
                        TokKind::Word(w) => w.clone(),
                        _ => unreachable!(),
                    }
                }
                _ => return Err(cur.err("`$` requires a variable name")),
            };
            let mut keys = Vec::new();
            loop {
                // Key-path dots are gapless too (`~` without `gap`).
                let dot = matches!(
                    cur.toks.get(cur.pos).map(|t| &t.kind),
                    Some(TokKind::Punct('.'))
                ) && cur.toks.get(cur.pos).is_some_and(|t| t.joint);
                let seg = cur.toks.get(cur.pos + 1);
                let seg_ok = dot
                    && matches!(seg.map(|t| &t.kind), Some(TokKind::Word(w)) if is_key_segment(w))
                    && seg.is_some_and(|t| t.joint);
                if !seg_ok {
                    break;
                }
                if let Some(TokKind::Word(w)) = seg.map(|t| &t.kind) {
                    keys.push(w.clone());
                }
                cur.pos += 2;
            }
            if keys.is_empty() {
                Ok(Expr::Var(base))
            } else {
                Ok(Expr::KeyPath { base, keys })
            }
        }
        Some(TokKind::Word(w)) if w == "env" => {
            // `env:KEY` read when the full shape matches gaplessly
            // (atomic `env_read`); otherwise a bare word (`env` alone,
            // `env:k` lowercase) for the error path, exactly like the
            // grammar's mismatch.
            let key_ok = matches!(
                cur.toks.get(cur.pos + 1).map(|t| &t.kind),
                Some(TokKind::Punct(':'))
            ) && cur.toks.get(cur.pos + 1).is_some_and(|t| t.joint)
                && matches!(
                    cur.toks.get(cur.pos + 2).map(|t| &t.kind),
                    Some(TokKind::Word(k)) if is_env_key(k)
                )
                && cur.toks.get(cur.pos + 2).is_some_and(|t| t.joint);
            if !key_ok {
                return parse_bare_atom(cur, ctx, false);
            }
            cur.pos += 1;
            cur.pos += 1;
            match cur.next() {
                Some(t) => match &t.kind {
                    TokKind::Word(k) => Ok(Expr::Env(k.clone())),
                    _ => Err(cur.err("unreachable")),
                },
                None => Err(cur.err("unreachable")),
            }
        }
        Some(TokKind::Group {
            delim: GroupDelim::Bracket,
            inner,
            ..
        }) => {
            let inner = inner.clone();
            cur.pos += 1;
            parse_list_literal(&inner, cur.stmt, ctx)
        }
        Some(TokKind::Group {
            delim: GroupDelim::Brace,
            inner,
            ..
        }) => {
            let inner = inner.clone();
            cur.pos += 1;
            parse_map_or_block(&inner, cur.stmt, ctx)
        }
        Some(TokKind::Str { text, .. }) => {
            let text = text.clone();
            cur.pos += 1;
            Ok(Expr::Literal(Value::string(text)))
        }
        Some(TokKind::BlockStr(full)) => {
            let full = full.clone();
            cur.pos += 1;
            Ok(Expr::Literal(Value::string(
                crate::command::lower_block_string(&full),
            )))
        }
        Some(TokKind::Word(w)) if w.chars().next().is_some_and(|c| c.is_ascii_digit()) => {
            parse_numeric_atom(cur, ctx)
        }
        Some(TokKind::Word(_)) => parse_bare_atom(cur, ctx, false),
        Some(TokKind::Punct('-')) => parse_bare_atom(cur, ctx, true),
        _ => Err(cur.err("missing expression operand")),
    }
}

/// Bare-word atom with a `-` lead (the caller proved it is not
/// negation): consumes the joint `-/.'/'` run for the `bare_word`
/// shape.
fn parse_bare_atom(cur: &mut Cursor, _ctx: &V2Ctx, leading_dash: bool) -> Result<Expr, V2Error> {
    let text = expr_bare_run(cur, leading_dash)?;
    match text.as_str() {
        "true" => Ok(Expr::Literal(Value::bool(true))),
        "false" => Ok(Expr::Literal(Value::bool(false))),
        _ => Ok(Expr::Literal(Value::string(text))),
    }
}

/// Numeric atom: rejoin dotted tails, apply the shape gate and the
/// follower rule, then the shared classifier. Anything failing the
/// gate or follower reparses as a bare word (durations, versions,
/// paths), exactly like the grammar's `numeric_literal`-or-`bare_word`
/// order.
fn parse_numeric_atom(cur: &mut Cursor, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    let start = cur.pos;
    let text = number_head(cur).ok_or_else(|| cur.err("missing expression operand"))?;
    let consumed = {
        let mut n = 1;
        let mut p = start + 1;
        while matches!(cur.toks.get(p).map(|t| &t.kind), Some(TokKind::Punct('.')))
            && cur.toks.get(p).is_some_and(|t| t.joint)
            && matches!(
                cur.toks.get(p + 1).map(|t| &t.kind),
                Some(TokKind::Word(w)) if w.chars().all(|c| c.is_ascii_digit() || c == '_')
            )
            && cur.toks.get(p + 1).is_some_and(|t| t.joint)
        {
            n += 2;
            p += 2;
        }
        n
    };
    if !numeric_shape_ok(&text) || number_falls_to_bare(cur, consumed) {
        return parse_bare_atom(cur, ctx, false);
    }
    cur.pos += consumed;
    classify_numeric_text(&text).map_err(|msg| cur.err(msg))
}

/// Full expression with the top-level boundary rejection (mirrors
/// `parse_expr`): a stray `UnsignedIntBoundary` that no unary `-`
/// consumed is an overflow error.
fn parse_expr_checked(cur: &mut Cursor, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    let expr = parse_expr_or(cur, ctx)?;
    if matches!(expr, Expr::UnsignedIntBoundary(_)) {
        return Err(cur.err("integer overflow: 9223372036854775808 exceeds i64::MAX"));
    }
    Ok(expr)
}

/// List literal over a bracket group's contents: `gap_nl` newlines
/// skip at the edges and after commas only, mirroring the grammar.
fn parse_list_literal(inner: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    let mut cur = Cursor::expr(inner, stmt);
    cur.skip_newlines();
    if cur.done() {
        return Ok(Expr::List(vec![]));
    }
    let mut items = Vec::new();
    loop {
        let item = parse_expr_checked(&mut cur, ctx)?;
        reject_boundary(&lowering_ctx(stmt), &item).map_err(|e| cur.err(e.to_string()))?;
        items.push(item);
        cur.skip_newlines();
        if cur.eat_punct(',') {
            cur.skip_newlines();
            continue;
        }
        break;
    }
    if !cur.done() {
        return Err(cur.err("unexpected text in list"));
    }
    Ok(Expr::List(items))
}

/// Map literal attempt over a brace group's contents: comma-separated
/// `key: value` entries with `gap` (never newline) around the colon.
/// Any entry failing the shape is not a map: the caller reports the
/// deferred block slice (the grammar tries `block` next, which fails
/// the same inputs at this stage).
fn parse_map_literal(inner: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    let mut entries = Vec::new();
    // Empty (or newline-only) braces are an empty map.
    if inner.iter().all(|t| matches!(&t.kind, TokKind::Newline)) {
        return Ok(Expr::Map(vec![]));
    }
    for part in split_top_commas(inner) {
        let mut sub = Cursor::expr(part, stmt);
        sub.skip_newlines();
        let key = match sub.toks.get(sub.pos).map(|t| &t.kind) {
            Some(TokKind::Str { text, .. }) => {
                let key = text.clone();
                sub.pos += 1;
                key
            }
            Some(TokKind::BlockStr(full)) => {
                let key = crate::command::lower_block_string(full);
                sub.pos += 1;
                key
            }
            Some(TokKind::Word(_)) | Some(TokKind::Punct('-')) => map_bare_key(&mut sub)?,
            _ => return Err(sub.err("not a map literal")),
        };
        if !sub.eat_punct(':') {
            return Err(sub.err("not a map literal"));
        }
        let val = parse_expr_checked(&mut sub, ctx)?;
        reject_boundary(&lowering_ctx(stmt), &val).map_err(|e| sub.err(e.to_string()))?;
        sub.skip_newlines();
        if !sub.done() {
            return Err(sub.err("not a map literal"));
        }
        entries.push((key, val));
    }
    Ok(Expr::Map(entries))
}

/// Bare map key: joint `-/.'/'`-run, mirroring `bare_word` charset
/// (no bool/numeric classification: keys are always strings).
fn map_bare_key(sub: &mut Cursor) -> Result<String, V2Error> {
    let mut out = String::new();
    if matches!(
        sub.toks.get(sub.pos).map(|t| &t.kind),
        Some(TokKind::Punct('-'))
    ) {
        out.push('-');
        sub.pos += 1;
    }
    match sub.toks.get(sub.pos).map(|t| &t.kind) {
        Some(TokKind::Word(w)) => {
            out.push_str(w);
            sub.pos += 1;
        }
        _ => return Err(sub.err("not a map literal")),
    }
    while let Some(t) = sub.toks.get(sub.pos) {
        if !t.joint {
            break;
        }
        match &t.kind {
            TokKind::Word(w) => {
                out.push_str(w);
                sub.pos += 1;
            }
            TokKind::Punct(c) if matches!(c, '-' | '.' | '/') => {
                out.push(*c);
                sub.pos += 1;
            }
            _ => break,
        }
    }
    if out.contains('\\') {
        return Err(sub.err("unexpected escape in map key"));
    }
    Ok(out)
}

fn parse_map_or_block(inner: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    match parse_map_literal(inner, stmt, ctx) {
        Ok(map) => Ok(map),
        Err(_) => Err(V2Error {
            stmt,
            msg: "unsupported in parse_v2: inline blocks (later slice)".to_string(),
        }),
    }
}

/// Assignment `=`: the first `=` neither glued to a following `=`
/// (`==`) nor glued after `!`/`<`/`>`/`=` (`!=`, `<=`, `>=`, second of
/// `==`). Glued-ness reads off the `=`'s own `joint` (gap before it)
/// and the follower's (gap after it). Anything else leaves the
/// statement bare (`LET $p: PIPE`), exactly where the grammar's `"="`
/// match fails.
fn find_assign_eq(body: &[Tok]) -> Option<usize> {
    for (i, tok) in body.iter().enumerate() {
        if !matches!(&tok.kind, TokKind::Punct('=')) {
            continue;
        }
        let next_eq = body
            .get(i + 1)
            .is_some_and(|t| t.joint && matches!(&t.kind, TokKind::Punct('=')));
        let prev_op = tok.joint
            && i > 0
            && matches!(&body[i - 1].kind, TokKind::Punct('!' | '<' | '>' | '='));
        if !next_eq && !prev_op {
            return Some(i);
        }
    }
    None
}

/// Render a type region joint-aware (`LIST< MAP >` keeps its gaps for
/// the shared canonicalizer, exactly like `type_tag.as_str()`).
fn render_type_region(toks: &[Tok], stmt: usize) -> Result<String, V2Error> {
    let mut out = String::new();
    for tok in toks {
        if !tok.joint && !out.is_empty() {
            out.push(' ');
        }
        match &tok.kind {
            TokKind::Word(w) => out.push_str(w),
            TokKind::Punct(c) => out.push(*c),
            _ => {
                return Err(V2Error {
                    stmt,
                    msg: "malformed type in LET".to_string(),
                });
            }
        }
    }
    if out.is_empty() {
        return Err(V2Error {
            stmt,
            msg: "LET requires explicit type: LET $var: TYPE = <expr>".to_string(),
        });
    }
    Ok(crate::canonicalize_spelling(&out))
}

fn expect_dollar_var(cur: &mut Cursor, msg: &str) -> Result<String, V2Error> {
    // Atomic `dollar_ident`: no gap between `$` and the name (the
    // enclosing statements are compound-atomic, so implicit spacing is
    // off and only explicit `gap`s admit whitespace).
    match cur.next() {
        Some(t) if matches!(&t.kind, TokKind::Punct('$')) => {}
        _ => return Err(cur.err(msg)),
    }
    match cur.next() {
        Some(t) if t.joint && matches!(&t.kind, TokKind::Word(w) if is_ident_word(w)) => {
            match &t.kind {
                TokKind::Word(w) => Ok(w.clone()),
                _ => unreachable!(),
            }
        }
        _ => Err(cur.err(msg)),
    }
}

fn parse_let_body(body: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<(StepKind, usize), V2Error> {
    let mut cur = Cursor::expr(body, stmt);
    match cur.next() {
        Some(t) if matches!(&t.kind, TokKind::Word(w) if w == "LET") => {}
        _ => return Err(cur.err("unreachable")),
    }
    let var = expect_dollar_var(&mut cur, "LET requires a variable")?;
    if !cur.eat_punct(':') {
        return Err(cur.err("LET requires explicit type: LET $var: TYPE = <expr>"));
    }
    // Type region runs to the assignment `=`; it cannot contain one.
    let type_start = cur.pos;
    let assign = find_assign_eq(&body[type_start..]).map(|i| i + type_start);
    let Some(eq) = assign else {
        // Bare `LET $p: PIPE` mints a fresh pipe and ends with the
        // type word (trailing elements loop on, probed); every other
        // bare type is a validation error, mirroring the grammar.
        if matches!(
            body.get(type_start).map(|t| &t.kind),
            Some(TokKind::Word(w)) if w == "PIPE"
        ) {
            return Ok((
                StepKind::Assign {
                    var,
                    decl_type: "PIPE".to_string(),
                    expr: Expr::FreshPipe,
                },
                type_start + 1,
            ));
        }
        return Err(cur.err(
            "LET requires an expression: LET $var: TYPE = <expr> (only LET $p: PIPE omits the initializer)",
        ));
    };
    let decl_type = render_type_region(&body[type_start..eq], stmt)?;
    let (kind, rhs_used) = parse_let_rhs(&body[eq + 1..], var, decl_type, stmt, ctx)?;
    Ok((kind, eq + 1 + rhs_used))
}

/// LET right-hand side dispatch, mirroring the grammar order
/// (`let_async`, then `let_capture`, then `let_statement`):
/// `WITH_IO`-led lines branch async/sync like
/// `parse_let_async_statement_from_pair`, `ASYNC`-led lines spawn
/// tasks, `TIMEOUT`-led lines capture through the wrapper,
/// `AWAIT $v` captures, RUN exec-form captures, known commands
/// capture through the generic parser (lowering failures fatal,
/// exactly like the committed grammar alternative), and anything
/// else parses as an expression (unknown leads included). Returns
/// the kind and RHS tokens consumed: captures ending in structural
/// pairs leave trailing elements for the caller, exactly like pest.
fn parse_let_rhs(
    rhs: &[Tok],
    var: String,
    decl_type: String,
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<(StepKind, usize), V2Error> {
    let head_word = match rhs.first().map(|t| &t.kind) {
        Some(TokKind::Word(w)) => Some(w.clone()),
        _ => None,
    };
    // `WITH_IO`-led lines share `let_async_statement` with the
    // `ASYNC` forms (the grammar claims every WITH_IO-led LET
    // there); shape misses fall through to generic below.
    if head_word.as_deref() == Some("WITH_IO")
        && let Some(found) = try_let_with_io_rhs(rhs, var.clone(), decl_type.clone(), stmt, ctx)?
    {
        return Ok(found);
    }
    // `ASYNC`-led lines (with followers; alone they commit to the
    // generic path like the grammar, failing in lowering) spawn
    // background tasks. No `WITH_IO` scan here (probed: the LET
    // path accepts what statement `ASYNC` rejects).
    if head_word.as_deref() == Some("ASYNC") && rhs.len() > 1 {
        let (kind, used) = parse_let_async_rhs(&rhs[1..], var, decl_type, stmt, ctx)?;
        return Ok((kind, 1 + used));
    }
    // `TIMEOUT`-led lines capture through the wrapper (shape misses
    // fall through to generic, which fails in lowering like pest).
    if head_word.as_deref() == Some("TIMEOUT")
        && let Some((kind, used)) = parse_timeout_single(rhs, 0, stmt, ctx)?
    {
        let span = lowering_ctx(stmt);
        crate::commands::reject_async_in_capture(&span, &kind).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
        })?;
        crate::commands::reject_pipe_stdout_in_capture(&span, &kind).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
        })?;
        return Ok((
            StepKind::AssignCapture {
                var,
                decl_type,
                cmd: Box::new(kind),
            },
            used,
        ));
    }
    // `AWAIT $v` captures the task output (`$`+name ride gapless in
    // this atomic position, like the grammar); anything else
    // `AWAIT`-led goes generic below (lowering fails it, probed).
    if head_word.as_deref() == Some("AWAIT")
        && rhs.len() == 3
        && let (Some(dollar), Some(task)) = (rhs.get(1), rhs.get(2))
        && matches!(&dollar.kind, TokKind::Punct('$'))
        && task.joint
        && let TokKind::Word(task_var) = &task.kind
        && is_ident_word(task_var)
    {
        return Ok((
            StepKind::AwaitCapture {
                out_var: var,
                out_type: decl_type,
                task_var: task_var.clone(),
            },
            3,
        ));
    }
    // RUN exec form captures the structured argv (gapped bracket;
    // a joint bracket falls through to generic, which fails it in
    // lowering like pest).
    if head_word.as_deref() == Some("RUN")
        && matches!(
            rhs.get(1).map(|t| (&t.kind, t.joint)),
            Some((
                TokKind::Group {
                    delim: GroupDelim::Bracket,
                    ..
                },
                false
            ))
        )
        && rhs.len() == 2
    {
        let inner = match &rhs[1].kind {
            TokKind::Group { inner, .. } => inner.clone(),
            _ => unreachable!(),
        };
        let list = parse_run_exec_list(&inner, stmt, ctx)?;
        let kind = (ctx.lower)("RUN", vec![Arg::Expr(list)]).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
        })?;
        return Ok((
            StepKind::AssignCapture {
                var,
                decl_type,
                cmd: Box::new(kind),
            },
            2,
        ));
    }
    // Known-command leads capture through the generic parser
    // (lowering failures are fatal here, exactly like the committed
    // grammar alternative: probed wrapper-alone and `ISOLATED`-led
    // shapes error in lowering); unknown leads parse as expressions.
    match &head_word {
        Some(head) if is_known_command(head) => {
            let (cmd, used) = parse_inner_command(rhs, stmt, ctx)?;
            let span = lowering_ctx(stmt);
            crate::commands::reject_async_in_capture(&span, &cmd).map_err(|e| V2Error {
                stmt,
                msg: e.to_string(),
            })?;
            crate::commands::reject_pipe_stdout_in_capture(&span, &cmd).map_err(|e| V2Error {
                stmt,
                msg: e.to_string(),
            })?;
            // `INHERIT_ENV`-led lines land here (generic): the
            // nesting check rejects them exactly like the finish
            // validator (probed).
            crate::commands::reject_nested_inherit_env(&span, &cmd).map_err(|e| V2Error {
                stmt,
                msg: e.to_string(),
            })?;
            return Ok((
                StepKind::AssignCapture {
                    var,
                    decl_type,
                    cmd: Box::new(cmd),
                },
                used,
            ));
        }
        _ => {}
    }
    let mut cur = Cursor::expr(rhs, stmt);
    let expr = parse_expr_checked(&mut cur, ctx)?;
    let used = cur.pos;
    Ok((
        StepKind::Assign {
            var,
            decl_type,
            expr,
        },
        used,
    ))
}

/// `WITH_IO`-led LET right-hand side, mirroring
/// `parse_let_async_statement_from_pair`'s `with_io_command` branch:
/// wrapping `ASYNC` binds a pipe-wired background task (multi-step
/// blocks fail loudly), wrapping a sync command captures its stdout
/// (async bodies and `stdout` pipes fail loudly), and a bare
/// bracket fails validation. `None` on shape miss (bad bracket:
/// the caller goes generic, which fails in lowering like pest).
fn try_let_with_io_rhs(
    rhs: &[Tok],
    var: String,
    decl_type: String,
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<Option<(StepKind, usize)>, V2Error> {
    let bracket = match rhs.get(1) {
        Some(t)
            if !t.joint
                && matches!(
                    &t.kind,
                    TokKind::Group {
                        delim: GroupDelim::Bracket,
                        ..
                    }
                ) =>
        {
            t
        }
        _ => return Ok(None),
    };
    let inner = match &bracket.kind {
        TokKind::Group { inner, .. } => inner.clone(),
        _ => unreachable!(),
    };
    let bindings = match parse_io_bindings(&inner, stmt)? {
        Some(bindings) => bindings,
        None => return Ok(None),
    };
    let fail = |msg: String| V2Error { stmt, msg };
    let rest = &rhs[2..];
    let Some(first) = rest.first() else {
        // Bare bracket: the grammar's committed validation error.
        return Err(fail(
            "LET $var: TYPE = WITH_IO requires an ASYNC command (e.g. LET $t = WITH_IO [stdin=$p] ASYNC WRITE \"f\")"
                .to_string(),
        ));
    };
    if first.joint {
        // The inner rides after a `sep`: joint followers miss the
        // shape and fail validation like the grammar's bare bracket.
        return Err(fail(
            "LET $var: TYPE = WITH_IO requires an ASYNC command (e.g. LET $t = WITH_IO [stdin=$p] ASYNC WRITE \"f\")"
                .to_string(),
        ));
    }
    if matches!(&first.kind, TokKind::Word(w) if w == "ASYNC") {
        // `WITH_IO [..] ASYNC <single|block>`: the bindings apply
        // inside the task thread (same shape as a one-step body
        // holding one `WITH_IO` step, which the runtime supports).
        // A bare `ASYNC` goes generic and fails in lowering
        // (`unknown command: ASYNC`, probed).
        let after = &rest[1..];
        if after.is_empty() {
            // Bare `ASYNC`: the grammar commits to `instruction`
            // and lowering fails it (`unknown command: ASYNC`,
            // probed); mirror by running generic, staying failed
            // even under a lenient dispatcher.
            match parse_inner_command(&rest[..1], stmt, ctx) {
                Err(e) => return Err(e),
                Ok(_) => {
                    return Err(fail("LET $var: HANDLE = ASYNC requires a body".to_string()));
                }
            }
        }
        if let Some(second) = after.first()
            && matches!(
                &second.kind,
                TokKind::Group {
                    delim: GroupDelim::Brace,
                    ..
                }
            )
            && !is_template_brace(second)
        {
            let brace_inner = match &second.kind {
                TokKind::Group { inner, .. } => inner.clone(),
                _ => unreachable!(),
            };
            let body = parse_body_steps(&brace_inner, stmt, ctx)?;
            if body.len() != 1 {
                return Err(fail(
                        "LET $var: TYPE = WITH_IO [..] ASYNC accepts a single command; use LET $var: HANDLE = ASYNC {{ ... }} with WITH_IO inside the block for multi-step tasks"
                            .to_string(),
                    ));
            }
            let wrapped = Step {
                guard: None,
                kind: StepKind::WithIo {
                    bindings,
                    cmd: Box::new(
                        body.into_iter()
                            .next()
                            .ok_or_else(|| {
                                fail("LET $var: HANDLE = ASYNC requires a body".to_string())
                            })?
                            .kind,
                    ),
                },
                scope_enter: 0,
                scope_exit: 0,
            };
            return Ok(Some((
                StepKind::AssignAsync {
                    var,
                    decl_type,
                    body: vec![wrapped],
                },
                2 + 2,
            )));
        }
        if after.first().is_some_and(|t| t.joint) {
            // Joint followers miss the inner `sep`: same validation
            // error as the bare bracket.
            return Err(fail(
                "LET $var: TYPE = WITH_IO requires an ASYNC command (e.g. LET $t = WITH_IO [stdin=$p] ASYNC WRITE \"f\")"
                    .to_string(),
            ));
        }
        let (cmd, used) = parse_single_inner(
            rest,
            1,
            stmt,
            ctx,
            InnerRules {
                allow_await: false,
                allow_with_io: false,
            },
        )?;
        let wrapped = Step {
            guard: None,
            kind: StepKind::WithIo {
                bindings,
                cmd: Box::new(cmd),
            },
            scope_enter: 0,
            scope_exit: 0,
        };
        let kind = StepKind::AssignAsync {
            var,
            decl_type,
            body: vec![wrapped],
        };
        let span = lowering_ctx(stmt);
        crate::commands::reject_nested_inherit_env(&span, &kind).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
        })?;
        return Ok(Some((kind, 2 + 1 + used)));
    }
    if matches!(
        &first.kind,
        TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        }
    ) {
        // A brace is no sync command: same validation error (probed).
        return Err(fail(
            "LET $var: TYPE = WITH_IO requires an ASYNC command (e.g. LET $t = WITH_IO [stdin=$p] ASYNC WRITE \"f\")"
                .to_string(),
        ));
    }
    // Sync inner: capture with the shared checks (the capture sink
    // owns stdout; async never runs inline).
    let (cmd, used) = parse_single_inner(
        rest,
        0,
        stmt,
        ctx,
        InnerRules {
            allow_await: false,
            allow_with_io: true,
        },
    )?;
    let span = lowering_ctx(stmt);
    if crate::commands::has_stdout_pipe(&bindings) {
        return Err(fail(
            "LET capture cannot use WITH_IO [stdout=$var]; the capture sink owns stdout"
                .to_string(),
        ));
    }
    crate::commands::reject_async_in_capture(&span, &cmd).map_err(|e| V2Error {
        stmt,
        msg: e.to_string(),
    })?;
    Ok(Some((
        StepKind::AssignCapture {
            var,
            decl_type,
            cmd: Box::new(StepKind::WithIo {
                bindings,
                cmd: Box::new(cmd),
            }),
        },
        2 + used,
    )))
}

/// `ASYNC`-led LET right-hand side (the tokens after `ASYNC`):
/// a block or single command becomes the task body. No `WITH_IO`
/// scan (probed: this path accepts what statement `ASYNC`
/// rejects); nesting still fails loudly through the shared check.
fn parse_let_async_rhs(
    rest: &[Tok],
    var: String,
    decl_type: String,
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<(StepKind, usize), V2Error> {
    let Some(first) = rest.first() else {
        return Err(V2Error {
            stmt,
            msg: "LET $var: HANDLE = ASYNC requires a body".to_string(),
        });
    };
    if matches!(
        &first.kind,
        TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        }
    ) && !is_template_brace(first)
    {
        let inner = match &first.kind {
            TokKind::Group { inner, .. } => inner.clone(),
            _ => unreachable!(),
        };
        let body = parse_body_steps(&inner, stmt, ctx)?;
        let kind = StepKind::AssignAsync {
            var,
            decl_type,
            body,
        };
        let span = lowering_ctx(stmt);
        crate::commands::reject_nested_inherit_env(&span, &kind).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
        })?;
        return Ok((kind, 1));
    }
    if first.joint {
        return Err(V2Error {
            stmt,
            msg: "LET $var: HANDLE = ASYNC requires a body".to_string(),
        });
    }
    let (cmd, used) = parse_single_inner(
        rest,
        0,
        stmt,
        ctx,
        InnerRules {
            allow_await: false,
            allow_with_io: false,
        },
    )?;
    let kind = StepKind::AssignAsync {
        var,
        decl_type,
        body: vec![Step {
            guard: None,
            kind: cmd,
            scope_enter: 0,
            scope_exit: 0,
        }],
    };
    let span = lowering_ctx(stmt);
    crate::commands::reject_nested_inherit_env(&span, &kind).map_err(|e| V2Error {
        stmt,
        msg: e.to_string(),
    })?;
    Ok((kind, used))
}

fn parse_set_body(body: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<(StepKind, usize), V2Error> {
    let mut cur = Cursor::expr(body, stmt);
    let var = expect_dollar_var(&mut cur, "mutation requires a variable: $var = <expr>")?;
    let assign = find_assign_eq(&body[cur.pos..]).map(|i| i + cur.pos);
    let eq = match assign {
        Some(eq) => eq,
        None => {
            return Err(cur.err("mutation requires a variable: $var = <expr>"));
        }
    };
    cur.pos = eq + 1;
    let expr = parse_expr_checked(&mut cur, ctx)?;
    Ok((StepKind::Set { var, expr }, cur.pos))
}

fn parse_return_body(body: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<(StepKind, usize), V2Error> {
    let mut cur = Cursor::expr(body, stmt);
    match cur.next() {
        Some(t) if matches!(&t.kind, TokKind::Word(w) if w == "RETURN") => {}
        _ => return Err(cur.err("unreachable")),
    }
    if cur.done() {
        return Ok((
            StepKind::Return {
                expr: Box::new(Expr::Literal(Value::string(String::new()))),
            },
            cur.pos,
        ));
    }
    let expr = parse_expr_checked(&mut cur, ctx)?;
    Ok((
        StepKind::Return {
            expr: Box::new(expr),
        },
        cur.pos,
    ))
}

/// Lower one statement to steps through the element loop: the
/// statement guard attaches to the first element only (trailing
/// elements ride guardless, probed), mirroring how pest's pending
/// guard clears after the first command token.
fn lower_stmt_steps(stmt: &Stmt, idx: usize, ctx: &V2Ctx) -> Result<Vec<Step>, V2Error> {
    let guard = parse_guard_groups(&stmt.guard, idx)?;
    parse_elements(&stmt.body, idx, ctx, guard)
}

/// Parse consecutive elements to end of run (the grammar's
/// `element*`): structural pairs (calls, RUN-exec, await/cancel,
/// wrapper blocks) end mid-run and trailing elements form following
/// steps; instructions and expressions consume greedily to the end.
fn parse_elements(
    toks: &[Tok],
    stmt: usize,
    ctx: &V2Ctx,
    guard: Option<GuardExpr>,
) -> Result<Vec<Step>, V2Error> {
    let mut steps = Vec::new();
    let mut pos = 0;
    let mut first_guard = Some(guard);
    while pos < toks.len() {
        let guard = first_guard.take().flatten();
        let (mut got, used) = parse_element_at(toks, pos, stmt, ctx, guard)?;
        if used == 0 {
            return Err(V2Error {
                stmt,
                msg: "element consumed no input".to_string(),
            });
        }
        steps.append(&mut got);
        pos += used;
    }
    Ok(steps)
}

/// Parse one element at `pos`, returning built steps (guard already
/// assigned: `first_guard` for the first, folded per io-block inner
/// step) and tokens consumed.
fn parse_element_at(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
    ctx: &V2Ctx,
    first_guard: Option<GuardExpr>,
) -> Result<(Vec<Step>, usize), V2Error> {
    let step = |kind: StepKind| Step {
        guard: first_guard.clone(),
        kind,
        scope_enter: 0,
        scope_exit: 0,
    };
    let head_word = match toks.get(pos).map(|t| &t.kind) {
        Some(TokKind::Word(w)) => Some(w.clone()),
        _ => None,
    };
    // Leading `{...}` blocks are slice-5 statements (guard blocks,
    // top-level bodies): defer loudly instead of drifting. The
    // `WITH_IO` io-block merge happens in the splitter, and wrapper
    // inline bodies never reach here, so every brace at element
    // level is out of slice.
    if matches!(
        toks.get(pos).map(|t| &t.kind),
        Some(TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        })
    ) {
        return Err(V2Error {
            stmt,
            msg: "unsupported in parse_v2: block statement (later slice)".to_string(),
        });
    }
    // LET/SET/RETURN heads parse with groups intact, then trailing
    // elements loop on (probed `LET $x: INT = 1 ECHO bye`).
    if head_word.as_deref() == Some("LET") {
        let (kind, used) = parse_let_body(&toks[pos..], stmt, ctx)?;
        return Ok((vec![step(kind)], used));
    }
    if matches!(toks.get(pos).map(|t| &t.kind), Some(TokKind::Punct('$'))) {
        let (kind, used) = parse_set_body(&toks[pos..], stmt, ctx)?;
        return Ok((vec![step(kind)], used));
    }
    if head_word.as_deref() == Some("RETURN") {
        let (kind, used) = parse_return_body(&toks[pos..], stmt, ctx)?;
        return Ok((vec![step(kind)], used));
    }
    // Calls route before heads (even known commands take the call
    // path with a joint paren, probed `ECHO("hi")`); the call ends
    // mid-run and trailing elements loop on (fixes the slice-4a
    // divergence, probed).
    if call_shape_at(toks, pos, true).is_some() {
        let (kind, used) = parse_inner_command(&toks[pos..], stmt, ctx)?;
        return Ok((vec![step(kind)], used));
    }
    // Wrappers (block or single); shape misses fall to generic.
    match head_word.as_deref() {
        Some("WITH_IO") => {
            return parse_with_io_element(toks, pos, stmt, ctx, first_guard);
        }
        Some("TIMEOUT") => {
            if let Some((kind, used)) = parse_timeout_single(toks, pos, stmt, ctx)? {
                return Ok((vec![step(kind)], used));
            }
        }
        Some("ISOLATED") => {
            if let Some((kind, used)) = parse_isolated_single(toks, pos, stmt, ctx)? {
                return Ok((vec![step(kind)], used));
            }
        }
        Some("ASYNC") => {
            if let Some((kind, used)) = parse_async_single(toks, pos, stmt, ctx)? {
                return Ok((vec![step(kind)], used));
            }
        }
        _ => {}
    }
    // AWAIT/CANCEL statements (element level admits the `$` gap,
    // probed); heads without `$` go generic (lowering fails them).
    if matches!(head_word.as_deref(), Some("AWAIT") | Some("CANCEL"))
        && let Some((kind, used)) = parse_await_cancel_at(toks, pos, stmt, false)?
    {
        return Ok((vec![step(kind)], used));
    }
    // `INHERIT_ENV [...]` parses strictly even at element level
    // (keys stay exact everywhere; slice 5 adds only the
    // prelude-order validation). Shape misses go generic.
    if head_word.as_deref() == Some("INHERIT_ENV")
        && let Some((kind, used)) = parse_inherit_env_at(toks, pos)?
    {
        return Ok((vec![step(kind)], used));
    }
    // Deferred structural shapes fail loudly, never drift (their
    // bodies arrive slice 5).
    if let Some(head) = &head_word
        && crate::STRUCTURAL_KEYWORDS.contains(&head.as_str())
    {
        return Err(V2Error {
            stmt,
            msg: format!("unsupported in parse_v2: `{head}` (later slice)"),
        });
    }
    // RUN-exec ends mid-run (trailing loops on, probed); generic
    // instructions consume everything.
    let (kind, used) = parse_inner_command(&toks[pos..], stmt, ctx)?;
    Ok((vec![step(kind)], used))
}

/// Element-level `WITH_IO`: a same-run brace (any gap: `]{` works,
/// probed) opens the io-block form, wrapping every inner step with
/// scope markers; a joint non-brace follower is the pending-guard
/// error (the splitter only holds bare brackets); anything else is
/// the single form. Shape misses go generic.
fn parse_with_io_element(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
    ctx: &V2Ctx,
    first_guard: Option<GuardExpr>,
) -> Result<(Vec<Step>, usize), V2Error> {
    // The bracket rides gapped (`sep`); anything else misses the
    // shape and generic fails it in lowering like pest.
    let bracket = match toks.get(pos + 1) {
        Some(t)
            if !t.joint
                && matches!(
                    &t.kind,
                    TokKind::Group {
                        delim: GroupDelim::Bracket,
                        ..
                    }
                ) =>
        {
            Some(t)
        }
        _ => None,
    };
    let Some(bracket) = bracket else {
        // Shape miss: generic fails it in lowering like pest.
        let (kind, used) = parse_inner_command(&toks[pos..], stmt, ctx)?;
        return Ok((
            vec![Step {
                guard: first_guard,
                kind,
                scope_enter: 0,
                scope_exit: 0,
            }],
            used,
        ));
    };
    let inner = match &bracket.kind {
        TokKind::Group { inner, .. } => inner.clone(),
        _ => unreachable!(),
    };
    let bindings = match parse_io_bindings(&inner, stmt)? {
        Some(bindings) => bindings,
        // Shape miss: generic fails it like pest.
        None => {
            let (kind, used) = parse_inner_command(&toks[pos..], stmt, ctx)?;
            return Ok((
                vec![Step {
                    guard: first_guard,
                    kind,
                    scope_enter: 0,
                    scope_exit: 0,
                }],
                used,
            ));
        }
    };
    let rest = pos + 2;
    // Same-run brace opens the io-block form (block-expanded
    // bindings skip the tty checks: the runtime repeats them).
    if let Some(next) = toks.get(rest)
        && matches!(
            &next.kind,
            TokKind::Group {
                delim: GroupDelim::Brace,
                ..
            }
        )
        && !is_template_brace(next)
    {
        let brace_inner = match &next.kind {
            TokKind::Group { inner, .. } => inner.clone(),
            _ => unreachable!(),
        };
        let mut steps = parse_body_steps(&brace_inner, stmt, ctx)?;
        if steps.is_empty() {
            return Err(V2Error {
                stmt,
                msg: "WITH_IO block must contain at least one command".to_string(),
            });
        }
        // Every inner step wraps (merging when already wrapped,
        // like nested io scopes); the line guard folds into each
        // inner guard, inner-first (probed fold order).
        for s in &mut steps {
            let merged = match std::mem::replace(&mut s.kind, StepKind::Continue) {
                StepKind::WithIo {
                    bindings: inner_bindings,
                    cmd,
                } => StepKind::WithIo {
                    bindings: crate::commands::merge_bindings(&bindings, &inner_bindings),
                    cmd,
                },
                other => StepKind::WithIo {
                    bindings: bindings.clone(),
                    cmd: Box::new(other),
                },
            };
            s.kind = merged;
            s.guard = crate::commands::and_guard_exprs(s.guard.take(), first_guard.clone());
        }
        let span = lowering_ctx(stmt);
        for s in &steps {
            crate::commands::reject_nested_inherit_env(&span, &s.kind).map_err(|e| V2Error {
                stmt,
                msg: e.to_string(),
            })?;
        }
        if let Some(first) = steps.first_mut() {
            first.scope_enter += 1;
        }
        if let Some(last) = steps.last_mut() {
            last.scope_exit += 1;
        }
        return Ok((steps, rest + 1 - pos));
    }
    if rest >= toks.len() {
        // Unreachable: the splitter holds bare brackets for `{`.
        // Kept for body-parsed runs, mirroring the transient node.
        return Ok((
            vec![Step {
                guard: first_guard,
                kind: StepKind::WithIoBlock { bindings },
                scope_enter: 0,
                scope_exit: 0,
            }],
            2,
        ));
    }
    // A joint follower misses the inner `sep`: the pending wait
    // fails exactly like the grammar.
    if toks[rest].joint {
        return Err(V2Error {
            stmt,
            msg: "WITH_IO block must be followed by '{'".to_string(),
        });
    }
    // Single form, shared with single-inner (element guards the step).
    let (cmd, used) = parse_single_inner(
        toks,
        rest,
        stmt,
        ctx,
        InnerRules {
            allow_await: false,
            allow_with_io: true,
        },
    )?;
    let span = lowering_ctx(stmt);
    check_tty_wrapping(&bindings, Some(&cmd), &span).map_err(|e| V2Error {
        stmt,
        msg: e.to_string(),
    })?;
    let kind = StepKind::WithIo {
        bindings,
        cmd: Box::new(cmd),
    };
    crate::commands::reject_nested_inherit_env(&span, &kind).map_err(|e| V2Error {
        stmt,
        msg: e.to_string(),
    })?;
    Ok((
        vec![Step {
            guard: first_guard,
            kind,
            scope_enter: 0,
            scope_exit: 0,
        }],
        2 + used,
    ))
}

/// Shared inner-command parser: statement calls (joint paren, even on
/// known commands, mirroring `call_head_paren` routing), RUN exec
/// form, and generic instructions. Returns the kind and tokens
/// consumed: calls and RUN-exec end mid-run (the caller continues
/// past them, like the grammar's `element*`); generic instructions
/// consume everything. Wrappers never route here (element and
/// single-inner dispatch own them); slice 4b reuses the call/RUN
/// arms for trailing elements.
fn parse_inner_command(
    body: &[Tok],
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<(StepKind, usize), V2Error> {
    // Bare `NAME(...)` calls invoke functions with one syntax; the head
    // and `(` ride gapless or the shape stays an instruction (which
    // fails later as UnknownCommand), exactly like the grammar.
    if let Some((call_name, paren)) = call_shape_at(body, 0, true) {
        let inner = match &body[paren].kind {
            TokKind::Group { inner, .. } => inner.clone(),
            _ => unreachable!(),
        };
        let args = parse_call_args(&inner, stmt, ctx)?;
        // Statement-level `INSPECT(...)` resolves like any other call
        // (only expression position mints the node); unknown functions
        // fail in the shared resolver with identical messages.
        if !call_name.contains("::") {
            crate::resolve::check_func_ident(&lowering_ctx(stmt), &call_name).map_err(|e| {
                V2Error {
                    stmt,
                    msg: e.to_string(),
                }
            })?;
        }
        let qualified = v2_resolve(ctx, stmt, &call_name)?;
        return Ok((
            StepKind::Call {
                name: qualified,
                args,
            },
            paren + 1,
        ));
    }
    let head = match body.first().map(|t| &t.kind) {
        Some(TokKind::Word(n)) => n.clone(),
        _ => {
            return Err(V2Error {
                stmt,
                msg: "statement must open with a command word".to_string(),
            });
        }
    };
    // RUN exec form: the list routes structured through `lower` (the
    // mock wraps it in `Run`; production maps it to `RunExec`); the
    // caller continues past the list, like the grammar. The list
    // rides gapped (`sep`): a joint bracket is a hard error, never
    // a spliced arg (probed `RUN["a"]`).
    if head == "RUN"
        && matches!(
            body.get(1).map(|t| &t.kind),
            Some(TokKind::Group {
                delim: GroupDelim::Bracket,
                ..
            })
        )
    {
        if body[1].joint {
            return Err(V2Error {
                stmt,
                msg: "RUN [...] requires a gap after RUN".to_string(),
            });
        }
        let inner = match &body[1].kind {
            TokKind::Group { inner, .. } => inner.clone(),
            _ => unreachable!(),
        };
        let list = parse_run_exec_list(&inner, stmt, ctx)?;
        let kind = (ctx.lower)("RUN", vec![Arg::Expr(list)]).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
        })?;
        return Ok((kind, 2));
    }
    let tokens = build_arg_tokens(&body[1..], stmt, ctx)?;
    let kind = lower_generic_args(&head, tokens, stmt, ctx)?;
    Ok((kind, body.len()))
}

/// Parse a token stream (either lexer) into steps. Markers normalize to
/// 0/0 like the fuzz harness; scopes arrive with block slices.
pub fn parse_v2(
    toks: &[Tok],
    lower: impl Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> Result<Vec<Step>, V2Error> {
    let modules = ModuleTable::default();
    let ctx = V2Ctx {
        lower: &lower,
        modules: &modules,
    };
    parse_v2_with_ctx(toks, &ctx)
}

/// Parse with an explicit context (module table for call resolution).
/// `parse_v2` uses empty modules, matching pest with empty tables.
pub fn parse_v2_with_ctx(toks: &[Tok], ctx: &V2Ctx) -> Result<Vec<Step>, V2Error> {
    let mut out = Vec::new();
    for (i, s) in split_statements(toks)?.iter().enumerate() {
        out.extend(lower_stmt_steps(s, i, ctx)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_lower_mock;

    fn lower(name: &str, args: Vec<Arg>) -> ParseResult<StepKind> {
        test_lower_mock::lower(name, args)
    }

    /// Differential oracle: v2 must equal pest on slice-2 scripts,
    /// through both lexers.
    fn assert_parity(text: &str) {
        let old = crate::parse_script(text, lower)
            .unwrap_or_else(|e| panic!("pest must parse parity script {text:?}: {e}"));
        let toks = crate::tok::lex_str(text).expect("string lex must succeed");
        let new = parse_v2(&toks, lower).unwrap_or_else(|e| panic!("v2 must parse {text:?}: {e}"));
        assert_eq!(new, old, "v2/string-lex mismatch for {text:?}");
        #[cfg(feature = "proc-macro-api")]
        {
            let ts: proc_macro2::TokenStream = text.parse().expect("must tokenize");
            let toks = crate::tok::lex_tokens(&ts).expect("token lex must succeed");
            let new = parse_v2(&toks, lower).expect("v2 must parse token script");
            assert_eq!(new, old, "v2/token-lex mismatch for {text:?}");
        }
    }

    /// String-path-only oracle for shapes `rustc` cannot carry at all
    /// (lifetime-like quotes). v2 must still equal pest there.
    fn assert_parity_string_only(text: &str) {
        let old = crate::parse_script(text, lower)
            .unwrap_or_else(|e| panic!("pest must parse parity script {text:?}: {e}"));
        let toks = crate::tok::lex_str(text).expect("string lex must succeed");
        let new = parse_v2(&toks, lower).unwrap_or_else(|e| panic!("v2 must parse {text:?}: {e}"));
        assert_eq!(new, old, "v2/string-lex mismatch for {text:?}");
    }

    #[test]
    fn v2_matches_pest_on_wrappers() {
        // Slice 4b: single-command wrappers, io-blocks, await/cancel,
        // LET wrapper forms — three-way agreement per script.
        for text in [
            "WITH_IO [stdout=$p] ECHO hi",
            "WITH_IO [stdin=$p] ECHO hi",
            "WITH_IO [stdin] ECHO hi",
            "WITH_IO [stdout] ECHO hi",
            "WITH_IO [stdin=$p, stdout=$q] ECHO hi",
            "WITH_IO [stdin=$p, stdin=$q] ECHO hi",
            "WITH_IO [tty] RUN echo hi",
            "WITH_IO [tty, rows=40, cols=100] RUN echo hi",
            "WITH_IO [rows=40, cols=100, tty] RUN echo hi",
            "WITH_IO [stdin =$p] ECHO hi",
            "WITH_IO [stdin= $p] ECHO hi",
            "WITH_IO [ stdin=$p ] ECHO hi",
            "WITH_IO [\nstdin=$p\n] ECHO hi",
            "WITH_IO [stdin=$p\n, stdout=$q] ECHO hi",
            "WITH_IO [stdout=$p] ASYNC ECHO hi",
            "WITH_IO [stdin=$p] WITH_IO [stderr=$e] ECHO hi",
            "WITH_IO [stdin=$p] TIMEOUT 5s ECHO hi",
            "WITH_IO [stdin=$p] ISOLATED ECHO hi",
            "WITH_IO [stdin=$p] CANCEL $t",
            "WITH_IO [stdin=$p] CANCEL $t ECHO bye",
            "WITH_IO [stdin=$p] { ECHO hi }",
            "WITH_IO [stdin=$p]\n{ ECHO hi }",
            "WITH_IO [stdin=$p]; { ECHO hi }",
            "WITH_IO [stdin=$p]{ ECHO hi }",
            "WITH_IO [stdin=$p] { ECHO a\nECHO b }",
            "WITH_IO [stdin=$p] { ECHO hi } ECHO bye",
            "WITH_IO [rows=40] { ECHO hi }",
            "WITH_IO [tty] { ECHO hi }",
            "WITH_IO [stdin=$p] { WITH_IO [stderr=$e] ECHO hi }",
            "TIMEOUT 10s ECHO hi",
            "TIMEOUT 10 ECHO hi",
            "TIMEOUT 500ms ECHO hi",
            "TIMEOUT 5m ECHO hi",
            "TIMEOUT 2h ECHO hi",
            "TIMEOUT $d ECHO hi",
            "TIMEOUT \"10s\" ECHO hi",
            "TIMEOUT \"30 s\" ECHO hi",
            "TIMEOUT {{ $d }} ECHO hi",
            "TIMEOUT 10s AWAIT $t",
            "TIMEOUT 10s CANCEL $t",
            "TIMEOUT 10s AWAIT $t ECHO bye",
            "TIMEOUT 10s CANCEL $t ECHO bye",
            "TIMEOUT 10s { ECHO hi }",
            "TIMEOUT 10s {\nECHO hi\n}",
            "TIMEOUT 10s { ECHO hi } ECHO bye",
            "TIMEOUT 10s { ECHO hi } TIMEOUT 5s ECHO bye",
            "TIMEOUT 10s TIMEOUT 5s ECHO hi",
            "TIMEOUT 10s { ECHO a\nECHO b }",
            "TIMEOUT 10s ECHO hi extra",
            "ISOLATED ECHO hi",
            "ISOLATED { ECHO hi }",
            "ISOLATED { ECHO hi } ECHO bye",
            "ISOLATED AWAIT $t",
            "ISOLATED INHERIT_ENV [M]",
            "ISOLATED { INHERIT_ENV [M] }",
            "ISOLATED { INHERIT_ENV [MODE]\nECHO hi }",
            "ASYNC ECHO hi",
            "ASYNC { ECHO hi }",
            "ASYNC{ECHO hi}",
            "ASYNC { ECHO a\nECHO b }",
            "ASYNC { ECHO hi } ECHO bye",
            "ASYNC CANCEL $t",
            "ASYNC CANCEL $t ECHO bye",
            "AWAIT $t",
            "AWAIT$t",
            "AWAIT $ t",
            "AWAIT $T",
            "AWAIT $t2",
            "AWAIT $_t",
            "CANCEL $t",
            "CANCEL$t",
            "CANCEL $ t",
            "AWAIT $t ECHO bye",
            "CANCEL $t ECHO bye",
            "AWAIT $t AWAIT $u",
            "AWAIT $t\nECHO bye",
            "LET $t: HANDLE = ASYNC ECHO hi",
            "LET $t: HANDLE = ASYNC { ECHO hi }",
            "LET $t: HANDLE = ASYNC { ECHO hi } ECHO bye",
            "LET $a: STRING = ASYNC ECHO hi",
            "LET $t: HANDLE = ASYNC { WITH_IO [stdin=$p] ECHO hi }",
            "LET $w: STRING = WITH_IO [stdin=$p] ASYNC ECHO hi",
            "LET $w: HANDLE = WITH_IO [stdin=$p] ASYNC ECHO hi",
            "LET $w: HANDLE = WITH_IO [stdin=$p] ASYNC { ECHO hi }",
            "LET $w: HANDLE = WITH_IO [stdin=$p] ASYNC { ECHO hi } ECHO bye",
            "LET $x: STRING = WITH_IO [stdin=$p] ECHO hi",
            "LET $x: STRING = WITH_IO [stdin=$p] TIMEOUT 5s ECHO hi",
            "LET $x: STRING = TIMEOUT 10s ECHO hi",
            "LET $x: STRING = TIMEOUT 10s { ECHO hi }",
            "LET $x: STRING = TIMEOUT 10s ECHO hi ECHO bye",
            "LET $o: STRING = AWAIT $t",
            "LET $p: PIPE ECHO bye",
            "TIMEOUT 10s WITH_IO [stdin=$p]",
            "TIMEOUT 10s WITH_IO [stdin=$p]ECHO hi",
            "ISOLATED INHERIT_ENV [A, B]",
            "ISOLATED INHERIT_ENV [A-B]",
            "TIMEOUT 10s { [os:linux] ECHO hi }",
            "ASYNC { [os:linux] ECHO hi }",
            "ISOLATED { [os:linux] ECHO hi }",
            "[os:linux] WITH_IO [stdin=$p]\n{ ECHO hi }",
            "LET $x: INT = 1 ECHO bye",
            "CANCEL $t LET $x: INT = 1",
            "CANCEL $t $x = 1",
            "CANCEL $t RETURN \"hi\"",
            "TIMEOUT 10s { ECHO hi } LET $x: INT = 1",
            "RUN [\"a\"] ECHO bye",
            "[os:linux] AWAIT $t",
            "[os:linux] TIMEOUT 10s { ECHO hi } ECHO bye",
            "[os:linux] ASYNC CANCEL $t ECHO bye",
            "[os:linux] WITH_IO [stdin=$p] { [os:macos] ECHO hi }",
        ] {
            assert_parity(text);
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
    fn v2_matches_pest_on_guarded_commands() {
        for text in [
            "[env:X] ECHO hi",
            "[env:A] [os:linux] ECHO hi",
            "[os:macos] CWD",
            "[not(os:windows)] WRITE f x",
            "[any(os:macos, os:linux)] ECHO hi",
            "[all(env:A, os:linux)] ECHO hi",
            "[eq(env:K, v)] ECHO hi",
            "[ne(env:K, v)] ECHO hi",
            "[bool:true] ECHO hi",
            "[arch:arm] ECHO hi",
            "[family:unix] ECHO hi",
            "[(os:linux)] ECHO hi",
            "[env:A, os:linux] ECHO hi",
            "CWD\n[env:X] ECHO hi",
            "[env:K] ECHO $x",
            "[__OXDOCK_INJECT_1__] ECHO hi",
            "[eq(env:K, \"a b\")] ECHO hi",
            "[\nenv:X\n] ECHO hi",
            // In-body brackets are raw text split on whitespace (probed
            // parity: `[a, b]` is args `"[a,"` + `"b]"`, quotes kept).
            "ECHO [a, b] hi",
            "WRITE f [env:X]",
            "ECHO [env:X] hi",
            "WRITE f [\"a b\"]",
            "ECHO foo[bar]baz",
            "ECHO [a ]",
            "ECHO [ a]",
            "WRITE out x [env:X]",
        ] {
            assert_parity(text);
        }
        // `rustc` lexes `'a` as a lifetime start and rejects the
        // space: string-path-only by construction.
        assert_parity_string_only("ECHO ['a b']");
    }

    #[test]
    fn v2_matches_pest_on_calls_and_exact_args() {
        use crate::ast::{ModuleFuncs, ModuleTable};
        use std::collections::{HashMap, HashSet};
        let std_table = || ModuleTable {
            modules: HashMap::from([(
                "STD".to_string(),
                Some(ModuleFuncs {
                    functions: HashSet::from(["TEXT_LENGTH".to_string()]),
                }),
            )]),
        };
        fn lower_fn(name: &str, args: Vec<Arg>) -> ParseResult<StepKind> {
            test_lower_mock::lower(name, args)
        }
        // One table for both pathways: STD exports TEXT_LENGTH.
        // Rebuilt per case (parse-time only, keeps borrows local).
        for text in [
            "LET $n: INT = STD::TEXT_LENGTH(\"hi\")",
            "STD::TEXT_LENGTH(\"hi\")",
            "LET $n: INT = STD::TEXT_LENGTH(\"a\", \"b\")",
            "LET $s: STRING = INSPECT($x)",
            "INSPECT($x)",
            "LET $x: STRING = ECHO hi",
            "LET $t: STRING = CWD",
            "LET $o: STRING = RUN echo hi",
            "RUN [\"echo\", \"hi\"]",
            "RUN echo [a]",
            "LET $r: STRING = RUN [\"echo\"]",
            "ECHO K=v",
            "ECHO K = v",
            "ECHO K=a b",
            "ECHO K=a, b",
            "ECHO K=\"a b\"",
            "ECHO K=$x",
            "ECHO K=",
            "ECHO a = = b",
            "WRITE f K=v",
            "ECHO $a + $b",
            "ECHO $a==$b",
            "ECHO a $x b",
            "ECHO 5 + 3",
            "WRITE f ( \"hi\" )",
            "ECHO FOO(\"a\")",
            "ECHO [a, b] hi",
            "ECHO {{x}}",
            "ECHO {{ $x }}",
            "ECHO $x=K",
            "ECHO a$x",
            "ECHO a$(x)",
            "ECHO K=$x tail",
            "EXPAND K1=$x K2=$y",
            "EXPAND a",
            "ENV K = v",
            "COPY a b",
            "ECHO foo[bar]baz",
            "LET $x: STRING = FOOBAR",
            "STD::TEXT_LENGTH(\"hi\") ECHO bye",
            "LET $n: INT = STD::TEXT_LENGTH(\"a\") ECHO bye",
            "TIMEOUT 10s STD::TEXT_LENGTH(\"hi\")",
            "TIMEOUT 10s STD::TEXT_LENGTH(\"hi\") ECHO bye",
            "LET $n: INT = STD::TEXT_LENGTH(\"a\")STD::TEXT_LENGTH(\"b\")",
        ] {
            let table = std_table();
            let old =
                crate::parse_script_with_modules(text, lower_fn, HashSet::new(), table.clone())
                    .unwrap_or_else(|e| panic!("pest must parse parity script {text:?}: {e}"));
            let ctx = V2Ctx {
                lower: &lower_fn,
                modules: &table,
            };
            let toks = crate::tok::lex_str(text).expect("string lex must succeed");
            let new = parse_v2_with_ctx(&toks, &ctx)
                .unwrap_or_else(|e| panic!("v2 must parse {text:?}: {e}"));
            assert_eq!(new, old, "v2/string-lex mismatch for {text:?}");
            #[cfg(feature = "proc-macro-api")]
            {
                let ts: proc_macro2::TokenStream = text.parse().expect("must tokenize");
                let toks = crate::tok::lex_tokens(&ts).expect("token lex must succeed");
                let new = parse_v2_with_ctx(&toks, &ctx)
                    .unwrap_or_else(|e| panic!("v2 must parse token {text:?}: {e}"));
                assert_eq!(new, old, "v2/token-lex mismatch for {text:?}");
            }
        }
    }

    #[test]
    fn v2_matches_pest_on_let_set_return() {
        for text in [
            "LET $x: INT = 1",
            "LET $s: STRING = \"hi\"",
            "LET $b: BOOL = true",
            "LET $f: FLOAT = 1.5",
            "LET $p: PIPE",
            "LET $l: LIST<STRING> = [\"a\", \"b\"]",
            "LET $e: LIST<STRING> = []",
            "LET $m: MAP = {k: 1}",
            "LET $m2: MAP = {\"a b\": 2, k: [1, 2]}",
            "LET $n: INT = $a + $b",
            "LET $c: BOOL = ($a == 3) && ($b == 4)",
            "LET $g: BOOL = !$ok",
            "LET $nn: BOOL = true && false",
            "LET $e2: STRING = env:HOME",
            "LET $k: STRING = $a.b.c",
            "LET $u: INT = -5",
            "LET $v: INT = 9223372036854775807",
            "LET $min: INT = -9223372036854775808",
            "LET $w: INT = 1_000",
            "LET $z: STRING = a/b",
            "LET $d: STRING = -f",
            "LET $t: STRING = 30s",
            "LET $q: STRING = \"\"\"hi\"\"\"",
            "LET $f2: FLOAT = 2 * -3",
            "LET $o: BOOL = 2 + 3 > 4",
            "LET $l2: LIST< MAP > = []",
            "LET $a1: INT = 1 + 2",
            "LET $s1: INT = 10 - 4 - 3",
            "LET $m1: INT = 2 * 3 + 4",
            "LET $x1: BOOL = 1 < 2 == true",
            "LET $or: BOOL = $a || $b",
            "LET $dbl: BOOL = !!$x",
            "LET $ver: STRING = 1.0.0",
            "LET $dash: STRING = a-b",
            "LET $div: INT = 4 / 2",
            "LET $bad: INT = 1__0 + 0",
            "LET $strdiv: STRING = 4/2",
            "LET $sp: INT = $a - $b",
            "$x = 1",
            "$x = $a + 1",
            "RETURN 1",
            "RETURN",
            "RETURN $x",
            "[env:X] LET $x: INT = 1",
            "[os:linux] $x = 2",
        ] {
            assert_parity(text);
        }
    }

    #[test]
    fn v2_rejects_slices_not_yet_built() {
        // Structural shapes v2 does not build yet (pest accepts; v2
        // must fail explicitly, never drift).
        for text in ["IF true { ECHO hi }", "FOR $x IN [1] { ECHO hi }"] {
            let toks = crate::tok::lex_str(text).expect("lex must succeed");
            assert!(
                parse_v2(&toks, lower).is_err(),
                "v2 must explicitly reject (not silently drift): {text:?}"
            );
        }
        // Malformed shapes fail on both pathways (messages may differ
        // until cutover; success/failure must agree). Unbalanced groups
        // fail even earlier, at lex time (the pest grammar rejects the
        // same inputs at parse): both layers reject, which is what the
        // oracle pins.
        for text in [
            "[a] ECHO hi",
            "[os:atari] ECHO hi",
            "[foo:bar] ECHO hi",
            "[any(os:linux)] ECHO hi",
            "[all()] ECHO hi",
            "[env:] ECHO hi",
            "[eq(env:K)] ECHO hi",
            "[not] ECHO hi",
            "ECHO [a;b]",
            "ECHO (a;b)",
            "ECHO a;b",
            "LET $x = 1",
            "LET $s: STRING",
            "LET $x: INT = 1 2",
            "LET $x: INT = (1",
            "LET $x: INT = 9223372036854775808",
            "$x =",
            "RETURN (",
            "[env:X]",
            "LET $x: INT = $a == $b == $c",
            "LET $bs: STRING = a\\nb",
            "FOO(\"a\")",
            "FOO (\"a\")",
            "ECHO(\"hi\")",
            "RUN [a] extra",
            "RUN[\"a\"]",
            "TIMEOUT 10s RUN[\"a\"]",
            "LET $r: STRING = RUN[\"a\"]",
            "LET $e: STRING = ECHO",
            "ENV A=1 B=2",
            "EXPAND \"a b\" c",
            "ECHO {a}",
            "ECHO K=${x}",
            "LET $x: STRING = AWAIT",
            "LET $o: STRING = CANCEL $t",
            "LET $x: STRING = ASYNC",
            "LET $x: STRING = WITH_IO",
            "LET $x: STRING = TIMEOUT",
            "LET $x: STRING = ISOLATED",
            "LET $x: STRING = ISOLATED ECHO hi",
            "COPY A=1",
            "WITH_IO [rows=40] RUN echo hi",
            "WITH_IO [tty] ECHO hi",
            "WITH_IO [tty, stderr=$p] RUN echo hi",
            "WITH_IO [tty=$p] RUN echo hi",
            "WITH_IO [cols=0] RUN echo hi",
            "WITH_IO [cols=65536] RUN echo hi",
            "WITH_IO [] ECHO hi",
            "WITH_IO [stdin=$p,] ECHO hi",
            "WITH_IO stdin=$p ECHO hi",
            "WITH_IO [STDIN=$p] ECHO hi",
            "WITH_IO [rows] RUN echo hi",
            "WITH_IO",
            "WITH_IO [stdout=$p]",
            "WITH_IO [stdin=$p]\n[os:linux] { ECHO hi }",
            "WITH_IO [stdin=$p] { }",
            "WITH_IO[stdin=$p] ECHO hi",
            "WITH_IO [stdin=$p]ECHO hi",
            "WITH_IO [stdin=$p] {{ $x }}",
            "ASYNC WITH_IO [stdout=$p] ECHO hi",
            "WITH_IO [stdin=$p] AWAIT $t",
            "ASYNC AWAIT $t",
            "WITH_IO [stdin=$p] RETURN \"hi\"",
            "ASYNC RETURN \"hi\"",
            "TIMEOUT 10s RETURN \"hi\"",
            "TIMEOUT banana ECHO hi",
            "TIMEOUT 10ss ECHO hi",
            "TIMEOUT 10 s ECHO hi",
            "TIMEOUT -5s ECHO hi",
            "TIMEOUT 0s ECHO hi",
            "TIMEOUT",
            "TIMEOUT 10s",
            "TIMEOUT { ECHO hi }",
            "TIMEOUT \"10s\"",
            "TIMEOUT 10s{ECHO hi}",
            "TIMEOUT 10sECHO hi",
            "TIMEOUT 10s {{ $d }}",
            "TIMEOUT 10s { $d }",
            "TIMEOUT { $d } ECHO hi",
            "ASYNC",
            "ISOLATED",
            "ISOLATED{ECHO hi}",
            "AWAIT",
            "CANCEL",
            "AWAIT t",
            "AWAIT $",
            "AWAIT $t extra",
            "CANCEL $t extra",
            "TIMEOUT 5s INHERIT_ENV [M]",
            "TIMEOUT 5s { INHERIT_ENV [M] }",
            "ASYNC { INHERIT_ENV [M] }",
            "LET $x: STRING = INHERIT_ENV [M]",
            "TIMEOUT 10s WITH_IO [stdin=$p] { ECHO hi }",
            "ISOLATED WITH_IO [stdin=$p] { ECHO hi }",
            "TIMEOUT 10s AWAIT $ t",
            "LET $o: STRING = AWAIT $ t",
            "ISOLATED AWAIT $ t",
            "WITH_IO [stdin=$p] CANCEL $ t",
            "LET $ x: INT = 1",
            "LET $x: STRING = WITH_IO [stdin=$p] { ECHO hi }",
            "LET $x: STRING = WITH_IO [stdin=$p] ASYNC",
            "LET $w: HANDLE = WITH_IO [stdin=$p] ASYNC { ECHO a\nECHO b }",
            "LET $x: STRING = WITH_IO [stdout=$p] ECHO hi",
            "LET $x: STRING = TIMEOUT 10s AWAIT $t",
            "LET $x: STRING = CANCEL $t",
            "LET $x: STRING = ASYNC",
            "LET $x: STRING = WITH_IO",
            "TIMEOUT 10s LET $x: INT = 1",
            "LET $x: STRING = FOO bar",
            "LET(\"a\")",
            "AWAIT($t)",
            "CANCEL($t)",
            "TIMEOUT(\"10s\") ECHO hi",
            "RETURN ECHO bye",
        ] {
            match crate::tok::lex_str(text) {
                Err(_) => {
                    assert!(
                        crate::parse_script(text, lower).is_err(),
                        "pest must also reject lex-broken input: {text:?}"
                    );
                    continue;
                }
                Ok(toks) => {
                    assert!(
                        parse_v2(&toks, lower).is_err(),
                        "v2 must explicitly reject (not silently drift): {text:?}"
                    );
                }
            }
            assert!(
                crate::parse_script(text, lower).is_err(),
                "pest must also reject (oracle agreement): {text:?}"
            );
        }
        // Deferred shapes pest accepts: v2 must fail with an explicit
        // unsupported marker (matched by prefix, so the oracle also
        // pins the deferral taxonomy).
        for text in [
            "LET $b: STRING = { RETURN \"hi\" }",
            "ECHO { RETURN \"hi\" }",
            "RUN [{ RETURN \"hi\" }]",
            "[os:linux] { ECHO hi }",
            "[os:linux]\n{ ECHO hi }",
            "TIMEOUT 10s WHILE true { ECHO hi }",
            "ASYNC WHILE true { ECHO hi }",
            "TIMEOUT 10s REMOTE prod { ECHO hi }",
        ] {
            let toks = crate::tok::lex_str(text).expect("lex must succeed");
            let err = parse_v2(&toks, lower).expect_err("v2 must defer, not drift");
            assert!(
                err.msg.starts_with("unsupported in parse_v2"),
                "deferral must be explicit: {text:?} -> {err}"
            );
            assert!(
                crate::parse_script(text, lower).is_ok(),
                "pest must accept deferred shape (proves the deferral): {text:?}"
            );
        }
    }
}
