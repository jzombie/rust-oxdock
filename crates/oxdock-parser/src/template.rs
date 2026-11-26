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

use oxdock_diagnostics::DiagnosticResult;

use crate::diagnostics as template_diag;

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
    let head_parsed =
        if let Some((left, right)) = head.split_once(crate::constants::MODULE_SEPARATOR) {
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
            let colon = head
                .find(crate::constants::MODULE_SEPARATOR)
                .expect("split found ::");
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
                module: Some(
                    &head[..head
                        .find(crate::constants::MODULE_SEPARATOR)
                        .expect("split found ::")],
                ),
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

/// Parse one string literal in any spelling: `"""` blocks lower
/// through the shared block lowering, `"..."` and `'...'` unescape
/// through the shared unescaper. Returns `None` when `src` is not a
/// single string literal. Single owner for placeholder string syntax:
/// value and key paths both route here, so a new spelling lands once.
///
/// Closer validation rides [`scan_atoms`]: the input must scan as one
/// quoted text atom spanning all of `src`, and the closer is matched
/// from the opener with escapes honored. Adjacent literals (`"a" "b"`),
/// infix expressions (`"a" + "b"`), and trailing-escape inputs (`"foo\"`)
/// fail closed instead of merging or truncating.
pub fn parse_string_literal(src: &str) -> Option<DiagnosticResult<String>> {
    let atoms = scan_atoms(src);
    let [atom] = atoms.as_slice() else {
        return None;
    };
    if atom.kind != AtomKind::Text || atom.start != 0 || atom.text != src {
        return None;
    }
    if src.starts_with("\"\"\"") {
        return Some(parse_block_literal(src));
    }
    for quote in ['"', '\''] {
        if src.starts_with(quote) {
            return Some(parse_quoted_literal(src, quote));
        }
    }
    None
}

/// Match one `"..."` or `'...'` literal from its opener to its first
/// unescaped closer, which must end the input. `\\` skips the next
/// byte unconditionally, so a trailing escape fails unterminated.
fn parse_quoted_literal(src: &str, quote: char) -> DiagnosticResult<String> {
    let bytes = src.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == quote as u8 {
            if i + 1 != src.len() {
                return Err(template_diag::template_string_trailing_text(src));
            }
            return unescape_literal(&src[1..i], quote);
        }
        i += 1;
    }
    Err(template_diag::template_string_unterminated(src))
}

/// Match one `"""..."""` block from its opener to its first unescaped
/// closer triple, which must end the input. Lowers through the shared
/// block lowering, so placeholder blocks read exactly like statement
/// block strings.
fn parse_block_literal(src: &str) -> DiagnosticResult<String> {
    let bytes = src.as_bytes();
    let mut i = 3;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == b'"' && bytes.get(i + 1) == Some(&b'"') && bytes.get(i + 2) == Some(&b'"') {
            if i + 3 != src.len() {
                return Err(template_diag::template_block_trailing_text(src));
            }
            return Ok(crate::command::lower_block_string(src));
        }
        i += 1;
    }
    Err(template_diag::template_block_unterminated(src))
}

/// Unescape a quoted literal: `\\` and the enclosing quote only.
/// Anything else keeps its backslash so template text stays predictable.
fn unescape_literal(src: &str, quote: char) -> DiagnosticResult<String> {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some(q) if q == quote => out.push(quote),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => {
                return Err(template_diag::template_string_unterminated_escape());
            }
        }
    }
    Ok(out)
}

