//! Typed parse errors for the OxDock DSL (issue #143).
//!
//! Beginners must always learn WHAT they did wrong: line and column,
//! what was found, what was expected, and a concrete example. A syntax
//! error must never surface as a bare `unknown command: X`.
//!
//! String and file parsers populate line, column span, and source line
//! from lexer spans. Token stream parsers (`macro_input.rs`) set
//! `source_line` to `None` and forward the compiler span instead, since
//! `proc_macro2::Span` carries coordinates but not source text.

use std::fmt;

use oxdock_diagnostics::{Diagnostic, Span};

/// Backwards compatible result alias for parser entrypoints.
/// Public functions return `ParseResult` directly instead of erasing
/// into `anyhow::Error`, so downstream crates match statically.
pub type ParseResult<T> = std::result::Result<T, ParseError>;

/// Machine readable classification of a parse failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// Low-level syntax failure: the input never reached lowering
    /// (lexer rejection or token-level structural failure).
    Syntax,
    /// First word is a known command or structural keyword, but the
    /// rest of the line is malformed. Never report these as unknown.
    InvalidSyntax { command: String },
    /// First word matches no known command or keyword in any case.
    /// Reserved strictly for truly unknown names.
    UnknownCommand { name: String },
    /// Guard, block, scope, or structural keyword failure
    /// (`WITH_IO`, `LET`, `FOR`, `IF`, `{{`, `}}`, guards).
    Structural { rule: String },
    /// Arity, flag, or static argument type failure for a known command.
    Validation { command: String },
}

/// Location and diagnostic context threaded through lowering.
///
/// String parsers fill `col_start`, `col_end`, and `source_line` from
/// lexer spans. Token stream parsers leave `source_line` as `None`;
/// macro call sites keep their own `proc_macro2::Span` for
/// `syn::Error` conversion (a span handle is not `Send`/`Sync`, so it
/// never lives inside `ParseError`, which must stay convertible into
/// `anyhow::Error`). Integer coordinates extracted from the span are
/// attached here instead.
#[derive(Debug, Clone, Default)]
pub struct SpanContext<'a> {
    /// 1-based line number. Always populated on the string path.
    pub line: usize,
    /// 1-based start column. `None` on token stream paths without locations.
    pub col_start: Option<usize>,
    /// 1-based end column (inclusive). `None` on token stream paths.
    pub col_end: Option<usize>,
    /// Raw source line text. `None` on token stream paths.
    pub source_line: Option<String>,
    /// Full script text (borrowed, zero copy). Lets span refinement
    /// resolve the exact source line for any line number, including
    /// multi-line bodies, without ever faking text. `None` on token
    /// stream paths and bare line fallbacks.
    pub source: Option<&'a str>,
    /// Compiler span for macro contexts, kept at the call site for
    /// `syn::Error` conversion. Gated on the same feature as
    /// `macro_input.rs` to avoid new mandatory parser dependencies.
    /// Never transferred into `ParseError`.
    #[cfg(feature = "proc-macro-api")]
    pub compiler_span: Option<proc_macro2::Span>,
    /// Zero-based step index in the parsed script, when known.
    pub step_index: Option<usize>,
}

impl<'a> SpanContext<'a> {
    /// Bare line context. Used only where no span is available
    /// (direct `lower_command` calls, end of script errors).
    pub fn line_only(line: usize) -> Self {
        Self {
            line,
            ..Self::default()
        }
    }

    /// Full string path context with column span and source text.
    pub fn full(line: usize, col_start: usize, col_end: usize, source_line: String) -> Self {
        Self {
            line,
            col_start: Some(col_start),
            col_end: Some(col_end),
            source_line: Some(source_line),
            source: None,
            #[cfg(feature = "proc-macro-api")]
            compiler_span: None,
            step_index: None,
        }
    }

    /// Attach the full script text for multi-line span resolution.
    pub fn with_source(mut self, source: &'a str) -> Self {
        if self.source_line.is_none() {
            self.source_line = source
                .lines()
                .nth(self.line.saturating_sub(1))
                .map(str::to_string);
        }
        self.source = Some(source);
        self
    }

