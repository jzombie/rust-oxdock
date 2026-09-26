use crate::ast::COMMANDS;
use crate::error::{ParseError, SpanContext};
use pest::{Parser, iterators::Pair};
use pest_derive::Parser;

#[derive(Parser)]
#[grammar = "dsl.pest"]
pub struct LanguageParser;

pub const LANGUAGE_SPEC: &str = include_str!("dsl.pest");

#[derive(Debug, Clone)]
pub enum RawToken<'a> {
    Guard {
        pair: Pair<'a, Rule>,
        line_end: usize,
        span: SpanContext<'a>,
    },
    BlockStart {
        line_no: usize,
        span: SpanContext<'a>,
    },
    BlockEnd {
        line_no: usize,
        span: SpanContext<'a>,
    },
    /// A structural command (WITH_IO, FOR, IF, LET, $var mutation) : parsed by the grammar.
    Command {
        pair: Pair<'a, Rule>,
        line_no: usize,
        span: SpanContext<'a>,
    },
    /// A generic instruction : command name + raw args, lowered by a function.
    Instruction {
        pair: Pair<'a, Rule>,
        line_no: usize,
        span: SpanContext<'a>,
    },
    /// A `RUN ["exe", "arg", ...]` exec-form statement : carries a structured
    /// `list_literal` pair, lowered without shell stringification.
    RunExec {
        pair: Pair<'a, Rule>,
        line_no: usize,
        span: SpanContext<'a>,
    },
}

/// Build a string path span context from a pest pair and the full input.
/// Multi-line spans clamp to the start line, matching the pest error rule.
pub fn span_of<'a>(pair: &Pair<Rule>, input: &'a str) -> SpanContext<'a> {
    let span = pair.as_span();
    let (line, col_start) = span.start_pos().line_col();
    let (end_line, end_col) = span.end_pos().line_col();
    let col_end = if end_line == line {
        end_col.saturating_sub(1).max(col_start)
    } else {
        col_start
    };
    let source_line = input
        .lines()
        .nth(line.saturating_sub(1))
        .unwrap_or("")
        .to_string();
    SpanContext::full(line, col_start, col_end, source_line).with_source(input)
}

/// Refine a statement level span to the exact sub-expression pair.
/// Coordinates come from the pair and the source line is resolved from
/// the shared script text, so multi-line bodies keep exact carets.
/// Falls back to the parent line text only when no shared text exists.
pub fn refine_span<'a>(ctx: &SpanContext<'a>, pair: &Pair<Rule>) -> SpanContext<'a> {
    let span = pair.as_span();
    let (line, col_start) = span.start_pos().line_col();
    let (end_line, end_col) = span.end_pos().line_col();
    let col_end = if end_line == line {
        end_col.saturating_sub(1).max(col_start)
    } else {
        col_start
    };
    let source_line = ctx
        .source
        .and_then(|text| text.lines().nth(line.saturating_sub(1)).map(str::to_string))
        .or_else(|| {
            if line == ctx.line {
                ctx.source_line.clone()
            } else {
                None
            }
        });
    SpanContext {
        line,
        col_start: Some(col_start),
        col_end: Some(col_end),
        source_line,
        source: ctx.source,
        #[cfg(feature = "proc-macro-api")]
        compiler_span: ctx.compiler_span,
        step_index: ctx.step_index,
    }
}
/// Span context for a bare line number (fallback for end of script errors).
pub fn span_for_line(input: &str, line_no: usize) -> SpanContext<'_> {
    let source_line = input
        .lines()
        .nth(line_no.saturating_sub(1))
        .unwrap_or("")
        .to_string();
    if source_line.is_empty() {
        SpanContext::line_only(line_no)
    } else {
        SpanContext::full(line_no, 1, 1, source_line)
    }
}

