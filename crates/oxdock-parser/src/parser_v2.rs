//! Unified parser backend: [`Tok`] stream to [`Step`].
//!
//! The single parser for string scripts and braced-macro token
//! streams (the pest grammar and its pair-walking backend are
//! retired; `parser.rs` entry points delegate here).
//!
//! Statement boundaries resolve here from grammar state, never in the
//! lexers: groups arrive structurally atomic, so at each nesting level
//! a `Newline`/`Semicolon` ends the statement. Continuation exceptions
//! (trailing `=`, `IN`, trailing `ASYNC`, bare wrapper heads) land with
//! their structural slices; the decision point is
//! `boundary_ends_statement`.
//!
//! Slice 1 covers generic commands only (`ECHO`, `WRITE`, `CWD`,
//! `ENV`, `RUN`, ...): head word plus gap-split args assembled with
//! single-quoted-string `quoted=true` and multi-fragment
//! space-or-empty joins by the `joint` flag.
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
//! Lowering runs through the shared constructors, so folding, RPN,
//! and overflow outcomes are single-sourced; only the tier walkers
//! are backend-native.
//!
//! Slice 4a implements calls, RUN-exec, and instruction exactness:
//! static resolution runs through the shared `resolve_call_name`
//! (the scope-tracking call resolver); generic
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
//! (`ASYNC`/`WITH_IO`/`TIMEOUT` right-hand sides) mirror the
//! `ASYNC`/`WITH_IO`/`TIMEOUT` capture branches; and
//! statements lower to step vectors (structural pairs end mid-line,
//! trailing elements loop on, fixing two slice-4a divergences:
//! calls and RUN-exec with trailing commands). Bare `WITH_IO [..]`
//! waits for its `{...}` across separators (bare-bracket holds).
//!
//! Slice 5 implements block statements, directives, and `REMOTE`:
//! `IF` (opportunistic `ELSE` chains, cross-newline holds with
//! junction markers), `FOR`/`WHILE` (typed headers, count and key
//! checks), `FUNC` (parse-then-declare, paren-group params, pushed
//! scopes), `REMOTE` (superset-charset targets, bindings, nesting
//! and containment checks), `TYPE` (maximal shape match),
//! `IMPORT`/`EXPORT` (frame/table directives with leak semantics),
//! and guard blocks (top-level scope marking, nested quirk formula,
//! in-wrapper 0/0). Comments ride as `TokKind::Comment` markers plus
//! mid-line `//`/`#`/`/*` gap rules; the splitter holds `WITH_IO`
//! and `ELSE` waits, tracks text-`/*` depth, and records brace
//! detachment. A whole-script post-pass mirrors the finish
//! validations (prelude order, COPY placement, EXPORT membership).
//! Inline-block expressions lower to `Expr::Block` over unmarked body
//! steps.
//!
//! Design record: token-native mirrors with shared lowering cores.
//! Each item below states what this backend mirrors and what it shares
//! (verified by the lexer-agreement oracle and the fuzz harness).
//! - Argument assembly matches single-quoted-string `quoted=true` and
//!   multi-fragment space-or-empty joins by the `joint` flag.
//! - `key_char`/`bare_run`/`value_run` implement the guard key/value
//!   charsets (everything but `: = ! , | ( ) ] $`, whitespace, and
//!   comment opens; `eq` values cross gaps like the atomic span).
//! - `ns_guard` validates namespaces with shared messages.
//! - Guard validation strings (`any(...)/all(...)/eq(...)` shapes)
//!   carry shared messages.
//! - `parse_guard_groups` folds stacked brackets with `All` (the
//!   statement-guard stash semantics).
//! - The expression tiers implement or/and/compare/ordering/add/mul/
//!   unary/atom precedence over [`Tok`]; lowering runs through the
//!   shared constructors (`make_arith`, `make_compare`,
//!   `apply_unary_neg`, `reject_boundary`, `classify_numeric_text`,
//!   `canonicalize_spelling`, `lower_block_string`).
//! - Static call resolution is truly shared (`resolve_call_name`,
//!   `check_func_ident` in `resolve.rs`).
//! - Instruction tokens are truly shared (`InsToken`,
//!   `lower_env_tokens`, `lower_expand_tokens`,
//!   `canonical_assignment_arg` in `commands.rs`); only the
//!   token-level classifiers (key runs, boundaries, call shapes,
//!   expression extension) are backend-native.
//! - Numeric `_`-separator shape gating, bare-word charsets, and
//!   `-`-prefix dispatch match the lexical rules (durations, paths,
//!   and versions lex as bare words; `-f` stays literal).
//! - Wrapper validators are truly shared (`check_tty_wrapping`,
//!   `reject_async_in_capture`, `reject_pipe_stdout_in_capture`,
//!   `rewrite_isolated_inherit`, `contains_inherit_env`,
//!   `reject_nested_inherit_env`, `merge_bindings`, `IoBindingSet`,
//!   `and_guard_exprs`, `reject_with_io_in_async` in `commands.rs`;
//!   in `commands.rs`); only the token-level shape classifiers
//!   (duration gate, binding splitter, await/cancel shapes,
//!   block-vs-template, inherit keys) are backend-native.
//! - The element loop lowers consecutive elements to end of run:
//!   structural pairs end mid-line and trailing elements form
//!   following steps (guards attach to the first only); the `WITH_IO`
//!   pending wait holds bare brackets across separators.
//! - Slice-5 headers mirror their grammar pairs: `IF`/`FOR`/`WHILE`
//!   conditions miss to generic on expression failure (inline-block
//!   failures propagate); `FUNC` paren-group params parse with `gap`
//!   discipline; `REMOTE` targets/blocks/bindings mirror the
//!   atomic/optional shapes; `TYPE` shapes mirror `type_tag`;
//!   `IMPORT`/`EXPORT` lists mirror the `ws`-gap shapes. Shared
//!   validators are truly shared (`validate_remote_copy_placement`,
//!   `contains_remote_block`, `collect_remote_body_symbols`,
//!   `validate_exports` in `commands.rs`).
//! - Scope marking mirrors the backend split: top-level guard nesting
//!   bumps each level (stateful `pending_scope_enters`); wrapper and
//!   guard-block bodies pin 0/0 (`parse_block_elements_with_lower`).
//! - Comment handling mirrors the dual reading: statement-position
//!   comments vanish (releasing `ELSE` holds), gap positions skip,
//!   arg positions type or glue literally; `/*` depth protects
//!   separators in the splitter, and group scanning copies comments
//!   through for the recursive re-lex.

use crate::ast::{
    Arg, ArithOp, CompareOp, Expr, Guard, GuardExpr, IoBinding, IoStream, LogicalOp, ModuleTable,
    Ns, Step, StepKind, Value,
};
use crate::command::ArgType;
use crate::commands::{
    InsToken, apply_unary_neg, canonical_assignment_arg, check_tty_wrapping, classify_numeric_text,
    is_known_command, lower_env_tokens, lower_expand_tokens, make_arith, make_compare,
    reject_boundary,
};
use crate::error::{ParseError, ParseErrorKind, ParseResult, SpanContext};

use crate::resolve::resolve_call_name;
use crate::tok::{GroupDelim, Tok, TokKind};
use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;

/// Backend error. Carries the statement index and message; mapped to
/// [`crate::ParseError`] at cutover. Lowering failures from the shared
/// `lower` dispatcher stringify through here so success paths compare
/// exactly while v2 error text is still its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2Error {
    pub stmt: usize,
    /// 1-based source line, `0` when unknown (the split loops stamp the
    /// enclosing statement's line on the way out, so only genuinely
    /// position-less errors stay `0`).
    pub line: usize,
    pub msg: String,
    /// Lowering failures preserve the dispatcher's classification
    /// (`UnknownCommand`, `InvalidSyntax`, `Validation`); `None` means
    /// a backend-native structural failure.
    pub kind: Option<ParseErrorKind>,
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

/// Lift a dispatcher/resolver [`ParseError`] into a [`V2Error`],
/// preserving its machine-readable classification (the cutover
/// converts it back verbatim, so `UnknownCommand`/`InvalidSyntax`/
/// `Validation` survive the backend swap). The message is the
/// dispatcher's own display text; the split loops stamp the line.
fn lower_err(stmt: usize, e: ParseError) -> V2Error {
    V2Error {
        stmt,
        line: 0,
        msg: e.to_string(),
        kind: Some(e.kind().clone()),
    }
}

/// One split statement: leading bracket groups (guard region,
/// slice 2) plus the body tokens. `brace_detached` records a
/// `Newline`/`Semicolon` between the guard region and a brace-headed
/// body: in-body such statements are hard errors (a bare `{` is
/// illegal inside blocks), while top level merges or parses guard
/// blocks regardless.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stmt {
    guard: Vec<Tok>,
    body: Vec<Tok>,
    brace_detached: bool,
    /// 1-based source line where the body starts (guards ride on
    /// earlier lines; errors report this).
    line: usize,
}

/// Grammar-state boundary decision: does this `Newline`/`Semicolon`
/// end the statement? Groups are atomic, so every statement fits one
/// logical token line; every keyword-headed statement (commands,
/// wrappers, `LET`/`AWAIT`, block statements, directives) ends here.
/// Only non-word heads fail, exactly like the grammar's element
/// mismatch.
fn boundary_ends_statement(body: &[Tok]) -> Result<bool, String> {
    let head = match body.first() {
        None => return Ok(true),
        Some(t) => t,
    };
    match &head.kind {
        TokKind::Word(_) | TokKind::Punct('$') => Ok(true),
        // A lone leading `{...}` ends here too: the splitter merges
        // it into pending waits (`WITH_IO`, `IF`/`ELSE` chains) or
        // emits it for block-statement lowering, never splitting
        // inside.
        TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        } => Ok(true),
        _ => Err("statement must open with a command word".to_string()),
    }
}

/// Bare `WITH_IO [..]` with no inner command: holds the line for a
/// following `{...}` block (bare-bracket hold), which merges into one
/// io-block statement. Anything else (or EOF) fails exactly like the
/// grammar's `must be followed by '{'`.
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

/// An if-tailed statement body: head `IF`, ends with a non-template
/// brace (the then/else-if block), ignoring trailing gap comments.
/// Trailing `//`/`#` runs and balanced `/* */` strip backward (the
/// grammar's `gap` eats them before the `ELSE` chain, joint state
/// irrelevant after a complete pair); anything else trailing (text,
/// groups, strings) breaks the tail. Over-holding is safe: a merged
/// non-`ELSE` follower releases, and a merged `ELSE` parses exactly
/// where the grammar would.
fn is_if_tail(body: &[Tok]) -> bool {
    if !matches!(body.first().map(|t| &t.kind), Some(TokKind::Word(w)) if w == "IF") {
        return false;
    }
    let mut end = body.len();
    loop {
        // Lexer comment markers.
        while end > 0 && matches!(&body[end - 1].kind, TokKind::Comment(_)) {
            end -= 1;
        }
        // Earliest comment opener (`//`-pair, `#`, `/*`-pair) starts
        // trailing comment territory.
        let mut cut = None;
        let mut k = 0;
        while k < end {
            match &body[k].kind {
                TokKind::Punct('#') => {
                    cut = Some(k);
                    break;
                }
                TokKind::Punct('/') if is_comment_open(&body[..end], k) => {
                    cut = Some(k);
                    break;
                }
                _ => k += 1,
            }
        }
        match cut {
            Some(k) => {
                // A balanced `/* */` may be followed by more gap
                // comments; anything else ends the strip for the
                // brace check below.
                if matches!(&body[k].kind, TokKind::Punct('/'))
                    && !matches!(body.get(k + 1).map(|t| &t.kind), Some(TokKind::Punct('/')))
                {
                    match skip_inline_comment(&body[..end], k) {
                        Some(past) if past < end => {
                            end = past;
                            continue;
                        }
                        _ => {}
                    }
                }
                end = k;
                continue;
            }
            None => break,
        }
    }
    matches!(
        body[..end].last().map(|t| &t.kind),
        Some(TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        })
    ) && body[..end].last().is_some_and(|t| !is_template_brace(t))
}

/// True when a body opens with a brace group (guard-block position
/// or bare block).
fn starts_brace(body: &[Tok]) -> bool {
    matches!(
        body.first().map(|t| &t.kind),
        Some(TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        })
    )
}

/// Source lines spanned by one token (for the splitter's line
/// counter): newlines, `/* */` comment interiors, and nested group
/// contents. Everything else occupies its own line.
fn tok_newlines(tok: &Tok) -> usize {
    match &tok.kind {
        TokKind::Newline => 1,
        TokKind::Comment(text) => text.matches('\n').count(),
        TokKind::Group { inner, .. } => inner.iter().map(tok_newlines).sum(),
        _ => 0,
    }
}

/// Stamp an error with the innermost statement's line when it has
/// none yet (deeper frames stamp first, so outer frames never
/// overwrite real positions).
fn stamp_line(err: V2Error, line: usize) -> V2Error {
    if err.line == 0 {
        V2Error { line, ..err }
    } else {
        err
    }
}

