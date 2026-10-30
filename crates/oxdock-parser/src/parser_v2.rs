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
//! stacked brackets combining with `All`), and bracket/paren groups in
//! arg position render back to text. In-body brackets are args (probed:
//! `ECHO [env:X]` keeps it as an argument); a leading bracket that is
//! not a guard (`[a] ECHO hi`) errors exactly like pest.
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

use crate::ast::{Arg, Guard, GuardExpr, Ns, Step, StepKind};
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

/// Render one arg run (gap-delimited token run).
/// TODO(cutover): mirrors `parse_fragments` + the `Arg::String` quoting
/// rule (lone quoted string keeps `quoted=true`, multi-fragment joins
/// gap-or-joint as unquoted, verbatim for runtime `expand_string`).
fn assemble_arg(run: &[Tok], stmt: usize) -> Result<Arg, V2Error> {
    let unsupported = |what: &str| V2Error {
        stmt,
        msg: format!("unsupported in parse_v2 slice 1: {what}"),
    };
    if run.len() == 1 {
        match &run[0].kind {
            TokKind::Str { text: s, .. } => return Ok(Arg::String(s.clone(), true)),
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
            TokKind::Str { text: s, .. } => out.push_str(s),
            TokKind::BlockStr(full) => {
                out.push_str(&crate::command::lower_block_string(full));
            }
            // Bracket/paren groups splice to raw text before assembly;
            // only brace groups (blocks) can still reach this point.
            TokKind::Group { .. } => return Err(unsupported("brace group inside args")),
            TokKind::Newline | TokKind::Semicolon => {
                return Err(unsupported("line break inside args"));
            }
        }
    }
    Ok(Arg::String(out, false))
}

/// Render a bracket/paren group back to source text for arg position
/// (in-body groups are args, never guards). Fragments join with a
/// space on gaps and glue on `joint`, so both lexers render the same
/// source identically. Known normalizations (documented, not silent):
/// single-quoted strings re-quote as double-quoted, and multi-space
/// gaps collapse to one.
fn render_group(open: char, close: char, inner: &[Tok], stmt: usize) -> Result<String, V2Error> {
    let unsupported = |what: &str| V2Error {
        stmt,
        msg: format!("unsupported in parse_v2 slice 2: {what}"),
    };
    let mut out = String::from(open);
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
                        ..
                    } => out.push_str(&render_group('[', ']', inner, stmt)?),
                    TokKind::Group {
                        delim: GroupDelim::Paren,
                        inner,
                        ..
                    } => out.push_str(&render_group('(', ')', inner, stmt)?),
                    TokKind::Group {
                        delim: GroupDelim::Brace,
                        ..
                    } => return Err(unsupported("brace group inside args")),
                    TokKind::Newline | TokKind::Semicolon => unreachable!(),
                }
            }
        }
        first = false;
    }
    out.push(close);
    Ok(out)
}

/// Positional cursor over guard/group token slices. Newlines skip
/// freely between tokens (`ws` admits linebreaks in the grammar);
/// bare key/value runs never cross them (the grammar excludes
/// linebreaks there), so runs read raw.
struct Cursor<'t> {
    toks: &'t [Tok],
    pos: usize,
    stmt: usize,
}

impl<'t> Cursor<'t> {
    fn new(toks: &'t [Tok], stmt: usize) -> Self {
        Self { toks, pos: 0, stmt }
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
        self.skip_newlines();
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<&'t Tok> {
        self.skip_newlines();
        let t = self.toks.get(self.pos)?;
        self.pos += 1;
        Some(t)
    }

    fn done(&mut self) -> bool {
        self.skip_newlines();
        self.pos >= self.toks.len()
    }

    fn eat_punct(&mut self, c: char) -> bool {
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
                ..
            } => {
                if !out.is_empty() && !tok.joint {
                    break;
                }
                out.push_str(&render_group('[', ']', inner, arg.stmt)?);
            }
            TokKind::Group {
                delim: GroupDelim::Paren,
                inner,
                ..
            } => {
                if !out.is_empty() && !tok.joint {
                    break;
                }
                out.push_str(&render_group('(', ')', inner, arg.stmt)?);
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

/// Lower one statement through the shared command dispatcher.
fn lower_stmt(
    stmt: &Stmt,
    idx: usize,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> Result<Step, V2Error> {
    let guard = parse_guard_groups(&stmt.guard, idx)?;
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
    // wherever a gap precedes a token. Bracket/paren groups splice to
    // raw text first (pest reads them as ordinary characters).
    let flat = splice_arg_groups(&stmt.body[1..], idx)?;
    let mut args = Vec::new();
    let mut run: Vec<Tok> = Vec::new();
    for tok in &flat {
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
        guard,
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

    /// Differential oracle: v2 must equal pest on slice-2 scripts,
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

    /// String-path-only oracle for shapes `rustc` cannot carry at all
    /// (lifetime-like quotes). v2 must still equal pest there.
    fn assert_parity_string_only(text: &str) {
        let old = crate::parse_script(text, lower).expect("pest must parse parity script");
        let toks = crate::tok::lex_str(text).expect("string lex must succeed");
        let new = parse_v2(&toks, lower).expect("v2 must parse parity script");
        assert_eq!(new, old, "v2/string-lex mismatch for {text:?}");
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
    fn v2_rejects_slices_not_yet_built() {
        // Structural shapes v2 does not build yet (pest accepts; v2
        // must fail explicitly, never drift).
        for text in [
            "LET $x: INT = 1",
            "WITH_IO [stdout=$p] ECHO hi",
            "IF true { ECHO hi }",
        ] {
            let toks = crate::tok::lex_str(text).expect("lex must succeed");
            assert!(
                parse_v2(&toks, lower).is_err(),
                "v2 must explicitly reject (not silently drift): {text:?}"
            );
        }
        // Malformed shapes fail on both pathways (messages may differ
        // until cutover; success/failure must agree).
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
        ] {
            let toks = crate::tok::lex_str(text).expect("lex must succeed");
            assert!(
                parse_v2(&toks, lower).is_err(),
                "v2 must explicitly reject (not silently drift): {text:?}"
            );
            assert!(
                crate::parse_script(text, lower).is_err(),
                "pest must also reject (oracle agreement): {text:?}"
            );
        }
    }
}
