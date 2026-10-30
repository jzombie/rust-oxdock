//! Shared token model for the unified parser backend.
//!
//! Two context-free lexers emit [`Tok`]: [`lex_str`] for string sources
//! and [`lex_tokens`] (behind `proc-macro-api`) for `TokenStream`.
//! Both are positional only: they record gap adjacency (`joint`) and
//! literal newlines, but never decide statement boundaries. Whether a
//! newline ends a statement (or a `[` opens a guard vs. args, or a word
//! continues a wrapper) is grammar state, so it lives in the parser
//! backend, never here.
//!
//! String semantics are single-sourced in [`lex_string_spelling`]: the
//! `TokenStream` frontend fuses span-adjacent shredded literals back to
//! their source spelling (`""` + `"hi"` + `""` is `"""hi"""`) and runs
//! the same scanner. Contents stay verbatim (escapes expand at runtime
//! `expand_string`, matching `parse_quoted_string`); `"""` bodies lower
//! later through the shared `lower_block_string`.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupDelim {
    Bracket,
    Paren,
    Brace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokKind {
    /// Bare word: commands, keywords, numbers-as-spelled, paths, flags.
    /// Any run of non-delimiter chars; never contains whitespace.
    Word(String),
    /// Verbatim inner of `"..."` / `'...'` (escapes intact for runtime).
    Str(String),
    /// Full `"""..."""` spelling including delimiters; lower with
    /// `lower_block_string` at parse time.
    BlockStr(String),
    /// Single punctuation char (`=`, `$`, `:`, `!`, `<`, `>`, `,`, `|`,
    /// `&`, `@`). Multi-char operators compose in the parser using
    /// `joint`.
    Punct(char),
    /// Delimited group with recursively lexed contents.
    Group { delim: GroupDelim, inner: Vec<Tok> },
    /// Literal newline (`\n`, `\r\n`, `\r` all normalize to one).
    Newline,
    /// Explicit `;` statement terminator.
    Semicolon,
}

/// One token plus the gap signal: `joint` is true when no gap (no
/// whitespace, comment, or newline) precedes it. The parser uses this
/// where the grammar requires adjacency (`::`, `$var`, `==`, `"a""b"`
/// fragment joins, `FOO (` not-a-call); it never affects token identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tok {
    pub kind: TokKind,
    pub joint: bool,
}

impl Tok {
    fn fresh(kind: TokKind, joint: bool) -> Self {
        Self { kind, joint }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub line: usize,
    pub col: usize,
    pub msg: String,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lex error at {}:{}: {}", self.line, self.col, self.msg)
    }
}

impl std::error::Error for LexError {}

/// Chars that always delimit words in string sources. The set mirrors
/// `rustc` tokenization so both frontends emit identical streams:
/// operators and structural chars split in both (`--flag` arrives as
/// `-`, `-`, `flag` with joint flags; the parser backend reassembles
/// payloads losslessly). `.` splits too (`a.b`, `1.0.0`); `~` and
/// backtick stay word chars (`rustc` rejects them, so they are
/// string-pathway-only by construction, like raw-`"` blocks).
fn is_delim(c: char) -> bool {
    matches!(
        c,
        '"' | '\''
            | '$'
            | ':'
            | ','
            | '='
            | '!'
            | '<'
            | '>'
            | '+'
            | '-'
            | '*'
            | '/'
            | '%'
            | '.'
            | '?'
            | '^'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | ';'
            | '#'
            | '|'
            | '&'
            | '@'
    )
}