/// Split top-level tokens into statements. Leading `[..]` groups attach
/// to the following statement as its guard region; `Newline`/`Semicolon`
/// end statements per [`boundary_ends_statement`]. Empty statements
/// (blank lines, stray `;`) drop, matching the `blank | semicolon`
/// pest elements. Comment markers drop silently everywhere except
/// pending `ELSE` waits, which a comment line breaks exactly like the
/// grammar. Mid-line `//` runs and `#`/`/*` opens with an empty body
/// skip as comments (statement position, like the grammar's silent
/// elements); anywhere else they stay payload text for the arg engine.
fn split_statements(toks: &[Tok], base: usize) -> Result<Vec<Stmt>, V2Error> {
    let mut stmts = Vec::new();
    let mut guard = Vec::new();
    let mut body: Vec<Tok> = Vec::new();
    let mut stmt_idx = 0;
    // 1-based source line tracker: `base` is the line `toks[0]` starts
    // on (1 at top level, the enclosing statement's line inside
    // groups, where headers are single-line so the brace shares it).
    // Groups and block comments advance past their inner newlines;
    // every pushed statement records its body's start line for errors.
    let mut line = base;
    let mut start_line = base;
    // Bare `WITH_IO [..]` awaiting its `{...}` block (see
    // `is_bare_with_io`): the guard and body hold across any
    // separator until the brace arrives or the wait fails.
    // Pending holds carry their body's start line with them.
    let mut pending_io: Option<(Vec<Tok>, Vec<Tok>, usize)> = None;
    // If-tailed statement awaiting an `ELSE` continuation across
    // newlines only (a `;` breaks the chain like the grammar).
    let mut pending_if: Option<(Vec<Tok>, Vec<Tok>, usize)> = None;
    // Separator crossed between the guard region and the body start:
    // only observed for brace-headed bodies (`brace_detached`).
    let mut detached = false;
    // Flush a statement, resolving pending waits first. `via_newline`
    // records the separator kind: only newlines hold/merge `ELSE`
    // chains; `;` and EOF release them. `line` is the current tracker
    // for wait-failure errors; `start_line` is this body's start.
    let flush_or_hold = |stmts: &mut Vec<Stmt>,
                         guard: &mut Vec<Tok>,
                         body: &mut Vec<Tok>,
                         detached: &mut bool,
                         stmt_idx: &mut usize,
                         pending_io: &mut Option<(Vec<Tok>, Vec<Tok>, usize)>,
                         pending_if: &mut Option<(Vec<Tok>, Vec<Tok>, usize)>,
                         via_newline: bool,
                         start_line: usize,
                         line: usize|
     -> Result<(), V2Error> {
        if body.is_empty() {
            guard.clear();
            *detached = false;
            return Ok(());
        }
        // Pending `WITH_IO` wait takes precedence (the holds are
        // mutually exclusive: setting either returns first).
        if let Some((pending_guard, mut pending_body, pending_line)) = pending_io.take() {
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
                    line: pending_line,
                    kind: None,
                });
            }
            pending_body.extend(std::mem::take(body));
            guard.clear();
            *detached = false;
            stmts.push(Stmt {
                guard: pending_guard,
                body: pending_body,
                brace_detached: false,
                line: pending_line,
            });
            *stmt_idx += 1;
            return Ok(());
        }
        // Pending `ELSE` wait: merge guard-less `ELSE`-led statements.
        // The wait was set across a newline (only newlines hold), so
        // the separator kind of *this* flush is irrelevant — `;` and
        // EOF merge too; anything else releases the held IF.
        if let Some((held_guard, mut held_body, held_line)) = pending_if.take() {
            let else_start = guard.is_empty()
                && matches!(
                    body.first().map(|t| &t.kind),
                    Some(TokKind::Word(w)) if w == "ELSE"
                );
            if !else_start {
                stmts.push(Stmt {
                    guard: held_guard,
                    body: held_body,
                    brace_detached: false,
                    line: held_line,
                });
                *stmt_idx += 1;
            } else {
                // The merged chain may hold again for a further
                // `ELSE`, newlines only, like the grammar. A newline
                // marker joins the parts: `#` comments end at it
                // (their extent is unrecoverable post-merge), while
                // `//`/`/*` carry their own extents.
                held_body.push(Tok::fresh(TokKind::Newline, false));
                held_body.extend(std::mem::take(body));
                guard.clear();
                *detached = false;
                if via_newline && is_if_tail(&held_body) {
                    *pending_if = Some((held_guard, held_body, held_line));
                    return Ok(());
                }
                stmts.push(Stmt {
                    guard: held_guard,
                    body: held_body,
                    brace_detached: false,
                    line: held_line,
                });
                *stmt_idx += 1;
                return Ok(());
            }
        }
        if is_bare_with_io(body) {
            if pending_io.is_some() {
                return Err(V2Error {
                    stmt: *stmt_idx,
                    msg: "previous WITH_IO block is still waiting for '{'".to_string(),
                    line,
                    kind: None,
                });
            }
            *pending_io = Some((std::mem::take(guard), std::mem::take(body), start_line));
            *detached = false;
            return Ok(());
        }
        // Ordinary statement: emit it, holding if-tailed bodies for
        // an `ELSE` continuation across newlines only.
        if via_newline && is_if_tail(body) {
            *pending_if = Some((std::mem::take(guard), std::mem::take(body), start_line));
            *detached = false;
            return Ok(());
        }
        let brace_detached = *detached && !guard.is_empty() && starts_brace(body);
        stmts.push(Stmt {
            guard: std::mem::take(guard),
            body: std::mem::take(body),
            brace_detached,
            line: start_line,
        });
        *detached = false;
        *stmt_idx += 1;
        Ok(())
    };
    // Release a pending `ELSE` wait by emitting the held IF as-is.
    // (Inlined at use sites: a closure would collide with
    // `flush_or_hold`'s borrow of the statement buffer.)
    let mut i = 0;
    // Open text-`/*` depth (line-start comments arrive as `Comment`
    // toks; `//` never spans separators): newlines and semicolons
    // inside never split, and `[` never guards (the closer, an
    // error, or downstream rules resolve everything, exactly like
    // the grammar).
    let mut comment_depth = 0usize;
    // True when `toks[i]` opens a text block comment (`/` plus a
    // joint `*`; `//` is line-comment territory, handled elsewhere).
    let block_open_at = |toks: &[Tok], i: usize| {
        matches!(toks.get(i).map(|t| &t.kind), Some(TokKind::Punct('/')))
            && matches!(
                toks.get(i + 1).map(|t| (&t.kind, t.joint)),
                Some((TokKind::Punct('*'), true))
            )
    };
    // True when `toks[i]` closes one (`*` plus a joint `/`).
    let block_close_at = |toks: &[Tok], i: usize| {
        matches!(toks.get(i).map(|t| &t.kind), Some(TokKind::Punct('*')))
            && matches!(
                toks.get(i + 1).map(|t| (&t.kind, t.joint)),
                Some((TokKind::Punct('/'), true))
            )
    };
    while i < toks.len() {
        let tok = &toks[i];
        // Statement-start `/*` (silent gap position): skip balanced,
        // failing unclosed like the grammar; mid-body opens only
        // deepen the tracker below and ride into args as payload.
        if body.is_empty() && comment_depth == 0 && block_open_at(toks, i) {
            match skip_inline_comment(toks, i) {
                Some(past) => {
                    // The skipped span may cross lines (`/*a\nb*/`).
                    line += toks[i..past].iter().map(tok_newlines).sum::<usize>();
                    i = past;
                    continue;
                }
                None => {
                    return Err(V2Error {
                        stmt: stmt_idx,
                        msg: "unclosed block comment".to_string(),
                        line,
                        kind: None,
                    });
                }
            }
        }
        // Nesting transitions on text pairs (mid-body opens deepen;
        // closers pop when nested).
        if block_open_at(toks, i) && (comment_depth > 0 || !body.is_empty()) {
            comment_depth += 1;
        } else if block_close_at(toks, i) && comment_depth > 0 {
            comment_depth -= 1;
        }
        // Inside a block comment everything rides into the current
        // body (never splits, never guards, never releases waits,
        // never skips): gap positions swallow it downstream, arg
        // positions fail or math over it, exactly like the grammar.
        if comment_depth > 0 {
            if body.is_empty() {
                start_line = line;
            }
            line += tok_newlines(tok);
            body.push(tok.clone());
            detached = false;
            i += 1;
            continue;
        }
        match &tok.kind {
            TokKind::Newline => {
                if body.is_empty() {
                    // A separator between the guard region and a
                    // following brace matters (see `brace_detached`).
                    detached = true;
                    line += 1;
                    i += 1;
                    continue;
                }
                match boundary_ends_statement(&body) {
                    Ok(true) => flush_or_hold(
                        &mut stmts,
                        &mut guard,
                        &mut body,
                        &mut detached,
                        &mut stmt_idx,
                        &mut pending_io,
                        &mut pending_if,
                        true,
                        start_line,
                        line,
                    )?,
                    Ok(false) => {}
                    Err(msg) => {
                        return Err(V2Error {
                            stmt: stmt_idx,
                            msg,
                            line,
                            kind: None,
                        });
                    }
                }
                line += 1;
                i += 1;
            }
            TokKind::Semicolon => {
                if body.is_empty() {
                    // A `;` breaks `ELSE` chains (release the hold)
                    // but not `WITH_IO` waits (probed `;` merge).
                    if let Some((held_guard, held_body, held_line)) = pending_if.take() {
                        stmts.push(Stmt {
                            guard: held_guard,
                            body: held_body,
                            brace_detached: false,
                            line: held_line,
                        });
                        stmt_idx += 1;
                    }
                    detached = true;
                    i += 1;
                    continue;
                }
                match boundary_ends_statement(&body) {
                    Ok(true) => flush_or_hold(
                        &mut stmts,
                        &mut guard,
                        &mut body,
                        &mut detached,
                        &mut stmt_idx,
                        &mut pending_io,
                        &mut pending_if,
                        false,
                        start_line,
                        line,
                    )?,
                    Ok(false) => {}
                    Err(msg) => {
                        return Err(V2Error {
                            stmt: stmt_idx,
                            msg,
                            line,
                            kind: None,
                        });
                    }
                }
                i += 1;
            }
            // Lexer comment markers drop silently, but release a
            // pending `ELSE` wait (a comment line breaks the chain
            // like the grammar); `WITH_IO` waits survive comments.
            TokKind::Comment(text) => {
                if let Some((held_guard, held_body, held_line)) = pending_if.take() {
                    stmts.push(Stmt {
                        guard: held_guard,
                        body: held_body,
                        brace_detached: false,
                        line: held_line,
                    });
                    stmt_idx += 1;
                }
                // Block markers may span lines (`/*a\nb*/`).
                line += text.matches('\n').count();
                i += 1;
            }
            // A `//` run with an empty body (mid-line, e.g. after
            // `;`) is a comment to the line end; with a non-empty
            // body the slash-run rule owns it as payload text.
            TokKind::Punct('/') if body.is_empty() && is_comment_open(toks, i) => {
                if matches!(toks.get(i + 1).map(|t| &t.kind), Some(TokKind::Punct('/'))) {
                    while i < toks.len() && !matches!(&toks[i].kind, TokKind::Newline) {
                        i += 1;
                    }
                } else {
                    match skip_inline_comment(toks, i) {
                        Some(past) => i = past,
                        None => {
                            return Err(V2Error {
                                stmt: stmt_idx,
                                msg: "unclosed block comment".to_string(),
                                line,
                                kind: None,
                            });
                        }
                    }
                }
            }
            // `#` with an empty body (e.g. after `;`) is a comment
            // to the line end; mid-body it stays arg text.
            TokKind::Punct('#') if body.is_empty() => {
                while i < toks.len() && !matches!(&toks[i].kind, TokKind::Newline) {
                    i += 1;
                }
            }
            TokKind::Group { delim, .. } if body.is_empty() && *delim == GroupDelim::Bracket => {
                // Guard groups may span lines; the body starts after.
                line += tok_newlines(tok);
                guard.push(tok.clone());
                detached = false;
                i += 1;
            }
            _ => {
                if body.is_empty() {
                    start_line = line;
                }
                line += tok_newlines(tok);
                body.push(tok.clone());
                i += 1;
            }
        }
    }
    if !body.is_empty() {
        match boundary_ends_statement(&body) {
            Ok(_) => flush_or_hold(
                &mut stmts,
                &mut guard,
                &mut body,
                &mut detached,
                &mut stmt_idx,
                &mut pending_io,
                &mut pending_if,
                false,
                start_line,
                line,
            )?,
            Err(msg) => {
                return Err(V2Error {
                    stmt: stmt_idx,
                    msg,
                    line,
                    kind: None,
                });
            }
        }
    } else if !guard.is_empty() {
        // Trailing guards with no following command error (probed:
        // pest rejects a lone `[env:X]`), never drop silently.
        return Err(V2Error {
            stmt: stmt_idx,
            msg: "guard declared without a following command".to_string(),
            line,
            kind: None,
        });
    }
    // Release a trailing `ELSE` wait; a trailing `WITH_IO` wait fails.
    if let Some((held_guard, held_body, held_line)) = pending_if.take() {
        stmts.push(Stmt {
            guard: held_guard,
            body: held_body,
            brace_detached: false,
            line: held_line,
        });
    }
    if pending_io.is_some() {
        return Err(V2Error {
            stmt: stmt_idx,
            msg: "WITH_IO block must be followed by '{'".to_string(),
            line,
            kind: None,
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
                            line: 0,
                            kind: None,
                        });
                    }
                    out.push('$');
                } else {
                    out.push(*c);
                }
            }
            // Standalone quotes strip (the caller handles single-tok
            // runs); glued quotes keep their marks, like the grammar's
            // mixed fragments (`>"a"` stays quoted, `"a"` strips).
            TokKind::Str { text: s, quote } => {
                let glued = tok.joint || toks.get(i + 1).is_some_and(|t| t.joint);
                if toks.len() > 1 && glued {
                    out.push(*quote);
                    out.push_str(s);
                    out.push(*quote);
                } else {
                    out.push_str(s);
                }
            }
            // Comments reproduce verbatim (templates keep comment
            // text like the atomic grammar span); gaps collapse
            // around them like other fragments per the documented
            // single-space normalization.
            TokKind::Comment(text) => out.push_str(text),
            TokKind::BlockStr(full) => {
                // Glued blocks keep their delimiters, like quotes.
                let glued = tok.joint || toks.get(i + 1).is_some_and(|t| t.joint);
                if toks.len() > 1 && glued {
                    out.push_str(full);
                } else {
                    out.push_str(&crate::command::lower_block_string(full));
                }
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
                    line: 0,
                    kind: None,
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
fn build_value_arg(
    region: &[Tok],
    stmt: usize,
    ctx: &V2Ctx,
    lone_ok: bool,
) -> Result<Arg, V2Error> {
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
            line: 0,
            kind: None,
        });
    }
    // Single-quoted values mid-line (`K='a b'`): the string lexer only
    // opens `'` gapped, so these arrive as fragments; the grammar's
    // `quoted_string` admits them anywhere. Reassemble a region that
    // opens with `'` and closes at its end, stripping exactly like a
    // structural string (escapes intact for runtime, like `Str`).
    if matches!(&region[0].kind, TokKind::Punct('\''))
        && let Some(text) = try_squote_value(region)
    {
        return Ok(Arg::String(text, true));
    }
    // Lone typed values apply only at a true end/boundary, never
    // past a comment (the grammar's `!assign_expr_cont` guard).
    if lone_ok && let Some(expr) = try_lone_value_expr(region, stmt, ctx)? {
        return Ok(expr);
    }
    Ok(Arg::String(render_raw_text(region, stmt)?, false))
}