    /// Token stream context: coordinates without source text.
    /// The caller keeps the span handle for `syn::Error` conversion;
    /// only the integer coordinates travel in the context.
    #[cfg(feature = "proc-macro-api")]
    pub fn from_compiler_span(
        line: usize,
        col_start: Option<usize>,
        col_end: Option<usize>,
        span: proc_macro2::Span,
    ) -> Self {
        Self {
            line,
            col_start,
            col_end,
            source_line: None,
            source: None,
            compiler_span: Some(span),
            step_index: None,
        }
    }

    /// Borrowed compiler span for `syn::Error` conversion at the call site.
    #[cfg(feature = "proc-macro-api")]
    pub fn compiler_span(&self) -> Option<&proc_macro2::Span> {
        self.compiler_span.as_ref()
    }

    /// Attach a step index (builder style).
    pub fn with_step(mut self, step_index: usize) -> Self {
        self.step_index = Some(step_index);
        self
    }
}

/// Typed parse error with structured diagnostics and a
/// backwards compatible `Display` rendering.
///
/// Diagnostic detail lives behind a `Box` so `ParseResult` stays small
/// in `Result` returns (cold path allocation only).
#[derive(Debug, Clone)]
pub struct ParseError {    kind: ParseErrorKind,
    line: usize,
    detail: Box<ErrorDetail>,
}

/// Heap stored diagnostic detail for [`ParseError`].
#[derive(Debug, Clone)]
struct ErrorDetail {
    col_start: Option<usize>,
    col_end: Option<usize>,
    source_line: Option<String>,
    step_index: Option<usize>,
    found: Option<String>,
    expected: Vec<String>,
    hint: Option<String>,
    /// Legacy message body. `Display` appends the location block,
    /// so existing `expect_error_contains` assertions keep passing.
    message: String,
    /// Owning keyed diagnostic, when this error was built through
    /// [`ParseError::from_diagnostic`]. Carries the stable key and
    /// structured fields; `message` mirrors its rendered text so
    /// `Display` output is unchanged during migration.
    diagnostic: Option<Diagnostic>,
}

/// Build the transport [`Span`] for a [`SpanContext`]. Returns `None`
/// when the line is unknown (`0`), so direct `lower_command` errors
/// stay spanless until [`ParseError::with_span`] enrichment.
fn span_from_ctx(ctx: &SpanContext) -> Option<Span> {
    if ctx.line == 0 {
        return None;
    }
    match (ctx.col_start, ctx.col_end, ctx.source_line.clone()) {
        (Some(start), Some(end), Some(line_text)) => {
            Some(Span::full(ctx.line, start, end, line_text))
        }
        _ => Some(Span::line_only(ctx.line)),
    }
}

impl ParseError {
    fn new(
        kind: ParseErrorKind,
        line: usize,
        ctx: &SpanContext,
        found: Option<String>,
        expected: Vec<String>,
        hint: Option<String>,
        message: String,
    ) -> Self {
        Self {
            kind,
            line,
            detail: Box::new(ErrorDetail {
                col_start: ctx.col_start,
                col_end: ctx.col_end,
                source_line: ctx.source_line.clone(),
                step_index: ctx.step_index,
                found,
                expected,
                hint,
                message,
                diagnostic: None,
            }),
        }
    }
    /// Low-level syntax failure. `message` must already contain the
    /// `parse error (expected: ...)` header; the location block is
    /// appended by `Display` unless already present.
    pub fn syntax(
        message: String,
        expected: Vec<String>,
        hint: Option<String>,
        ctx: &SpanContext,
    ) -> Self {
        Self::new(
            ParseErrorKind::Syntax,
            ctx.line,
            ctx,
            None,
            expected,
            hint,
            message,
        )
    }

    /// Known command or keyword with malformed arguments.
    pub fn invalid_syntax(
        command: &str,
        message: String,
        found: Option<String>,
        expected: Vec<String>,
        hint: Option<String>,
        ctx: &SpanContext,
    ) -> Self {
        Self::new(
            ParseErrorKind::InvalidSyntax {
                command: command.to_string(),
            },
            ctx.line,
            ctx,
            found,
            expected,
            hint,
            message,
        )
    }

