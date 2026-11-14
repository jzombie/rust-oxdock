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
/// `{{ ... }}` with its trimmed key plus the verbatim raw slice.
/// Escapes are NOT processed here; consumers apply them to
/// [`TemplateSegment::Text`] themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateSegment<'a> {
    /// Literal text up to the next placeholder (may hold backslashes
    /// and unclosed `{{`, verbatim).
    Text(&'a str),
    /// An unescaped placeholder; `key` is the trimmed interior and
    /// `raw` is the full `{{ ... }}` slice verbatim (braces plus the
    /// original inner spacing), so rewriters can splice the key
    /// without touching surrounding bytes.
    Placeholder { key: &'a str, raw: &'a str },
}

/// One parsed `{{ ... }}` call body: `[MODULE::]FUNC(args_raw)`.
///
/// Single grammar behind every phase: parse-time qualification,
/// static checking, and runtime dispatch all read the same shape,
/// so the three can never disagree on what counts as a call.
/// `module` is `None` for bare `FUNC(...)` heads (statement-scope
/// resolution decides those); `name_span` is the byte range of
/// `name` inside the input `key`, so rewriters can splice exactly
/// the head without touching surrounding spacing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceholderCall<'a> {
    /// `MODULE` in `MODULE::FUNC(...)`, or `None` for bare heads.
    pub module: Option<&'a str>,
    /// Function head (`FUNC`).
    pub name: &'a str,
    /// Byte range of `name` inside the input `key`.
    pub name_span: std::ops::Range<usize>,
    /// Raw bytes between the parens (untrimmed, unsplit).
    pub args_raw: &'a str,
}

fn is_module_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn is_bare_head(name: &str) -> bool {
    let mut chars = name.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_uppercase()) {
        return false;
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Parse one placeholder key as `[MODULE::]FUNC(args)`.
///
/// Returns `None` when the key is not exactly one call: missing
/// parens, lowercase bare heads, bad `MODULE`/`FUNC` spellings, an
/// unbalanced paren group, or trailing bytes past the closer
/// (compounds like `LEN($a) + LEN($b)` stay untouched). The paren
/// scan tracks both quote styles with backslash escapes, so a paren
/// inside `'a(b'` or `"a(b"` never affects depth; whether the arg
/// grammar accepts that style is the caller's decision.
pub fn parse_placeholder_call(key: &str) -> Option<PlaceholderCall<'_>> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return None;
    }
    let base = key.find(trimmed).unwrap_or(0);
    let paren = trimmed.find('(')?;
    let (head_raw, rest) = trimmed.split_at(paren);
    let head = head_raw.trim_end();
    if head.is_empty() {
        return None;
    }
    enum Head<'a> {
        Bare(&'a str),
        Qualified { name: &'a str, start: usize },
    }
    let head_parsed = if let Some((left, right)) = head.split_once("::") {
        if left.is_empty() || !left.chars().all(is_module_char) {
            return None;
        }
        let name = right.trim();
        if name.is_empty() || !name.chars().all(is_module_char) {
            return None;
        }
        // Qualified heads stay permissive here (runtime spellings
        // allow `alnum _ -`); resolution validates case later, so an
        // unknown-case head still fails naming the culprit.
        // `head` opens `trimmed`, so the name offset inside `head`
        // is its offset inside `trimmed` too.
        let colon = head.find("::").expect("split found ::");
        let leading = head[colon + 2..].len() - head[colon + 2..].trim_start().len();
        Head::Qualified {
            name,
            start: colon + 2 + leading,
        }
    } else {
        if !is_bare_head(head) {
            return None;
        }
        Head::Bare(head)
    };
    // The shared atom scan drives depth: quoted runs (including
    // `"""` blocks) arrive whole as Text, so parens inside strings
    // never count. Only structural opens and closes move depth.
    let mut depth: usize = 0;
    let mut close_at: Option<usize> = None;
    for atom in scan_atoms(rest) {
        match atom.kind {
            // Only parens move depth here, exactly as before: quoted
            // runs arrive whole as Text, and other brackets never
            // counted. Unbalanced cross-kind input fails closed below.
            AtomKind::Open if atom.text == "(" => depth += 1,
            AtomKind::Close if atom.text == ")" => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    close_at = Some(atom.start);
                    break;
                }
            }
            _ => {}
        }
    }
    let close_at = close_at?;
    if !rest[close_at + 1..].trim().is_empty() {
        return None;
    }
    let args_raw = &rest[1..close_at];
    match head_parsed {
        Head::Bare(name) => {
            let start = base + trimmed.find(name).unwrap_or(0);
            Some(PlaceholderCall {
                module: None,
                name,
                name_span: start..start + name.len(),
                args_raw,
            })
        }
        Head::Qualified { name, start } => {
            let abs = base + start;
            Some(PlaceholderCall {
                module: Some(&head[..head.find("::").expect("split found ::")]),
                name,
                name_span: abs..abs + name.len(),
                args_raw,
            })
        }
    }
}