/// Reassemble a single-quoted assignment value (`K='a b'`) from its
/// lexer fragments: the opener must close at the region end (a
/// trailing fragment falls back to raw text, which fails downstream
/// like the grammar). Inner gaps join with one space and nested
/// double quotes re-quote (the atomic span keeps them); `\'` escapes
/// pass through intact for runtime, like structural strings. `None`
/// on shape miss.
fn try_squote_value(region: &[Tok]) -> Option<String> {
    // Closing `'`: first unescaped quote past the opener.
    let mut close = None;
    let mut i = 1;
    while i < region.len() {
        match &region[i].kind {
            TokKind::Punct('\\') => i += 2,
            TokKind::Punct('\'') => {
                close = Some(i);
                break;
            }
            _ => i += 1,
        }
    }
    if close != Some(region.len() - 1) {
        return None;
    }
    let mut out = String::new();
    for tok in &region[1..region.len() - 1] {
        if !tok.joint && !out.is_empty() {
            out.push(' ');
        }
        match &tok.kind {
            TokKind::Word(w) => out.push_str(w),
            TokKind::Punct(c) => out.push(*c),
            TokKind::Semicolon => out.push(';'),
            // Nested double quotes keep their marks (the atomic span
            // never strips them); triple quotes pass through whole.
            TokKind::Str { text, quote } => {
                out.push(*quote);
                out.push_str(text);
                out.push(*quote);
            }
            TokKind::BlockStr(full) => out.push_str(full),
            // Groups and newlines never occur in value regions (the
            // splitter owns them); anything else breaks the shape.
            _ => return None,
        }
    }
    Some(out)
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
                    line: 0,
                    kind: None,
                });
            };
            if let Expr::Var(var) = arg {
                return Ok(Some(Arg::Expr(Expr::Inspect(var.clone()))));
            }
            return Err(V2Error {
                stmt,
                msg: format!("INSPECT requires a $variable argument, found {arg:?}"),
                line: 0,
                kind: None,
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
    // A bare `env` head only extends into a real `env:KEY` read (joint
    // `:`); dotted bare words (`env.txt`) stay raw strings like the
    // grammar's `unquoted_arg`, which precedes `expr`.
    if matches!(toks.get(pos).map(|t| &t.kind), Some(TokKind::Word(w)) if w == "env")
        && !matches!(
            (
                toks.get(pos + 1).map(|t| &t.kind),
                toks.get(pos + 1).map(|t| t.joint)
            ),
            (Some(TokKind::Punct(':')), Some(true))
        )
    {
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
/// as maps/blocks in `build_arg_tokens`).
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
            // Value runs to the next boundary, but raw text also
            // stops at `//` / `/*` (mirroring `raw_text_char`):
            // a line comment swallows the rest silently, a block
            // comment must be followed by nothing or a boundary
            // (anything else strands text and fails the line, like
            // the grammar). Lone typed values (`$x`, calls) apply
            // only at a true end/boundary, never past a comment
            // (the grammar's `!assign_expr_cont` guard).
            let mut vend = key_end + 1;
            let mut lone_ok = true;
            let mut failed: Option<String> = None;
            'scan: while vend < flat.len() {
                if !flat[vend].joint && at_assign_boundary(&flat, vend) {
                    break;
                }
                // Lexer comment markers end the raw span like `//`
                // (they run to end of line by construction).
                if matches!(&flat[vend].kind, TokKind::Comment(_)) {
                    lone_ok = false;
                    vend = flat.len();
                    break;
                }
                if matches!(&flat[vend].kind, TokKind::Punct('/')) && is_comment_open(&flat, vend) {
                    // Comments end the raw span: line comments eat
                    // the rest silently, block comments must be
                    // followed by nothing, a boundary, or more
                    // comments (stranded text fails the line, like
                    // the grammar).
                    lone_ok = false;
                    loop {
                        if vend >= flat.len()
                            || (!flat[vend].joint && at_assign_boundary(&flat, vend))
                        {
                            break 'scan;
                        }
                        if !matches!(&flat[vend].kind, TokKind::Punct('/'))
                            || !is_comment_open(&flat, vend)
                        {
                            failed = Some("unexpected text after assignment value".to_string());
                            break 'scan;
                        }
                        if matches!(
                            flat.get(vend + 1).map(|t| &t.kind),
                            Some(TokKind::Punct('/'))
                        ) {
                            vend = flat.len();
                            break 'scan;
                        }
                        match skip_inline_comment(&flat, vend) {
                            None => {
                                failed = Some("unclosed block comment".to_string());
                                break 'scan;
                            }
                            Some(past) => vend = past,
                        }
                    }
                }
                vend += 1;
            }
            if let Some(msg) = failed {
                return Err(V2Error {
                    stmt,
                    msg,
                    line: 0,
                    kind: None,
                });
            }
            let value = build_value_arg(&flat[key_end + 1..vend], stmt, ctx, lone_ok)?;
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
        // Slash-pair-led runs (`/*c*/`; `//` arrives as `Comment`
        // toks) parse as expression fragments (typed when single,
        // glued verbatim otherwise).
        if is_slash_run(run) {
            out.push(InsToken::Pos(parse_slash_run(run, stmt, ctx)?));
            pos = run_end;
            continue;
        }
        // Trailing `Comment` (`//` to end of line) glues as a
        // literal fragment (typed when alone, verbatim otherwise),
        // exactly like the grammar's expression fragments.
        if let Some(TokKind::Comment(text)) = run.last().map(|t| &t.kind) {
            if run.len() == 1 {
                out.push(InsToken::Pos(Arg::Expr(Expr::Literal(Value::string(
                    text.clone(),
                )))));
            } else {
                let mut bytes = render_raw_text(&run[..run.len() - 1], stmt)?;
                bytes.push_str(text);
                out.push(InsToken::Pos(Arg::String(bytes, false)));
            }
            pos = run_end;
            continue;
        }
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

/// A raw run starting with `//` or `/*` (a slash-pair: `/` plus a
/// joint `/` or `*`) parses as expression fragments: pest's
/// `unquoted_arg` lookaheads reject both there, routing to `expr`
/// (so `//c` types as a literal and `/*c*/` as math). A single
/// fully-consuming expression stays typed; anything longer glues to
/// verbatim text exactly like the grammar's mixed fragments (byte
/// for byte, since joint runs admit no span gaps); an unparseable
/// start fails loudly (the argument fails there too).
fn parse_slash_run(run: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<Arg, V2Error> {
    let mut cur = Cursor::expr(run, stmt);
    let expr = parse_expr_checked(&mut cur, ctx)?;
    if cur.done() {
        return Ok(Arg::Expr(expr));
    }
    Ok(Arg::String(render_raw_text(run, stmt)?, false))
}

/// True when a raw run opens with a slash-pair (`/` plus a joint
/// `/` or `*`), routing it to expression fragments (see
/// [`parse_slash_run`]). A lone `/` (`/a`, `/`) stays raw text.
fn is_slash_run(run: &[Tok]) -> bool {
    matches!(run.first().map(|t| &t.kind), Some(TokKind::Punct('/')))
        && matches!(
            run.get(1).map(|t| (&t.kind, t.joint)),
            Some((TokKind::Punct('/'), true)) | Some((TokKind::Punct('*'), true))
        )
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
            line: 0,
            kind: None,
        });
    }
    if name == "EXPAND" {
        return lower_expand_tokens(&lowering_ctx(stmt), tokens).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
            line: 0,
            kind: None,
        });
    }
    let args = tokens
        .into_iter()
        .map(|token| match token {
            InsToken::Pos(arg) => arg,
            InsToken::Assign(key, value) => canonical_assignment_arg(&key, &value),
        })
        .collect();
    (ctx.lower)(name, args).map_err(|e| lower_err(stmt, e))
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
            // Comments reproduce verbatim (templates keep comment
            // text like the atomic grammar span); like newlines they
            // carry their own separation, never a synthetic space.
            TokKind::Comment(text) => out.push_str(text),
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
                    // Comments reproduce verbatim (nested groups keep
                    // comment text like the atomic grammar span).
                    TokKind::Comment(text) => out.push_str(text),
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
            line: 0,
            msg: msg.into(),
            kind: None,
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

    /// Skip one gap's worth of comments (`gap`: `//`-to-newline,
    /// balanced `/* */` even across newlines, `#`-to-newline —
    /// never a standalone newline, which ends single-line
    /// expressions exactly like the grammar). Mirrors pest's greedy
    /// gap before operators and operands; whitespace itself is
    /// already invisible in the token stream.
    fn skip_gap(&mut self) {
        loop {
            match self.toks.get(self.pos).map(|t| &t.kind) {
                Some(TokKind::Comment(_)) => {
                    self.pos += 1;
                }
                Some(TokKind::Punct('/')) if is_comment_open(self.toks, self.pos) => {
                    match skip_inline_comment(self.toks, self.pos) {
                        Some(next) => self.pos = next,
                        // Unclosed block comments fail the gap (and
                        // the enclosing rule) like the grammar.
                        None => break,
                    }
                }
                Some(TokKind::Punct('#')) => {
                    while self.pos < self.toks.len()
                        && !matches!(&self.toks[self.pos].kind, TokKind::Newline)
                    {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    /// Skip gap comments plus standalone newlines (`gap_nl`: call
    /// args, list items, maps, parenthesized expressions).
    fn skip_gap_nl(&mut self) {
        loop {
            match self.toks.get(self.pos).map(|t| &t.kind) {
                Some(TokKind::Newline) => {
                    self.pos += 1;
                }
                _ => {
                    let before = self.pos;
                    self.skip_gap();
                    if self.pos == before {
                        break;
                    }
                }
            }
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

/// Punctuation admissible inside bare guard key runs: everything but
/// the terminators the parser enforces separately. `;` arrives as its
/// own token kind; the key charset admits it, so runs accept it literally.
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

/// Namespace validation for guard predicates with closed domains
/// (`family`, `os`, `arch`); open namespaces pass through.
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
    // A `!` prefix is never negation in guards (only `not(...)` negates).
    if matches!(&tok.kind, TokKind::Punct('!')) {
        return Err(cur.err("unexpected `!` in guard; negation uses `not(...)`"));
    }
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
            let key = bare_run(&mut arg, |_| true, "guard key")?;
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
            let val = bare_run(cur, |_| true, "bool value")?;
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
            let key = bare_run(cur, |_| true, "guard key")?;
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
            let val = bare_run(cur, |_| true, "guard value")?;
            check_term_end(cur)?;
            ns_guard(&word, &val, cur)
        }
    }
}

/// Bare `eq`/`ne` value run: key charset plus `:`/`=`/`!` (the grammar
/// admits them in values, never in keys), with nested groups rendered
/// for the pathological `(`-in-value shape. `$`, `|`, `,` terminate
/// with an error downstream.
/// Bare `eq`/`ne` value mirror of atomic `bare_guard_value`:
/// everything except `, | ) ] $`, linebreaks, and comment opens —
/// gaps included (joined with one space; the token stream cannot
/// preserve gap width). Quotes ride literally mid-run (the atomic
/// span keeps them); a *leading* quote never reaches here (the
/// caller strips it as the `quoted_string` alternative, which then
/// requires end-of-region like the grammar's committed choice).
fn value_run(arg: &mut Cursor) -> Result<String, V2Error> {
    let mut out = String::new();
    while let Some(tok) = arg.toks.get(arg.pos) {
        // Terminators: the grammar's negative-lookahead set.
        match &tok.kind {
            TokKind::Newline | TokKind::Comment(_) => break,
            TokKind::Punct(',')
            | TokKind::Punct('|')
            | TokKind::Punct(']')
            | TokKind::Punct(')') => break,
            TokKind::Punct('$') => break,
            TokKind::Punct('/') if is_comment_open(arg.toks, arg.pos) => break,
            TokKind::Group { .. } => break,
            _ => {}
        }
        if !out.is_empty() && !tok.joint {
            out.push(' ');
        }
        match &tok.kind {
            TokKind::Word(w) => out.push_str(w),
            TokKind::Punct(c) => out.push(*c),
            TokKind::Semicolon => out.push(';'),
            // Mid-run quotes keep their marks, like the atomic span.
            TokKind::Str { text, quote } => {
                out.push(*quote);
                out.push_str(text);
                out.push(*quote);
            }
            TokKind::BlockStr(full) => out.push_str(full),
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
    // Leading newlines skip (the grammar's implicit whitespace gaps
    // multi-line guards, e.g. `[\nnot(env:K)\n]`).
    while matches!(cur.toks.get(pos).map(|t| &t.kind), Some(TokKind::Newline)) {
        pos += 1;
    }
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

/// Parse one standalone guard expression from lexed tokens (the
/// `parse_guard_expr_str` entry point): a bare `guard_seq` with full
/// consumption, mirroring the grammar rule exactly.
pub(crate) fn parse_single_guard(toks: &[Tok], stmt: usize) -> Result<GuardExpr, V2Error> {
    let mut cur = Cursor::new(toks, stmt);
    if cur.done() {
        return Err(cur.err("empty guard"));
    }
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
                    line: 0,
                    kind: None,
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
// cores instead of the injected dispatcher.

/// Parse context: shared lowering dispatcher plus module table for
/// static call resolution, with `FUNC`/import frames per braced body.
pub struct V2Ctx<'a> {
    pub lower: &'a dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
    pub modules: &'a ModuleTable,
    /// Host-registered function names for the `FUNC` shadow check
    /// (empty when hosts are unknown).
    pub reserved: &'a HashSet<String>,
    /// `FUNC` scopes (innermost last) and import frames, pushed per
    /// braced body; short borrows only,
    /// never held across recursive parses.
    pub funcs: RefCell<Vec<HashSet<String>>>,
    pub imports: RefCell<Vec<Vec<String>>>,
    /// `EXPORT` names in source order, validated at end of parse
    /// (forward references allowed).
    pub exports: RefCell<Vec<String>>,
    /// 1-based source line of the statement currently lowering (the
    /// innermost split statement; updated per statement by the body
    /// loops, read as the base for nested splits). Lets every error
    /// carry its statement's line without threading it through every
    /// header function by hand.
    pub current_line: RefCell<usize>,
}

impl<'a> V2Ctx<'a> {
    /// Context with empty tables.
    pub fn empty(
        lower: &'a dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
        modules: &'a ModuleTable,
        reserved: &'a HashSet<String>,
    ) -> Self {
        Self {
            lower,
            modules,
            reserved,
            funcs: RefCell::new(vec![HashSet::new()]),
            imports: RefCell::new(vec![Vec::new()]),
            exports: RefCell::new(Vec::new()),
            current_line: RefCell::new(0),
        }
    }

    /// Snapshot of everything visible at this point: `FUNC` names
    /// unioned across frames plus imports flattened outer-to-inner
    /// (unions `FUNC` names across frames, flattens imports outer-to-inner).
    fn visible_snapshot(&self) -> (HashSet<String>, Vec<String>) {
        let mut funcs = HashSet::new();
        for scope in self.funcs.borrow().iter() {
            funcs.extend(scope.iter().cloned());
        }
        let mut imports = Vec::new();
        for frame in self.imports.borrow().iter() {
            for module in frame {
                if !imports.contains(module) {
                    imports.push(module.clone());
                }
            }
        }
        (funcs, imports)
    }

    /// Record a `FUNC` name in the innermost scope. Returns false when
    /// already defined in that same scope (nested shadowing stays
    /// allowed (nested shadowing stays permitted).
    fn declare_func(&self, name: &str) -> bool {
        match self.funcs.borrow_mut().last_mut() {
            Some(current) => current.insert(name.to_string()),
            None => true,
        }
    }

    /// Record `IMPORT`ed modules in the innermost frame (deduped,
    /// (deduped; the unknown-module table check lives at the import site),
    /// which lives at the import site).
    fn import_into_frame(&self, modules: &[String]) {
        match self.imports.borrow_mut().last_mut() {
            Some(frame) => {
                for module in modules {
                    if !frame.contains(module) {
                        frame.push(module.clone());
                    }
                }
            }
            None => {
                self.imports.borrow_mut().push(modules.to_vec());
            }
        }
    }
}

fn v2_resolve(ctx: &V2Ctx, stmt: usize, name: &str) -> Result<String, V2Error> {
    let (funcs, imports) = ctx.visible_snapshot();
    resolve_call_name(ctx.modules, &funcs, &imports, &lowering_ctx(stmt), name)
        .map_err(|e| lower_err(stmt, e))
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
    cur.skip_gap_nl();
    if cur.done() {
        return Ok(vec![]);
    }
    let mut args = Vec::new();
    loop {
        let arg = parse_expr_or(&mut cur, ctx)?;
        reject_boundary(&lowering_ctx(stmt), &arg).map_err(|e| cur.err(e.to_string()))?;
        args.push(arg);
        cur.skip_gap_nl();
        if cur.eat_punct(',') {
            cur.skip_gap_nl();
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
                line: 0,
                kind: None,
            });
        };
        if let Expr::Var(var) = arg {
            return Ok(Expr::Inspect(var.clone()));
        }
        return Err(V2Error {
            stmt,
            msg: format!("INSPECT requires a $variable argument, found {arg:?}"),
            line: 0,
            kind: None,
        });
    }
    let qualified = v2_resolve(ctx, stmt, &name)?;
    Ok(Expr::Call {
        name: qualified,
        args,
    })
}

/// RUN exec-form items: atoms only (mirroring `run_exec_arg`), except
/// parenthesized/calls/lists/maps which recurse fully like pest.
/// Anything that is not a clean comma-separated list misses the whole
/// exec shape (`None`): the grammar falls through to the shell path,
/// which reads the bracket literally and lets the shared lowering
/// decide (POSIX `[ -f ... ]` stays shell; non-map blocks and malformed
/// shapes behave exactly like the grammar's fallthrough).
/// (`instruction`), which reads the bracket literally and lets the
/// shared lowering decide (POSIX `[ -f ... ]` stays shell, malformed
/// shapes fail there exactly like the grammar). Only the
/// joint-bracket gate stays a hard error (it precedes this function).
fn parse_run_exec_list(inner: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<Option<Expr>, V2Error> {
    let mut cur = Cursor::expr(inner, stmt);
    cur.skip_gap();
    if cur.done() {
        return Ok(Some(Expr::List(vec![])));
    }
    let mut items = Vec::new();
    loop {
        let item = match parse_run_exec_atom(&mut cur, ctx) {
            Ok(Some(item)) => item,
            // Not an exec shape at all: shell fallthrough.
            _ => return Ok(None),
        };
        if reject_boundary(&lowering_ctx(stmt), &item).is_err() {
            return Ok(None);
        }
        items.push(item);
        cur.skip_gap();
        if cur.eat_punct(',') {
            cur.skip_gap();
            continue;
        }
        break;
    }
    if !cur.done() {
        return Ok(None);
    }
    Ok(Some(Expr::List(items)))
}

/// One RUN-exec item: brace groups try maps only (blocks miss);
/// everything else parses as a full atom (nested lists and map
/// values still admit blocks, like the grammar).
fn parse_run_exec_atom(cur: &mut Cursor, ctx: &V2Ctx) -> Result<Option<Expr>, V2Error> {
    if matches!(
        cur.toks.get(cur.pos).map(|t| &t.kind),
        Some(TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        })
    ) {
        let inner = match &cur.toks[cur.pos].kind {
            TokKind::Group { inner, .. } => inner.clone(),
            _ => unreachable!(),
        };
        return match parse_map_literal(&inner, cur.stmt, ctx) {
            Ok(map) => {
                cur.pos += 1;
                Ok(Some(map))
            }
            Err(_) => Ok(None),
        };
    }
    parse_expr_atom(cur, ctx).map(Some)
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
        line: 0,
        kind: None,
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
    let fail = |msg: String| V2Error {
        stmt,
        msg,
        line: 0,
        kind: None,
    };
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
                line: 0,
                kind: None,
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
/// and tokens consumed: dedicated await/cancel, while/remote, calls,
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
    // Nested `WHILE`/`REMOTE` recurse structurally (both ride every
    // inner-command set in the grammar); misses fall through to
    // generic below.
    if head_word.as_deref() == Some("WHILE") {
        match parse_while_single(toks, pos, stmt, ctx) {
            Ok(Some((kind, used))) => return Ok((kind, used)),
            Ok(None) => {}
            Err(e) => return Err(e),
        }
    }
    if head_word.as_deref() == Some("REMOTE") {
        match parse_remote_single(toks, pos, stmt, ctx) {
            Ok(Some((kind, used))) => return Ok((kind, used)),
            Ok(None) => {}
            Err(e) => return Err(e),
        }
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
            crate::resolve::check_func_ident(&lowering_ctx(stmt), &call_name)
                .map_err(|e| lower_err(stmt, e))?;
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
                line: 0,
                kind: None,
            });
        }
        let inner = match &toks[pos + 1].kind {
            TokKind::Group { inner, .. } => inner.clone(),
            _ => unreachable!(),
        };
        let exec_list = parse_run_exec_list(&inner, stmt, ctx)?;
        // Non-map blocks miss the exec shape (the grammar has no
        // `block` alternative here); fall through to generic below,
        // which reads the shell path literally.
        if let Some(list) = exec_list {
            let kind = (ctx.lower)("RUN", vec![Arg::Expr(list)]).map_err(|e| lower_err(stmt, e))?;
            return Ok((kind, 2));
        }
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
            if let Some(found) = parse_inherit_env_at(toks, pos, stmt)? {
                return Ok(found);
            }
        }
        _ => {}
    }
    // Generic instruction: consumes the whole run; unknown heads
    // (LET/RETURN/FUNC/IF/...) fail in lowering.
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
fn parse_inherit_keys(inner: &[Tok], stmt: usize) -> Result<Option<Vec<String>>, V2Error> {
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
        let Some(key) = render_inherit_key(part, stmt)? else {
            return Ok(None);
        };
        out.push(key);
    }
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some(out))
}