    /// Truly unknown command name. Callers must ensure `classify`
    /// sent keyword led lines to `invalid_syntax` instead.
    pub fn unknown_command(
        name: &str,
        message: String,
        hint: Option<String>,
        ctx: &SpanContext,
    ) -> Self {
        Self::new(
            ParseErrorKind::UnknownCommand {
                name: name.to_string(),
            },
            ctx.line,
            ctx,
            Some(name.to_string()),
            Vec::new(),
            hint,
            message,
        )
    }

    /// Structural failure (guards, blocks, `WITH_IO`, `LET`, `FOR`, `IF`).
    pub fn structural(rule: &str, message: String, ctx: &SpanContext) -> Self {
        Self::new(
            ParseErrorKind::Structural {
                rule: rule.to_string(),
            },
            ctx.line,
            ctx,
            None,
            Vec::new(),
            None,
            message,
        )
    }

    /// Arity, flag, or static type failure for a known command.
    pub fn validation(command: &str, message: String, ctx: &SpanContext) -> Self {
        Self::new(
            ParseErrorKind::Validation {
                command: command.to_string(),
            },
            ctx.line,
            ctx,
            None,
            Vec::new(),
            None,
            message,
        )
    }

    /// Build from an owning keyed [`Diagnostic`]. The diagnostic's
    /// rendered message becomes the legacy body (so `Display` output
    /// is unchanged) and its hint is forwarded when the diagnostic
    /// carries one. The `ctx` span is synced onto both the error and
    /// the embedded diagnostic, so `err.diagnostic().span()` matches
    /// the error's span; later [`ParseError::with_span`] enrichment
    /// keeps them in sync the same way.
    pub fn from_diagnostic(
        kind: ParseErrorKind,
        diagnostic: Diagnostic,
        ctx: &SpanContext,
    ) -> Self {
        let message = diagnostic.message().to_string();
        let hint = diagnostic.hint().map(str::to_string);
        let diagnostic = match span_from_ctx(ctx) {
            Some(span) => diagnostic.with_span(span),
            None => diagnostic,
        };
        let mut err = Self::new(kind, ctx.line, ctx, None, Vec::new(), hint, message);
        err.detail.diagnostic = Some(diagnostic);
        err
    }

    /// Owning keyed diagnostic, when built via [`ParseError::from_diagnostic`].
    pub fn diagnostic(&self) -> Option<&Diagnostic> {
        self.detail.diagnostic.as_ref()
    }

