use crate::ast::COMMANDS;
use anyhow::{Result, anyhow, bail};
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
    },
    BlockStart {
        line_no: usize,
    },
    BlockEnd {
        line_no: usize,
    },
    Command {
        pair: Pair<'a, Rule>,
        line_no: usize,
    },
}

pub fn tokenize(input: &str) -> Result<Vec<RawToken<'_>>> {
    let mut tokens = Vec::new();
    let mut pairs = LanguageParser::parse(Rule::script, input)
        .map_err(|err| anyhow!(format_pest_error(err)))?;
    let Some(root) = pairs.next() else {
        return Ok(tokens);
    };

    for pair in root.into_inner() {
        let line_no = pair.as_span().start_pos().line_col().0;
        match pair.as_rule() {
            Rule::blank | Rule::hash_comment | Rule::semicolon | Rule::EOI | Rule::COMMENT => {}
            Rule::guard_line => {
                let (line_end, _) = pair.as_span().end_pos().line_col();
                tokens.push(RawToken::Guard { pair, line_end });
            }
            Rule::block_start => tokens.push(RawToken::BlockStart { line_no }),
            Rule::block_end => tokens.push(RawToken::BlockEnd { line_no }),
            Rule::workdir_command
            | Rule::workspace_command
            | Rule::env_command
            | Rule::echo_command
            | Rule::run_command
            | Rule::run_bg_command
            | Rule::copy_command
            | Rule::with_io_command
            | Rule::copy_git_command
            | Rule::hash_sha256_command
            | Rule::inherit_env_command
            | Rule::symlink_command
            | Rule::mkdir_command
            | Rule::ls_command
            | Rule::cwd_command
            | Rule::read_command
            | Rule::write_command
            | Rule::append_command
            | Rule::assert_file_hash_command
            | Rule::assert_file_content_command
            | Rule::assert_dir_command
            | Rule::assert_absent_command
            | Rule::assert_stdout_command
            | Rule::exit_command => tokens.push(RawToken::Command { pair, line_no }),
            other => bail!("unexpected parser rule {:?}", other),
        }
    }
    Ok(tokens)
}

fn format_pest_error(err: pest::error::Error<Rule>) -> String {
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

    let mut msg = String::new();
    match &err.variant {
        ErrorVariant::ParsingError {
            positives,
            negatives,
        } => {
            msg.push_str("parse error");
            if !positives.is_empty() {
                let expected = positives
                    .iter()
                    .map(|r| format!("{:?}", r))
                    .collect::<Vec<_>>()
                    .join(", ");
                msg.push_str(&format!(" (expected: {expected})"));
            }
            if !negatives.is_empty() {
                let unexpected = negatives
                    .iter()
                    .map(|r| format!("{:?}", r))
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

    if let Some(note) = detect_case_error(line) {
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

    msg
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