/// True when a `/` opens an inline comment: joint `//` or `/*`
/// (mirroring the grammar's comment literals, which need adjacency;
/// a gapped second char is payload text, like `a/ /b`). Used
/// everywhere `gap`/`gap_nl` admit comments.
fn is_comment_open(toks: &[Tok], pos: usize) -> bool {
    matches!(toks.get(pos).map(|t| &t.kind), Some(TokKind::Punct('/')))
        && matches!(
            toks.get(pos + 1).map(|t| (&t.kind, t.joint)),
            Some((TokKind::Punct('/'), true)) | Some((TokKind::Punct('*'), true))
        )
}

/// Skip one inline comment starting at `pos` (which holds the `/`;
/// see [`is_comment_open`): `//` runs to a newline or the end,
/// `/* */` balances (nesting like the grammar) across newlines.
/// Returns the first position past the comment, or `None` when the
/// block never closes (the grammar fails unclosed comments too).
fn skip_inline_comment(toks: &[Tok], pos: usize) -> Option<usize> {
    let second = match toks.get(pos + 1).map(|t| &t.kind) {
        Some(TokKind::Punct('/')) => '/',
        Some(TokKind::Punct('*')) => '*',
        _ => return None,
    };
    if second == '/' {
        let mut p = pos + 2;
        while p < toks.len() && !matches!(&toks[p].kind, TokKind::Newline) {
            p += 1;
        }
        return Some(p);
    }
    let mut depth = 1usize;
    let mut p = pos + 2;
    while p < toks.len() {
        let is_open = matches!(toks.get(p).map(|t| &t.kind), Some(TokKind::Punct('/')))
            && matches!(
                toks.get(p + 1).map(|t| (&t.kind, t.joint)),
                Some((TokKind::Punct('*'), true))
            );
        let is_close = matches!(toks.get(p).map(|t| &t.kind), Some(TokKind::Punct('*')))
            && matches!(
                toks.get(p + 1).map(|t| (&t.kind, t.joint)),
                Some((TokKind::Punct('/'), true))
            );
        if is_open {
            depth += 1;
            p += 2;
        } else if is_close {
            depth -= 1;
            p += 2;
            if depth == 0 {
                return Some(p);
            }
        } else {
            p += 1;
        }
    }
    None
}

/// Parse one inherit key: a maximal key-char run, then only
/// trailing gap content (`gap_nl`: newlines, comments, `#`). Key
/// chars mirror `guard_key_char` (everything but `: = ! , | ( ) ] $`,
/// whitespace, and the `//` / `/*` comment opens); a joint `#`
/// stays literal, a gapped one opens a trailing comment. Groups and
/// strings render raw for the exclusion check (pest reads them as
/// ordinary characters here). `None` on shape miss.
fn render_inherit_key(part: &[Tok], stmt: usize) -> Result<Option<String>, V2Error> {
    let mut out = String::new();
    let mut i = 0;
    // Key run: words, puncts (except comment opens), groups and
    // strings rendered raw; ends at any gap, newline, comment, or
    // `//` / `/*` open.
    while i < part.len() {
        let tok = &part[i];
        if i > 0 && !tok.joint {
            break;
        }
        match &tok.kind {
            TokKind::Word(w) => {
                out.push_str(w);
                i += 1;
            }
            TokKind::Punct('/') if is_comment_open(part, i) => break,
            // `#` stays literal at key start (`#c`) or joint
            // (`a#b`); gapped mid-key it opens a trailing
            // comment like the grammar's `gap_nl`.
            TokKind::Punct('#') if tok.joint || out.is_empty() => {
                // `#` stays literal at key start (`#c`) or joint
                // (`a#b`).
                out.push('#');
                i += 1;
            }
            // Gapped mid-key `#` opens a trailing comment (handled
            // by the trailer check below, like `gap_nl`).
            TokKind::Punct('#') => break,
            TokKind::Punct(c) => {
                out.push(*c);
                i += 1;
            }
            TokKind::Semicolon => {
                out.push(';');
                i += 1;
            }
            TokKind::Str { text, quote } => {
                out.push(*quote);
                out.push_str(text);
                out.push(*quote);
                i += 1;
            }
            TokKind::BlockStr(full) => {
                out.push_str(full);
                i += 1;
            }
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
                i += 1;
            }
            // Newlines and comment markers end the key; the trailer
            // check below admits them.
            TokKind::Newline | TokKind::Comment(_) => break,
        }
    }
    if out.is_empty() {
        return Ok(None);
    }
    // Trailer: only gap content (`gap_nl`) may follow the key.
    let mut j = i;
    while j < part.len() {
        let tok = &part[j];
        match &tok.kind {
            TokKind::Newline | TokKind::Comment(_) => {
                j += 1;
            }
            TokKind::Punct('/') if is_comment_open(part, j) => {
                j = skip_inline_comment(part, j).ok_or(V2Error {
                    stmt,
                    msg: "unclosed block comment".to_string(),
                    line: 0,
                    kind: None,
                })?;
            }
            TokKind::Punct('#') => {
                // `#` comment runs to a newline or the part end.
                while j < part.len() && !matches!(&part[j].kind, TokKind::Newline) {
                    j += 1;
                }
            }
            _ => return Ok(None),
        }
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
fn parse_inherit_env_at(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
) -> Result<Option<(StepKind, usize)>, V2Error> {
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
    match parse_inherit_keys(inner, stmt)? {
        Some(keys) => Ok(Some((StepKind::InheritEnv { keys }, 2))),
        None => Ok(None),
    }
}

/// Parse a brace block body into steps: statements split and lower
/// recursively, each keeping its own guard. `mark` selects scope
/// marking: top-level guard-block bodies mark (each nesting level
/// bumps first/last, like the pest stateful backend); wrapper bodies
/// (`IF`/`FOR`/`WHILE`/`FUNC`/`REMOTE`/wrappers/io-blocks) never mark
/// (the pest `parse_block_elements_with_lower` path pins 0/0).
fn parse_body_steps(
    inner: &[Tok],
    stmt: usize,
    ctx: &V2Ctx,
    mark: bool,
) -> Result<Vec<Step>, V2Error> {
    // One function/import scope per braced body, mirroring the
    // pest backend (and the runtime push/pop boundary): same-block
    // `FUNC` redefinition errors, nested shadowing stays allowed.
    // Errors abort the whole parse (frames discarded with it), so no
    // exit runs on failure — exactly like the pest backend.
    ctx.funcs.borrow_mut().push(HashSet::new());
    ctx.imports.borrow_mut().push(Vec::new());
    // Nested splits count lines from the enclosing statement's line
    // (headers are single-line, so the brace shares it). Save and
    // restore the discipline around the loop: sibling bodies after
    // this one must see the enclosing line, not the last inner one.
    // (On error the parse aborts and the context is discarded.)
    let saved = *ctx.current_line.borrow();
    let mut out = Vec::new();
    for s in split_statements(inner, saved)? {
        // The innermost statement owns error positions: record it for
        // nested splits and stamp failures that have none yet (deeper
        // frames stamp first, so this never overwrites real positions).
        *ctx.current_line.borrow_mut() = s.line;
        out.extend(lower_stmt_steps(&s, stmt, ctx, true, mark).map_err(|e| stamp_line(e, s.line))?);
    }
    *ctx.current_line.borrow_mut() = saved;
    ctx.funcs.borrow_mut().pop();
    ctx.imports.borrow_mut().pop();
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
            line: 0,
            kind: None,
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
        line: 0,
        kind: None,
    })?;
    let kind = StepKind::WithIo {
        bindings,
        cmd: Box::new(cmd),
    };
    crate::commands::reject_nested_inherit_env(&span, &kind).map_err(|e| V2Error {
        stmt,
        msg: e.to_string(),
        line: 0,
        kind: None,
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
        let steps = parse_body_steps(&inner, stmt, ctx, false)?;
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
        line: 0,
        kind: None,
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
        (parse_body_steps(&inner, stmt, ctx, false)?, 1)
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
        let body = parse_body_steps(&inner, stmt, ctx, false)?;
        let span = lowering_ctx(stmt);
        crate::commands::reject_with_io_in_async(&span, &body).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
            line: 0,
            kind: None,
        })?;
        let kind = StepKind::AsyncBlock { body };
        crate::commands::reject_nested_inherit_env(&span, &kind).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
            line: 0,
            kind: None,
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
        line: 0,
        kind: None,
    })?;
    let kind = StepKind::AsyncBlock { body };
    crate::commands::reject_nested_inherit_env(&span, &kind).map_err(|e| V2Error {
        stmt,
        msg: e.to_string(),
        line: 0,
        kind: None,
    })?;
    Ok(Some((kind, 1 + used)))
}

// ---- Slice 5: block statements, directives, REMOTE ----
// `IF`/`FOR`/`WHILE`/`FUNC` parse their headers token-natively with
// miss-to-generic fallback (the grammar's committed `instruction`
// path); bodies lower recursively with pushed scopes. `TYPE` builds
// aliases through a maximal type-shape match; `IMPORT`/`EXPORT` are
// zero-step frame/table directives; `REMOTE` validates targets,
// bindings, bodies, and outer-call containment through the shared
// validators. Guard blocks fold guards with scope markers at top
// level and mirror the nested quirk formula in bodies. A whole-script
// post-pass mirrors the finish validations (prelude order, COPY
// placement, EXPORT membership).

