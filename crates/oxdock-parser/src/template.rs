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
    let bytes = rest.as_bytes();
    let mut depth: usize = 0;
    let mut quote: u8 = 0;
    let mut escaped = false;
    let mut close_at: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if quote != 0 {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == quote {
                quote = 0;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' => quote = b,
            b'(' => depth += 1,
            b')' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    close_at = Some(i);
                    break;
                }
            }
            _ => {}
        }
        i += 1;
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

/// Split call args on top-level commas, keeping quoted strings and
/// `{...}` / `[...]` / `(...)` literals intact. Both quote styles
/// shield commas (with backslash escapes); depth applies to all
/// three bracket shapes so nested literals never split.
pub fn split_placeholder_args(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: u8 = 0;
    let mut escaped = false;
    let mut depth: usize = 0;
    for c in src.chars() {
        if quote != 0 {
            current.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == quote as char {
                quote = 0;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = c as u8;
                current.push(c);
            }
            '{' | '[' | '(' => {
                depth += 1;
                current.push(c);
            }
            '}' | ']' | ')' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ',' if depth == 0 => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(c),
        }
    }
    let last = current.trim();
    if !(out.is_empty() && last.is_empty()) {
        out.push(last.to_string());
    }
    out
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
}
