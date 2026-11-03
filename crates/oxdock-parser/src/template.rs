//! Template placeholder scanning shared by expansion and static
//! checks: one implementation of the `{{ ... }}` rules, so the two
//! can never disagree on what counts as a read.
//!
//! Rules (mirroring the historical `expand_string` loop one for one):
//! - `\{{` is literal text, never an opener. `\\` is a literal
//!   backslash and never escapes what follows it.
//! - A lone backslash escapes nothing for scanning: only the
//!   backslash is literal, the next character reprocesses normally.
//! - `{{` opens only when a matching `}}` follows; otherwise the rest
//!   is literal text, exactly as expansion renders it verbatim.
//! - Placeholder keys arrive trimmed; closers never nest.
//! - Byte-oriented and allocation-free: braces and the backslash are
//!   ASCII, so no multibyte character can match, and every cut lands
//!   on an ASCII boundary.

/// One raw segment of template text: literal bytes, or one unescaped
/// `{{ ... }}` with its trimmed key. Escapes are NOT processed here;
/// consumers apply them to [`TemplateSegment::Text`] themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateSegment<'a> {
    /// Literal text up to the next placeholder (may hold backslashes
    /// and unclosed `{{`, verbatim).
    Text(&'a str),
    /// An unescaped placeholder; `key` is the trimmed interior.
    Placeholder { key: &'a str },
}

/// Allocation-free segment iterator over template text. See the module
/// docs for the exact rules; the unit tests below pin every escape
/// and boundary shape.
#[derive(Debug, Clone)]
pub struct TemplateSegments<'a> {
    text: &'a str,
    pos: usize,
    pending: Option<(usize, &'a str)>,
}

impl<'a> TemplateSegments<'a> {
    /// Scan `text` front to back.
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            pos: 0,
            pending: None,
        }
    }
}

impl<'a> Iterator for TemplateSegments<'a> {
    type Item = TemplateSegment<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((end, key)) = self.pending.take() {
            self.pos = end;
            return Some(TemplateSegment::Placeholder { key });
        }
        // Copy out: every slice below then ties to `'a`, never to the
        // `&mut self` borrow, so staging `pending` never conflicts.
        let text: &'a str = self.text;
        let bytes = text.as_bytes();
        let len = bytes.len();
        if self.pos >= len {
            return None;
        }
        let start = self.pos;
        let mut i = start;
        while i < len {
            if bytes[i] == b'\\' {
                // `\\` consumes both (the opener after it stays live);
                // `\{{` consumes all three as literal text; anything
                // else consumes only the backslash and reprocesses.
                if i + 2 < len && bytes[i + 1] == b'{' && bytes[i + 2] == b'{' {
                    i += 3;
                } else if i + 1 < len && bytes[i + 1] == b'\\' {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            if bytes[i] == b'{' && i + 1 < len && bytes[i + 1] == b'{' {
                let mut j = i + 2;
                while j + 1 < len && !(bytes[j] == b'}' && bytes[j + 1] == b'}') {
                    j += 1;
                }
                if j + 1 >= len {
                    break;
                }
                let key = text[i + 2..j].trim();
                if i > start {
                    self.pending = Some((j + 2, key));
                    return Some(TemplateSegment::Text(&text[start..i]));
                }
                self.pos = j + 2;
                return Some(TemplateSegment::Placeholder { key });
            }
            i += 1;
        }
        self.pos = len;
        Some(TemplateSegment::Text(&text[start..]))
    }
}

#[cfg(test)]
mod tests {
    use super::{TemplateSegment, TemplateSegments};

    fn segments(text: &str) -> Vec<TemplateSegment<'_>> {
        TemplateSegments::new(text).collect()
    }

    #[test]
    fn plain_text_is_one_segment() {
        assert_eq!(segments("hello"), vec![TemplateSegment::Text("hello")],);
    }

    #[test]
    fn placeholder_splits_around_text() {
        assert_eq!(
            segments("a{{ $x }}b"),
            vec![
                TemplateSegment::Text("a"),
                TemplateSegment::Placeholder { key: "$x" },
                TemplateSegment::Text("b"),
            ],
        );
    }

    #[test]
    fn keys_trim() {
        assert_eq!(
            segments("{{  env:A  }}"),
            vec![TemplateSegment::Placeholder { key: "env:A" }],
        );
    }

    #[test]
    fn escaped_opener_stays_text() {
        assert_eq!(
            segments("\\{{ env:A }}"),
            vec![TemplateSegment::Text("\\{{ env:A }}")],
        );
    }

    #[test]
    fn escaped_backslash_keeps_opener_live() {
        assert_eq!(
            segments("\\\\{{ env:A }}"),
            vec![
                TemplateSegment::Text("\\\\"),
                TemplateSegment::Placeholder { key: "env:A" },
            ],
        );
    }

    #[test]
    fn escaped_opener_before_braces_stays_text() {
        // `\{{{x}}`: the `\{{` consumes as literal, exactly like the
        // historical expansion loop; no placeholder opens inside it.
        assert_eq!(
            segments("\\{{{x}}"),
            vec![TemplateSegment::Text("\\{{{x}}")],
        );
    }

    #[test]
    fn unclosed_opener_stays_text() {
        assert_eq!(
            segments("hi {{ unclosed"),
            vec![TemplateSegment::Text("hi {{ unclosed")],
        );
    }

    #[test]
    fn empty_and_blank_keys_are_placeholders() {
        assert_eq!(
            segments("{{}}"),
            vec![TemplateSegment::Placeholder { key: "" }],
        );
        assert_eq!(
            segments("{{ }}"),
            vec![TemplateSegment::Placeholder { key: "" }],
        );
    }

    #[test]
    fn adjacent_placeholders_emit_no_empty_text() {
        assert_eq!(
            segments("{{a}}{{b}}"),
            vec![
                TemplateSegment::Placeholder { key: "a" },
                TemplateSegment::Placeholder { key: "b" },
            ],
        );
    }
}