/// Scan one string literal at the head of `s` (which must start with
/// `"`, `'`, or `"""`), mirroring the `quoted_string` / `block_string`
/// pest rules. Returns the token kind plus bytes consumed.
fn scan_string(s: &str) -> Result<(TokKind, usize), String> {
    if s.starts_with("\"\"\"") {
        let mut end = 3;
        while end < s.len() {
            if s[end..].starts_with('\\') {
                let next_len = s[end + 1..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                if next_len == 0 {
                    return Err("trailing backslash in block string".to_string());
                }
                end += 1 + next_len;
                continue;
            }
            if s[end..].starts_with("\"\"\"") {
                end += 3;
                return Ok((TokKind::BlockStr(s[..end].to_string()), end));
            }
            end += s[end..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        }
        return Err("unterminated block string".to_string());
    }
    let quote = s.as_bytes()[0] as char;
    if quote != '"' && quote != '\'' {
        return Err("string must open with a quote".to_string());
    }
    let mut end = 1;
    while end < s.len() {
        let rest = &s[end..];
        let c = rest.chars().next().unwrap();
        if c == '\\' {
            let mut it = rest.chars();
            it.next();
            match it.next() {
                Some(n) if n == quote => end += 1 + n.len_utf8(),
                _ => end += 1,
            }
            continue;
        }
        if c == quote {
            end += c.len_utf8();
            return Ok((TokKind::Str(s[1..end - 1].to_string()), end));
        }
        end += c.len_utf8();
    }
    Err("unterminated string".to_string())
}

/// Lex one or more adjacent string spellings (the fused `TokenStream`
/// shreds, or a single literal): `""".."""` reads as one
/// [`TokKind::BlockStr`], `"a""b"` as two [`TokKind::Str`]. The first
/// token inherits `joint`; later ones are always joint (no gap inside a
/// spelling).
fn lex_string_spelling(s: &str, joint: bool) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let mut rest = s;
    let mut first = true;
    while !rest.is_empty() {
        let (kind, used) = scan_string(rest)?;
        out.push(Tok::fresh(kind, if first { joint } else { true }));
        first = false;
        rest = &rest[used..];
    }
    if out.is_empty() {
        return Err("empty string spelling".to_string());
    }
    Ok(out)
}

/// A `'` opens a string where a gap precedes it (`= 'hi'`, `['a']`);
/// inside a payload word (`coverage's`) it stays a literal char. The
/// top-level lexer tracks this via its gap flag; group scanning uses
/// the raw text before the quote for the same decision.
fn quote_opens_before(src: &str) -> bool {
    src.chars()
        .rev()
        .find(|c| !c.is_whitespace())
        .map(|c| {
            matches!(
                c,
                '[' | '(' | '{' | ',' | ':' | '=' | '$' | '!' | '<' | '>' | '|' | '&'
            )
        })
        .unwrap_or(true)
}

struct StrLexer<'a> {
    src: &'a str,
    pos: usize,
    line: usize,
    col: usize,
    out: Vec<Tok>,
    /// True when a gap (space, tab, comment, newline, `;`) precedes the
    /// next token. Reset after each emitted token.
    gapped: bool,
}

