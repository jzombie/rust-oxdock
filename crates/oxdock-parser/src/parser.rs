//! Script entry points over the token backend.
//!
//! The pest grammar (`dsl.pest`) and its pair-walking backend are retired:
//! every entry point here lexes to [`Tok`](crate::tok::Tok) and lowers
//! through [`parser_v2`](crate::parser_v2), the single parser for both
//! string scripts and braced-macro token streams. Public signatures are
//! unchanged, and lowering failures keep their machine-readable
//! `ParseErrorKind` by construction
//! (the backend preserves the dispatcher's classification instead of
//! stringifying it). Backend-native syntax failures report the
//! statement's source line with its text; column spans stay where the
//! lexer knows them exactly (unbalanced groups, literal scans).

use crate::ast::{Arg, Expr, GuardExpr, ModuleTable, Step, StepKind};
use crate::error::{ParseError, ParseErrorKind, ParseResult, SpanContext};
use crate::parser_v2::{V2Ctx, V2Error, parse_v2_with_ctx};
use crate::tok::{LexError, lex_str};
use std::collections::HashSet;

/// Source text of a 1-based line. The line just past a trailing
/// newline (where unbalanced groups report) reads as empty text so
/// caret blocks still render, exactly like the old backend.
fn source_line_of(input: &str, line: usize) -> Option<String> {
    let count = input.lines().count();
    let idx = line.checked_sub(1)?;
    if idx < count {
        input.lines().nth(idx).map(str::to_string)
    } else if idx == count {
        Some(String::new())
    } else {
        None
    }
}

/// Last script line (end-of-script validations blame it, like the old
/// finish pass's EOF span).
fn last_line(input: &str) -> usize {
    input.lines().count().max(1)
}

/// Convert a lexer failure: low-level syntax kind, exact lexer
/// position, and the source line for the caret block. Unclosed groups
/// name their opener (no grammar dumps); the closer expectation rides
/// along so `expected()` stays informative. A lexer-known kind (an
/// unclosed `WITH_IO` bracket) converts verbatim.
fn lex_error(input: &str, e: LexError) -> ParseError {
    let line = e.line.max(1);
    let ctx = match source_line_of(input, line) {
        Some(source_line) => SpanContext::full(line, e.col.max(1), e.col.max(1), source_line),
        None => SpanContext::line_only(line),
    };
    if let Some(ParseErrorKind::InvalidSyntax { command }) = e.kind {
        // Lexer-known command syntax (an unclosed `WITH_IO` bracket).
        return ParseError::invalid_syntax(
            &command,
            e.msg,
            None,
            vec!["`]`".to_string()],
            None,
            &ctx,
        );
    }
    let expected = if e.msg.starts_with("unclosed") {
        vec!["`}`".to_string()]
    } else {
        Vec::new()
    };
    ParseError::pest(e.msg, expected, None, &ctx)
}

/// Convert a backend failure, preserving the dispatcher's error kind
/// and completing line/source context the token stream knows. Spans
/// cover the statement's whole first line (multi-line spans clamp to
/// the start line, like the old backend), so caret blocks render for
/// every string-path error.
fn v2_error(input: &str, e: V2Error) -> ParseError {
    let line = if e.line == 0 {
        last_line(input)
    } else {
        e.line
    };
    let ctx = match source_line_of(input, line) {
        Some(source_line) => {
            let end = source_line.len().max(1);
            SpanContext::full(line, 1, end, source_line)
        }
        None => SpanContext::line_only(line),
    };
    // Column spans are statement granularity (every error names its
    // statement's first column); the caret block still renders.
    match e.kind {
        Some(crate::error::ParseErrorKind::UnknownCommand { name }) => {
            ParseError::unknown_command(&name, e.msg, None, &ctx)
        }
        Some(crate::error::ParseErrorKind::InvalidSyntax { command }) => {
            ParseError::invalid_syntax(&command, e.msg, None, Vec::new(), None, &ctx)
        }
        Some(crate::error::ParseErrorKind::Validation { command }) => {
            ParseError::validation(&command, e.msg, &ctx)
        }
        Some(crate::error::ParseErrorKind::PestParse) => {
            ParseError::pest(e.msg, Vec::new(), None, &ctx)
        }
        Some(crate::error::ParseErrorKind::Structural { rule }) => {
            ParseError::structural(&rule, e.msg, &ctx)
        }
        None => ParseError::structural("statement", e.msg, &ctx),
    }
}