/// Strip one wrapping delimiter pair, built on [`scan_atoms`] instead
/// of a local quote loop: the opener at 0 must match the closer at the
/// end with no earlier top-level closer. `"""` blocks scan as single
/// text atoms, so brackets inside them never disturb the depth.
pub fn strip_wrapped_delimiters(src: &str, open: char, close: char) -> Option<&str> {
    let atoms = scan_atoms(src);
    let first = atoms.first()?;
    if first.kind != AtomKind::Open || first.start != 0 || first.text.chars().count() != 1 {
        return None;
    }
    if !first.text.starts_with(open) {
        return None;
    }
    let mut depth = 0usize;
    for atom in &atoms {
        match atom.kind {
            AtomKind::Open => depth += 1,
            AtomKind::Close => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if atom.start + atom.text.len() != src.len() {
                        return None;
                    }
                    if !atom.text.starts_with(close) {
                        return None;
                    }
                    return Some(&src[open.len_utf8()..src.len() - close.len_utf8()]);
                }
            }
            AtomKind::Text | AtomKind::Sep(_) => {}
        }
    }
    None
}
/// One variable read inside a `{{ ... }}` key: a `$a.b` path as its
/// dotted parts, or a bare `key` as one literal lookup. Call args
/// contribute their nested `$` refs; env keys contribute nothing.
/// Bare `CONST` shapes (`ConstRef`, `KeyPath`, `Var`) still classify
/// here so phases can name them in the `$`-sigil error; only `$`
/// paths ever resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateVar {
    Path(Vec<String>),
    Bare(String),
    ConstRef { alias: String, name: String },
    KeyPath { base: String, keys: Vec<String> },
    Var(String),
}