pub fn tokenize(input: &str) -> Result<Vec<RawToken<'_>>, ParseError> {
    let mut tokens = Vec::new();
    let mut pairs = LanguageParser::parse(Rule::script, input).map_err(parse_pest_error)?;
    let Some(root) = pairs.next() else {
        return Ok(tokens);
    };

    for pair in root.into_inner() {
        let line_no = pair.as_span().start_pos().line_col().0;
        let span = span_of(&pair, input);
        match pair.as_rule() {
            Rule::blank | Rule::hash_comment | Rule::semicolon | Rule::EOI | Rule::COMMENT => {}
            Rule::guard_line => {
                let (line_end, _) = pair.as_span().end_pos().line_col();
                tokens.push(RawToken::Guard {
                    pair,
                    line_end,
                    span,
                });
            }
            Rule::block_start => tokens.push(RawToken::BlockStart { line_no, span }),
            Rule::block_end => tokens.push(RawToken::BlockEnd { line_no, span }),
            // Structural commands : parsed by grammar-specific rules
            Rule::with_io_command
            | Rule::inherit_env_command
            | Rule::import_statement
            | Rule::export_statement
            |             Rule::async_statement
            | Rule::async_statement_block
            | Rule::timeout_statement
            | Rule::remote_statement
            | Rule::let_async_statement
            | Rule::let_capture_statement
            | Rule::await_statement
            | Rule::cancel_statement
            | Rule::for_statement
            | Rule::let_statement
            | Rule::mutate_statement
            | Rule::if_statement
            | Rule::while_statement
            | Rule::func_def
            | Rule::call_statement
            | Rule::return_statement
            | Rule::break_statement
            | Rule::continue_statement => tokens.push(RawToken::Command {
                pair,
                line_no,
                span,
            }),
            // Generic instructions : lowered by a function
            Rule::instruction | Rule::instruction_inner => tokens.push(RawToken::Instruction {
                pair,
                line_no,
                span,
            }),
            // RUN exec form : structured list lowering, never shell text
            Rule::run_exec_statement | Rule::run_exec_inner => tokens.push(RawToken::RunExec {
                pair,
                line_no,
                span,
            }),
            other => {
                return Err(ParseError::structural(
                    "parser",
                    format!("unexpected parser rule {other:?}"),
                    &span,
                ));
            }
        }
    }
    Ok(tokens)
}

/// Convert a pest failure into a typed error, preserving the exact
/// legacy message (header, uppercase note, caret block) and exposing
/// the expected sets and hint as structured fields. Shared by script
/// tokenizing and snippet expression parsing.
pub fn parse_pest_error(err: pest::error::Error<Rule>) -> ParseError {
    use pest::error::{ErrorVariant, LineColLocation};

    let (line_no, col_start, col_end) = match err.line_col {
        LineColLocation::Pos((line, col)) => (line, col, col),
        LineColLocation::Span((line, col), (end_line, end_col)) => {
            if line == end_line {
                let end = end_col.saturating_sub(1).max(col);
                (line, col, end)
            } else {
                (line, col, col)
            }
        }
    };

    let mut expected: Vec<String> = Vec::new();
    let mut msg = String::new();
    match &err.variant {
        ErrorVariant::ParsingError {
            positives,
            negatives,
        } => {
            msg.push_str("parse error");
            if !positives.is_empty() {
                let list = positives
                    .iter()
                    .map(|r| format!("{r:?}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                expected = positives.iter().map(|r| format!("{r:?}")).collect();
                msg.push_str(&format!(" (expected: {list})"));
            }
            if !negatives.is_empty() {
                let unexpected = negatives
                    .iter()
                    .map(|r| format!("{r:?}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                msg.push_str(&format!(" (unexpected: {unexpected})"));
            }
        }
        ErrorVariant::CustomError { message } => {
            msg.push_str(message);
        }
    }

    let line = err.line();
    let caret_len = col_end.saturating_sub(col_start).saturating_add(1).max(1);
    let caret_pad = " ".repeat(col_start.saturating_sub(1));
    let caret_mark = "^".repeat(caret_len);

    let note = detect_case_error(line);
    if let Some(note) = &note {
        msg.push_str(&format!("\nnote: {note}"));
    }

    msg.push_str(&format!(
        "\n  --> line {line_no}, col {col_start}-{col_end}\n  {line_no} | {line}\n    | {pad}{caret}",
        line_no = line_no,
        col_start = col_start,
        col_end = col_end,
        line = line,
        pad = caret_pad,
        caret = caret_mark
    ));

    let ctx = SpanContext::full(line_no, col_start, col_end, line.to_string());
    ParseError::pest(msg, expected, note, &ctx)
}

fn detect_case_error(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("//") || trimmed.starts_with("/*") {
        return None;
    }
    if matches!(trimmed.chars().next(), Some('[' | '{' | '}' | ']')) {
        return None;
    }
    let word = trimmed.split_whitespace().next()?;
    for cmd in COMMANDS {
        let expected = cmd.as_str();
        if word.eq_ignore_ascii_case(expected) && word != expected {
            return Some(format!(
                "command must be uppercase: found `{found}`, expected `{expected}`",
                found = word,
                expected = expected
            ));
        }
    }
    None
}