/// One lexical atom of placeholder arg text. Bracket depth tracking
/// and top-level splitting share this single scan, so quote rules —
/// including `"""` blocks — can never disagree between the splitter
/// and the paren matcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomKind {
    /// One of `([{`.
    Open,
    /// One of `)]}`.
    Close,
    /// A separator candidate (`,` or `:`); consumers decide which splits.
    Sep(char),
    /// Anything else: a maximal run that is either one quoted string
    /// (`".."`, `'..'`, or a whole `""".."""` block with backslash
    /// escapes honored) or unquoted text up to the next structural
    /// character. Commas and brackets inside a quoted run never surface.
    Text,
}

/// A scanned atom with its byte offset in the scanned source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Atom<'a> {
    pub kind: AtomKind,
    pub text: &'a str,
    pub start: usize,
}

/// Scan placeholder arg text into atoms. Unterminated quotes run to
/// end of input as text; downstream value resolution fails those
/// closed, exactly as before.
pub fn scan_atoms(src: &str) -> Vec<Atom<'_>> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut text_from: Option<usize> = None;
    while i < bytes.len() {
        let b = bytes[i];
        // A `"""` opener consumes through its unescaped closer.
        if b == b'"' && bytes.get(i + 1) == Some(&b'"') && bytes.get(i + 2) == Some(&b'"') {
            if let Some(s) = text_from.take()
                && s != i
            {
                out.push(Atom {
                    kind: AtomKind::Text,
                    text: &src[s..i],
                    start: s,
                });
            }
            let start = i;
            i += 3;
            let mut escaped = false;
            while i < bytes.len() {
                if escaped {
                    escaped = false;
                    i += 1;
                } else if bytes[i] == b'\\' {
                    escaped = true;
                    i += 1;
                } else if bytes[i] == b'"'
                    && bytes.get(i + 1) == Some(&b'"')
                    && bytes.get(i + 2) == Some(&b'"')
                {
                    i += 3;
                    break;
                } else {
                    i += 1;
                }
            }
            out.push(Atom {
                kind: AtomKind::Text,
                text: &src[start..i],
                start,
            });
            continue;
        }
        // A single-quote opener consumes through its unescaped closer.
        if b == b'"' || b == b'\'' {
            if let Some(s) = text_from.take()
                && s != i
            {
                out.push(Atom {
                    kind: AtomKind::Text,
                    text: &src[s..i],
                    start: s,
                });
            }
            let q = b;
            let start = i;
            i += 1;
            let mut escaped = false;
            while i < bytes.len() {
                if escaped {
                    escaped = false;
                } else if bytes[i] == b'\\' {
                    escaped = true;
                } else if bytes[i] == q {
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(Atom {
                kind: AtomKind::Text,
                text: &src[start..i],
                start,
            });
            continue;
        }
        let kind = match b {
            b'(' | b'[' | b'{' => Some(AtomKind::Open),
            b')' | b']' | b'}' => Some(AtomKind::Close),
            b',' | b':' => Some(AtomKind::Sep(b as char)),
            _ => None,
        };
        if let Some(kind) = kind {
            if let Some(s) = text_from.take()
                && s != i
            {
                out.push(Atom {
                    kind: AtomKind::Text,
                    text: &src[s..i],
                    start: s,
                });
            }
            out.push(Atom {
                kind,
                text: &src[i..i + 1],
                start: i,
            });
            i += 1;
        } else {
            if text_from.is_none() {
                text_from = Some(i);
            }
            i += 1;
        }
    }
    if let Some(s) = text_from.take()
        && s < bytes.len()
    {
        out.push(Atom {
            kind: AtomKind::Text,
            text: &src[s..],
            start: s,
        });
    }
    out
}

/// Split on a top-level separator, keeping quoted strings, `"""`
/// blocks, and bracket groups intact. Shared by comma arg lists and
/// map-entry `key: value` pairs; only the separator differs.
fn split_on(src: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth: usize = 0;
    for atom in scan_atoms(src) {
        match atom.kind {
            AtomKind::Open => {
                depth += 1;
                current.push_str(atom.text);
            }
            AtomKind::Close => {
                depth = depth.saturating_sub(1);
                current.push_str(atom.text);
            }
            AtomKind::Sep(c) if c == sep && depth == 0 => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push_str(atom.text),
        }
    }
    let last = current.trim();
    if !(out.is_empty() && last.is_empty()) {
        out.push(last.to_string());
    }
    out
}

/// Split call args on top-level commas, keeping quoted strings,
/// `"""` blocks, and bracket groups intact.
pub fn split_placeholder_args(src: &str) -> Vec<String> {
    split_on(src, ',')
}

/// Split on a top-level separator character. Only `,` and `:` are
/// tokenized by [`scan_atoms`]; anything else is a programming error
/// and fails fast. (A plain assert: the workspace bans `cfg`-based
/// macros outside the `sys` crates, so no `debug_assert!` here.)
pub fn split_top_level(src: &str, sep: char) -> Vec<String> {
    assert!(
        matches!(sep, ',' | ':'),
        "scan_atoms only tokenizes ',' and ':' separators"
    );
    split_on(src, sep)
}
/// Allocation-free segment iterator over template text. See the module
/// docs for the exact rules; the unit tests below pin every escape
/// and boundary shape.
#[derive(Debug, Clone)]
pub struct TemplateSegments<'a> {
    text: &'a str,
    pos: usize,
    pending: Option<(usize, &'a str, &'a str)>,
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
        if let Some((end, key, raw)) = self.pending.take() {
            self.pos = end;
            return Some(TemplateSegment::Placeholder { key, raw });
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
                let raw = &text[i..j + 2];
                if i > start {
                    self.pending = Some((j + 2, key, raw));
                    return Some(TemplateSegment::Text(&text[start..i]));
                }
                self.pos = j + 2;
                return Some(TemplateSegment::Placeholder { key, raw });
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
                TemplateSegment::Placeholder {
                    key: "$x",
                    raw: "{{ $x }}"
                },
                TemplateSegment::Text("b"),
            ],
        );
    }

    #[test]
    fn keys_trim() {
        assert_eq!(
            segments("{{  env:A  }}"),
            vec![TemplateSegment::Placeholder {
                key: "env:A",
                raw: "{{  env:A  }}"
            }],
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
                TemplateSegment::Placeholder {
                    key: "env:A",
                    raw: "{{ env:A }}"
                },
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
            vec![TemplateSegment::Placeholder {
                key: "",
                raw: "{{}}"
            }],
        );
        assert_eq!(
            segments("{{ }}"),
            vec![TemplateSegment::Placeholder {
                key: "",
                raw: "{{ }}"
            }],
        );
    }

    #[test]
    fn adjacent_placeholders_emit_no_empty_text() {
        assert_eq!(
            segments("{{a}}{{b}}"),
            vec![
                TemplateSegment::Placeholder {
                    key: "a",
                    raw: "{{a}}"
                },
                TemplateSegment::Placeholder {
                    key: "b",
                    raw: "{{b}}"
                },
            ],
        );
    }

    #[test]
    fn call_shapes_parse_once() {
        use super::{parse_placeholder_call, split_placeholder_args};
        // Bare head: module-less, name span covers the head.
        let call = parse_placeholder_call("LEN($x)").expect("bare parses");
        assert_eq!(call.module, None);
        assert_eq!(call.name, "LEN");
        assert_eq!(&"LEN($x)"[call.name_span.clone()], "LEN");
        assert_eq!(call.args_raw, "$x");
        // Qualified head with gap before parens.
        let call = parse_placeholder_call("STD::LEN ($x)").expect("qualified parses");
        assert_eq!(call.module, Some("STD"));
        assert_eq!(call.name, "LEN");
        assert_eq!(call.args_raw, "$x");
        // Quoted parens never affect depth, both styles.
        assert!(parse_placeholder_call("LEN(\"a(b\")").is_some());
        assert!(parse_placeholder_call("LEN('a(b')").is_some());
        assert!(parse_placeholder_call("STD::LEN('a(b')").is_some());
        // Compounds and trailing garbage are not single calls.
        assert!(parse_placeholder_call("LEN($a) + LEN($b)").is_none());
        assert!(parse_placeholder_call("LEN($x))").is_none());
        // Lowercase bare heads keep non-call handling.
        assert!(parse_placeholder_call("foo($x)").is_none());
        // Empty and missing parens miss.
        assert!(parse_placeholder_call("$x").is_none());
        assert!(parse_placeholder_call("").is_none());
        // Args split shields commas in both quote styles.
        assert_eq!(
            split_placeholder_args("$a, \"b,c\", 'd,e'"),
            vec!["$a".to_string(), "\"b,c\"".to_string(), "'d,e'".to_string()],
        );
    }

    #[test]
    fn triple_quoted_blocks_split_and_parse_whole() {
        use super::{parse_placeholder_call, split_placeholder_args};
        // Commas, parens, and brackets inside `"""` never split depth.
        assert_eq!(
            split_placeholder_args("\"\"\"a, b (c) [d]\"\"\", $x"),
            vec!["\"\"\"a, b (c) [d]\"\"\"".to_string(), "$x".to_string()],
        );
        // A block arg spanning lines survives as one arg.
        assert_eq!(
            split_placeholder_args("\"\"\"line one\nline two\"\"\""),
            vec!["\"\"\"line one\nline two\"\"\"".to_string()],
        );
        // Parens inside blocks never affect the call paren match.
        let call = parse_placeholder_call("M::F(\"\"\"a(b\"\"\", $x)").expect("block parens parse");
        assert_eq!(call.name, "F");
        assert_eq!(call.args_raw, "\"\"\"a(b\"\"\", $x");
    }
}
