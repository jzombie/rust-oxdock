//! Generic keyed diagnostics for OxDock.
//!
//! This crate owns only the transport: a stable dotted `key`, a rendered
//! message, structured fields, an optional span and hint, and an optional
//! chained source. It owns no domain templates.
//!
//! Domain constructors live with their owners:
//!
//! - DSL spelling diagnostics live in `oxdock-parser`
//!   (`dsl.spelling.*`, `dsl.alias.*` arity).
//! - DSL typing diagnostics live in `oxdock-core`
//!   (`dsl.map.*`, `dsl.alias.*` uniqueness).
//! - CLI diagnostics live in `oxdock-cli/src/errors.rs` (`cli.*`).
//!
//! Rules:
//!
//! - Call sites pass structured fields, never `format!` strings. Each
//!   template lives once inside its owning constructor.
//! - Constructors take decoded fields, never raw mid-parse text. The
//!   transport sanitizes the internal boundary marker (`\u{1f}`) to a
//!   space so untrusted input can never crash a release build through
//!   this path. Tests assert the absence of the marker via
//!   [`assert_no_internal_encodings`].
//! - The crate stays dependency-light (std plus `Arc`, never workspace
//!   crates) so parser, core, and CLI can all depend on it without a
//!   cycle. Conversion into `anyhow::Error` happens at call sites via
//!   `anyhow::Error::new`, never here.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// Internal boundary marker that must never leak into user output.
/// Mirrors `WS_SENTINEL` in `oxdock-parser/src/tag.rs`: transient
/// mid-parse only, decoded or sanitized before display.
pub const INTERNAL_SENTINEL: char = '\u{1f}';

/// Replace the internal boundary marker with a space. The transport
/// applies this to every message, field, and hint so untrusted input
/// containing `\u{1f}` renders safely instead of crashing release.
pub fn sanitize_internal_encoding(text: &str) -> String {
    text.replace(INTERNAL_SENTINEL, " ")
}

/// Test helper asserting user-facing text carries no internal
/// encoding. Panics when the marker is present. Production code never
/// calls this on untrusted input; the transport sanitizes instead.
/// Downstream test suites may call this directly.
pub fn assert_no_internal_encodings(text: &str) {
    if text.contains(INTERNAL_SENTINEL) {
        panic!("internal encoding leaked into user-facing diagnostic: {text:?}");
    }
}

/// Optional source span for a diagnostic. CLI diagnostics leave this
/// empty; parser diagnostics fill line plus column range and source text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Span {
    /// 1-based line number. Zero means unknown.
    pub line: usize,
    /// 1-based start column, when known.
    pub col_start: Option<usize>,
    /// 1-based end column inclusive, when known.
    pub col_end: Option<usize>,
    /// Raw source line text, when available.
    pub source_line: Option<String>,
}

impl Span {
    /// Bare line, used when no column information exists.
    pub fn line_only(line: usize) -> Self {
        Self {
            line,
            col_start: None,
            col_end: None,
            source_line: None,
        }
    }

    /// Full span with columns and source text.
    pub fn full(line: usize, col_start: usize, col_end: usize, source_line: String) -> Self {
        Self {
            line,
            col_start: Some(col_start),
            col_end: Some(col_end),
            source_line: Some(source_line),
        }
    }

    /// Start position without end column or source text (e.g. token
    /// head coordinates). Downstream crates construct spans only
    /// through these constructors, never struct literals, so future
    /// fields (file ids, byte offsets) stay additive.
    pub fn line_col(line: usize, col: usize) -> Self {
        Self {
            line,
            col_start: Some(col),
            col_end: None,
            source_line: None,
        }
    }
}

/// One user-facing error: stable key, rendered message, structured
/// fields, optional span, hint, and chained source. Construct through
/// [`Diagnostic::new`] from the owning domain module, one constructor
/// per diagnostic at its owner.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    key: &'static str,
    message: String,
    fields: BTreeMap<String, String>,
    span: Option<Span>,
    hint: Option<String>,
    source: Option<Arc<dyn std::error::Error + Send + Sync>>,
}

impl Diagnostic {
    /// Generic transport constructor. Domain crates wrap this in one
    /// named constructor per diagnostic. Message, field keys and values,
    /// and hints are sanitized: any [`INTERNAL_SENTINEL`] becomes a
    /// space. Never panics on untrusted input.
    pub fn new(
        key: &'static str,
        message: String,
        fields: BTreeMap<String, String>,
    ) -> Self {
        let message = sanitize_internal_encoding(&message);
        let fields = fields
            .into_iter()
            .map(|(k, v)| {
                (
                    sanitize_internal_encoding(&k),
                    sanitize_internal_encoding(&v),
                )
            })
            .collect();
        Self {
            key,
            message,
            fields,
            span: None,
            hint: None,
            source: None,
        }
    }