/// One `{{ ... }}` key, classified. The single grammar behind every
/// expansion and check phase: parse-time qualification, the static
/// gate, script-engine expansion, and override-map expansion all route
/// through this, so the phases can never disagree on what shape a key
/// is. Payloads stay raw (untrimmed, unsplit): trimming, splitting,
/// lookup sources, and errors belong to the caller, whose contracts
/// differ. Input must arrive trimmed: every producer trims at its
/// boundary (`TemplateSegments` yields trimmed keys; the streaming
/// expander trims on extract).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaceholderKey<'a> {
    /// Empty key: every phase rejects it.
    Empty,
    /// `$` plus the raw rest: split through [`split_var_path`].
    /// Never holds `ALIAS::NAME`: those classify as
    /// [`PlaceholderKey::DollarQualified`].
    Dollar(&'a str),
    /// `$ALIAS::NAME` with optional `.key` tail. Single owner for
    /// the split (see [`parse_dollar_qualified`]), so no call site
    /// restates [`crate::constants::split_qualified`].
    /// The empty-tail shape resolves; a `.key` tail fails with
    /// [`qualified_keypath_error`].
    DollarQualified {
        alias: &'a str,
        name: &'a str,
        keys: Vec<String>,
    },
    /// `env:` plus the raw name.
    Env(&'a str),
    /// `script_env:` plus the raw name.
    ScriptEnv(&'a str),
    /// One `[MODULE::]FUNC(args)` body, through the shared
    /// [`parse_placeholder_call`] grammar.
    Call(PlaceholderCall<'a>),
    /// Bare `ALIAS::NAME` shape. Never resolves: phases reject it
    /// with [`bare_sigil_error`] (use `$ALIAS::NAME`).
    ConstRef { alias: &'a str, name: &'a str },
    /// Bare `NAME.key...` shape (keys validated non-empty, no
    /// spaces or commas; base is a bare identifier). Never resolves:
    /// phases reject it with [`bare_sigil_error`].
    KeyPath { base: &'a str, keys: Vec<String> },
    /// Bare identifier shape (`NAME`). Never resolves: phases reject
    /// it with [`bare_sigil_error`] (use `$NAME`).
    Var(&'a str),
    /// Anything else, verbatim: script-var lookup, step-override
    /// lookup, or a hard error, per the caller's contract.
    Bare(&'a str),
}

/// Bare identifier rule shared by every CONST shape: ASCII
/// alphanumeric plus underscore, non-empty. Dots, commas, spaces,
/// dashes, and sigils never qualify, so override keys with `-` stay
/// `Bare` and CONST shapes classify identically everywhere.
pub fn is_bare_ident(src: &str) -> bool {
    !src.is_empty() && src.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// One `ALIAS::NAME` CONST reference from trimmed source. `None` means
/// not that shape: the caller keeps its own invalid-syntax error.
/// Single owner for qualified-CONST syntax; every phase routes here.
pub fn parse_qualified_const(src: &str) -> Option<(&str, &str)> {
    let (alias, rest) = crate::constants::split_qualified(src)?;
    if !is_bare_ident(alias) || !is_bare_ident(rest) {
        return None;
    }
    if rest.contains(['.', ',', ' ', '\t']) {
        return None;
    }
    Some((alias, rest))
}

/// One `BASE.key...` path from trimmed source. Base and every dot
/// segment are bare identifiers. `None` means not that shape.
/// Single owner for key-path syntax; empty segments (`A.`, `A..b`)
/// and punctuation (`A.foo-bar`, `A.@#$`) never parse here, so
/// static and runtime agree by construction.
pub fn parse_bare_key_path(src: &str) -> Option<(&str, Vec<String>)> {
    let (base, tail) = src.split_once('.')?;
    if !is_bare_ident(base) {
        return None;
    }
    if tail.is_empty() {
        return None;
    }
    let segs: Vec<String> = tail.split('.').map(|s| s.to_string()).collect();
    if segs.iter().any(|s| !is_bare_ident(s)) {
        return None;
    }
    Some((base, segs))
}

/// Error text for a bare word where only `$` reads a binding:
/// command arguments, `{{ }}` keys, and placeholder call args never
/// resolve bare words. Single owner for the wording, so the static
/// gate and runtime expansion agree word for word.
pub fn bare_sigil_error(src: &str) -> String {
    format!(
        "bare `{src}` never reads a binding here; use `{}{src}`",
        crate::constants::VAR_SIGIL
    )
}

/// Error text for a key path on `$ALIAS::NAME`. Qualified reads
/// never traverse: copy to a local binding first. Single owner for
/// the wording, so every phase agrees word for word.
pub fn qualified_keypath_error() -> String {
    "key paths on $ALIAS::NAME are not supported; copy to a local with LET or CONST first"
        .to_string()
}

/// One `$ALIAS::NAME[.key...]` shape from post-`$` rest. Returns the
/// alias, the name, and the dot tail (possibly empty). `None` means
/// not that shape. Single owner for the `$`-qualified split (see
/// [`crate::constants::split_qualified`]), so no call site restates it.
pub fn parse_dollar_qualified(rest: &str) -> Option<(&str, &str, Vec<String>)> {
    let (head, tail) = match rest.split_once('.') {
        Some((h, t)) => (h, Some(t)),
        None => (rest, None),
    };
    let (alias, name) = crate::constants::split_qualified(head)?;
    if !is_bare_ident(alias) || !is_bare_ident(name) {
        return None;
    }
    let keys = match tail {
        Some(t) => t.split('.').map(|s| s.to_string()).collect::<Vec<_>>(),
        None => Vec::new(),
    };
    Some((alias, name, keys))
}

/// Classify one trimmed placeholder key. Shape only, no resolution:
/// runtimes keep their own check order (e.g. overrides-first) and
/// their own trimming, sources, and errors.
pub fn classify_placeholder_key(key: &str) -> PlaceholderKey<'_> {
    if key.is_empty() {
        return PlaceholderKey::Empty;
    }
    if let Some(rest) = key.strip_prefix('$') {
        if let Some((alias, name, keys)) = parse_dollar_qualified(rest) {
            return PlaceholderKey::DollarQualified { alias, name, keys };
        }
        return PlaceholderKey::Dollar(rest);
    }
    if let Some(rest) = key.strip_prefix("env:") {
        return PlaceholderKey::Env(rest);
    }
    if let Some(rest) = key.strip_prefix("script_env:") {
        return PlaceholderKey::ScriptEnv(rest);
    }
    if let Some(call) = parse_placeholder_call(key) {
        return PlaceholderKey::Call(call);
    }
    if let Some((alias, name)) = parse_qualified_const(key) {
        return PlaceholderKey::ConstRef { alias, name };
    }
    if let Some((base, keys)) = parse_bare_key_path(key) {
        return PlaceholderKey::KeyPath { base, keys };
    }
    if is_bare_ident(key) {
        return PlaceholderKey::Var(key);
    }
    PlaceholderKey::Bare(key)
}

/// One `$a.b` path split: on `.` with each part trimmed, exactly like
/// runtime expansion. No validity judgment: callers decide what empty
/// parts mean under their own contract.
pub fn split_var_path(rest: &str) -> Vec<String> {
    rest.split('.')
        .map(|part| part.trim().to_string())
        .collect()
}

/// Variable reads inside one placeholder key, routed through the
/// single [`classify_placeholder_key`] owner: `$` paths parse through
/// the single path parser below, env keys contribute nothing, call
/// shapes contribute their nested arg refs, and bare keys stay
/// literal.
/// Call args recurse through the shared literal owners
/// (`parse_string_literal`, `strip_wrapped_delimiters`,
/// `split_top_level`), so new arg spellings land once, here.
pub fn placeholder_vars(key: &str) -> Vec<TemplateVar> {
    match classify_placeholder_key(key.trim()) {
        PlaceholderKey::Empty => Vec::new(),
        PlaceholderKey::Dollar(rest) => parse_dollar_path(rest).into_iter().collect(),
        PlaceholderKey::DollarQualified { .. } => Vec::new(),
        PlaceholderKey::Env(_) | PlaceholderKey::ScriptEnv(_) => Vec::new(),
        PlaceholderKey::Call(call) => {
            let mut out = Vec::new();
            for arg in split_placeholder_args(call.args_raw) {
                collect_call_arg_vars(&arg, &mut out);
            }
            out
        }
        PlaceholderKey::ConstRef { alias, name } => vec![TemplateVar::ConstRef {
            alias: alias.to_string(),
            name: name.to_string(),
        }],
        PlaceholderKey::KeyPath { base, keys } => vec![TemplateVar::KeyPath {
            base: base.to_string(),
            keys,
        }],
        PlaceholderKey::Var(name) => vec![TemplateVar::Var(name.to_string())],
        PlaceholderKey::Bare(key) => vec![TemplateVar::Bare(key.to_string())],
    }
}

/// One `$a.b` path from its post-`$` rest. `None` means malformed:
/// the caller owns that error. Single owner for `$` path syntax.
pub fn parse_var_path(rest: &str) -> Option<Vec<String>> {
    let parts = split_var_path(rest);
    if parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    Some(parts)
}

/// Maximum nesting for map and list literals in placeholder call
/// args. Single depth budget shared by static inference and runtime
/// resolution.
pub const MAX_TEMPLATE_ARG_DEPTH: usize = 16;

/// One placeholder call argument's shape. Single owner for the
/// argument grammar: every path classifies here, then acts on the
/// result with its own types and errors.
pub enum TemplateArg<'a> {
    /// `$` variable path: raw rest after `$` (validate with
    /// [`parse_var_path`]). Never holds `ALIAS::NAME`: those classify
    /// as [`TemplateArg::DollarQualified`].
    Dollar(&'a str),
    /// `$ALIAS::NAME` with optional `.key` tail: the empty tail
    /// resolves, a `.key` tail fails with [`qualified_keypath_error`].
    /// Single owner for the split (see [`parse_dollar_qualified`]),
    /// so no call site restates [`crate::constants::split_qualified`].
    DollarQualified {
        alias: &'a str,
        name: &'a str,
        keys: Vec<String>,
    },
    /// String literal source in any spelling (parse with
    /// [`parse_string_literal`]).
    Str(&'a str),
    /// `{...}` map inner source.
    Map(&'a str),
    /// `[...]` list inner source.
    List(&'a str),
    /// Bare scalar source (`true`, `false`, numbers; see
    /// [`parse_scalar_literal`]).
    Scalar(&'a str),
    /// Bare `ALIAS::NAME` shape. Never resolves: phases reject it
    /// with [`bare_sigil_error`].
    ConstRef { alias: &'a str, name: &'a str },
    /// Bare `NAME.key...` path (validated by [`parse_bare_key_path`]).
    /// Never resolves: phases reject it with [`bare_sigil_error`].
    KeyPath { base: &'a str, keys: Vec<String> },
    /// Bare `NAME` identifier. Never resolves: phases reject it with
    /// [`bare_sigil_error`] (use `$NAME`).
    Var(&'a str),
    /// Anything else: the caller owns the invalid-syntax error.
    Invalid(&'a str),
}

/// Classify one call argument by shape. Inputs arrive trimmed from
/// [`split_placeholder_args`].
pub fn classify_template_arg(src: &str) -> TemplateArg<'_> {
    let src = src.trim();
    if let Some(rest) = src.strip_prefix('$') {
        if let Some((alias, name, keys)) = parse_dollar_qualified(rest) {
            return TemplateArg::DollarQualified { alias, name, keys };
        }
        return TemplateArg::Dollar(rest);
    }
    if parse_string_literal(src).is_some() {
        return TemplateArg::Str(src);
    }
    if let Some(inner) = strip_wrapped_delimiters(src, '{', '}') {
        return TemplateArg::Map(inner);
    }
    if let Some(inner) = strip_wrapped_delimiters(src, '[', ']') {
        return TemplateArg::List(inner);
    }
    if parse_scalar_literal(src).is_some() {
        return TemplateArg::Scalar(src);
    }
    if let Some((alias, name)) = parse_qualified_const(src) {
        return TemplateArg::ConstRef { alias, name };
    }
    if let Some((base, keys)) = parse_bare_key_path(src) {
        return TemplateArg::KeyPath { base, keys };
    }
    if is_bare_ident(src) {
        return TemplateArg::Var(src);
    }
    TemplateArg::Invalid(src)
}

/// One bare scalar literal's value shape. Shared by static inference
/// (tags) and runtime resolution (values), which map the same shapes
/// instead of restating the cascade.
pub enum ScalarLit {
    Bool(bool),
    Int(i64),
    Float(f64),
}

/// Parse one bare scalar literal: `true`/`false`, then int, then
/// float. `None` means not scalar-shaped at all.
pub fn parse_scalar_literal(src: &str) -> Option<ScalarLit> {
    match src {
        "true" => Some(ScalarLit::Bool(true)),
        "false" => Some(ScalarLit::Bool(false)),
        _ => src
            .parse::<i64>()
            .map(ScalarLit::Int)
            .ok()
            .or_else(|| src.parse::<f64>().map(ScalarLit::Float).ok()),
    }
}

/// One `{key: value}` entry's key and value sources, split once on a
/// top-level `:`. `None` when the entry is not exactly one pair.
pub fn split_map_entry(entry: &str) -> Option<(String, String)> {
    let mut parts = split_top_level(entry, ':');
    if parts.len() != 2 {
        return None;
    }
    let value = parts.pop().expect("split_map_entry checked len");
    let key = parts.pop().expect("split_map_entry checked len");
    Some((key, value))
}

/// One map literal key: a string literal in any spelling or a bare
/// ASCII identifier. Shared by static inference and runtime
/// resolution, which report identical diagnostics.
pub fn parse_map_key(src: &str) -> DiagnosticResult<String> {
    if let Some(literal) = parse_string_literal(src) {
        return literal;
    }
    if !src.is_empty() && src.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Ok(src.to_string());
    }
    Err(template_diag::template_map_key_invalid(src))
}

fn parse_dollar_path(rest: &str) -> Option<TemplateVar> {
    parse_var_path(rest).map(TemplateVar::Path)
}

/// Nested `$` refs inside one call arg, through the shared argument
/// classifier: strings, scalars, and invalid shapes hold none; maps
/// and lists recurse into their entries; malformed entries are
/// runtime-owned invalid syntax and contribute nothing here.
fn collect_call_arg_vars(src: &str, out: &mut Vec<TemplateVar>) {
    match classify_template_arg(src) {
        TemplateArg::Dollar(rest) => {
            out.extend(parse_dollar_path(rest));
        }
        TemplateArg::Map(inner) => {
            if inner.trim().is_empty() {
                return;
            }
            for entry in split_top_level(inner, ',') {
                if let Some((_, value)) = split_map_entry(&entry) {
                    collect_call_arg_vars(&value, out);
                }
            }
        }
        TemplateArg::List(inner) => {
            if inner.trim().is_empty() {
                return;
            }
            for item in split_top_level(inner, ',') {
                collect_call_arg_vars(&item, out);
            }
        }
        TemplateArg::ConstRef { alias, name } => {
            out.push(TemplateVar::ConstRef {
                alias: alias.to_string(),
                name: name.to_string(),
            });
        }
        TemplateArg::KeyPath { base, keys } => {
            out.push(TemplateVar::KeyPath {
                base: base.to_string(),
                keys,
            });
        }
        TemplateArg::Var(name) => {
            out.push(TemplateVar::Var(name.to_string()));
        }
        TemplateArg::DollarQualified { .. } => {}
        TemplateArg::Str(_) | TemplateArg::Scalar(_) | TemplateArg::Invalid(_) => {}
    }
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
    fn placeholder_vars_cover_keys_calls_and_bad_paths() {
        use super::{TemplateVar, placeholder_vars};
        assert_eq!(
            placeholder_vars("$x"),
            vec![TemplateVar::Path(vec!["x".to_string()])],
        );
        assert_eq!(
            placeholder_vars(" $a.b.c "),
            vec![TemplateVar::Path(vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
            ])],
        );
        assert_eq!(
            placeholder_vars("key"),
            vec![TemplateVar::Var("key".to_string())],
        );
        assert!(placeholder_vars("env:FOO").is_empty());
        assert!(placeholder_vars("script_env:BAR").is_empty());
        assert!(placeholder_vars("").is_empty());
        assert_eq!(
            placeholder_vars("M::F($a, {k: [$b, \"ignored\"]}, 'ignored', \"\"\"ignored\"\"\")"),
            vec![
                TemplateVar::Path(vec!["a".to_string()]),
                TemplateVar::Path(vec!["b".to_string()]),
            ],
        );
        assert!(placeholder_vars("$").is_empty());
        assert!(placeholder_vars("$a.").is_empty());
        assert!(placeholder_vars("$.a").is_empty());
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

    #[test]
    fn string_literal_spellings_agree() {
        use super::parse_string_literal;
        let plain = parse_string_literal("\"Release installs\"")
            .expect("quoted")
            .expect("ok");
        let single = parse_string_literal("'Release installs'")
            .expect("single")
            .expect("ok");
        let block = parse_string_literal("\"\"\"Release installs\"\"\"")
            .expect("block")
            .expect("ok");
        assert_eq!(plain, "Release installs");
        assert_eq!(single, plain);
        assert_eq!(block, plain);
    }

    #[test]
    fn string_literal_rejects_merged_and_unterminated() {
        use super::parse_string_literal;
        // Adjacent literals and infix expressions scan as several atoms:
        // not one literal, so the caller rejects them.
        assert!(parse_string_literal("\"a\" \"b\"").is_none());
        assert!(parse_string_literal("\"a\" + \"b\"").is_none());
        assert!(parse_string_literal("\"\"\"a\"\"\" \"\"\"b\"\"\"").is_none());
        // Trailing escapes and missing closers fail closed.
        assert!(parse_string_literal("\"foo\\\"").expect("quoted").is_err());
        assert!(parse_string_literal("'foo\\'").expect("single").is_err());
        assert!(
            parse_string_literal("\"\"\"foo\\")
                .expect("block escape")
                .is_err()
        );
        assert!(
            parse_string_literal("\"\"\"foo\"\"")
                .expect("block short")
                .is_err()
        );
        assert!(parse_string_literal("\"foo").expect("open").is_err());
        assert!(
            parse_string_literal("\"\"\"foo")
                .expect("open block")
                .is_err()
        );
    }

    #[test]
    fn wrapped_delimiters_see_through_block_strings() {
        use super::strip_wrapped_delimiters;
        assert_eq!(
            strip_wrapped_delimiters("[\"\"\"a]b\"\"\"]", '[', ']'),
            Some("\"\"\"a]b\"\"\"")
        );
        assert_eq!(
            strip_wrapped_delimiters("{k: \"\"\"a}b\"\"\"}", '{', '}'),
            Some("k: \"\"\"a}b\"\"\"")
        );
        assert_eq!(strip_wrapped_delimiters("[a, b]", '[', ']'), Some("a, b"));
        assert!(strip_wrapped_delimiters("[a], [b]", '[', ']').is_none());
        assert!(strip_wrapped_delimiters("[a", '[', ']').is_none());
    }
}