/// Shared parse driver: lex, lower with preseeded frames, and collect
/// `EXPORT` names (validated at end of parse, forward references
/// allowed, exactly like the old finish pass).
fn parse_with_ctx(
    input: &str,
    lower: impl Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
    reserved_names: HashSet<String>,
    modules: ModuleTable,
    preseed_funcs: HashSet<String>,
    preseed_imports: Vec<String>,
) -> ParseResult<(Vec<Step>, Vec<String>)> {
    let toks = lex_str(input).map_err(|e| lex_error(input, e))?;
    let ctx = V2Ctx::empty(&lower, &modules, &reserved_names);
    // Snippet re-parses seed their base frames so lookup behaves
    // identically; snippets never define, only read.
    ctx.funcs.borrow_mut()[0].extend(preseed_funcs);
    ctx.imports.borrow_mut()[0].extend(preseed_imports);
    let steps = parse_v2_with_ctx(&toks, &ctx).map_err(|e| v2_error(input, e))?;
    let mut steps = steps;
    stamp_func_docs(input, &mut steps);
    Ok((steps, ctx.exports.borrow().clone()))
}

/// Parse a script with an unknown host environment.
pub fn parse_script(
    input: &str,
    lower: impl Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<Vec<Step>> {
    Ok(parse_with_ctx(
        input,
        lower,
        HashSet::new(),
        ModuleTable::default(),
        HashSet::new(),
        Vec::new(),
    )?
    .0)
}

/// Parse a snippet with an enclosing parse's visible scope state, so
/// re-parsed inner commands (async bodies) resolve calls identically.
/// The snippet never defines, only reads: seeds affect lookup alone.
pub fn parse_script_with_preseed(
    input: &str,
    lower: impl Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
    reserved_names: HashSet<String>,
    modules: ModuleTable,
    preseed_funcs: HashSet<String>,
    preseed_imports: Vec<String>,
) -> ParseResult<Vec<Step>> {
    Ok(parse_with_ctx(
        input,
        lower,
        reserved_names,
        modules,
        preseed_funcs,
        preseed_imports,
    )?
    .0)
}

/// Parse with a module provenance table so calls resolve statically:
/// qualified `MODULE::NAME` checks membership, bare `NAME` resolves through
/// `SCRIPT` definitions and `IMPORT`ed modules, and anything else fails at
/// parse time instead of at runtime.
pub fn parse_script_with_modules(
    input: &str,
    lower: impl Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
    reserved_names: HashSet<String>,
    modules: ModuleTable,
) -> ParseResult<Vec<Step>> {
    Ok(parse_script_with_modules_and_exports(input, lower, reserved_names, modules)?.steps)
}

/// Parse with modules plus single-file `EXPORT` metadata and `#` doc
/// comments for top-level `FUNC`s. Used by the MCP host; the linear
/// runner keeps using `parse_script_with_modules` and ignores exports.
pub fn parse_script_with_modules_and_exports(
    input: &str,
    lower: impl Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
    reserved_names: HashSet<String>,
    modules: ModuleTable,
) -> ParseResult<ParsedScript> {
    let (steps, exports) = parse_with_ctx(
        input,
        lower,
        reserved_names,
        modules,
        HashSet::new(),
        Vec::new(),
    )?;
    Ok(ParsedScript {
        func_docs: collect_func_docs(input, &steps),
        steps,
        exports,
    })
}

/// Parse without modules plus `EXPORT` metadata. Used by tests and
/// tooling that lower with the shared mock.
pub fn parse_script_with_exports(
    input: &str,
    lower: impl Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<ParsedScript> {
    let (steps, exports) = parse_with_ctx(
        input,
        lower,
        HashSet::new(),
        ModuleTable::default(),
        HashSet::new(),
        Vec::new(),
    )?;
    Ok(ParsedScript {
        func_docs: collect_func_docs(input, &steps),
        steps,
        exports,
    })
}

/// Parse one expression from source text with an empty module scope, for
/// tooling that shows lowering without running (docs demos, explain output).
/// No imports exist here, so bare function calls fail and qualified calls
/// fail against the empty table; only `INSPECT` passes through untouched.
/// Arithmetic, comparison, variables, env reads, keypaths, and literals
/// lower normally, with literal only subtrees folded before flattening.
pub fn parse_expression_str(input: &str) -> ParseResult<Expr> {
    // The grammar leaves trailing whitespace unconsumed, so lex the
    // trimmed text: otherwise padded but valid input fails the full
    // consumption check.
    let trimmed = input.trim();
    let toks = lex_str(trimmed).map_err(|e| lex_error(trimmed, e))?;
    let modules = ModuleTable::default();
    let reserved = HashSet::new();
    let lower = crate::commands::lower_command as fn(&str, Vec<Arg>) -> ParseResult<StepKind>;
    let ctx = V2Ctx::empty(&lower, &modules, &reserved);
    *ctx.current_line.borrow_mut() = 1;
    crate::parser_v2::parse_single_expr(&toks, 0, &ctx).map_err(|e| v2_error(trimmed, e))
}

/// Parse one guard expression from source text (a bare `guard_seq`:
/// predicates, `not`/`any`/`all`, comma lists), for tooling and tests.
pub fn parse_guard_expr_str(input: &str) -> ParseResult<GuardExpr> {
    let toks = lex_str(input).map_err(|e| lex_error(input, e))?;
    crate::parser_v2::parse_single_guard(&toks, 0).map_err(|e| v2_error(input, e))
}

/// Parsed script with single-file `EXPORT` metadata. `steps` is the
/// linear execution plan, `exports` the `EXPORT` names in source order,
/// and `func_docs` maps top-level `FUNC` base names to (summary, docs)
/// collected from the immediately preceding `#` comment block.
#[derive(Debug, Clone, Default)]
pub struct ParsedScript {
    pub steps: Vec<Step>,
    pub exports: Vec<String>,
    pub func_docs: std::collections::HashMap<String, (String, String)>,
}

fn collect_func_docs(
    input: &str,
    steps: &[Step],
) -> std::collections::HashMap<String, (String, String)> {
    use std::collections::{HashMap, HashSet};
    let lines: Vec<&str> = input.lines().collect();
    // Top-level FUNC definition lines keyed by base name. Brace depth
    // tracks nesting so only depth-0 `FUNC` lines count; the first
    // occurrence wins because same-scope duplicates fail at lowering.
    let mut def_lines: HashMap<String, usize> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut depth: usize = 0;
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let is_comment = trimmed.starts_with('#');
        let is_guard_only = trimmed.starts_with('[');
        if !is_comment
            && !is_guard_only
            && let Some(rest) = strip_func_head(trimmed)
            && depth == 0
            && !seen.contains(&rest)
        {
            seen.insert(rest.clone());
            def_lines.insert(rest, idx + 1);
        }
        if !is_comment {
            for ch in line.chars() {
                if ch == '{' {
                    depth = depth.saturating_add(1);
                } else if ch == '}' {
                    depth = depth.saturating_sub(1);
                }
            }
        }
    }
    let mut out: HashMap<String, (String, String)> = HashMap::new();
    for step in steps {
        if let StepKind::FuncDef { name, .. } = &step.kind
            && !out.contains_key(name)
            && let Some(&line_no) = def_lines.get(name)
            && let Some((summary, docs)) = docs_above(&lines, line_no)
        {
            out.insert(name.clone(), (summary, docs));
        }
    }
    out
}

/// Stamp `#` doc blocks onto top-level `FUNC` steps, from the same map
/// that feeds `func_docs`, so step-carried docs and the sidecar can
/// never disagree. Only top-level definitions qualify (same rule as
/// collection); nested shadowing keeps `docs: None`.
fn stamp_func_docs(input: &str, steps: &mut [Step]) {
    let docs = collect_func_docs(input, steps);
    for step in steps {
        if let StepKind::FuncDef {
            name, docs: slot, ..
        } = &mut step.kind
            && slot.is_none()
            && let Some(pair) = docs.get(name)
        {
            *slot = Some(pair.clone());
        }
    }
}

fn strip_func_head(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix("FUNC")?;
    let after = rest.strip_prefix([' ', '\t'])?;
    if after.is_empty() {
        return None;
    }
    let name: String = after
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b == b'_' || b.is_ascii_digit())
    {
        return None;
    }
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(name)
}

fn docs_above(lines: &[&str], line_no: usize) -> Option<(String, String)> {
    if line_no <= 1 {
        return None;
    }
    let mut block: Vec<String> = Vec::new();
    let mut idx = line_no - 1;
    while idx > 0 {
        idx -= 1;
        let trimmed = lines[idx].trim_start();
        if trimmed.is_empty() {
            break;
        }
        if let Some(body) = trimmed.strip_prefix('#') {
            let text = body.strip_prefix(' ').unwrap_or(body);
            block.push(text.to_string());
        } else {
            break;
        }
        if idx == 0 {
            break;
        }
    }
    if block.is_empty() {
        return None;
    }
    block.reverse();
    let summary = block.first().cloned().unwrap_or_default();
    let docs = block.join("\n");
    Some((summary, docs))
}