    /// Build a sanitized field map from pairs. Domain constructors use
    /// this so no `format!` leaks to call sites and no sentinel leaks
    /// to output.
    pub fn make_fields(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        for (k, v) in pairs {
            map.insert(
                sanitize_internal_encoding(k),
                sanitize_internal_encoding(v),
            );
        }
        map
    }

    /// Stable dotted key, e.g. `dsl.alias.duplicate_on_field`.
    /// Unused for rendering; tests assert on it so a template reword
    /// cannot silently fork one call site.
    pub fn key(&self) -> &'static str {
        self.key
    }

    /// Whole-sentence rendered message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Structured fields that built the message.
    pub fn fields(&self) -> &BTreeMap<String, String> {
        &self.fields
    }

    /// Optional source span.
    pub fn span(&self) -> Option<&Span> {
        self.span.as_ref()
    }

    /// Optional concrete hint with an example.
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// Attach a source span (builder style).
    pub fn with_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    /// Attach a hint (builder style). Sanitized like the message.
    pub fn with_hint(mut self, hint: String) -> Self {
        self.hint = Some(sanitize_internal_encoding(&hint));
        self
    }

    /// Attach a chained source error (builder style). Preserves error
    /// chains across I/O, filesystem, process, and network boundaries
    /// without `anyhow`.
    pub fn with_source(mut self, source: Arc<dyn std::error::Error + Send + Sync>) -> Self {
        self.source = Some(source);
        self
    }
}

impl PartialEq for Diagnostic {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.message == other.message
            && self.fields == other.fields
            && self.span == other.span
            && self.hint == other.hint
    }
}

impl Eq for Diagnostic {}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, "\nhint: {hint}")?;
        }
        if let Some(span) = &self.span
            && span.line > 0
        {
            match (span.col_start, span.col_end) {
                (Some(s), Some(e)) => {
                    write!(f, "\n  --> line {}, col {s}-{e}", span.line)?;
                    if let Some(line_text) = &span.source_line {
                        write!(f, "\n  {} | {line_text}", span.line)?;
                        let width = e.saturating_sub(s).saturating_add(1).max(1);
                        let pad = " ".repeat(s.saturating_sub(1));
                        let carets = "^".repeat(width.min(80));
                        let gutter = " ".repeat(span.line.to_string().len());
                        write!(f, "\n  {gutter} | {pad}{carets}")?;
                    }
                }
                _ => {
                    write!(f, "\n  --> line {}", span.line)?;
                    if let Some(line_text) = &span.source_line {
                        write!(f, "\n  {} | {line_text}", span.line)?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostic {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|s| s.as_ref() as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn transport_sanitizes_sentinel_instead_of_panicking() {
        let evil = format!("bad{}text", INTERNAL_SENTINEL);
        let d = Diagnostic::new(
            "test.sanitize",
            evil.clone(),
            Diagnostic::make_fields(&[("k", &evil)]),
        );
        assert!(!d.message().contains(INTERNAL_SENTINEL));
        assert!(d.message().contains(' '));
        assert_no_internal_encodings(d.message());
    }

    #[test]
    fn source_chain_is_preserved() {
        let io_err = Arc::new(io::Error::new(io::ErrorKind::NotFound, "missing file"));
        let d = Diagnostic::new(
            "test.io",
            "failed to read script".to_string(),
            Diagnostic::make_fields(&[("path", "x.ox")]),
        )
        .with_source(io_err);
        let src = std::error::Error::source(&d).expect("source preserved");
        assert!(src.to_string().contains("missing file"));
    }

    #[test]
    fn caret_gutter_aligns_for_any_line_width() {
        for line in [1usize, 9, 10, 99, 100, 1234] {
            let d = Diagnostic::new(
                "test.span",
                "boom".to_string(),
                Diagnostic::make_fields(&[]),
            )
            .with_span(Span::full(line, 2, 3, "hi".to_string()));
            let rendered = d.to_string();
            let width = line.to_string().len();
            let gutter = format!("  {} |", " ".repeat(width));
            assert!(rendered.contains(&gutter), "line {line}: {rendered}");
        }
    }

    #[test]
    fn equality_ignores_source_chain() {
        let a = Diagnostic::new("k", "m".to_string(), Diagnostic::make_fields(&[]));
        let b = a.clone().with_source(Arc::new(io::Error::other("chain")));
        assert_eq!(a, b);
    }
}
