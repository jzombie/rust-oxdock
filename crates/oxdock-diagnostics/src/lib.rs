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
//!   cycle. Conversion into `anyhow::Error` needs no local impl:
//!   `anyhow`'s blanket `From<E: StdError + Send + Sync + 'static>`
//!   already covers [`Diagnostic`], so `?` and `.into()` carry keyed
//!   diagnostics into not-yet-migrated `anyhow::Result` signatures
//!   with the message and chain intact. New fallible signatures
//!   should return [`DiagnosticResult`] instead.

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

/// Fallible result with a keyed diagnostic. New fallible signatures
/// return this instead of `anyhow::Result`: callers add context with
/// [`Diagnostic::with_hint`] / [`Diagnostic::with_source`] and convert
/// into `anyhow::Error` with `?` or `.into()` (via `anyhow`'s blanket
/// `From`) while the rest of the workspace migrates leaf to root.
pub type DiagnosticResult<T> = Result<T, Diagnostic>;

/// `{anyhow:#}`-equivalent flattening: joins one-layer node texts
/// with `": "`, exactly what the alternate renderer prints for the
/// same chain, so boundary messages that inline the cause keep the
/// pre-flip byte-identical wording after an `&anyhow::Error` to
/// `&Diagnostic` flip. Wrappers that inline stay sourceless (the
/// chain text is the message); wrappers that chain stay bare (the
/// join renders the depth). Mixing both prints every cause twice.
/// Single owner for the whole workspace: leaf crates (`utils`,
/// `pty`, `process`, plugins) and `exec`/`build` all reuse this,
/// never restate the walk.
pub fn flatten_chain(reason: &Diagnostic) -> String {
    let mut parts = vec![reason.to_string()];
    let mut current =
        std::error::Error::source(reason).map(|node| node as &(dyn std::error::Error + 'static));
    while let Some(node) = current {
        parts.push(node.to_string());
        current = node.source();
    }
    parts.join(": ")
}

/// One user-facing error: stable key, rendered message, structured
/// fields, optional span, hint, and chained source. Construct through
/// [`Diagnostic::new`] from the owning domain module, one constructor
/// per diagnostic at its owner.
///
/// Pointer-sized by design: the payload lives behind one `Box` so
/// [`DiagnosticResult`] returns stay at the `anyhow::Error` footprint
/// on the happy path (the error value is constructed only on the cold
/// path). A test pins `size_of::<Diagnostic>()` below.
#[derive(Debug, Clone)]
pub struct Diagnostic(Box<DiagnosticInner>);

/// Boxed payload behind [`Diagnostic`]. Private so the footprint
/// guarantee and the constructor-per-diagnostic rule cannot drift.
#[derive(Debug, Clone)]
struct DiagnosticInner {
    key: &'static str,
    message: String,
    fields: BTreeMap<String, String>,
    span: Option<Span>,
    hint: Option<String>,
    source: Option<Arc<dyn std::error::Error + Send + Sync>>,
    /// Outer call-path frames, outermost first. `message()` stays
    /// pure; `Display` renders frames as a `: `-joined prefix so a
    /// lone diagnostic reads like the anyhow context chain it
    /// replaces across `DiagnosticResult` boundaries. Appended
    /// during unwinding innermost-first, so later (outer) frames
    /// prepend ahead of earlier (inner) ones.
    contexts: Vec<String>,
    /// Snapshot section attached at the failure boundary
    /// (`error_with_snapshot`). Rendered after the failure text,
    /// never folded into `message()`. Excluded from equality
    /// alongside contexts and source.
    snapshot: Option<String>,
}

impl Diagnostic {
    /// Generic transport constructor. Domain crates wrap this in one
    /// named constructor per diagnostic. Message, field keys and values,
    /// and hints are sanitized: any [`INTERNAL_SENTINEL`] becomes a
    /// space. Never panics on untrusted input.
    pub fn new(key: &'static str, message: String, fields: BTreeMap<String, String>) -> Self {
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
        Self(Box::new(DiagnosticInner {
            key,
            message,
            fields,
            span: None,
            hint: None,
            source: None,
            contexts: Vec::new(),
            snapshot: None,
        }))
    }

    /// Build a sanitized field map from pairs. Domain constructors use
    /// this so no `format!` leaks to call sites and no sentinel leaks
    /// to output.
    pub fn make_fields(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        for (k, v) in pairs {
            map.insert(sanitize_internal_encoding(k), sanitize_internal_encoding(v));
        }
        map
    }

    /// Stable dotted key, e.g. `dsl.alias.duplicate_on_field`.
    /// Unused for rendering; tests assert on it so a template reword
    /// cannot silently fork one call site.
    pub fn key(&self) -> &'static str {
        self.0.key
    }

    /// Whole-sentence rendered message.
    pub fn message(&self) -> &str {
        &self.0.message
    }

    /// Structured fields that built the message.
    pub fn fields(&self) -> &BTreeMap<String, String> {
        &self.0.fields
    }

    /// Optional source span.
    pub fn span(&self) -> Option<&Span> {
        self.0.span.as_ref()
    }

    /// Optional concrete hint with an example.
    pub fn hint(&self) -> Option<&str> {
        self.0.hint.as_deref()
    }

    /// Outer call-path frames, outermost first. Empty until
    /// a boundary replays anyhow contexts natively (see
    /// [`Diagnostic::with_contexts`]).
    pub fn contexts(&self) -> &[String] {
        &self.0.contexts
    }

    /// Snapshot section attached at the failure boundary, if any.
    pub fn snapshot(&self) -> Option<&str> {
        self.0.snapshot.as_deref()
    }

    /// Shared handle to the chained source, when one is attached.
    /// Lets owners re-chain a cause under a new typed node (such as
    /// an exit code) without dropping it.
    pub fn shared_source(&self) -> Option<Arc<dyn std::error::Error + Send + Sync>> {
        self.0.source.clone()
    }

    /// Attach a source span (builder style).
    pub fn with_span(mut self, span: Span) -> Self {
        self.0.span = Some(span);
        self
    }

    /// Attach a hint (builder style). Sanitized like the message.
    pub fn with_hint(mut self, hint: String) -> Self {
        self.0.hint = Some(sanitize_internal_encoding(&hint));
        self
    }

    /// Attach a chained source error (builder style). Preserves error
    /// chains across I/O, filesystem, process, and network boundaries
    /// without `anyhow`.
    pub fn with_source(mut self, source: Arc<dyn std::error::Error + Send + Sync>) -> Self {
        self.0.source = Some(source);
        self
    }

    /// Append outer call-path frames (builder style, outermost
    /// first). Sanitized like the message. Unwinding attaches
    /// innermost first, so later (outer) frames prepend ahead of
    /// earlier (inner) ones and the stored order stays outermost
    /// first. Lets `DiagnosticResult` boundaries carry the `step N`
    /// call path without anyhow context layers; `message()` stays
    /// pure.
    pub fn with_contexts(mut self, frames: Vec<String>) -> Self {
        self.0.contexts.splice(
            0..0,
            frames.into_iter().map(|f| sanitize_internal_encoding(&f)),
        );
        self
    }

    /// Attach the snapshot section (builder style). Sanitized like
    /// the message. Rendered after the failure text, never folded
    /// into `message()`.
    pub fn with_snapshot(mut self, snapshot: &str) -> Self {
        self.0.snapshot = Some(sanitize_internal_encoding(snapshot));
        self
    }
}

impl PartialEq for Diagnostic {
    fn eq(&self, other: &Self) -> bool {
        // Key, message, fields, span, and hint decide equality.
        // Sources, call-path frames, and the snapshot section are
        // provenance, excluded exactly like the source chain (see
        // `equality_ignores_source_chain`).
        self.0.key == other.0.key
            && self.0.message == other.0.message
            && self.0.fields == other.0.fields
            && self.0.span == other.0.span
            && self.0.hint == other.0.hint
    }
}

impl Eq for Diagnostic {}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(snapshot) = &self.0.snapshot {
            // Failure-boundary rendering: the ordered frame sequence
            // (outer contexts, own message plus hint, source chain)
            // folds exactly like the historic flattened renderer, so
            // `Display` stays byte-identical while the key, span, and
            // code ride natively. The span block stays out: the
            // position already reads inline above, and the span
            // itself stays machine-readable via `span()`.
            let mut seq: Vec<String> = self.0.contexts.clone();
            let mut own = self.0.message.clone();
            if let Some(hint) = &self.0.hint {
                own = format!("{own}\nhint: {hint}");
            }
            seq.push(own);
            let mut current = self
                .0
                .source
                .as_ref()
                .map(|s| s.as_ref() as &(dyn std::error::Error + 'static));
            while let Some(node) = current {
                seq.push(node.to_string());
                current = node.source();
            }
            if seq.len() <= 1 {
                write!(f, "{}\n{snapshot}", seq[0])?;
            } else if seq.len() == 2 {
                write!(f, "{} ({})\n{snapshot}", seq[0], seq[1])?;
            } else {
                let causes = seq[2..].join("\n  ");
                write!(
                    f,
                    "{} ({})\ncauses:\n  {causes}\n{snapshot}",
                    seq[0], seq[1]
                )?;
            }
            return Ok(());
        }
        if self.0.contexts.is_empty() {
            write!(f, "{}", self.0.message)?;
        } else {
            write!(f, "{}: {}", self.0.contexts.join(": "), self.0.message)?;
        }
        if let Some(hint) = &self.0.hint {
            write!(f, "\nhint: {hint}")?;
        }
        if let Some(span) = &self.0.span
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
        // Rendered causes: the common path renders one layer only.
        // Chain joining belongs to a single owner (`anyhow` `{:#}`
        // over the source chain, matching the production CLI render
        // in `oxdock/src/main.rs`): every wrapper keeps its child as
        // a chained source with a bare message, so `{:#}` prints each
        // layer exactly once. Walking sources here as well would
        // print every cause twice (once inline, once joined).
        Ok(())
    }
}

impl std::error::Error for Diagnostic {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0
            .source
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
    fn diagnostic_stays_pointer_sized() {
        assert_eq!(
            std::mem::size_of::<Diagnostic>(),
            std::mem::size_of::<usize>(),
            "Diagnostic must stay one pointer wide so DiagnosticResult keeps the anyhow footprint"
        );
    }

    #[test]
    fn equality_ignores_source_chain() {
        let a = Diagnostic::new("k", "m".to_string(), Diagnostic::make_fields(&[]));
        let b = a.clone().with_source(Arc::new(io::Error::other("chain")));
        assert_eq!(a, b);
    }

    #[test]
    fn contexts_prefix_display_without_touching_message() {
        let d = Diagnostic::new("k", "boom".to_string(), Diagnostic::make_fields(&[]))
            .with_contexts(vec!["step 1".to_string(), "step 2".to_string()]);
        assert_eq!(d.message(), "boom");
        assert_eq!(d.contexts(), &["step 1".to_string(), "step 2".to_string()]);
        assert_eq!(d.to_string(), "step 1: step 2: boom");
        let bare = Diagnostic::new("k", "boom".to_string(), Diagnostic::make_fields(&[]));
        assert_eq!(bare.to_string(), "boom");
        assert_eq!(d, bare, "frames are provenance, excluded like sources");
        let evil = Diagnostic::new("k", "m".to_string(), Diagnostic::make_fields(&[]))
            .with_contexts(vec![format!("bad{}frame", INTERNAL_SENTINEL)]);
        assert!(!evil.contexts()[0].contains(INTERNAL_SENTINEL));
    }

    #[test]
    fn later_outer_frames_prepend_ahead_of_inner_ones() {
        // Unwinding attaches innermost first: the outer caller adds
        // its frames afterward, and they must still render first.
        let d = Diagnostic::new("k", "boom".to_string(), Diagnostic::make_fields(&[]))
            .with_contexts(vec!["step 2".to_string()])
            .with_contexts(vec!["step 1".to_string()]);
        assert_eq!(d.contexts(), &["step 1".to_string(), "step 2".to_string()]);
        assert_eq!(d.to_string(), "step 1: step 2: boom");
    }

    #[test]
    fn snapshot_renders_failure_then_section_without_flattening_key() {
        let snapshot = "filesystem snapshot: pending";
        let d = Diagnostic::new("k", "boom".to_string(), Diagnostic::make_fields(&[]))
            .with_contexts(vec!["step 1".to_string()])
            .with_snapshot(snapshot);
        assert_eq!(d.message(), "boom");
        assert_eq!(d.snapshot(), Some(snapshot));
        assert_eq!(d.to_string(), format!("step 1 (boom)\n{snapshot}"));
        let bare = Diagnostic::new("k", "boom".to_string(), Diagnostic::make_fields(&[]));
        assert_eq!(bare.to_string(), "boom");
        assert_eq!(
            d, bare,
            "snapshot section is provenance, excluded like sources"
        );
    }

    #[test]
    fn snapshot_renders_three_node_chain_with_causes_block() {
        let snapshot = "filesystem snapshot: pending";
        let d = Diagnostic::new("k", "boom".to_string(), Diagnostic::make_fields(&[]))
            .with_contexts(vec!["step 1".to_string()])
            .with_source(Arc::new(io::Error::other("root cause")))
            .with_snapshot(snapshot);
        assert_eq!(d.message(), "boom");
        assert_eq!(d.snapshot(), Some(snapshot));
        assert_eq!(
            d.to_string(),
            format!("step 1 (boom)\ncauses:\n  root cause\n{snapshot}")
        );
    }

    #[test]
    fn result_alias_carries_ok_and_err() {
        let ok: DiagnosticResult<u32> = Ok(7);
        let got = match ok {
            Ok(got) => got,
            Err(_) => panic!("expected ok"),
        };
        assert_eq!(got, 7);
        let diag = Diagnostic::new(
            "test.alias",
            "boom".to_string(),
            Diagnostic::make_fields(&[]),
        );
        let err: DiagnosticResult<u32> = Err(diag.clone());
        let got = match err {
            Ok(_) => panic!("expected err"),
            Err(got) => got,
        };
        assert_eq!(got, diag);
    }

    #[test]
    fn anyhow_blanket_from_preserves_message_and_chain() {
        let io_err = Arc::new(io::Error::other("missing file"));
        let diag = Diagnostic::new(
            "test.interop",
            "failed to read script".to_string(),
            Diagnostic::make_fields(&[("path", "x.ox")]),
        )
        .with_source(io_err);
        let anyhow_err: anyhow::Error = diag.into();
        assert!(anyhow_err.to_string().contains("failed to read script"));
        assert!(format!("{anyhow_err:#}").contains("missing file"));
    }

    #[test]
    fn question_mark_converts_into_anyhow_result() {
        fn fallible() -> DiagnosticResult<u32> {
            Err(Diagnostic::new(
                "test.qmark",
                "boom".to_string(),
                Diagnostic::make_fields(&[]),
            ))
        }
        fn caller() -> anyhow::Result<u32> {
            Ok(fallible()? + 1)
        }
        let err = caller().expect_err("must propagate");
        assert!(err.to_string().contains("boom"));
    }
}