/// Uppercase identifier mirror (`func_ident`, `type_name`,
/// `import_module`): ASCII upper start, upper/digit/`_` rest.
fn is_upper_ident(w: &str) -> bool {
    !w.is_empty()
        && w.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && w.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Lowercase identifier mirror (`field_name`): ASCII lower start,
/// lower/digit/`_` rest.
fn is_lower_ident(w: &str) -> bool {
    !w.is_empty()
        && w.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && w.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `$name` with gapless adjacency (atomic `dollar_ident`); `None` on
/// shape miss (the caller falls back to generic, which fails in
/// lowering like the grammar's committed `instruction`).
fn peek_dollar_var(toks: &[Tok], pos: usize) -> Option<(String, usize)> {
    match (toks.get(pos), toks.get(pos + 1)) {
        (Some(dollar), Some(name))
            if matches!(&dollar.kind, TokKind::Punct('$'))
                && name.joint
                && matches!(&name.kind, TokKind::Word(w) if is_ident_word(w)) =>
        {
            match &name.kind {
                TokKind::Word(w) => Some((w.clone(), 2)),
                _ => unreachable!(),
            }
        }
        _ => None,
    }
}

/// Maximal `type_tag` match over toks (non-atomic: gaps free
/// everywhere): atoms with `<>` args, `|` unions, `name?:` fields.
/// Returns the exclusive end index, or `None` on shape miss.
fn parse_type_shape(toks: &[Tok], pos: usize) -> Option<usize> {
    let mut p = parse_type_atom(toks, pos)?;
    while matches!(toks.get(p).map(|t| &t.kind), Some(TokKind::Punct('|'))) {
        p = parse_type_atom(toks, p + 1)?;
    }
    Some(p)
}

fn parse_type_atom(toks: &[Tok], pos: usize) -> Option<usize> {
    match toks.get(pos).map(|t| &t.kind) {
        Some(TokKind::Word(w)) if is_upper_ident(w) => {}
        _ => return None,
    }
    let mut p = pos + 1;
    if matches!(toks.get(p).map(|t| &t.kind), Some(TokKind::Punct('<'))) {
        p = parse_type_args(toks, p + 1)?;
        if !matches!(toks.get(p).map(|t| &t.kind), Some(TokKind::Punct('>'))) {
            return None;
        }
        p += 1;
    }
    Some(p)
}

fn parse_type_args(toks: &[Tok], pos: usize) -> Option<usize> {
    let mut p = parse_type_field(toks, pos)?;
    while matches!(toks.get(p).map(|t| &t.kind), Some(TokKind::Punct(','))) {
        p = parse_type_field(toks, p + 1)?;
    }
    Some(p)
}

fn parse_type_field(toks: &[Tok], pos: usize) -> Option<usize> {
    let mut p = pos;
    // Optional `name?:` prefix (lowercase names only, like the
    // grammar's `field_name`).
    if let Some(TokKind::Word(w)) = toks.get(p).map(|t| &t.kind)
        && is_lower_ident(w)
    {
        let mut q = p + 1;
        if matches!(toks.get(q).map(|t| &t.kind), Some(TokKind::Punct('?'))) {
            q += 1;
        }
        if matches!(toks.get(q).map(|t| &t.kind), Some(TokKind::Punct(':'))) {
            p = q + 1;
        }
    }
    parse_type_shape(toks, p)
}

/// Single (non-block) `IF`: condition plus then-block with an
/// opportunistic `ELSE` chain (a malformed `ELSE` stays for the
/// element loop, which fails it in lowering exactly like the
/// grammar). `None` on shape miss (missing/joint-agnostic block or
/// bad condition: generic fails it like pest); expression failures
/// with an inline-block failure propagate (blocks lower here, so
/// their errors surface directly).
fn parse_if_at(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<Option<(StepKind, usize)>, V2Error> {
    // Condition entry skips leading gap comments (`IF` uses `gap`).
    let mut lead = Cursor::expr(&toks[pos + 1..], stmt);
    lead.skip_gap();
    let mut cur = Cursor::expr(&toks[pos + 1 + lead.pos..], stmt);
    let cond = match parse_expr_checked(&mut cur, ctx) {
        Ok(expr) => expr,
        Err(e) if e.msg.starts_with("unsupported in parse_v2") => return Err(e),
        Err(_) => return Ok(None),
    };
    let cond_end = pos + 1 + lead.pos + cur.pos;
    // Then-block: any gap (`gap` zero-width), never a template.
    let then_brace = match toks.get(cond_end) {
        Some(t)
            if matches!(
                &t.kind,
                TokKind::Group {
                    delim: GroupDelim::Brace,
                    ..
                }
            ) && !is_template_brace(t) =>
        {
            t
        }
        _ => return Ok(None),
    };
    let then_inner = match &then_brace.kind {
        TokKind::Group { inner, .. } => inner.clone(),
        _ => unreachable!(),
    };
    let then_body = parse_body_steps(&then_inner, stmt, ctx, false)?;
    let mut p = cond_end + 1;
    // Gap comments between the blocks and `ELSE` vanish (merge
    // junctions contribute newlines, crossed like `blank*`).
    {
        let mut gcur = Cursor::expr(&toks[p..], stmt);
        gcur.skip_gap_nl();
        p += gcur.pos;
    }
    let mut else_ifs = Vec::new();
    let mut else_body = None;
    loop {
        // `ELSE IF cond block`: any gaps (non-atomic chain).
        let is_else_if = matches!(
            toks.get(p).map(|t| &t.kind),
            Some(TokKind::Word(w)) if w == "ELSE"
        ) && matches!(
            toks.get(p + 1).map(|t| &t.kind),
            Some(TokKind::Word(w)) if w == "IF"
        );
        if is_else_if {
            let mut ecur = Cursor::expr(&toks[p + 2..], stmt);
            ecur.skip_gap();
            let econd = match parse_expr_checked(&mut ecur, ctx) {
                Ok(expr) => expr,
                Err(e) if e.msg.starts_with("unsupported in parse_v2") => return Err(e),
                Err(_) => break,
            };
            let ebrace = p + 2 + ecur.pos;
            let eblock_ok = matches!(
                toks.get(ebrace).map(|t| &t.kind),
                Some(TokKind::Group {
                    delim: GroupDelim::Brace,
                    ..
                })
            ) && toks.get(ebrace).is_some_and(|t| !is_template_brace(t));
            if !eblock_ok {
                break;
            }
            let einner = match &toks[ebrace].kind {
                TokKind::Group { inner, .. } => inner.clone(),
                _ => unreachable!(),
            };
            let ebody = parse_body_steps(&einner, stmt, ctx, false)?;
            else_ifs.push((Box::new(econd), ebody));
            p = ebrace + 1;
            // Gap comments before a further `ELSE` vanish too
            // (merge junctions contribute newlines).
            {
                let mut gcur = Cursor::expr(&toks[p..], stmt);
                gcur.skip_gap_nl();
                p += gcur.pos;
            }
            continue;
        }
        // `ELSE block`: same opportunism (a bare `ELSE` stays for
        // the loop, failing in lowering like the grammar).
        let is_else = matches!(
            toks.get(p).map(|t| &t.kind),
            Some(TokKind::Word(w)) if w == "ELSE"
        ) && matches!(
            toks.get(p + 1).map(|t| &t.kind),
            Some(TokKind::Group {
                delim: GroupDelim::Brace,
                ..
            })
        ) && toks.get(p + 1).is_some_and(|t| !is_template_brace(t));
        if !is_else {
            break;
        }
        let einner = match &toks[p + 1].kind {
            TokKind::Group { inner, .. } => inner.clone(),
            _ => unreachable!(),
        };
        else_body = Some(parse_body_steps(&einner, stmt, ctx, false)?);
        p += 2;
        break;
    }
    let kind = StepKind::If {
        cond: Box::new(cond),
        then_body,
        else_ifs,
        else_body,
    };
    Ok(Some((kind, p - pos)))
}

/// Single (non-block) `FOR`: `$var[: TYPE] (, $var[: TYPE])? IN
/// expr` plus block. Header shapes miss to generic (a third variable,
/// a missing block, a bad name all fail in lowering like the
/// grammar); completed headers validate counts and key types loudly.
/// Expression failures from inline blocks propagate (blocks lower
/// here); other expression failures miss (generic fails them like a
/// committed `instruction` would).
fn parse_for_at(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<Option<(StepKind, usize)>, V2Error> {
    let mut p = pos + 1;
    let mut idents = Vec::new();
    let mut types = Vec::new();
    // First `$var[: TYPE]` (gapless `$var`; gaps free elsewhere).
    let Some((var1, _)) = peek_dollar_var(toks, p) else {
        return Ok(None);
    };
    idents.push(var1);
    p += 2;
    let mut cur = Cursor::expr(&toks[p..], stmt);
    cur.skip_gap();
    p += cur.pos;
    if matches!(toks.get(p).map(|t| &t.kind), Some(TokKind::Punct(':'))) {
        p += 1;
        let mut tcur = Cursor::expr(&toks[p..], stmt);
        tcur.skip_gap();
        let tstart = p + tcur.pos;
        let Some(tend) = parse_type_shape(toks, tstart) else {
            return Ok(None);
        };
        // `render_type_region` canonicalizes (shared spelling).
        types.push(render_type_region(&toks[tstart..tend], stmt)?);
        p = tend;
    }
    // Optional `, $var2[: TYPE2]`.
    {
        let mut ccur = Cursor::expr(&toks[p..], stmt);
        ccur.skip_gap();
        if matches!(
            ccur.toks.get(ccur.pos).map(|t| &t.kind),
            Some(TokKind::Punct(','))
        ) {
            ccur.pos += 1;
            ccur.skip_gap();
            let base = p + ccur.pos;
            let Some((var2, _)) = peek_dollar_var(toks, base) else {
                return Ok(None);
            };
            idents.push(var2);
            let mut q = base + 2;
            let mut qcur = Cursor::expr(&toks[q..], stmt);
            qcur.skip_gap();
            q += qcur.pos;
            if matches!(toks.get(q).map(|t| &t.kind), Some(TokKind::Punct(':'))) {
                q += 1;
                let mut tcur = Cursor::expr(&toks[q..], stmt);
                tcur.skip_gap();
                let tstart = q + tcur.pos;
                let Some(tend) = parse_type_shape(toks, tstart) else {
                    return Ok(None);
                };
                types.push(render_type_region(&toks[tstart..tend], stmt)?);
                q = tend;
            }
            p = q;
        }
    }
    // `IN expr block`.
    {
        let mut icur = Cursor::expr(&toks[p..], stmt);
        icur.skip_gap();
        if !matches!(
            icur.toks.get(icur.pos).map(|t| &t.kind),
            Some(TokKind::Word(w)) if w == "IN"
        ) {
            return Ok(None);
        }
        icur.pos += 1;
        p += icur.pos;
    }
    let mut ecur = Cursor::expr(&toks[p..], stmt);
    ecur.skip_gap();
    let in_expr = match parse_expr_checked(&mut ecur, ctx) {
        Ok(expr) => expr,
        Err(e) if e.msg.starts_with("unsupported in parse_v2") => return Err(e),
        Err(_) => return Ok(None),
    };
    p += ecur.pos;
    let block_ok = matches!(
        toks.get(p).map(|t| &t.kind),
        Some(TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        })
    ) && toks.get(p).is_some_and(|t| !is_template_brace(t));
    if !block_ok {
        return Ok(None);
    }
    let inner = match &toks[p].kind {
        TokKind::Group { inner, .. } => inner.clone(),
        _ => unreachable!(),
    };
    let body = parse_body_steps(&inner, stmt, ctx, false)?;
    p += 1;
    if idents.len() != types.len() {
        return Err(V2Error {
            stmt,
            msg: format!(
                "FOR requires explicit types: FOR $item: TYPE IN <expr> (got {} vars, {} types)",
                idents.len(),
                types.len()
            ),
            line: 0,
            kind: Some(ParseErrorKind::Validation {
                command: "FOR".to_string(),
            }),
        });
    }
    let (key_var, key_type, var, var_type) = match idents.len() {
        1 => (
            None,
            None,
            idents.into_iter().next().unwrap_or_default(),
            types.into_iter().next().unwrap_or_default(),
        ),
        2 => {
            let mut iv = idents.into_iter();
            let mut tv = types.into_iter();
            (
                Some(iv.next().unwrap_or_default()),
                Some(tv.next().unwrap_or_default()),
                iv.next().unwrap_or_default(),
                tv.next().unwrap_or_default(),
            )
        }
        _ => {
            return Err(V2Error {
                stmt,
                msg: "FOR requires one or two variables".to_string(),
                line: 0,
                kind: Some(ParseErrorKind::Validation {
                    command: "FOR".to_string(),
                }),
            });
        }
    };
    if let Some(kt) = &key_type
        && kt != "STRING"
        && kt != "INT"
    {
        return Err(V2Error {
            stmt,
            msg: format!("FOR key variable must be INT or STRING, got {kt}"),
            line: 0,
            kind: Some(ParseErrorKind::Validation {
                command: "FOR".to_string(),
            }),
        });
    }
    let kind = StepKind::For {
        key_var,
        key_type,
        var,
        var_type,
        in_expr,
        body,
    };
    Ok(Some((kind, p - pos)))
}

/// Single (non-block) `WHILE`: condition plus block. `None` on
/// shape miss (missing block: generic fails it like pest);
/// expression failures from inline blocks propagate, others miss
/// (generic fails them like a committed `instruction` would).
/// No leading gap-skip: `WHILE` takes `sep`, so a joint follower
/// never reaches here with a valid shape... (misses fall through
/// the same way regardless).
fn parse_while_single(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<Option<(StepKind, usize)>, V2Error> {
    let mut cur = Cursor::expr(&toks[pos + 1..], stmt);
    let cond = match parse_expr_checked(&mut cur, ctx) {
        Ok(expr) => expr,
        Err(e) if e.msg.starts_with("unsupported in parse_v2") => return Err(e),
        Err(_) => return Ok(None),
    };
    let brace = pos + 1 + cur.pos;
    let block_ok = matches!(
        toks.get(brace).map(|t| &t.kind),
        Some(TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        })
    ) && toks.get(brace).is_some_and(|t| !is_template_brace(t));
    if !block_ok {
        return Ok(None);
    }
    let inner = match &toks[brace].kind {
        TokKind::Group { inner, .. } => inner.clone(),
        _ => unreachable!(),
    };
    let body = parse_body_steps(&inner, stmt, ctx, false)?;
    let kind = StepKind::While {
        cond: Box::new(cond),
        body,
    };
    Ok(Some((kind, brace + 1 - pos)))
}

/// Single (non-block) `FUNC`: name, parenthesized params, block. The
/// shape parses purely first (misses fall to generic, declaring
/// nothing — the grammar only lowers matched pairs); then the name
/// declares in the enclosing scope before the body lowers (so
/// recursion resolves), and the body pushes a fresh scope.
/// Duplicates, reserved shadows, and parameter dupes fail loudly.
fn parse_func_at(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<Option<(StepKind, usize)>, V2Error> {
    let fail = |msg: String| V2Error {
        stmt,
        msg,
        line: 0,
        kind: Some(ParseErrorKind::Validation {
            command: "FUNC".to_string(),
        }),
    };
    // Phase 1 (pure shape): name, parens, params, block presence.
    // Name: UPPERCASE ident (lowercase misses to generic, probed).
    let def_name = match toks.get(pos + 1).map(|t| &t.kind) {
        Some(TokKind::Word(w))
            if crate::resolve::check_func_ident(&lowering_ctx(stmt), w).is_ok() =>
        {
            w.clone()
        }
        _ => return Ok(None),
    };
    // Paren group at any gap (`gap` zero-width): params live inside
    // its inner tokens, parsed with `gap` discipline (newlines miss,
    // like the grammar).
    let mut p = pos + 2;
    let paren_inner = match toks.get(p).map(|t| &t.kind) {
        Some(TokKind::Group {
            delim: GroupDelim::Paren,
            inner,
            ..
        }) => inner.clone(),
        _ => return Ok(None),
    };
    p += 1;
    let mut params = Vec::new();
    {
        let mut pcur = Cursor::expr(&paren_inner, stmt);
        pcur.skip_gap();
        if !pcur.done() {
            loop {
                let base = pcur.pos;
                let Some((pname, _)) = peek_dollar_var(&paren_inner, base) else {
                    return Ok(None);
                };
                pcur.pos = base + 2;
                pcur.skip_gap();
                if !pcur.eat_punct(':') {
                    return Ok(None);
                }
                pcur.skip_gap();
                let tstart = pcur.pos;
                let Some(tend) = parse_type_shape(&paren_inner, tstart) else {
                    return Ok(None);
                };
                // Post-match the span holds words/puncts only, so the
                // shared renderer cannot fail; stay failed regardless.
                let ptype =
                    render_type_region(&paren_inner[tstart..tend], stmt).unwrap_or_default();
                params.push((pname, ptype));
                pcur.pos = tend;
                pcur.skip_gap();
                if pcur.eat_punct(',') {
                    pcur.skip_gap();
                    // Trailing comma misses (a param must follow).
                    if pcur.done() {
                        return Ok(None);
                    }
                    continue;
                }
                break;
            }
            // Trailing junk (including newlines) misses to generic.
            if !pcur.done() {
                return Ok(None);
            }
        }
    }
    // Block at any gap (never a template); missing blocks miss to
    // generic (probed `FUNC FOO() ECHO hi`).
    let block_ok = matches!(
        toks.get(p).map(|t| &t.kind),
        Some(TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        })
    ) && toks.get(p).is_some_and(|t| !is_template_brace(t));
    if !block_ok {
        return Ok(None);
    }
    // Phase 2 (effects): reserved shadows and same-scope duplicates
    // fail at the definition line, verbatim like the pest backend.
    if ctx.reserved.contains(&def_name) || ctx.modules.reserved_base_names().contains(&def_name) {
        return Err(fail(format!(
            "FUNC {def_name} cannot shadow reserved function `{def_name}`"
        )));
    }
    if !ctx.declare_func(&def_name) {
        return Err(fail(format!(
            "duplicate function `{def_name}` in same scope"
        )));
    }
    let mut seen = HashSet::new();
    for (pname, _) in &params {
        if !seen.insert(pname.clone()) {
            return Err(fail(format!(
                "FUNC {def_name} declares duplicate parameter ${pname}"
            )));
        }
    }
    let inner = match &toks[p].kind {
        TokKind::Group { inner, .. } => inner.clone(),
        _ => unreachable!(),
    };
    let body = parse_body_steps(&inner, stmt, ctx, false)?;
    let kind = StepKind::FuncDef {
        name: def_name,
        params,
        body,
        docs: None,
    };
    Ok(Some((kind, p + 1 - pos)))
}

/// `REMOTE` header bindings `[$var, env:KEY]` over a bracket
/// group's inner tokens: comma-separated, newlines and comments free
/// like `gap_nl`. `$`+name and `env`+`:`+key all ride gapless (the
/// enclosing rule is compound-atomic). Empty lists parse (probed);
/// anything else misses the shape (`None`: generic fails it like
/// the grammar's dropped optional).
#[allow(clippy::type_complexity)]
fn parse_remote_bindings(
    inner: &[Tok],
    stmt: usize,
) -> Result<Option<(Vec<String>, Vec<String>)>, V2Error> {
    let _ = stmt;
    let mut parts: Vec<&[Tok]> = Vec::new();
    let mut start = 0;
    for (i, tok) in inner.iter().enumerate() {
        if matches!(&tok.kind, TokKind::Punct(',')) {
            parts.push(&inner[start..i]);
            start = i + 1;
        }
    }
    parts.push(&inner[start..]);
    // An empty list (`[]`, whitespace/comment-only) holds no bindings.
    if parts.len() == 1
        && parts[0]
            .iter()
            .all(|t| matches!(&t.kind, TokKind::Newline | TokKind::Comment(_)))
    {
        return Ok(Some((Vec::new(), Vec::new())));
    }
    let mut vars = Vec::new();
    let mut env = Vec::new();
    for part in parts {
        // Edges admit gaps, newlines, and comments, like `gap_nl`.
        let mut part = part;
        while matches!(
            part.first().map(|t| &t.kind),
            Some(TokKind::Newline | TokKind::Comment(_))
        ) {
            part = &part[1..];
        }
        while matches!(
            part.last().map(|t| &t.kind),
            Some(TokKind::Newline | TokKind::Comment(_))
        ) {
            part = &part[..part.len() - 1];
        }
        // `$var`: gapless `$` + ident.
        if matches!(part.first().map(|t| &t.kind), Some(TokKind::Punct('$'))) && part.len() == 2 {
            if part[1].joint
                && let TokKind::Word(w) = &part[1].kind
                && is_ident_word(w)
            {
                vars.push(w.clone());
                continue;
            }
            return Ok(None);
        }
        // `env:KEY`: gapless triple with an uppercase key.
        if part.len() == 3
            && matches!(&part[0].kind, TokKind::Word(w) if w == "env")
            && matches!(&part[1].kind, TokKind::Punct(':'))
            && part[1].joint
            && part[2].joint
        {
            if let TokKind::Word(k) = &part[2].kind
                && is_env_key(k)
            {
                env.push(k.clone());
                continue;
            }
            return Ok(None);
        }
        return Ok(None);
    }
    Ok(Some((vars, env)))
}

/// `IMPORT [...]` or bare `IMPORT MOD` at `pos`: bracket rides
/// gapped (`sep`), modules validate against the table with the exact
/// backend message, and the innermost frame records them (deduped).
/// Returns tokens consumed, or `None` on shape miss (generic fails
/// it in lowering like the grammar). Emits no step; the caller
/// owns guard handling (top-level guarded fails, in-block leaks).
fn parse_import_at(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<Option<usize>, V2Error> {
    let fail = |msg: String| V2Error {
        stmt,
        msg,
        line: 0,
        kind: Some(ParseErrorKind::Validation {
            command: "IMPORT".to_string(),
        }),
    };
    let next = match toks.get(pos + 1) {
        // `sep` after `IMPORT`: joint followers miss.
        Some(t) if !t.joint => t,
        _ => return Ok(None),
    };
    let mut modules = Vec::new();
    if matches!(
        &next.kind,
        TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        }
    ) {
        return Ok(None);
    }
    if let TokKind::Group {
        delim: GroupDelim::Bracket,
        inner,
        ..
    } = &next.kind
    {
        // `import_list`: comma-separated uppercase modules, `ws?`
        // gaps (newlines ok, comments MISS — `ws` admits neither).
        let mut parts: Vec<&[Tok]> = Vec::new();
        let mut start = 0;
        for (i, tok) in inner.iter().enumerate() {
            if matches!(&tok.kind, TokKind::Punct(',')) {
                parts.push(&inner[start..i]);
                start = i + 1;
            }
        }
        parts.push(&inner[start..]);
        for part in parts {
            let mut part = part;
            while matches!(part.first().map(|t| &t.kind), Some(TokKind::Newline)) {
                part = &part[1..];
            }
            while matches!(part.last().map(|t| &t.kind), Some(TokKind::Newline)) {
                part = &part[..part.len() - 1];
            }
            match part {
                [tok] => match &tok.kind {
                    TokKind::Word(w) if is_upper_ident(w) => modules.push(w.clone()),
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            }
        }
        if modules.is_empty() {
            return Ok(None);
        }
    } else if let TokKind::Word(w) = &next.kind {
        // Bare `IMPORT MOD` (single module, probed).
        if !is_upper_ident(w) {
            return Ok(None);
        }
        modules.push(w.clone());
    } else {
        return Ok(None);
    }
    // Unknown modules fail with the exact backend message (sorted
    // known list (sorted, like the old backend)).
    for module in &modules {
        if !ctx.modules.modules.contains_key(module) {
            let mut known: Vec<String> = ctx.modules.modules.keys().cloned().collect();
            known.sort();
            return Err(fail(format!(
                "unknown module `{module}`; known modules: {}",
                known.join(", ")
            )));
        }
    }
    ctx.import_into_frame(&modules);
    Ok(Some(2))
}

/// One `EXPORT` name: an uppercase word with an optional joint
/// `::UPPER` tail. Returns the name and tokens consumed, or `None`
/// on shape miss.
fn parse_export_name(toks: &[Tok], pos: usize) -> Option<(String, usize)> {
    let head = match toks.get(pos).map(|t| &t.kind) {
        Some(TokKind::Word(w)) if is_upper_ident(w) => w.clone(),
        _ => return None,
    };
    let mut name = head;
    let mut used = 1;
    // Greedy-prefix `::tail` (atomic, like the grammar): a valid
    // tail joins, an invalid one stays trailing (failing downstream
    // exactly like the grammar's committed prefix).
    if matches!(
        toks.get(pos + 1).map(|t| &t.kind),
        Some(TokKind::Punct(':'))
    ) && toks.get(pos + 1).is_some_and(|t| t.joint)
        && matches!(
            toks.get(pos + 2).map(|t| &t.kind),
            Some(TokKind::Punct(':'))
        )
        && toks.get(pos + 2).is_some_and(|t| t.joint)
        && let Some(TokKind::Word(tail)) = toks.get(pos + 3).map(|t| &t.kind)
        && toks[pos + 3].joint
        && is_upper_ident(tail)
    {
        name.push_str("::");
        name.push_str(tail);
        used = 4;
    }
    Some((name, used))
}

/// `EXPORT ...` names at `pos`: bracket list (strict, `ws` gaps:
/// newlines ok, comments miss) or a bare greedy-prefix name.
/// Returns names plus tokens consumed, or `None` on shape miss
/// (generic fails it in lowering like the grammar). Recording and
/// duplicate checks live with the caller (they need the guard and
/// level context).
fn parse_export_at(toks: &[Tok], pos: usize) -> Option<(Vec<String>, usize)> {
    let next = match toks.get(pos + 1) {
        // `sep` after `EXPORT`: joint followers miss.
        Some(t) if !t.joint => t,
        _ => return None,
    };
    if let TokKind::Group {
        delim: GroupDelim::Bracket,
        inner,
        ..
    } = &next.kind
    {
        let mut parts: Vec<&[Tok]> = Vec::new();
        let mut start = 0;
        for (i, tok) in inner.iter().enumerate() {
            if matches!(&tok.kind, TokKind::Punct(',')) {
                parts.push(&inner[start..i]);
                start = i + 1;
            }
        }
        parts.push(&inner[start..]);
        let mut names = Vec::new();
        for part in parts {
            let mut part = part;
            while matches!(part.first().map(|t| &t.kind), Some(TokKind::Newline)) {
                part = &part[1..];
            }
            while matches!(part.last().map(|t| &t.kind), Some(TokKind::Newline)) {
                part = &part[..part.len() - 1];
            }
            // Exactly one name per entry (entries never trail).
            let (name, used) = parse_export_name(part, 0)?;
            if used != part.len() {
                return None;
            }
            names.push(name);
        }
        if names.is_empty() {
            return None;
        }
        return Some((names, 2));
    }
    if matches!(&next.kind, TokKind::Group { .. }) {
        return None;
    }
    // Bare name (greedy `::` prefix; trailing loops outside).
    parse_export_name(toks, pos + 1).map(|(name, used)| (vec![name], 1 + used))
}

/// `TYPE NAME = SHAPE` at `pos`: uppercase name, single `=`, maximal
/// type-tag shape canonicalized through the shared spelling.
/// Returns the alias plus tokens consumed, or `None` on shape miss
/// (generic fails it in lowering like the grammar).
fn parse_type_stmt_at(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
) -> Result<Option<(StepKind, usize)>, V2Error> {
    let name = match toks.get(pos + 1).map(|t| &t.kind) {
        Some(TokKind::Word(w)) if is_upper_ident(w) => w.clone(),
        _ => return Ok(None),
    };
    // Single `=` at any gap (`gap` zero-width); `==` misses.
    if !matches!(
        toks.get(pos + 2).map(|t| &t.kind),
        Some(TokKind::Punct('='))
    ) {
        return Ok(None);
    }
    if matches!(
        toks.get(pos + 3).map(|t| &t.kind),
        Some(TokKind::Punct('='))
    ) {
        return Ok(None);
    }
    let Some(end) = parse_type_shape(toks, pos + 3) else {
        return Ok(None);
    };
    // Post-match the span holds words/puncts only, so the shared
    // renderer cannot fail; stay failed regardless.
    let target = render_type_region(&toks[pos + 3..end], stmt).unwrap_or_default();
    Ok(Some((StepKind::TypeAlias { name, target }, end - pos)))
}

/// Single (non-block) `REMOTE`: target, optional bindings, block.
/// The target reads a maximal joint run in the grammar's superset
/// charset (misses fall to generic); strict validity fails loudly
/// with the exact backend message. Bindings miss to generic;
/// empty/missing bodies and nesting violations fail loudly.
fn parse_remote_single(
    toks: &[Tok],
    pos: usize,
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<Option<(StepKind, usize)>, V2Error> {
    let fail = |msg: String| V2Error {
        stmt,
        msg,
        line: 0,
        kind: Some(ParseErrorKind::Validation {
            command: "REMOTE".to_string(),
        }),
    };
    // Target: joint run in the superset charset (alnum/`_`/`-`/`.`/`:`).
    // Anything else (joint or not) ends the run; an empty run misses
    // to generic. (`sep` needs no explicit check: a joint `(` is a
    // call routed before this ever runs, and anything unparseable
    // misses the same way.)
    let mut target = String::new();
    let mut p = pos + 1;
    while let Some(t) = toks.get(p) {
        if !t.joint && !target.is_empty() {
            break;
        }
        match &t.kind {
            TokKind::Word(w)
                if w.chars().all(|c| {
                    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == ':'
                }) =>
            {
                // Words arrive whole from the lexer; a bad word ends
                // the run (never partially appends), like the grammar.
                target.push_str(w);
                p += 1;
            }
            TokKind::Punct(c)
                if matches!(c, '_' | '-' | '.' | ':') && (target.is_empty() || t.joint) =>
            {
                target.push(*c);
                p += 1;
            }
            _ => break,
        }
    }
    if target.is_empty() {
        return Ok(None);
    }
    if !crate::commands::is_valid_remote_target(&target) {
        return Err(fail(format!(
            "invalid REMOTE target `{target}`: start alphanumeric, then alphanumeric, `_`, or `-`, max 64 chars, never SNAPSHOT, LOCAL, CACHE, SYSTEM, or REMOTE"
        )));
    }
    // Optional bindings `[$var, env:KEY]` at any gap (even joint,
    // probed); malformed bindings miss the whole shape to generic.
    let mut vars = Vec::new();
    let mut env = Vec::new();
    if let Some(bracket) = toks.get(p)
        && matches!(
            &bracket.kind,
            TokKind::Group {
                delim: GroupDelim::Bracket,
                ..
            }
        )
    {
        let inner = match &bracket.kind {
            TokKind::Group { inner, .. } => inner.clone(),
            _ => unreachable!(),
        };
        match parse_remote_bindings(&inner, stmt)? {
            Some((v, e)) => {
                vars = v;
                env = e;
            }
            None => return Ok(None),
        }
        p += 1;
    }
    // Block required (any gap, never a template); missing blocks
    // miss to generic.
    let block_ok = matches!(
        toks.get(p).map(|t| &t.kind),
        Some(TokKind::Group {
            delim: GroupDelim::Brace,
            ..
        })
    ) && toks.get(p).is_some_and(|t| !is_template_brace(t));
    if !block_ok {
        return Ok(None);
    }
    let inner = match &toks[p].kind {
        TokKind::Group { inner, .. } => inner.clone(),
        _ => unreachable!(),
    };
    let body = parse_body_steps(&inner, stmt, ctx, false)?;
    p += 1;
    if body.is_empty() {
        return Err(fail("REMOTE requires a non-empty braced block".to_string()));
    }
    if crate::commands::contains_remote_block(&body) {
        return Err(fail(format!(
            "REMOTE blocks cannot nest (target `{target}` contains another REMOTE block)"
        )));
    }
    if body
        .iter()
        .any(|s| crate::commands::contains_inherit_env(&s.kind))
    {
        return Err(fail(
            "INHERIT_ENV cannot appear inside a REMOTE block: the guest inherits its own process environment".to_string(),
        ));
    }
    // Outer-function containment: body calls resolving to visible
    // outer `SCRIPT` defs (without same-body definitions) fail,
    // mirroring the pest backend verbatim.
    {
        let mut inner_defs = HashSet::new();
        let mut calls = Vec::new();
        crate::commands::collect_remote_body_symbols(&body, &mut inner_defs, &mut calls);
        let outer_defs: HashSet<String> = ctx.funcs.borrow().iter().flatten().cloned().collect();
        for call in &calls {
            let bare = crate::constants::base_name(call);
            let is_script_call = match crate::constants::split_qualified(call) {
                Some((module, _)) => module == crate::constants::SCRIPT_MODULE_NAME,
                None => true,
            };
            if is_script_call && outer_defs.contains(bare) && !inner_defs.contains(bare) {
                return Err(fail(format!(
                    "REMOTE '{target}' calls outer function '{bare}' (functions must be defined inside the block)"
                )));
            }
        }
    }
    Ok(Some((
        StepKind::RemoteBlock {
            target,
            vars,
            env,
            body,
        },
        p - pos,
    )))
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
    // Pest checks `gap` (comments included) before every operator.
    cur.skip_gap();
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
        cur.skip_gap();
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
        cur.skip_gap();
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
    cur.skip_gap();
    let right = parse_expr_ord(cur, ctx)?;
    let expr = make_compare(&lowering_ctx(cur.stmt), op, left, right)
        .map_err(|e| cur.err(e.to_string()))?;
    reject_chained_compare(cur, false)?;
    Ok(expr)
}

/// Reject a comparison operator trailing a complete comparison:
/// no grammar position continues with one, so the shape always fails —
/// loudly here instead of stranding text for a confusing downstream
/// error. The equality tier rejects all six (`$a == $b == $c`,
/// `$a == $b < $c < $d`); the ordering tier rejects ordering only
/// (`$a < $b < $c`), since `$a < $b == $c` is a legitimate equality
/// over an ordering result. Probes without consuming.
fn reject_chained_compare(cur: &mut Cursor, ordering_only: bool) -> Result<(), V2Error> {
    let mut probe = Cursor::expr(cur.toks, cur.stmt);
    probe.pos = cur.pos;
    probe.skip_gap();
    let chained = match probe.toks.get(probe.pos).map(|t| &t.kind) {
        Some(TokKind::Punct('<' | '>')) => true,
        Some(TokKind::Punct('=' | '!')) => !ordering_only,
        _ => false,
    };
    if chained {
        return Err(cur
            .err("chained comparison operators need explicit parentheses, e.g. `($a < $b) < $c`"));
    }
    Ok(())
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
    cur.skip_gap();
    let right = parse_expr_add(cur, ctx)?;
    let expr = make_compare(&lowering_ctx(cur.stmt), op, left, right)
        .map_err(|e| cur.err(e.to_string()))?;
    reject_chained_compare(cur, true)?;
    Ok(expr)
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
        cur.skip_gap();
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
        cur.skip_gap();
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
                // Pest gaps each prefix (`(not_op | neg_op) ~ gap`).
                cur.skip_gap();
                prefixes.push(false);
            }
            Some(TokKind::Punct('-')) if !dash_is_bare(cur) => {
                cur.pos += 1;
                cur.skip_gap();
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
/// shape (which admits `/` anywhere, so `//c`, `/a`, `a/b`, and the
/// `/`-`c`-`/` operands of `/*c*/` math all read literally here).
/// Callers classify (`true`/`false`/string); the numeric path
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
            // Leading `/` (operand position only; operators are
            // consumed by the tiers before operands parse).
            TokKind::Punct('/') => out.push('/'),
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
            sub.skip_gap_nl();
            let expr = parse_expr_or(&mut sub, ctx)?;
            sub.skip_gap_nl();
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
        Some(TokKind::Punct('/')) => parse_bare_atom(cur, ctx, false),
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
    cur.skip_gap_nl();
    if cur.done() {
        return Ok(Expr::List(vec![]));
    }
    let mut items = Vec::new();
    loop {
        let item = parse_expr_checked(&mut cur, ctx)?;
        reject_boundary(&lowering_ctx(stmt), &item).map_err(|e| cur.err(e.to_string()))?;
        items.push(item);
        cur.skip_gap_nl();
        if cur.eat_punct(',') {
            cur.skip_gap_nl();
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
    // Empty (or blank/comment-only) braces are an empty map.
    if inner
        .iter()
        .all(|t| matches!(&t.kind, TokKind::Newline | TokKind::Comment(_)))
    {
        return Ok(Expr::Map(vec![]));
    }
    for part in split_top_commas(inner) {
        let mut sub = Cursor::expr(part, stmt);
        sub.skip_gap_nl();
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
            Some(TokKind::Word(_)) | Some(TokKind::Punct('-')) | Some(TokKind::Punct('/')) => {
                map_bare_key(&mut sub)?
            }
            _ => return Err(sub.err("not a map literal")),
        };
        sub.skip_gap();
        if !sub.eat_punct(':') {
            return Err(sub.err("not a map literal"));
        }
        let val = parse_expr_checked(&mut sub, ctx)?;
        reject_boundary(&lowering_ctx(stmt), &val).map_err(|e| sub.err(e.to_string()))?;
        sub.skip_gap_nl();
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
    // Leading `/` reads literally too (pest `bare_word` admits `/`
    // anywhere: `{/a: 1}`, `{//c: 1}`); the joint loop below
    // continues the run.
    if matches!(
        sub.toks.get(sub.pos).map(|t| &t.kind),
        Some(TokKind::Punct('/'))
    ) {
        out.push('/');
        sub.pos += 1;
    }
    match sub.toks.get(sub.pos).map(|t| &t.kind) {
        Some(TokKind::Word(w)) => {
            out.push_str(w);
            sub.pos += 1;
        }
        // A leading `/` above already started the run; anything
        // else still needs a word head.
        _ if !out.is_empty() => {}
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

/// Map-or-block over a brace group's contents: maps parse literally;
/// anything else is an inline block (`Expr::Block` over the body
/// steps, scopes unmarked like every braced body, mirroring pest's
/// `parse_block_elements_with_lower`).
fn parse_map_or_block(inner: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    match parse_map_literal(inner, stmt, ctx) {
        Ok(map) => Ok(map),
        Err(_) => Ok(Expr::Block(parse_body_steps(inner, stmt, ctx, false)?)),
    }
}

/// Parse one standalone expression from lexed tokens (the
/// `parse_expression_str` entry point): a full tier parse with the
/// top-level boundary rejection and full consumption, mirroring the
/// grammar rule exactly.
pub(crate) fn parse_single_expr(toks: &[Tok], stmt: usize, ctx: &V2Ctx) -> Result<Expr, V2Error> {
    let mut cur = Cursor::expr(toks, stmt);
    let expr = parse_expr_checked(&mut cur, ctx)?;
    if !cur.done() {
        return Err(cur.err("unexpected text in expression"));
    }
    Ok(expr)
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
                    line: 0,
                    kind: None,
                });
            }
        }
    }
    if out.is_empty() {
        return Err(V2Error {
            stmt,
            msg: "LET requires explicit type: LET $var: TYPE = <expr>".to_string(),
            line: 0,
            kind: None,
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

/// `LET` body: `LET` is already matched. Returns `None` (miss to
/// generic, which fails in lowering like the grammar's committed
/// `instruction`) when the shape cannot match any `LET` form: no
/// gapless `$var` (pest needs `dollar_ident`), or no `:` after it
/// (every form needs `: TYPE`). Past that the shape is committed and
/// malformed tails fail loudly.
fn parse_let_body(
    body: &[Tok],
    stmt: usize,
    ctx: &V2Ctx,
) -> Result<Option<(StepKind, usize)>, V2Error> {
    let mut cur = Cursor::expr(body, stmt);
    match cur.next() {
        Some(t) if matches!(&t.kind, TokKind::Word(w) if w == "LET") => {}
        _ => return Err(cur.err("unreachable")),
    }
    // No `$var`: no `LET` form can match (miss, never a hard fail).
    let var = match expect_dollar_var(&mut cur, "LET requires a variable") {
        Ok(var) => var,
        Err(_) => return Ok(None),
    };
    // Pest gaps the header (`gap` admits comments, never newlines).
    cur.skip_gap();
    // No `:`: no `LET` form can match (miss, like `LET $x = 1`).
    if !cur.eat_punct(':') {
        return Ok(None);
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
            return Ok(Some((
                StepKind::Assign {
                    var,
                    decl_type: "PIPE".to_string(),
                    expr: Expr::FreshPipe,
                },
                type_start + 1,
            )));
        }
        return Err(cur.err(
            "LET requires an expression: LET $var: TYPE = <expr> (only LET $p: PIPE omits the initializer)",
        ));
    };
    let decl_type = render_type_region(&body[type_start..eq], stmt)?;
    let (kind, rhs_used) = parse_let_rhs(&body[eq + 1..], var, decl_type, stmt, ctx)?;
    Ok(Some((kind, eq + 1 + rhs_used)))
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
    // Leading gap comments vanish like the `=` gap (but a `//`/`#`
    // to end of input leaves no expression, failing like pest).
    let mut lead = Cursor::expr(rhs, stmt);
    lead.skip_gap();
    let (rhs, skipped) = (&rhs[lead.pos..], lead.pos);
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
        let (kind, used) = found;
        return Ok((kind, skipped + used));
    }
    // `ASYNC`-led lines (with followers; alone they commit to the
    // generic path like the grammar, failing in lowering) spawn
    // background tasks. No `WITH_IO` scan here (probed: the LET
    // path accepts what statement `ASYNC` rejects).
    if head_word.as_deref() == Some("ASYNC") && rhs.len() > 1 {
        let (kind, used) = parse_let_async_rhs(&rhs[1..], var, decl_type, stmt, ctx)?;
        return Ok((kind, skipped + 1 + used));
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
            line: 0,
            kind: None,
        })?;
        crate::commands::reject_pipe_stdout_in_capture(&span, &kind).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
            line: 0,
            kind: None,
        })?;
        return Ok((
            StepKind::AssignCapture {
                var,
                decl_type,
                cmd: Box::new(kind),
            },
            skipped + used,
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
            skipped + 3,
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
        // Non-map blocks miss the exec shape; fall through below
        // (known-command capture, then expression), mirroring the
        // grammar's ordered alternatives.
        if let Some(list) = parse_run_exec_list(&inner, stmt, ctx)? {
            let kind = (ctx.lower)("RUN", vec![Arg::Expr(list)]).map_err(|e| lower_err(stmt, e))?;
            return Ok((
                StepKind::AssignCapture {
                    var,
                    decl_type,
                    cmd: Box::new(kind),
                },
                skipped + 2,
            ));
        }
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
                line: 0,
                kind: None,
            })?;
            crate::commands::reject_pipe_stdout_in_capture(&span, &cmd).map_err(|e| V2Error {
                stmt,
                msg: e.to_string(),
                line: 0,
                kind: None,
            })?;
            // `INHERIT_ENV`-led lines land here (generic): the
            // nesting check rejects them exactly like the finish
            // validator (probed).
            crate::commands::reject_nested_inherit_env(&span, &cmd).map_err(|e| V2Error {
                stmt,
                msg: e.to_string(),
                line: 0,
                kind: None,
            })?;
            return Ok((
                StepKind::AssignCapture {
                    var,
                    decl_type,
                    cmd: Box::new(cmd),
                },
                skipped + used,
            ));
        }
        _ => {}
    }
    let mut cur = Cursor::expr(rhs, stmt);
    let expr = parse_expr_checked(&mut cur, ctx)?;
    let used = skipped + cur.pos;
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
    let fail = |msg: String| V2Error {
        stmt,
        msg,
        line: 0,
        kind: None,
    };
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
            let body = parse_body_steps(&brace_inner, stmt, ctx, false)?;
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
            line: 0,
            kind: None,
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
        line: 0,
        kind: None,
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
            line: 0,
            kind: None,
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
        let body = parse_body_steps(&inner, stmt, ctx, false)?;
        let kind = StepKind::AssignAsync {
            var,
            decl_type,
            body,
        };
        let span = lowering_ctx(stmt);
        crate::commands::reject_nested_inherit_env(&span, &kind).map_err(|e| V2Error {
            stmt,
            msg: e.to_string(),
            line: 0,
            kind: None,
        })?;
        return Ok((kind, 1));
    }
    if first.joint {
        return Err(V2Error {
            stmt,
            msg: "LET $var: HANDLE = ASYNC requires a body".to_string(),
            line: 0,
            kind: None,
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
        line: 0,
        kind: None,
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
    // Leading gap comments vanish like the `=` gap.
    cur.skip_gap();
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
fn lower_stmt_steps(
    stmt: &Stmt,
    idx: usize,
    ctx: &V2Ctx,
    in_block: bool,
    mark: bool,
) -> Result<Vec<Step>, V2Error> {
    let guard = parse_guard_groups(&stmt.guard, idx)?;
    // Statement-head braces route here (mid-run braces fail inside
    // the element loop): templates and guardless/bare blocks fail
    // loudly; guarded braces open guard blocks (top level) or the
    // nested quirk (in bodies).
    if starts_brace(&stmt.body) {
        return parse_brace_head(stmt, guard, idx, ctx, in_block, mark);
    }
    parse_elements(&stmt.body, idx, ctx, guard, in_block)
}

/// Statement-head `{...}` dispatcher. Template braces fail (the
/// grammar strands them); guardless top-level braces fail; detached
/// or guardless in-body braces fail (no `block_start` in
/// `block_element`); guarded braces open a guard scope at top level
/// or mirror the nested quirk formula in bodies. Trailing elements
/// loop on guardless outside (like the grammar's continued line).
fn parse_brace_head(
    stmt: &Stmt,
    guard: Option<GuardExpr>,
    idx: usize,
    ctx: &V2Ctx,
    in_block: bool,
    mark: bool,
) -> Result<Vec<Step>, V2Error> {
    let brace = &stmt.body[0];
    if is_template_brace(brace) {
        return Err(V2Error {
            stmt: idx,
            msg: "'{' without a pending guard".to_string(),
            line: 0,
            kind: None,
        });
    }
    let inner = match &brace.kind {
        TokKind::Group { inner, .. } => inner.clone(),
        _ => unreachable!(),
    };
    if in_block {
        // Attached guarded braces only: pending holds all groups but
        // the last (the block's own), mirroring the pair structure.
        if guard.is_none() || stmt.brace_detached || stmt.guard.is_empty() {
            return Err(V2Error {
                stmt: idx,
                msg: "unexpected '{' in block body".to_string(),
                line: 0,
                kind: None,
            });
        }
        let n = stmt.guard.len();
        let mut pending = None;
        if n > 1 {
            pending = parse_guard_groups(&stmt.guard[..n - 1], idx)?;
        }
        let block_guard = parse_guard_groups(&stmt.guard[n - 1..n], idx)?.ok_or(V2Error {
            stmt: idx,
            msg: "unexpected '{' in block body".to_string(),
            line: 0,
            kind: None,
        })?;
        let mut steps = parse_body_steps(&inner, idx, ctx, mark)?;
        // Guard-block bodies propagate the ambient marking context
        // (top-level nesting marks like pest's stateful backend;
        // wrapper bodies never mark, like `parse_block_elements`).
        // Quirk formula verbatim (including the dropped block guard
        // with inner guards, probed).
        for s in &mut steps {
            let inner_guard = s.guard.take();
            s.guard = Some(match (pending.clone(), inner_guard) {
                (Some(p), Some(e)) => GuardExpr::all(vec![p, e]),
                (Some(p), None) => GuardExpr::all(vec![p, block_guard.clone()]),
                (None, Some(e)) => e,
                (None, None) => block_guard.clone(),
            });
        }
        // Marking contexts bump each nesting level (probed top-level
        // double-block 2/2); wrapper bodies stay 0/0 like pest.
        // Trailing elements ride outside, unmarked.
        if mark {
            if let Some(first) = steps.first_mut() {
                first.scope_enter += 1;
            }
            if let Some(last) = steps.last_mut() {
                last.scope_exit += 1;
            }
        }
        // Trailing elements loop on guardless outside.
        if stmt.body.len() > 1 {
            let mut rest = parse_elements(&stmt.body[1..], idx, ctx, None, in_block)?;
            steps.append(&mut rest);
        }
        return Ok(steps);
    }
    // Top level: guardless braces fail; guarded braces open a guard
    // scope (each step folds outer-then-inner with markers).
    let Some(outer) = guard else {
        return Err(V2Error {
            stmt: idx,
            msg: "'{' without a pending guard".to_string(),
            line: 0,
            kind: None,
        });
    };
    let mut steps = parse_body_steps(&inner, idx, ctx, mark)?;
    if steps.is_empty() {
        return Err(V2Error {
            stmt: idx,
            msg: "guard block must contain at least one command".to_string(),
            line: 0,
            kind: None,
        });
    }
    for s in &mut steps {
        let inner_guard = s.guard.take();
        s.guard = Some(match inner_guard {
            Some(e) => GuardExpr::all(vec![outer.clone(), e]),
            None => outer.clone(),
        });
    }
    if let Some(first) = steps.first_mut() {
        first.scope_enter += 1;
    }
    if let Some(last) = steps.last_mut() {
        last.scope_exit += 1;
    }
    if stmt.body.len() > 1 {
        let mut rest = parse_elements(&stmt.body[1..], idx, ctx, None, in_block)?;
        steps.append(&mut rest);
    }
    Ok(steps)
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
    in_block: bool,
) -> Result<Vec<Step>, V2Error> {
    let mut steps = Vec::new();
    let mut pos = 0;
    // Statement guard: consumed by the first element that yields a
    // step. `IMPORT` yields none, so the guard leaks to the next
    // element exactly like the pest backend (probed).
    let mut pending_guard: Option<GuardExpr> = guard;
    while pos < toks.len() {
        // Trailing gap comments between elements (`} #c`, `F("a")
        // /*c*/ ECHO`, `LET $x = 1 //c` — the pair ended, so the
        // comment is silent like the grammar, never payload):
        // `#` and lexer `Comment` markers run to the run end, `//`
        // likewise (line comments eat `;` too, and runs hold no
        // newlines), `/* */` balances. Mid-run `#`/`/*` stay payload
        // text inside the generic path, which consumes everything
        // and never loops back here.
        if matches!(&toks[pos].kind, TokKind::Punct('#'))
            || matches!(&toks[pos].kind, TokKind::Comment(_))
        {
            break;
        }
        if matches!(&toks[pos].kind, TokKind::Punct('/')) && is_comment_open(toks, pos) {
            if matches!(
                toks.get(pos + 1).map(|t| &t.kind),
                Some(TokKind::Punct('/'))
            ) {
                break;
            }
            match skip_inline_comment(toks, pos) {
                Some(past) => {
                    pos = past;
                    continue;
                }
                None => {
                    return Err(V2Error {
                        stmt,
                        msg: "unclosed block comment".to_string(),
                        line: 0,
                        kind: None,
                    });
                }
            }
        }
        // `IMPORT` directives update the import frames and emit no
        // step; the guard leaks to the next element (probed) unless
        // this is top level, where a guarded `IMPORT` fails. Shape
        // misses fall through to generic (lowering fails them).
        if matches!(
            toks.get(pos).map(|t| &t.kind),
            Some(TokKind::Word(w)) if w == "IMPORT"
        ) && let Some(used) = parse_import_at(toks, pos, stmt, ctx)?
        {
            if !in_block && pending_guard.is_some() {
                return Err(V2Error {
                    stmt,
                    msg: "IMPORT cannot be guarded".to_string(),
                    line: 0,
                    kind: None,
                });
            }
            pos += used;
            continue;
        }
        let guard = pending_guard.take();
        let (mut got, used) = parse_element_at(toks, pos, stmt, ctx, guard, in_block)?;
        if used == 0 {
            return Err(V2Error {
                stmt,
                msg: "element consumed no input".to_string(),
                line: 0,
                kind: None,
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
    in_block: bool,
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
    // Mid-run `{...}` (after a call, wrapper block, ...): the
    // grammar strands it (`block_start` needs a pending guard or
    // `WITH_IO` wait, both consumed), so fail loudly. Statement-head
    // braces route through `lower_stmt_steps`, never here.
    if starts_brace(&toks[pos..]) {
        return Err(V2Error {
            stmt,
            msg: "'{' without a pending guard".to_string(),
            line: 0,
            kind: None,
        });
    }
    // LET/SET/RETURN heads parse with groups intact, then trailing
    // elements loop on (probed `LET $x: INT = 1 ECHO bye`). LET shape
    // misses fall to generic (lowering fails them like a committed
    // `instruction` would, e.g. `LET foo`).
    if head_word.as_deref() == Some("LET")
        && let Some((kind, used)) = parse_let_body(&toks[pos..], stmt, ctx)?
    {
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
    // Block statements (shape misses fall to generic, which fails
    // them in lowering like the grammar's committed `instruction`).
    // `BREAK`/`CONTINUE` take no args (structural, like the grammar):
    // each consumes one token and trailing elements loop on (probed
    // `BREAK ECHO hi`); a lone trailing word fails downstream like
    // the grammar's committed mismatch.
    match head_word.as_deref() {
        Some("BREAK") => {
            return Ok((vec![step(StepKind::Break)], 1));
        }
        Some("CONTINUE") => {
            return Ok((vec![step(StepKind::Continue)], 1));
        }
        Some("IF") => {
            if let Some((kind, used)) = parse_if_at(toks, pos, stmt, ctx)? {
                return Ok((vec![step(kind)], used));
            }
        }
        Some("FOR") => {
            if let Some((kind, used)) = parse_for_at(toks, pos, stmt, ctx)? {
                return Ok((vec![step(kind)], used));
            }
        }
        Some("WHILE") => {
            if let Some((kind, used)) = parse_while_single(toks, pos, stmt, ctx)? {
                return Ok((vec![step(kind)], used));
            }
        }
        Some("FUNC") => {
            if let Some((kind, used)) = parse_func_at(toks, pos, stmt, ctx)? {
                return Ok((vec![step(kind)], used));
            }
        }
        Some("REMOTE") => {
            if let Some((kind, used)) = parse_remote_single(toks, pos, stmt, ctx)? {
                return Ok((vec![step(kind)], used));
            }
        }
        _ => {}
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
        && let Some((kind, used)) = parse_inherit_env_at(toks, pos, stmt)?
    {
        return Ok((vec![step(kind)], used));
    }
    // `EXPORT ...` records names (zero steps) or misses to generic;
    // level and guard violations fail loudly like the backend.
    if head_word.as_deref() == Some("EXPORT") {
        if in_block {
            return Err(V2Error {
                stmt,
                msg: "EXPORT is only allowed at the top level".to_string(),
                line: 0,
                kind: None,
            });
        }
        if first_guard.is_some() {
            return Err(V2Error {
                stmt,
                msg: "EXPORT cannot be guarded".to_string(),
                line: 0,
                kind: None,
            });
        }
        if let Some((names, used)) = parse_export_at(toks, pos) {
            for name in &names {
                for part in name.split("::") {
                    crate::resolve::check_func_ident(&lowering_ctx(stmt), part)
                        .map_err(|e| lower_err(stmt, e))?;
                }
                if ctx.exports.borrow().contains(name) {
                    return Err(V2Error {
                        stmt,
                        msg: format!("duplicate EXPORT `{name}`"),
                        line: 0,
                        kind: Some(ParseErrorKind::Validation {
                            command: "EXPORT".to_string(),
                        }),
                    });
                }
                ctx.exports.borrow_mut().push(name.clone());
            }
            return Ok((Vec::new(), used));
        }
    }
    // `TYPE NAME = SHAPE` aliases (zero or one step); level and guard
    // violations fail loudly, shape misses fall to generic.
    if head_word.as_deref() == Some("TYPE") {
        if in_block {
            return Err(V2Error {
                stmt,
                msg: "TYPE must appear at top level".to_string(),
                line: 0,
                kind: None,
            });
        }
        if first_guard.is_some() {
            return Err(V2Error {
                stmt,
                msg: "TYPE cannot be guarded".to_string(),
                line: 0,
                kind: None,
            });
        }
        if let Some((kind, used)) = parse_type_stmt_at(toks, pos, stmt)? {
            return Ok((vec![step(kind)], used));
        }
    }
    // Lowercase heads name their uppercase form (the grammar's
    // `command_name` mismatch): never a bare unknown-command.
    if let Some(head) = head_word.as_deref()
        && head.chars().all(|c| c.is_ascii_lowercase())
        && let upper = head.to_ascii_uppercase()
        && (is_known_command(&upper) || crate::STRUCTURAL_KEYWORDS.contains(&upper.as_str()))
    {
        return Err(V2Error {
            stmt,
            msg: format!("command must be uppercase: found `{head}`, expected `{upper}`"),
            line: 0,
            kind: Some(ParseErrorKind::PestParse),
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
        let mut steps = parse_body_steps(&brace_inner, stmt, ctx, false)?;
        if steps.is_empty() {
            return Err(V2Error {
                stmt,
                msg: "WITH_IO block must contain at least one command".to_string(),
                line: 0,
                kind: None,
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
                line: 0,
                kind: None,
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
            line: 0,
            kind: None,
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
        line: 0,
        kind: None,
    })?;
    let kind = StepKind::WithIo {
        bindings,
        cmd: Box::new(cmd),
    };
    crate::commands::reject_nested_inherit_env(&span, &kind).map_err(|e| V2Error {
        stmt,
        msg: e.to_string(),
        line: 0,
        kind: None,
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
            crate::resolve::check_func_ident(&lowering_ctx(stmt), &call_name)
                .map_err(|e| lower_err(stmt, e))?;
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
                line: 0,
                kind: None,
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
                line: 0,
                kind: None,
            });
        }
        let inner = match &body[1].kind {
            TokKind::Group { inner, .. } => inner.clone(),
            _ => unreachable!(),
        };
        // Non-map blocks miss the exec shape (the grammar has no
        // `block` alternative here); fall through to the shell path
        // below, which reads the bracket literally.
        if let Some(list) = parse_run_exec_list(&inner, stmt, ctx)? {
            let kind = (ctx.lower)("RUN", vec![Arg::Expr(list)]).map_err(|e| lower_err(stmt, e))?;
            return Ok((kind, 2));
        }
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
    let reserved = HashSet::new();
    let ctx = V2Ctx::empty(&lower, &modules, &reserved);
    parse_v2_with_ctx(toks, &ctx)
}

/// Parse with an explicit context (module table for call resolution).
/// `parse_v2` uses empty modules, matching pest with empty tables.
pub fn parse_v2_with_ctx(toks: &[Tok], ctx: &V2Ctx) -> Result<Vec<Step>, V2Error> {
    let mut out = Vec::new();
    *ctx.current_line.borrow_mut() = 1;
    for (i, s) in split_statements(toks, 1)?.iter().enumerate() {
        *ctx.current_line.borrow_mut() = s.line;
        out.extend(lower_stmt_steps(s, i, ctx, false, true).map_err(|e| stamp_line(e, s.line))?);
    }
    // Whole-script validations mirroring the pest finish pass (which
    // delegates to the same shared validators where promoted).
    // `INHERIT_ENV` prelude order: only leading positions read.
    {
        let mut seen_non_prelude = false;
        for step in &out {
            match &step.kind {
                StepKind::InheritEnv { .. } => {
                    if seen_non_prelude {
                        return Err(V2Error {
                            stmt: out.len(),
                            msg: "INHERIT_ENV must appear before any other commands".to_string(),
                            line: 0,
                            kind: None,
                        });
                    }
                }
                kind => {
                    if crate::commands::contains_inherit_env(kind) {
                        return Err(V2Error {
                            stmt: out.len(),
                            msg: "INHERIT_ENV cannot be nested inside other commands".to_string(),
                            line: 0,
                            kind: None,
                        });
                    }
                    seen_non_prelude = true;
                }
            }
        }
    }
    crate::commands::validate_remote_copy_placement(&out, &lowering_ctx(out.len())).map_err(
        |e| V2Error {
            stmt: out.len(),
            msg: e.to_string(),
            line: 0,
            kind: None,
        },
    )?;
    {
        let top: HashSet<String> = ctx.funcs.borrow().first().cloned().unwrap_or_default();
        crate::commands::validate_exports(
            &lowering_ctx(out.len()),
            &ctx.exports.borrow(),
            &top,
            ctx.modules,
        )
        .map_err(|e| V2Error {
            stmt: out.len(),
            msg: e.to_string(),
            line: 0,
            kind: None,
        })?;
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

    /// Lexer-agreement oracle: both frontends must deliver identical
    /// steps through the single backend. `parse_script` is the string
    /// entry point; `parse_v2` runs each lexer's output directly.
    fn assert_parity(text: &str) {
        let old = crate::parse_script(text, lower)
            .unwrap_or_else(|e| panic!("backend must parse parity script {text:?}: {e}"));
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
    /// (lifetime-like quotes). The backend must still agree there.
    fn assert_parity_string_only(text: &str) {
        let old = crate::parse_script(text, lower)
            .unwrap_or_else(|e| panic!("backend must parse parity script {text:?}: {e}"));
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
    fn v2_matches_pest_on_blocks() {
        // Slice 5: `IF`/`FOR`/`WHILE`/`FUNC` with inline and
        // cross-newline `ELSE` chains, guard blocks (top-level and
        // nested quirk), and multi-element trailing — three-way
        // agreement per script.
        for text in [
            "IF true { ECHO hi }",
            "IF $x { ECHO hi } ELSE { ECHO bye }",
            "IF true { ECHO a } ELSE IF false { ECHO b } ELSE { ECHO c }",
            "IF true{ ECHO hi }",
            "IF true { ECHO hi } ECHO bye",
            "IF true { ECHO hi }\nELSE { ECHO bye }",
            "IF true { ECHO hi }\nELSE IF false { ECHO b }\nELSE { ECHO c }",
            "IF true { ECHO hi } //c\nELSE { ECHO bye }",
            "IF true { ECHO hi } #c\nELSE { ECHO bye }",
            "IF true { ECHO hi }//c\nELSE { ECHO bye }",
            "IF true { ECHO hi } /*c*/ ELSE { ECHO bye }",
            "IF true { ECHO hi } /*c1*/ /*c2*/\nELSE { ECHO bye }",
            "IF true { /*c*/ ECHO hi }",
            "IF true { [os:linux] ECHO hi }",
            "TIMEOUT 10s { IF true { ECHO hi } }",
            "TIMEOUT 10s { IF true { ECHO hi }\nELSE { ECHO bye } }",
            "FOR $x: INT IN [1] { ECHO hi }",
            "FOR $k: STRING, $v: INT IN $m { ECHO hi }",
            "FOR $k: INT IN [1] { ECHO hi }",
            "FOR $x: INT IN [1]{ ECHO hi }",
            "FOR $x: MAP<name: STRING> IN [1] { ECHO hi }",
            "FOR $x: INT IN [1] { ECHO hi } ECHO bye",
            "WHILE true { ECHO hi }",
            "WHILE $x { ECHO hi }",
            "WHILE true{ ECHO hi }",
            "TIMEOUT 10s WHILE true { ECHO hi }",
            "ASYNC WHILE true { ECHO hi }",
            "FUNC FOO() { RETURN 1 }",
            "FUNC FOO(){ RETURN 1 }",
            "FUNC FOO($x: INT) { RETURN $x }",
            "FUNC FOO($x: INT, $y: STRING) { RETURN $x }",
            "FUNC FOO() { FUNC FOO() { RETURN 1 } }",
            "FUNC FOO() { RETURN 1 }\nFUNC BAR() { FOO() }\nBAR()",
            "TYPE PERSON = MAP<name: STRING>",
            "TYPE A=B",
            "TYPE A = B | C",
            "TYPE A = B\nTYPE A = C",
            "INHERIT_ENV [A]\nECHO hi",
            "[os:linux] INHERIT_ENV [M]",
            "INHERIT_ENV [A]\nINHERIT_ENV [B]\nECHO hi",
            "ISOLATED INHERIT_ENV [M]",
            "ISOLATED { INHERIT_ENV [M] }",
            "ISOLATED INHERIT_ENV [A, B]",
            "ISOLATED INHERIT_ENV [A-B]",
            "REMOTE prod { ECHO hi }",
            "REMOTE prod [$v] { ECHO hi }",
            "REMOTE prod [$v, env:E] { ECHO hi }",
            "REMOTE prod [] { ECHO hi }",
            "REMOTE prod[$v] { ECHO hi }",
            "REMOTE prod [$v]{ ECHO hi }",
            "REMOTE prod{ ECHO hi }",
            "REMOTE  prod  { ECHO hi }",
            "REMOTE prod [$v, $v] { ECHO hi }",
            "REMOTE prod { ECHO hi } ECHO bye",
            "TIMEOUT 10s REMOTE prod { ECHO hi }",
            "ASYNC REMOTE prod [$v] { ECHO hi }",
            "[os:linux] { ECHO hi }",
            "[os:linux]\n{ ECHO hi }",
            "[os:linux] { [os:macos] ECHO hi }",
            "[os:linux] { [os:macos] ECHO hi\nECHO bye }",
            "[os:linux] { [os:macos] { ECHO hi } }",
            "TIMEOUT 10s { [os:linux] { [os:macos] ECHO hi } }",
            "TIMEOUT 10s { [os:linux] [os:windows] { ECHO hi } }",
            "TIMEOUT 10s { [os:linux] { ECHO hi } ECHO bye }",
            "LET $x: INT = 1 ECHO bye",
            "BREAK",
            "CONTINUE",
            "BREAK ECHO hi",
            "FOR $x: INT IN [1] { BREAK }",
            "LET $b: STRING = { RETURN \"hi\" }",
            "ECHO { RETURN \"hi\" }",
            "RUN [{ RETURN \"hi\" }]",
            "[not(env:FOO)] WRITE x",
            "[\nnot(env:FOO)\n] WRITE x",
            "WRITE env.txt \"{{ env:GREETING }}\"",
            "CANCEL $t LET $x: INT = 1",
            "RUN [\"a\"] ECHO bye",
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
                    .unwrap_or_else(|e| panic!("backend must parse parity script {text:?}: {e}"));
            let reserved = HashSet::new();
            let ctx = V2Ctx::empty(&lower_fn, &table, &reserved);
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
        // Malformed shapes fail on both lexers (messages may differ;
        // success/failure must agree). Unbalanced groups fail even
        // earlier, at lex time: both layers reject, which is what the
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
            "LET $x: BOOL = $a < $b < $c",
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
            "[!env:A==1] RUN echo no",
            "run echo hi",
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
                        "backend must also reject lex-broken input: {text:?}"
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
                "backend must also reject (oracle agreement): {text:?}"
            );
        }
        // No deferred shapes remain: every statement-level block and
        // inline-block expression parses natively (the cutover gate).
    }
}