impl<'a> StrLexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            line: 1,
            col: 1,
            out: Vec::new(),
            gapped: true,
        }
    }

    fn err(&self, msg: impl Into<String>) -> LexError {
        LexError {
            line: self.line,
            col: self.col,
            msg: msg.into(),
        }
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn bump(&mut self, n: usize, newlines: usize) {
        self.pos += n;
        if newlines > 0 {
            self.line += newlines;
            self.col = 1;
        } else {
            self.col += n;
        }
    }

    fn push(&mut self, kind: TokKind) {
        let joint = !self.gapped;
        self.gapped = false;
        self.out.push(Tok::fresh(kind, joint));
    }

    /// True when only whitespace precedes on this line (comment position).
    fn at_line_start(&self) -> bool {
        let line_start = self.src[..self.pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
        self.src[line_start..self.pos]
            .chars()
            .all(|c| c == ' ' || c == '\t' || c == '\r')
    }

    fn lex_word(&mut self) {
        let mut word = String::new();
        while self.pos < self.src.len() {
            let rest = self.rest();
            let c = rest.chars().next().unwrap();
            if c == '\\' {
                let mut it = rest.chars();
                it.next();
                match it.next() {
                    Some(n) => {
                        word.push('\\');
                        word.push(n);
                        self.bump(1 + n.len_utf8(), 0);
                    }
                    None => {
                        word.push('\\');
                        self.bump(1, 0);
                    }
                }
                continue;
            }
            if c.is_whitespace() || is_delim(c) {
                break;
            }
            word.push(c);
            self.bump(c.len_utf8(), 0);
        }
        self.push(TokKind::Word(word));
    }

    /// Scan a delimited group, copying its raw inner text (with `"..."`
    /// and `'...'` regions protected so brackets inside strings never
    /// affect nesting) and re-lexing it recursively.
    fn lex_group(&mut self, close: char, delim: GroupDelim) -> Result<(), LexError> {
        let (start_line, start_col) = (self.line, self.col);
        self.bump(1, 0);
        let mut stack: Vec<char> = Vec::new();
        let mut inner_src = String::new();
        while self.pos < self.src.len() {
            let rest = self.rest();
            let c = rest.chars().next().unwrap();
            if c == '"' || (c == '\'' && quote_opens_before(&inner_src)) {
                let (kind, used) = scan_string(rest).map_err(|m| self.err(m))?;
                match kind {
                    TokKind::Str(t) => {
                        inner_src.push('"');
                        inner_src.push_str(&t);
                        inner_src.push('"');
                    }
                    TokKind::BlockStr(full) => inner_src.push_str(&full),
                    _ => unreachable!(),
                }
                let newlines = rest[..used].matches('\n').count();
                self.bump(used, newlines);
                continue;
            }
            if c == '[' || c == '(' || c == '{' {
                stack.push(c);
            } else if c == ']' || c == ')' || c == '}' {
                if c == close && stack.is_empty() {
                    self.bump(c.len_utf8(), 0);
                    let inner = lex_str_inner(&inner_src).map_err(|e| LexError {
                        line: start_line,
                        col: start_col,
                        msg: e.msg,
                    })?;
                    self.push(TokKind::Group { delim, inner });
                    return Ok(());
                }
                if stack.pop().is_none() {
                    return Err(self.err(format!("unmatched `{c}`")));
                }
            }
            if c == '\r' && rest.starts_with("\r\n") {
                inner_src.push('\n');
                self.bump(2, 1);
                continue;
            }
            if c == '\n' || c == '\r' {
                inner_src.push('\n');
                self.bump(c.len_utf8(), 1);
                continue;
            }
            inner_src.push(c);
            self.bump(c.len_utf8(), 0);
        }
        Err(LexError {
            line: start_line,
            col: start_col,
            msg: format!("unclosed group, expected `{close}`"),
        })
    }

    fn skip_line_comment(&mut self) {
        while self.pos < self.src.len() {
            let c = self.rest().chars().next().unwrap();
            if c == '\n' || c == '\r' {
                break;
            }
            self.bump(c.len_utf8(), 0);
        }
    }

    fn skip_block_comment(&mut self) -> Result<(), LexError> {
        self.bump(2, 0);
        let mut depth = 1usize;
        while self.pos < self.src.len() {
            let rest = self.rest();
            if rest.starts_with("/*") {
                depth += 1;
                self.bump(2, 0);
            } else if rest.starts_with("*/") {
                depth -= 1;
                self.bump(2, 0);
                if depth == 0 {
                    self.gapped = true;
                    return Ok(());
                }
            } else {
                let c = rest.chars().next().unwrap();
                if c == '\r' && rest.starts_with("\r\n") {
                    self.bump(2, 1);
                } else if c == '\n' || c == '\r' {
                    self.bump(c.len_utf8(), 1);
                } else {
                    self.bump(c.len_utf8(), 0);
                }
            }
        }
        Err(self.err("unclosed block comment"))
    }

    fn run(&mut self) -> Result<(), LexError> {
        while self.pos < self.src.len() {
            let rest = self.rest();
            let c = rest.chars().next().unwrap();
            match c {
                ' ' | '\t' => {
                    self.bump(c.len_utf8(), 0);
                    self.gapped = true;
                }
                '\r' if rest.starts_with("\r\n") => {
                    self.bump(2, 1);
                    self.out.push(Tok::fresh(TokKind::Newline, false));
                    self.gapped = true;
                }
                '\n' | '\r' => {
                    self.bump(c.len_utf8(), 1);
                    self.out.push(Tok::fresh(TokKind::Newline, false));
                    self.gapped = true;
                }
                ';' => {
                    self.bump(1, 0);
                    self.out.push(Tok::fresh(TokKind::Semicolon, false));
                    self.gapped = true;
                }
                // Comments only count at line start (statement level):
                // inside payloads `//` and `#` stay ordinary text.
                '/' if rest.starts_with("//") && self.at_line_start() => {
                    self.skip_line_comment();
                    self.gapped = true;
                }
                '/' if rest.starts_with("/*") && self.at_line_start() => {
                    self.skip_block_comment()?;
                }
                '#' if self.at_line_start() => {
                    self.skip_line_comment();
                    self.gapped = true;
                }
                '"' => {
                    let joint = !self.gapped;
                    let (kind, used) = scan_string(rest).map_err(|m| self.err(m))?;
                    let newlines = rest[..used].matches('\n').count();
                    self.bump(used, newlines);
                    self.gapped = false;
                    self.out.push(Tok::fresh(kind, joint));
                }
                '\'' if self.gapped => {
                    let joint = !self.gapped;
                    match scan_string(rest) {
                        Ok((kind, used)) => {
                            self.bump(used, 0);
                            self.gapped = false;
                            self.out.push(Tok::fresh(kind, joint));
                        }
                        // Lone `'` (lifetimes, apostrophes): matches the
                        // `TokenStream` shape (`'` punct + word) instead
                        // of failing where `rustc` would not.
                        Err(_) => {
                            self.bump(1, 0);
                            self.push(TokKind::Punct('\''));
                        }
                    }
                }
                '[' => self.lex_group(']', GroupDelim::Bracket)?,
                '(' => self.lex_group(')', GroupDelim::Paren)?,
                '{' => self.lex_group('}', GroupDelim::Brace)?,
                ']' | ')' | '}' => {
                    return Err(self.err(format!("unmatched `{c}`")));
                }
                _ if is_delim(c) => {
                    self.bump(c.len_utf8(), 0);
                    self.push(TokKind::Punct(c));
                }
                _ => self.lex_word(),
            }
        }
        Ok(())
    }
}

fn lex_str_inner(s: &str) -> Result<Vec<Tok>, LexError> {
    let mut lx = StrLexer::new(s);
    lx.run()?;
    Ok(lx.out)
}

/// Lex a string source into context-free tokens. Newlines normalize to
/// [`TokKind::Newline`] (one per line break); statement-level `//`,
/// `/* */`, and whole-line `#` comments vanish (the newline itself
/// still emits).
pub fn lex_str(s: &str) -> Result<Vec<Tok>, LexError> {
    lex_str_inner(s)
}

/// Lex a `TokenStream` into the same context-free tokens. Newlines come
/// from span positions only (one per line advanced, inside groups too);
/// no syntax is consulted. Span-adjacent string shreds fuse back to
/// source spelling before [`lex_string_spelling`]; degenerate
/// (zero-width, e.g. `quote!`) spans carry no adjacency signal, so
/// literals there take the spaced reading.
#[cfg(feature = "proc-macro-api")]
pub fn lex_tokens(ts: &proc_macro2::TokenStream) -> Result<Vec<Tok>, LexError> {
    use proc_macro2::{Delimiter, Spacing, TokenTree};

    struct Walker {
        out: Vec<Tok>,
        gapped: bool,
        prev_end: Option<proc_macro2::LineColumn>,
        prev_joint: bool,
        /// Fused raw spellings of an adjacent string-literal run plus
        /// the joint flag of its first piece.
        pending_str: Option<(String, bool)>,
    }

    impl Walker {
        fn new() -> Self {
            Self {
                out: Vec::new(),
                gapped: true,
                prev_end: None,
                prev_joint: false,
                pending_str: None,
            }
        }

        /// Positional only: newlines from line advances, adjacency from
        /// span equality (or joint spacing). Degenerate spans reset to
        /// gapped: they carry no position signal.
        fn note_position(&mut self, start: proc_macro2::LineColumn, end: proc_macro2::LineColumn) {
            if start == end {
                self.gapped = true;
            } else {
                match self.prev_end {
                    None => self.gapped = true,
                    Some(prev) => {
                        if start.line > prev.line {
                            for _ in 0..(start.line - prev.line) {
                                self.out.push(Tok::fresh(TokKind::Newline, false));
                            }
                            self.gapped = true;
                        } else {
                            self.gapped = !(prev == start || self.prev_joint);
                        }
                    }
                }
            }
            self.prev_end = Some(end);
            self.prev_joint = false;
        }

        fn flush_str(&mut self) -> Result<(), LexError> {
            if let Some((raw, joint)) = self.pending_str.take() {
                let toks = lex_string_spelling(&raw, joint).map_err(|m| LexError {
                    line: 0,
                    col: 0,
                    msg: m,
                })?;
                self.out.extend(toks);
            }
            Ok(())
        }

        fn push(&mut self, kind: TokKind) {
            let joint = !self.gapped;
            self.gapped = false;
            self.out.push(Tok::fresh(kind, joint));
        }

        fn walk_stream(&mut self, ts: &proc_macro2::TokenStream) -> Result<(), LexError> {
            let trees: Vec<TokenTree> = ts.clone().into_iter().collect();
            self.walk_trees(&trees)
        }

        fn walk_trees(&mut self, trees: &[TokenTree]) -> Result<(), LexError> {
            for tt in trees {
                match tt {
                    TokenTree::Group(g) => {
                        let (start, end) = (g.span().start(), g.span().end());
                        self.flush_str()?;
                        self.note_position(start, end);
                        let mut inner = Walker::new();
                        inner.walk_stream(&g.stream())?;
                        inner.flush_str()?;
                        match g.delimiter() {
                            Delimiter::Parenthesis => self.push(TokKind::Group {
                                delim: GroupDelim::Paren,
                                inner: inner.out,
                            }),
                            Delimiter::Bracket => self.push(TokKind::Group {
                                delim: GroupDelim::Bracket,
                                inner: inner.out,
                            }),
                            Delimiter::Brace => self.push(TokKind::Group {
                                delim: GroupDelim::Brace,
                                inner: inner.out,
                            }),
                            // Transparent: splice contents through.
                            Delimiter::None => {
                                for mut t in inner.out {
                                    if matches!(t.kind, TokKind::Newline | TokKind::Semicolon) {
                                        self.out.push(t);
                                        self.gapped = true;
                                    } else {
                                        t.joint = false;
                                        self.out.push(t);
                                        self.gapped = false;
                                    }
                                }
                            }
                        }
                    }
                    TokenTree::Ident(ident) => {
                        let (start, end) = (ident.span().start(), ident.span().end());
                        self.flush_str()?;
                        self.note_position(start, end);
                        self.push(TokKind::Word(ident.to_string()));
                    }
                    TokenTree::Literal(lit) => {
                        let (start, end) = (lit.span().start(), lit.span().end());
                        let spelling = lit.to_string();
                        let stringy = spelling.starts_with('"') || spelling.starts_with('\'');
                        if stringy
                            && start != end
                            && self.prev_end.map(|p| p == start).unwrap_or(false)
                        {
                            // Shredded `"""` run (or `"a""b"`): fuse raw.
                            let raw = match self.pending_str.take() {
                                Some((mut raw, joint)) => {
                                    raw.push_str(&spelling);
                                    (raw, joint)
                                }
                                None => (spelling, false),
                            };
                            self.pending_str = Some(raw);
                            self.prev_end = Some(end);
                            self.prev_joint = false;
                            continue;
                        }
                        self.flush_str()?;
                        self.note_position(start, end);
                        if stringy {
                            let joint = !self.gapped;
                            self.pending_str = Some((spelling, joint));
                            self.gapped = false;
                        } else {
                            self.push(TokKind::Word(spelling));
                        }
                    }
                    TokenTree::Punct(p) => {
                        let (start, end) = (p.span().start(), p.span().end());
                        let ch = p.as_char();
                        self.flush_str()?;
                        self.note_position(start, end);
                        if ch == ';' {
                            self.out.push(Tok::fresh(TokKind::Semicolon, false));
                            self.gapped = true;
                        } else {
                            self.push(TokKind::Punct(ch));
                        }
                        if p.spacing() == Spacing::Joint {
                            self.prev_joint = true;
                            self.gapped = false;
                        }
                    }
                }
            }
            Ok(())
        }
    }

    let mut w = Walker::new();
    w.walk_stream(ts)?;
    w.flush_str()?;
    Ok(w.out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Tok {
        Tok::fresh(TokKind::Word(s.to_string()), false)
    }

    fn wj(s: &str) -> Tok {
        Tok::fresh(TokKind::Word(s.to_string()), true)
    }

    fn punct(c: char) -> Tok {
        Tok::fresh(TokKind::Punct(c), false)
    }

    fn punctj(c: char) -> Tok {
        Tok::fresh(TokKind::Punct(c), true)
    }

    fn str_tok(s: &str) -> Tok {
        Tok::fresh(TokKind::Str(s.to_string()), false)
    }

    fn block_tok(s: &str) -> Tok {
        Tok::fresh(TokKind::BlockStr(s.to_string()), false)
    }

    fn nl() -> Tok {
        Tok::fresh(TokKind::Newline, false)
    }

    fn semi() -> Tok {
        Tok::fresh(TokKind::Semicolon, false)
    }

    /// Both frontends must emit identical streams for the same source.
    /// `TokenStream` parsing normalizes `\r\n` to `\n`, so inputs here
    /// use `\n` only.
    #[cfg(feature = "proc-macro-api")]
    fn assert_both(text: &str, expected: Vec<Tok>) {
        let from_str = lex_str(text).expect("string lex must succeed");
        assert_eq!(from_str, expected, "string lexer mismatch for {text:?}");
        let ts: proc_macro2::TokenStream = text.parse().expect("must tokenize");
        let from_tokens = lex_tokens(&ts).expect("token lex must succeed");
        assert_eq!(
            from_tokens, expected,
            "token lexer mismatch for {text:?}\nstring: {from_str:?}"
        );
    }

    #[test]
    fn lex_simple_command() {
        let toks = lex_str("ECHO hi").unwrap();
        assert_eq!(toks, vec![w("ECHO"), w("hi")]);
    }

    #[test]
    fn lex_newlines_and_semicolons() {
        let toks = lex_str("ECHO hi\nWRITE x").unwrap();
        assert_eq!(toks, vec![w("ECHO"), w("hi"), nl(), w("WRITE"), w("x")]);
        let toks = lex_str("ECHO hi; WRITE x").unwrap();
        assert_eq!(toks, vec![w("ECHO"), w("hi"), semi(), w("WRITE"), w("x")]);
    }

    #[test]
    fn lex_operators_are_positional() {
        let toks = lex_str("LET $x: INT = $a == 1").unwrap();
        assert_eq!(
            toks,
            vec![
                w("LET"),
                punct('$'),
                wj("x"),
                punctj(':'),
                w("INT"),
                punct('='),
                punct('$'),
                wj("a"),
                punct('='),
                punctj('='),
                w("1"),
            ]
        );
    }

    #[test]
    fn lex_strings_verbatim() {
        let toks = lex_str("ECHO \"a\\nb\"").unwrap();
        assert_eq!(toks, vec![w("ECHO"), str_tok("a\\nb")]);
        let toks = lex_str("ECHO 'hi'").unwrap();
        assert_eq!(toks, vec![w("ECHO"), str_tok("hi")]);
    }

    #[test]
    fn lex_block_string_atomic() {
        let toks = lex_str("ECHO \"\"\"hi\"\"\"").unwrap();
        assert_eq!(toks, vec![w("ECHO"), block_tok("\"\"\"hi\"\"\"")]);
    }

    #[test]
    fn lex_groups_nest() {
        let toks = lex_str("[env:X] ECHO hi").unwrap();
        assert_eq!(
            toks,
            vec![
                Tok::fresh(
                    TokKind::Group {
                        delim: GroupDelim::Bracket,
                        inner: vec![w("env"), punctj(':'), wj("X")],
                    },
                    false,
                ),
                w("ECHO"),
                w("hi"),
            ]
        );
    }

    #[test]
    fn lex_comments_vanish_but_newline_stays() {
        let toks = lex_str("// hello\nECHO hi").unwrap();
        assert_eq!(toks, vec![nl(), w("ECHO"), w("hi")]);
        // Mid-line `/* */` is payload text, not a comment: comments are
        // statement-level (line start) only, matching the grammar.
        let toks = lex_str("ECHO hi /* mid */").unwrap();
        assert_eq!(
            toks,
            vec![
                w("ECHO"),
                w("hi"),
                punct('/'),
                punctj('*'),
                w("mid"),
                punct('*'),
                punctj('/'),
            ]
        );
    }

    #[test]
    fn lex_splits_where_rustc_splits() {
        // Operators split in both frontends (joint flags keep payloads
        // reassemblable in the parser); only `10s`-style alphanumerics
        // stay whole.
        let toks = lex_str("RUN echo --flag a/b 10s 1.0.0").unwrap();
        assert_eq!(
            toks,
            vec![
                w("RUN"),
                w("echo"),
                punct('-'),
                punctj('-'),
                wj("flag"),
                w("a"),
                punctj('/'),
                wj("b"),
                w("10s"),
                w("1"),
                punctj('.'),
                wj("0"),
                punctj('.'),
                wj("0"),
            ]
        );
    }

    #[test]
    fn lex_crlf_normalizes() {
        let toks = lex_str("ECHO hi\r\nWRITE x").unwrap();
        assert_eq!(toks, vec![w("ECHO"), w("hi"), nl(), w("WRITE"), w("x")]);
    }

    #[test]
    fn lex_unclosed_group_errors() {
        assert!(lex_str("ECHO [hi").is_err());
        assert!(lex_str("ECHO \"hi").is_err());
    }

    #[cfg(feature = "proc-macro-api")]
    #[test]
    fn both_frontends_agree_on_commands() {
        assert_both("ECHO hi", vec![w("ECHO"), w("hi")]);
        assert_both(
            "ECHO hi\nWRITE x",
            vec![w("ECHO"), w("hi"), nl(), w("WRITE"), w("x")],
        );
        assert_both(
            "[env:X] ECHO hi",
            vec![
                Tok::fresh(
                    TokKind::Group {
                        delim: GroupDelim::Bracket,
                        inner: vec![w("env"), punctj(':'), wj("X")],
                    },
                    false,
                ),
                w("ECHO"),
                w("hi"),
            ],
        );
    }

    #[cfg(feature = "proc-macro-api")]
    #[test]
    fn both_frontends_agree_on_exprs() {
        assert_both(
            "LET $x: INT = $a + $b",
            vec![
                w("LET"),
                punct('$'),
                wj("x"),
                punctj(':'),
                w("INT"),
                punct('='),
                punct('$'),
                wj("a"),
                punct('+'),
                punct('$'),
                wj("b"),
            ],
        );
    }

    #[cfg(feature = "proc-macro-api")]
    #[test]
    fn both_frontends_fuse_block_strings() {
        assert_both(
            "ECHO \"\"\"hi\"\"\"",
            vec![w("ECHO"), block_tok("\"\"\"hi\"\"\"")],
        );
    }

    #[cfg(feature = "proc-macro-api")]
    #[test]
    fn degenerate_spans_take_spaced_reading() {
        let ts = quote::quote! { ECHO "a" "b" };
        let toks = lex_tokens(&ts).expect("degenerate lex must succeed");
        let strs: Vec<_> = toks
            .iter()
            .filter(|t| matches!(t.kind, TokKind::Str(_)))
            .collect();
        assert_eq!(strs.len(), 2, "synthetic spans must not fuse: {toks:?}");
    }
}