    /// Stable dotted key of the owning diagnostic, when present.
    pub fn key(&self) -> Option<&'static str> {
        self.detail.diagnostic.as_ref().map(Diagnostic::key)
    }

    /// Enrich an error produced without span context (e.g. from a
    /// direct `lower_command` call) with the caller's location.
    /// Existing span fields are only overwritten when the current
    /// value is `None`, so lexer spans are never clobbered. The
    /// embedded diagnostic (when present) is synced the same way:
    /// a span is attached only while it has none.
    pub fn with_span(mut self, ctx: &SpanContext) -> Self {
        if self.line == 0 {
            self.line = ctx.line;
        }
        let detail = &mut self.detail;
        if detail.col_start.is_none() {
            detail.col_start = ctx.col_start;
        }
        if detail.col_end.is_none() {
            detail.col_end = ctx.col_end;
        }
        if detail.source_line.is_none() {
            detail.source_line = ctx.source_line.clone();
        }
        if detail.step_index.is_none() {
            detail.step_index = ctx.step_index;
        }
        if detail
            .diagnostic
            .as_ref()
            .is_some_and(|d| d.span().is_none())
            && let Some(span) = span_from_ctx(ctx)
            && let Some(d) = detail.diagnostic.take()
        {
            detail.diagnostic = Some(d.with_span(span));
        }
        self
    }

    /// Attach a step index (builder style).
    pub fn with_step(mut self, step_index: usize) -> Self {
        self.detail.step_index = Some(step_index);
        self
    }

    /// Attach the found text (builder style). Used by the
    /// `invalid_syntax` path, whose helper computes it alongside the
    /// diagnostic.
    pub fn with_found(mut self, found: Option<String>) -> Self {
        self.detail.found = found;
        self
    }

    /// Attach expected syntax alternatives (builder style). Used by
    /// the `invalid_syntax` path to keep `expected()` informative
    /// through the diagnostic migration.
    pub fn with_expected(mut self, expected: Vec<String>) -> Self {
        self.detail.expected = expected;
        self
    }

    /// Attach the hint (builder style). Used by the `invalid_syntax`
    /// and `unknown_command` paths: the hint rides inline in the
    /// diagnostic message for legacy `Display` parity, and here so
    /// the `hint()` accessor keeps working.
    pub fn with_hint(mut self, hint: Option<String>) -> Self {
        self.detail.hint = hint;
        self
    }

    /// Machine readable kind.
    pub fn kind(&self) -> &ParseErrorKind {
        &self.kind
    }

    /// 1-based line number (`0` means unknown, direct calls only).
    pub fn line(&self) -> usize {
        self.line
    }

    /// 1-based start column, when known.
    pub fn col_start(&self) -> Option<usize> {
        self.detail.col_start
    }

    /// 1-based end column (inclusive), when known.
    pub fn col_end(&self) -> Option<usize> {
        self.detail.col_end
    }

    /// Raw source line text, when available.
    pub fn source_line(&self) -> Option<&str> {
        self.detail.source_line.as_deref()
    }

    /// Zero-based step index, when known.
    pub fn step_index(&self) -> Option<usize> {
        self.detail.step_index
    }

    /// What was found (command name or rendered args), when known.
    pub fn found(&self) -> Option<&str> {
        self.detail.found.as_deref()
    }

    /// Expected syntax alternatives.
    pub fn expected(&self) -> &[String] {
        &self.detail.expected
    }

    /// Concrete hint with an example, when known.
    pub fn hint(&self) -> Option<&str> {
        self.detail.hint.as_deref()
    }

    /// Render the caret block for the stored span, if complete.
    fn caret_block(&self) -> Option<String> {
        let (start, end) = match (self.detail.col_start, self.detail.col_end) {
            (Some(s), Some(e)) => (s, e),
            _ => return None,
        };
        let line_text = self.detail.source_line.as_deref()?;
        if self.line == 0 || start == 0 {
            return None;
        }
        let width = end.saturating_sub(start).saturating_add(1).max(1);
        let pad = " ".repeat(start.saturating_sub(1));
        let carets = "^".repeat(width.min(80));
        Some(format!(
            "\n  --> line {line}, col {start}-{end}\n  {line} | {line_text}\n    | {pad}{carets}",
            line = self.line,
        ))
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.detail.message)?;
        if let Some(block) = self.caret_block() {
            // Messages already ending with the caret block do not duplicate it.
            if !self.detail.message.contains("--> line") {
                write!(f, "{block}")?;
            }
        } else if self.line > 0 && !self.detail.message.contains(&format!("line {}", self.line)) {
            write!(f, "\n  --> line {}", self.line)?;
        }
        Ok(())
    }
}

impl std::error::Error for ParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_diagnostic_syncs_span_onto_both() {
        let ctx = SpanContext::full(3, 2, 5, "WRITE $x".to_string());
        let diag = crate::diagnostics::missing_argument("WRITE", "path");
        let err = ParseError::from_diagnostic(
            ParseErrorKind::Validation {
                command: "WRITE".to_string(),
            },
            diag,
            &ctx,
        );
        assert_eq!(err.line(), 3);
        let span = err.diagnostic().expect("payload").span().expect("span synced");
        assert_eq!(span.line, 3);
        assert_eq!(span.col_start, Some(2));
        assert_eq!(span.col_end, Some(5));
        assert_eq!(span.source_line.as_deref(), Some("WRITE $x"));
    }

    #[test]
    fn with_span_enrichment_reaches_embedded_diagnostic() {
        let diag = crate::diagnostics::unknown_flag("WRITE", "--nope");
        let err = ParseError::from_diagnostic(
            ParseErrorKind::Validation {
                command: "WRITE".to_string(),
            },
            diag,
            &SpanContext::line_only(0),
        );
        assert!(err.diagnostic().expect("payload").span().is_none());
        let ctx = SpanContext::full(7, 1, 4, "WRITE --nope".to_string());
        let err = err.with_span(&ctx);
        assert_eq!(err.line(), 7);
        let span = err.diagnostic().expect("payload").span().expect("span synced");
        assert_eq!(span.line, 7);
        assert_eq!(span.col_start, Some(1));
    }
}
