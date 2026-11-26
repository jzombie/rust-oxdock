//! Static type tags for function metadata and variable bindings.
//!
//! The DSL surface stays text (`LET $x: MAP`, `DESCRIBE`), but everything
//! the engine reasons about is a [`TypeTag`]: builtins are enum variants
//! a typo cannot spell, host handles carry their descriptor, and shaped
//! maps carry their field table. Strings survive only at display and
//! parse boundaries through [`TypeTag::name`] and [`TypeTag::builtin`].

use anyhow::{Result, bail};

use crate::diagnostics as dsl_diag;
use oxdock_diagnostics::Diagnostic;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::{TypeDescriptor, Value, constants::VAR_SIGIL};

/// Maximum generic nesting for one spelling: syntactic `<` levels
/// and alias hops share this budget at resolution, so neither deep
/// nesting nor long alias chains can wedge the pass. Parser-bounded
/// scripts never approach it; exceeding it bails naming the shape.
pub const MAX_GENERIC_DEPTH: usize = 8;

/// Build a union tag from member variant idents: the spelling
/// derives from the members' own names at construction, so no
/// hand-written string can drift from the set it names. Unknown
/// idents fail at the `TypeTag::` path, so misspelled members never
/// compile. Empty sets are unrepresentable (one or more idents).
/// Runtime-built (shared `Arc` payloads, freed with the last
/// owner): not for `const` or `static` initializers.
#[macro_export]
macro_rules! union_tag {
    ($($member:ident),+ $(,)?) => {
        $crate::TypeTag::union(vec![$($crate::TypeTag::$member),+])
    };
}

/// One named field of a [`TypeTag::Record`] schema.
#[derive(Debug, Clone)]
pub struct Field {
    /// Field key as it appears in the value map. Owned: schema names
    /// arrive from runtime spellings, so interning them would leak
    /// per distinct spelling.
    pub name: String,
    /// Expected type of the field value. Nests freely.
    pub ty: TypeTag,
    /// What the field carries. Hand-registered schemas document every
    /// field; fields synthesized from inline spellings carry none.
    pub docs: &'static str,
    /// Optional fields (`MAP<permit?: PERMIT>`) may be absent: missing
    /// optional keys pass, missing required keys fail, extra keys
    /// always fail. A default (`MAP<retries: INT = 2>`) fills when
    /// absent: defaults satisfy every binding check, and readers
    /// never observe the hole.
    pub optional: bool,
    /// Fill value for absent fields (`None` when the shape carries
    /// none). Materialized where the alias resolves and checked
    /// against the field type there, so ill-typed defaults fail
    /// naming the declaration.
    pub default: Option<Value>,
    /// Alternate CLI spelling (`MAP<h: BOOL ALIAS help>`): accepted on
    /// the command line, canonical key stays `name` in the parsed map.
    /// `None` for ordinary fields and every hand-registered schema.
    pub alias: Option<String>,
}

/// A declarative type for function params, returns, and bindings.
///
/// Builtins are variants, so exhaustiveness is compiler-checked wherever
/// tags are matched. [`TypeTag::Custom`] carries a host descriptor for
/// opaque handles. [`TypeTag::ListOf`], [`TypeTag::Record`],
/// [`TypeTag::MapOf`], and [`TypeTag::Union`] share their payloads
/// through [`Arc`]: cheap clones, no leaks, freed
/// when the last owner drops. Bare `List` and `Map`
/// are runtime words only, never declaration spellings. Unknown
/// lists declare `LIST<ANY>`; unknown maps declare [`TypeTag::MapOf`]
/// with [`TypeTag::Any`] values (`MAP<ANY>`).
#[derive(Clone)]
pub enum TypeTag {
    String,
    Int,
    Float,
    Bool,
    List,
    Map,
    Pipe,
    Handle,
    HandleOf(Arc<TypeTag>),
    Void,
    Duration,
    Path,
    Semaphore,
    Permit,
    /// Every word: for parameters that accept anything by design
    /// (coercion inputs, generic carriers). Boundary and coercion
    /// both accept unconditionally, so the tag never lies.
    Any,
    Custom(&'static TypeDescriptor),
    ListOf(Arc<TypeTag>),
    Record(Arc<[Field]>),
    MapOf(Arc<TypeTag>),
    /// Admission set over existing words (`STRING|PIPE`): values stay
    /// whatever words the members name, so no word reader changes.
    /// The spelling is computed once at construction from the
    /// members and shared; `name()` borrows it with no allocation.
    /// Members are flat, deduplicated, and order-free by
    /// construction, and every comparison is subset-based, so member
    /// order never matters.
    Union(Arc<str>, Arc<[TypeTag]>),
}

/// The `ANY` word for rendered tags: one shared instance so generic
/// spellings like `MAP<ANY>` never mint competing statics.
pub const ANY_TAG: TypeTag = TypeTag::Any;

/// The MAP word for `TypeTag::ListOf` metadata: one shared instance
/// so generated registrations never mint competing statics.
pub const MAP_TAG: TypeTag = TypeTag::Map;

/// The LIST word for `TypeTag` metadata, matching `MAP_TAG`.
pub const LIST_TAG: TypeTag = TypeTag::List;

/// The canonical source spelling of the handle word: declaration
/// sites compare against this instead of a literal, so the spelling
/// lives once beside the tag it names.
pub const HANDLE_TYPE_NAME: &str = "HANDLE";

/// The canonical source spelling of the list word, matching
/// [`HANDLE_TYPE_NAME`]: generic heads compare against this.
pub const LIST_TYPE_NAME: &str = "LIST";

/// The canonical source spelling of the map word, matching
/// [`HANDLE_TYPE_NAME`].
pub const MAP_TYPE_NAME: &str = "MAP";

/// The canonical source spelling of the any word, matching
/// [`HANDLE_TYPE_NAME`].
pub const ANY_TYPE_NAME: &str = "ANY";

/// The canonical source spelling of the string word.
pub const STRING_TYPE_NAME: &str = "STRING";

/// The canonical source spelling of the int word.
pub const INT_TYPE_NAME: &str = "INT";

/// The canonical source spelling of the float word.
pub const FLOAT_TYPE_NAME: &str = "FLOAT";

/// The canonical source spelling of the bool word.
pub const BOOL_TYPE_NAME: &str = "BOOL";

/// The canonical source spelling of the pipe word.
pub const PIPE_TYPE_NAME: &str = "PIPE";

/// The canonical source spelling of the void word.
pub const VOID_TYPE_NAME: &str = "VOID";

/// The canonical source spelling of the duration word.
pub const DURATION_TYPE_NAME: &str = "DURATION";

/// The canonical source spelling of the path word.
pub const PATH_TYPE_NAME: &str = "PATH";

/// The canonical source spelling of the semaphore word.
pub const SEMAPHORE_TYPE_NAME: &str = "SEMAPHORE";

/// The canonical source spelling of the permit word.
pub const PERMIT_TYPE_NAME: &str = "PERMIT";

/// The canonical source spelling of the `ALIAS` field-alias keyword:
/// spelling and shape matching compare against this instead of a
/// literal, so the keyword lives once beside the tags it modifies.
pub const ALIAS_KEYWORD: &str = "ALIAS";
impl std::fmt::Debug for TypeTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeTag::Custom(descriptor) => {
                write!(f, "Custom({})", descriptor.name)
            }
            TypeTag::ListOf(element) => write!(f, "ListOf({element:?})"),
            TypeTag::HandleOf(inner) => write!(f, "HandleOf({inner:?})"),
            TypeTag::MapOf(values) => write!(f, "MapOf({values:?})"),
            TypeTag::Record(fields) => {
                let names: Vec<&str> = fields.iter().map(|field| field.name.as_str()).collect();
                write!(f, "Record({})", names.join(", "))
            }
            // Unions render their stored spelling, like shaped tags
            // render their contents: the spelling names every member,
            // so no member detail is lost. Bare words share the
            // `name()` delegation below, which is itself exhaustive.
            TypeTag::Union(spelling, _) => write!(f, "Union({spelling})"),
            tag => write!(f, "{}", tag.name()),
        }
    }
}

impl std::fmt::Display for TypeTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
impl TypeTag {
    /// Build a union tag from resolved members, computing the
    /// canonical spelling from their structural renders. Single
    /// source for the spelling: the macro and the resolve path
    /// share it, so no hand-written string exists anywhere. Members
    /// pass through as given (no normalization here): the resolve
    /// path normalizes through its own pass before calling.
    pub fn union(members: Vec<TypeTag>) -> TypeTag {
        let spelling: Arc<str> = members
            .iter()
            .map(render_structural)
            .collect::<Vec<_>>()
            .join("|")
            .into();
        TypeTag::Union(spelling, Arc::from(members))
    }

    /// Display name: the builtin label, the custom descriptor name, or
    /// the coarse word kind for shaped compounds. Field detail renders
    /// structurally, never through this string. Borrowed in every
    /// case: constants for words, the stored spelling for unions.
    pub fn name(&self) -> &str {
        match self {
            TypeTag::String => STRING_TYPE_NAME,
            TypeTag::Int => INT_TYPE_NAME,
            TypeTag::Float => FLOAT_TYPE_NAME,
            TypeTag::Bool => BOOL_TYPE_NAME,
            TypeTag::List => LIST_TYPE_NAME,
            TypeTag::Map => MAP_TYPE_NAME,
            TypeTag::Pipe => PIPE_TYPE_NAME,
            TypeTag::Handle => HANDLE_TYPE_NAME,
            TypeTag::HandleOf(_) => HANDLE_TYPE_NAME,
            TypeTag::Void => VOID_TYPE_NAME,
            TypeTag::Duration => DURATION_TYPE_NAME,
            TypeTag::Path => PATH_TYPE_NAME,
            TypeTag::Semaphore => SEMAPHORE_TYPE_NAME,
            TypeTag::Permit => PERMIT_TYPE_NAME,
            TypeTag::Any => ANY_TYPE_NAME,
            // Borrowed from the stored spelling: computed once at
            // construction, shared by every clone.
            TypeTag::Union(spelling, _) => spelling,
            TypeTag::Custom(descriptor) => descriptor.name,
            TypeTag::ListOf(_) => LIST_TYPE_NAME,
            TypeTag::MapOf(_) => MAP_TYPE_NAME,
            TypeTag::Record(_) => MAP_TYPE_NAME,
        }
    }

    /// Resolve a builtin label to its tag. Custom, shaped, and unknown
    /// names return `None`: customs resolve through the run's type
    /// directory, shapes through the schema directory.
    pub fn builtin(name: &str) -> Option<TypeTag> {
        match name {
            STRING_TYPE_NAME => Some(TypeTag::String),
            INT_TYPE_NAME => Some(TypeTag::Int),
            FLOAT_TYPE_NAME => Some(TypeTag::Float),
            BOOL_TYPE_NAME => Some(TypeTag::Bool),
            LIST_TYPE_NAME => Some(TypeTag::List),
            MAP_TYPE_NAME => Some(TypeTag::Map),
            PIPE_TYPE_NAME => Some(TypeTag::Pipe),
            HANDLE_TYPE_NAME => Some(TypeTag::Handle),
            VOID_TYPE_NAME => Some(TypeTag::Void),
            DURATION_TYPE_NAME => Some(TypeTag::Duration),
            PATH_TYPE_NAME => Some(TypeTag::Path),
            SEMAPHORE_TYPE_NAME => Some(TypeTag::Semaphore),
            PERMIT_TYPE_NAME => Some(TypeTag::Permit),
            ANY_TYPE_NAME => Some(TypeTag::Any),
            _ => None,
        }
    }

    /// Field table for [`TypeTag::Record`], `None` otherwise.
    /// Every non-record variant is listed: a new variant fails to
    /// compile here until its fieldlessness is explicit, so record
    /// structure can never silently leak into (or out of) another tag.
    pub fn fields(&self) -> Option<&[Field]> {
        match self {
            TypeTag::Record(fields) => Some(fields),
            TypeTag::String
            | TypeTag::Int
            | TypeTag::Float
            | TypeTag::Bool
            | TypeTag::List
            | TypeTag::Map
            | TypeTag::Pipe
            | TypeTag::Handle
            | TypeTag::HandleOf(_)
            | TypeTag::Void
            | TypeTag::Duration
            | TypeTag::Path
            | TypeTag::Semaphore
            | TypeTag::Permit
            | TypeTag::Any
            | TypeTag::Custom(_)
            | TypeTag::ListOf(_)
            | TypeTag::MapOf(_)
            | TypeTag::Union(..) => None,
        }
    }
}

/// Render one tag structurally: shaped tags name their contents
/// (`LIST<MAP>`, `MAP<name: TYPE, ...>`), so signatures show the
/// generics the extractor enforces instead of the coarse word
/// kind. Single renderer for `DESCRIBE` and oxdock-render alike.
pub fn render_structural(tag: &TypeTag) -> String {
    match tag {
        TypeTag::ListOf(inner) => format!("{LIST_TYPE_NAME}<{}>", render_structural(inner)),
        TypeTag::HandleOf(inner) => format!("{HANDLE_TYPE_NAME}<{}>", render_structural(inner)),
        TypeTag::MapOf(values) => format!("{MAP_TYPE_NAME}<{}>", render_structural(values)),
        TypeTag::Record(fields) => {
            let field_list = fields
                .iter()
                .map(|field| {
                    format!(
                        "{}{}: {}{}{}",
                        field.name,
                        if field.optional { "?" } else { "" },
                        render_structural(&field.ty),
                        field
                            .alias
                            .as_ref()
                            .map(|a| format!(" {ALIAS_KEYWORD} {a}"))
                            .unwrap_or_default(),
                        field
                            .default
                            .as_ref()
                            .map(|d| format!(" = {}", render_default_value(d)))
                            .unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{MAP_TYPE_NAME}<{field_list}>")
        }
        tag => tag.name().to_string(),
    }
}

/// One parsed type spelling: a bare name, a generic application
/// with positional (`LIST<MAP>`) or named (`MAP<name: STRING>`)
/// arguments, or a union (`STRING|PIPE`) of sibling shapes. A named
/// field with a `?` suffix is optional
/// (`MAP<permit?: PERMIT>`). Arity and shape enforce at resolution,
/// never here: `LIST<A, B>` and `STRING<INT>` parse and fail later
/// naming the rule each breaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spelled {
    Named(String),
    Generic {
        name: String,
        args: Vec<SpelledField>,
    },
    Union(Vec<Spelled>),
}

/// One generic argument: a bare type or a named (possibly optional)
/// record field carrying its nested spelling, optional `ALIAS`
/// alternate flag spelling, and optional `= literal` default.
/// Defaults are plain data here and become Values where the
/// alias resolves, so spellings stay total and comparable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpelledField {
    pub name: Option<String>,
    pub optional: bool,
    pub ty: Spelled,
    pub alias: Option<String>,
    pub default: Option<SpelledDefault>,
}

/// One field default literal (`= 2`, `= "hi"`): integers, floats
/// (stored as bits, since floats are not `Eq`), booleans, and
/// double-quoted strings with no escapes. Plain data by design:
/// resolution materializes the Value and checks it against the
/// field type, so an ill-typed default fails naming the alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpelledDefault {
    Str(String),
    Int(i64),
    Float(u64),
    Bool(bool),
}

impl SpelledDefault {
    /// Materialize the literal as a runtime word. Infallible by
    /// construction: parsing validates, resolution converts.
    pub fn to_value(&self) -> Value {
        match self {
            SpelledDefault::Str(s) => Value::string(s.clone()),
            SpelledDefault::Int(n) => Value::int(*n),
            SpelledDefault::Float(bits) => Value::float(f64::from_bits(*bits)),
            SpelledDefault::Bool(b) => Value::bool(*b),
        }
    }

    /// Canonical literal text: integers and booleans verbatim,
    /// floats through `Debug` (shortest round-trip, always a
    /// decimal point), strings double-quoted.
    pub fn render(&self) -> String {
        match self {
            SpelledDefault::Str(s) => format!("\"{s}\""),
            SpelledDefault::Int(n) => format!("{n}"),
            SpelledDefault::Float(bits) => format!("{:?}", f64::from_bits(*bits)),
            SpelledDefault::Bool(b) => format!("{b}"),
        }
    }
}

/// Canonical default literal text from a materialized word:
/// integers and booleans verbatim, floats through `Debug`
/// (shortest round-trip, always a decimal point), strings
/// double-quoted. Must agree with [`SpelledDefault::render`]
/// on every spelling-produced value; the round-trip test pins
/// the agreement. Non-literals cannot arise from spellings;
/// they render Debug rather than failing totality.
pub fn render_default_value(value: &Value) -> String {
    if let Some(n) = value.as_i64() {
        return format!("{n}");
    }
    if let Some(f) = value.as_f64() {
        return format!("{f:?}");
    }
    if let Some(b) = value.as_bool() {
        return format!("{b}");
    }
    if let Some(s) = value.as_str() {
        return format!("\"{s}\"");
    }
    format!("{value:?}")
}

/// Canonical spelling: exactly the structural representation
/// (`MAP<name: STRING, age: INT>`), so stored declarations, intern
/// keys, alias targets, and rendered signatures are one form, never
/// two. Any input spacing parses to the same shape and renders to
/// this form, which keeps every parse pathway in agreement.
/// Malformed input passes through trimmed for the resolver to
/// reject by name.
pub fn canonicalize_spelling(s: &str) -> String {
    match parse_spelling(s) {
        Ok(spelled) => render_spelled(&spelled),
        Err(_) => s.trim().to_string(),
    }
}

/// Render one parsed spelling in canonical form: `LIST<MAP>`,
/// `MAP<name: STRING, age: INT>`, `STRING|PIPE`. Deterministic by
/// construction, so equal shapes share intern entries.
pub fn render_spelled(spelled: &Spelled) -> String {
    match spelled {
        Spelled::Named(name) => name.clone(),
        Spelled::Union(members) => members
            .iter()
            .map(render_spelled)
            .collect::<Vec<_>>()
            .join("|"),
        Spelled::Generic { name, args } => {
            let rendered: Vec<String> = args
                .iter()
                .map(|field| match &field.name {
                    Some(field_name) => format!(
                        "{}{}: {}{}{}",
                        field_name,
                        if field.optional { "?" } else { "" },
                        render_spelled(&field.ty),
                        field
                            .alias
                            .as_ref()
                            .map(|a| format!(" {ALIAS_KEYWORD} {a}"))
                            .unwrap_or_default(),
                        field
                            .default
                            .as_ref()
                            .map(|d| format!(" = {}", d.render()))
                            .unwrap_or_default(),
                    ),
                    None => render_spelled(&field.ty),
                })
                .collect();
            format!("{name}<{}>", rendered.join(", "))
        }
    }
}

/// Parse a spelling into its shape. Hand-rolled descent: the
/// grammar guarantees shape for parser strings, this keeps the
/// public resolution API safe for direct callers. Whitespace strips
/// quote-aware (double-quoted default strings keep their spaces);
/// any other spaced form parses.
pub fn parse_spelling(s: &str) -> Result<Spelled, Diagnostic> {
    let text: String = strip_spelling_whitespace(s);
    let (spelled, rest) = parse_spelled_one(&text, 0)?;
    if !rest.is_empty() {
        return Err(dsl_diag::spelling_trailing_text(
            &dsl_diag::decode_spelling_fragment(rest),
        ));
    }
    Ok(spelled)
}

/// Decode a mid-parse fragment for user output: the transient
/// [`WS_SENTINEL`] boundary becomes a space, so no internal encoding
/// ever reaches a [`Diagnostic`]. Single owner for the decode rule.
fn decode_spelling_fragment(raw: &str) -> String {
    dsl_diag::decode_spelling_fragment(raw)
}

/// Boundary marker surviving whitespace stripping: records where
/// whitespace separated two word characters, so keywords fused by
/// stripping stay distinguishable (`BOOL ALIAS help` keeps its
/// boundaries while `FLAG_ALIAS` never gains one). Never valid in
/// spellings; matched transiently mid-parse only.
pub(crate) const WS_SENTINEL: char = '\u{1f}';

/// Word characters for boundary tracking: ASCII alphanumerics plus
/// underscore, matching field, type, and alias name shapes.
fn is_spelling_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

/// Strip whitespace outside double-quoted default strings: quoted
/// regions keep their bytes verbatim, everything else drops gaps,
/// except a [`WS_SENTINEL`] is left where whitespace separated two
/// word characters so later stages still see the boundary.
/// No escapes: a quote always toggles, so a default string cannot
/// contain one (parse fails loudly instead of misreading).
fn strip_spelling_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut quoted = false;
    let mut pending_space = false;
    let mut prev_word = false;
    for ch in s.chars() {
        if ch == '"' {
            pending_space = false;
            out.push(ch);
            quoted = !quoted;
            prev_word = false;
            continue;
        }
        if !quoted && ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && prev_word && !quoted && is_spelling_word_char(ch) {
            out.push(WS_SENTINEL);
        }
        pending_space = false;
        out.push(ch);
        prev_word = !quoted && is_spelling_word_char(ch);
    }
    out
}

fn parse_spelled_one(text: &str, depth: usize) -> Result<(Spelled, &str), Diagnostic> {
    let (first, mut rest) = parse_spelled_shape(text, depth)?;
    if !rest.starts_with('|') {
        return Ok((first, rest));
    }
    // Union tail: `A|B|C` accumulates sibling shapes at any level, so
    // unions combine inside generic arguments as well as at top
    // level. Members share the depth budget (siblings, not nesting);
    // every piece consumes input, so the loop always terminates.
    let mut members = vec![first];
    while let Some(tail) = rest.strip_prefix('|') {
        let (next, after) = parse_spelled_shape(tail, depth)?;
        members.push(next);
        rest = after;
    }
    Ok((Spelled::Union(members), rest))
}

fn parse_spelled_shape(text: &str, depth: usize) -> Result<(Spelled, &str), Diagnostic> {
    if depth > MAX_GENERIC_DEPTH {
        return Err(dsl_diag::spelling_max_depth(
            MAX_GENERIC_DEPTH,
            &decode_spelling_fragment(text),
        ));
    }
    let mut chars = text.char_indices();
    let Some((_, first)) = chars.next() else {
        return Err(dsl_diag::spelling_expected_type_name());
    };
    if !first.is_ascii_uppercase() {
        return Err(dsl_diag::spelling_expected_uppercase(&first.to_string()));
    }
    let mut end = first.len_utf8();
    for (idx, ch) in chars {
        if ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_' {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    let (name, mut rest) = text.split_at(end);
    let name = name.to_string();
    if !rest.starts_with('<') {
        return Ok((Spelled::Named(name), rest));
    }
    rest = &rest[1..];
    let mut args = Vec::new();
    loop {
        let (field_name, optional, after_name) = parse_spelled_field_name(rest);
        let (arg, after_arg) = parse_spelled_one(after_name, depth + 1)?;
        let (alias, after_alias) = parse_spelled_alias(after_arg)?;
        // Aliases ride named MAP fields only: bare arguments never
        // carry an alternate spelling.
        if alias.is_some() && field_name.is_none() {
            return Err(dsl_diag::alias_needs_field(&decode_spelling_fragment(rest)));
        }
        // Second ALIAS on the same field: the spelling parser owns
        // arity, so decode both names and fail here naming the rule.
        // The token skipper in `parser.rs` never restates this.
        if let Some(second) = peek_second_alias(after_alias) {
            let field = field_name.as_deref().unwrap_or("<bare>");
            let first = alias.as_deref().unwrap_or("<unknown>");
            return Err(dsl_diag::alias_duplicate_on_field(field, first, &second));
        }
        let (default, after_default) = parse_spelled_default(after_alias)?;
        args.push(SpelledField {
            name: field_name,
            optional,
            ty: arg,
            alias,
            default,
        });
        rest = after_default;
        if let Some(tail) = rest.strip_prefix(',') {
            rest = tail;
            continue;
        }
        if let Some(tail) = rest.strip_prefix('>') {
            rest = tail;
            break;
        }
        return Err(dsl_diag::spelling_expected_delimiter(
            &decode_spelling_fragment(rest),
        ));
    }
    Ok((Spelled::Generic { name, args }, rest))
}

/// Peek for a second `ALIAS name` after a first alias: returns the
/// decoded second name when the keyword follows. Single owner for the
/// duplicate-`ALIAS` rule alongside [`parse_spelled_alias`]. Bounded:
/// only the next token up to `,`, `>`, `=`, or a word boundary is
/// reported, never the unparsed remainder.
fn peek_second_alias(after: &str) -> Option<String> {
    let text = after.strip_prefix(WS_SENTINEL).unwrap_or(after);
    let after_keyword = text.strip_prefix(ALIAS_KEYWORD)?;
    let after_keyword = after_keyword
        .strip_prefix(WS_SENTINEL)
        .unwrap_or(after_keyword);
    if after_keyword.is_empty() {
        return Some("<missing>".to_string());
    }
    let mut end = 0usize;
    for (idx, ch) in after_keyword.char_indices() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        let token: String = after_keyword
            .chars()
            .take_while(|c| {
                !matches!(c, ',' | '>' | '=') && *c != WS_SENTINEL && !c.is_whitespace()
            })
            .collect();
        if token.is_empty() {
            return Some("<invalid>".to_string());
        }
        return Some(dsl_diag::decode_spelling_fragment(&token));
    }
    Some(after_keyword[..end].to_string())
}

/// Optional `field:` or `field?:` prefix of one generic argument. A
/// lowercase name followed by an optional `?` and `:` claims the
/// prefix (recording optionality); anything else leaves the text
/// untouched for the bare-argument path.
fn parse_spelled_field_name(text: &str) -> (Option<String>, bool, &str) {
    let mut chars = text.char_indices();
    let Some((_, first)) = chars.next() else {
        return (None, false, text);
    };
    if !first.is_ascii_lowercase() {
        return (None, false, text);
    };
    let mut end = first.len_utf8();
    for (idx, ch) in chars {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    let (candidate, rest) = text.split_at(end);
    let (optional, rest) = match rest.strip_prefix('?') {
        Some(after) => (true, after),
        None => (false, rest),
    };
    match rest.strip_prefix(':') {
        Some(after) => (Some(candidate.to_string()), optional, after),
        None => (None, false, text),
    }
}

/// Optional `ALIAS name` after a field type: one alternate CLI
/// spelling for `TYPE FLAGS` fields. Whitespace stripping leaves a
/// [`WS_SENTINEL`] boundary around a separately-written keyword, so
/// `BOOL ALIAS help` arrives splittable while `FLAG_ALIAS` never
/// does; lowercase alias names keep the same shape as field names.
/// No `ALIAS` means no alternate spelling.
fn parse_spelled_alias(text: &str) -> Result<(Option<String>, &str), Diagnostic> {
    let text = text.strip_prefix(WS_SENTINEL).unwrap_or(text);
    let Some(after_keyword) = text.strip_prefix(ALIAS_KEYWORD) else {
        return Ok((None, text));
    };
    let after_keyword = after_keyword
        .strip_prefix(WS_SENTINEL)
        .unwrap_or(after_keyword);
    if after_keyword.is_empty() {
        return Err(dsl_diag::alias_needs_name());
    }
    let mut chars = after_keyword.char_indices();
    let Some((_, first)) = chars.next() else {
        return Err(dsl_diag::alias_needs_name());
    };
    if !first.is_ascii_lowercase() {
        if !first.is_ascii_alphabetic() {
            return Err(dsl_diag::alias_needs_name());
        }
        return Err(dsl_diag::alias_name_lowercase(
            &first.to_string(),
            &decode_spelling_fragment(after_keyword),
        ));
    }
    let mut end = first.len_utf8();
    for (idx, ch) in chars {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    let (alias, rest) = after_keyword.split_at(end);
    if alias.is_empty() {
        return Err(dsl_diag::alias_needs_name());
    }
    Ok((Some(alias.to_string()), rest))
}

/// Optional `= literal` default after a field type: integers
/// (optional `-`), decimals, `true`/`false`, or a double-quoted
/// string with no escapes. No `=` means no default. Anything else
/// fails naming the position, so malformed defaults never parse as
/// a longer type.
fn parse_spelled_default(text: &str) -> Result<(Option<SpelledDefault>, &str), Diagnostic> {
    let Some(after_eq) = text.strip_prefix('=') else {
        return Ok((None, text));
    };
    let (lit, rest) = parse_spelled_literal(after_eq)?;
    Ok((Some(lit), rest))
}

/// One default literal: the single owner for default spellings, so
/// resolution and rendering agree on what parses. Trailing boundary
/// checks belong to the caller: anything but `,` or `>` after the
/// literal fails there naming the remainder.
fn parse_spelled_literal(text: &str) -> Result<(SpelledDefault, &str), Diagnostic> {
    if let Some(after) = text.strip_prefix('"') {
        match after.find('"') {
            Some(end) => {
                let (body, rest) = after.split_at(end);
                return Ok((SpelledDefault::Str(body.to_string()), &rest[1..]));
            }
            None => {
                return Err(dsl_diag::spelling_unterminated_string(
                    &decode_spelling_fragment(text),
                ));
            }
        }
    }
    if let Some(rest) = text.strip_prefix("true") {
        return Ok((SpelledDefault::Bool(true), rest));
    }
    if let Some(rest) = text.strip_prefix("false") {
        return Ok((SpelledDefault::Bool(false), rest));
    }
    let bytes = text.as_bytes();
    let mut i = 0;
    if bytes.first() == Some(&b'-') {
        i += 1;
    }
    let int_start = i;
    while bytes.get(i).is_some_and(|b| b.is_ascii_digit()) {
        i += 1;
    }
    if i == int_start {
        return Err(dsl_diag::spelling_expected_literal(
            &decode_spelling_fragment(text),
        ));
    }
    let mut is_float = false;
    if bytes.get(i) == Some(&b'.') && bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit()) {
        is_float = true;
        i += 1;
        while bytes.get(i).is_some_and(|b| b.is_ascii_digit()) {
            i += 1;
        }
    }
    let (num, rest) = text.split_at(i);
    if is_float {
        match num.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok((SpelledDefault::Float(f.to_bits()), rest)),
            _ => Err(dsl_diag::spelling_invalid_float(&decode_spelling_fragment(
                num,
            ))),
        }
    } else {
        match num.parse::<i64>() {
            Ok(n) => Ok((SpelledDefault::Int(n), rest)),
            Err(_) => Err(dsl_diag::spelling_invalid_int(&decode_spelling_fragment(
                num,
            ))),
        }
    }
}

/// Process-global intern table for rendered tags: one shared
/// `&'static TypeTag` per distinct canonical spelling. The lock is
/// held only for lookup/insert; leaf resolution happens outside it
/// in the caller.
static RENDERD_TAGS: OnceLock<Mutex<HashMap<String, TypeTag>>> = OnceLock::new();

fn rendered_table() -> &'static Mutex<HashMap<String, TypeTag>> {
    RENDERD_TAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Look up an already-interned spelling. Resolution checks this
/// before building, so identical spellings share one `Arc` payload
/// family (pointer identity holds across clones).
pub fn lookup_rendered(canonical: &str) -> Option<TypeTag> {
    rendered_table()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(canonical)
        .cloned()
}

/// Intern one rendered tag under its canonical spelling: a raced
/// build stores once (the loser drops its duplicate, still bounded
/// by distinct spellings per process). The table owns every entry
/// (reachable, freed with the process like any cache): clones share
/// payloads through `Arc`, so identity holds without leaks.
pub fn intern_rendered(canonical: String, tag: TypeTag) -> TypeTag {
    let mut table = rendered_table()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(hit) = table.get(&canonical) {
        return hit.clone();
    }
    table.insert(canonical, tag.clone());
    tag
}

/// Check a value against a tag, failing naming the expected shape and
/// the offending path. Records reject missing AND extra fields: a
/// schema is a lock, not a hint. Fields with defaults count as
/// satisfied when absent (every binding site fills them before
/// verifying), so the lock holds without restating it per call site.
pub fn check_value(tag: &TypeTag, value: &Value) -> Result<()> {
    check_value_at(tag, value, VAR_SIGIL)
}

/// Shape conformance with a caller-chosen path prefix: the single
/// implementation behind generated parameter extractors, so boundary
/// errors name the function, the parameter, and the nested position
/// (`F() argument `$maps`[1]: expected MAP, got INT`) with no second
/// copy of the shape logic anywhere.
pub fn check_value_at(tag: &TypeTag, value: &Value, path: &str) -> Result<()> {
    match tag {
        TypeTag::Any => Ok(()),
        TypeTag::String if value.as_str().is_some() => Ok(()),
        TypeTag::Int if value.as_i64().is_some() => Ok(()),
        TypeTag::Float if value.as_f64().is_some() => Ok(()),
        TypeTag::Bool if value.as_bool().is_some() => Ok(()),
        TypeTag::Pipe if value.as_pipe_handle().is_some() => Ok(()),
        TypeTag::Handle if value.as_handle().is_some() => Ok(()),
        TypeTag::HandleOf(_) if value.as_handle().is_some() => Ok(()),
        TypeTag::Void => bail!(
            "{path}: expected {VOID_TYPE_NAME}, got {}",
            value.type_name()
        ),
        TypeTag::Duration if value.as_duration().is_some() => Ok(()),
        TypeTag::Path if value.as_path().is_some() => Ok(()),
        TypeTag::Semaphore if value.as_semaphore().is_some() => Ok(()),
        TypeTag::Permit if value.as_permit().is_some() => Ok(()),
        TypeTag::List => match value.as_list() {
            Some(_) => Ok(()),
            None => bail!(
                "{path}: expected {LIST_TYPE_NAME}, got {}",
                value.type_name()
            ),
        },
        TypeTag::Map => match value.as_map() {
            Some(_) => Ok(()),
            None => bail!(
                "{path}: expected {MAP_TYPE_NAME}, got {}",
                value.type_name()
            ),
        },
        TypeTag::ListOf(element) => match value.as_list() {
            Some(items) => {
                for (idx, item) in items.iter().enumerate() {
                    check_value_at(element, item, &format!("{path}[{idx}]"))?;
                }
                Ok(())
            }
            None => bail!(
                "{path}: expected {LIST_TYPE_NAME}, got {}",
                value.type_name()
            ),
        },
        TypeTag::MapOf(values) => match value.as_map() {
            Some(map) => {
                for (key, item) in map.iter() {
                    check_value_at(values, item, &format!("{path}.{key}"))?;
                }
                Ok(())
            }
            None => bail!(
                "{path}: expected {}, got {}",
                render_structural(tag),
                value.type_name()
            ),
        },
        TypeTag::Record(fields) => match value.as_map() {
            Some(map) => {
                for field in fields.iter() {
                    match map.get(field.name.as_str()) {
                        Some(item) => {
                            check_value_at(&field.ty, item, &format!("{path}.{}", field.name))?;
                        }
                        // Optional or defaulted fields may be absent:
                        // every binding site fills defaults before
                        // verifying, so absence here only fails for
                        // required fields with no default.
                        None if field.optional || field.default.is_some() => {}
                        None => bail!(
                            "{path}: missing field '{}'; expected fields: {}",
                            field.name,
                            field_list(fields),
                        ),
                    }
                }
                for key in map.keys() {
                    if !fields.iter().any(|field| field.name == *key) {
                        bail!(
                            "{path}: unknown field '{key}'; expected fields: {}",
                            field_list(fields),
                        );
                    }
                }
                Ok(())
            }
            None => bail!(
                "{path}: expected {MAP_TYPE_NAME}, got {}",
                value.type_name()
            ),
        },
        TypeTag::Custom(descriptor) => {
            if value.type_name() == descriptor.name {
                Ok(())
            } else {
                bail!(
                    "{path}: expected {}, got {}",
                    descriptor.name,
                    value.type_name(),
                )
            }
        }
        // Admission set: any member passing admits the value, so
        // member order never matters. All members failing names the
        // set, never a single member.
        TypeTag::Union(_, members) => {
            if members
                .iter()
                .any(|member| check_value(member, value).is_ok())
            {
                Ok(())
            } else {
                bail!(
                    "{path}: expected {}, got {}",
                    render_structural(tag),
                    value.type_name()
                )
            }
        }
        // Scalar mismatches: every scalar is listed so a new word
        // kind fails to compile here until its mismatch text is
        // explicit. No future variant can silently inherit this bail.
        TypeTag::String
        | TypeTag::Int
        | TypeTag::Float
        | TypeTag::Bool
        | TypeTag::Pipe
        | TypeTag::Handle
        | TypeTag::HandleOf(_)
        | TypeTag::Duration
        | TypeTag::Path
        | TypeTag::Semaphore
        | TypeTag::Permit => bail!("{path}: expected {}, got {}", tag.name(), value.type_name()),
    }
}

fn field_list(fields: &[Field]) -> String {
    fields
        .iter()
        .map(|field| field.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    #[test]
    fn builtin_round_trips_through_names() {
        for name in [
            "STRING",
            "INT",
            "FLOAT",
            "BOOL",
            "LIST",
            "MAP",
            "PIPE",
            "HANDLE",
            "DURATION",
            "PATH",
            "SEMAPHORE",
            "PERMIT",
        ] {
            let tag = TypeTag::builtin(name).expect("builtin");
            assert_eq!(tag.name(), name);
        }
        assert!(TypeTag::builtin("NOPE").is_none());
        assert!(TypeTag::builtin("string").is_none());
    }

    #[test]
    fn scalars_conform_by_discriminant() {
        assert!(check_value(&TypeTag::String, &Value::string("x".to_string())).is_ok());
        assert!(check_value(&TypeTag::Int, &Value::int(1)).is_ok());
        assert!(check_value(&TypeTag::String, &Value::int(1)).is_err());
        // No cross coercion here: 3.0 is FLOAT, not INT.
        assert!(check_value(&TypeTag::Int, &Value::float(3.0)).is_err());
    }

    #[test]
    fn record_rejects_missing_and_extra_fields() {
        let fields: Arc<[Field]> = Arc::from([
            Field {
                name: "a".to_string(),
                ty: TypeTag::String,
                docs: "",
                optional: false,
                default: None,

                alias: None,
            },
            Field {
                name: "b".to_string(),
                ty: TypeTag::Int,
                docs: "",
                optional: false,
                default: None,

                alias: None,
            },
        ]);
        let tag = TypeTag::Record(fields);
        let good = map(&[("a", Value::string("x".to_string())), ("b", Value::int(1))]);
        assert!(check_value(&tag, &good).is_ok());
        let missing = map(&[("a", Value::string("x".to_string()))]);
        let err = check_value(&tag, &missing).expect_err("missing must fail");
        assert!(format!("{err:#}").contains('b'), "names the field: {err:#}");
        let extra = map(&[
            ("a", Value::string("x".to_string())),
            ("b", Value::int(1)),
            ("c", Value::int(2)),
        ]);
        let err = check_value(&tag, &extra).expect_err("extra must fail");
        assert!(format!("{err:#}").contains('c'), "names the field: {err:#}");
        let mistyped = map(&[
            ("a", Value::string("x".to_string())),
            ("b", Value::string("nope".to_string())),
        ]);
        assert!(check_value(&tag, &mistyped).is_err());
    }

    #[test]
    fn record_optional_fields_may_be_absent() {
        // Optional fields validate when present and pass when absent;
        // required fields and extra keys still fail exactly.
        let fields: Arc<[Field]> = Arc::from([
            Field {
                name: "held".to_string(),
                ty: TypeTag::Int,
                docs: "",
                optional: false,
                default: None,

                alias: None,
            },
            Field {
                name: "permit".to_string(),
                ty: TypeTag::String,
                docs: "",
                optional: true,
                default: None,

                alias: None,
            },
        ]);
        let tag = TypeTag::Record(fields);
        let full = map(&[
            ("held", Value::int(1)),
            ("permit", Value::string("p".to_string())),
        ]);
        assert!(check_value(&tag, &full).is_ok());
        let missing_optional = map(&[("held", Value::int(0))]);
        assert!(check_value(&tag, &missing_optional).is_ok());
        let missing_required = map(&[("permit", Value::string("p".to_string()))]);
        let err = check_value(&tag, &missing_required).expect_err("missing required must fail");
        assert!(
            format!("{err:#}").contains("held"),
            "names the field: {err:#}"
        );
        let mistyped_optional = map(&[("held", Value::int(1)), ("permit", Value::int(2))]);
        assert!(check_value(&tag, &mistyped_optional).is_err());
    }

    #[test]
    fn list_of_checks_elements_with_index() {
        let tag = TypeTag::ListOf(Arc::new(TypeTag::Int));
        let good = Value::list(vec![Value::int(1), Value::int(2)]);
        assert!(check_value(&tag, &good).is_ok());
        let bad = Value::list(vec![Value::int(1), Value::string("x".to_string())]);
        let err = check_value(&tag, &bad).expect_err("bad element must fail");
        assert!(
            format!("{err:#}").contains("[1]"),
            "names the index: {err:#}"
        );
    }

    #[test]
    fn canonicalization_unifies_input_spacing() {
        // Every spaced form lands on the structural representation:
        // the stored, keyed, and rendered form is one form.
        assert_eq!(canonicalize_spelling("LIST<MAP>"), "LIST<MAP>");
        assert_eq!(canonicalize_spelling("LIST< MAP >"), "LIST<MAP>");
        assert_eq!(
            canonicalize_spelling("MAP<name:STRING,age:INT>"),
            "MAP<name: STRING, age: INT>"
        );
        assert_eq!(
            canonicalize_spelling("MAP<name : STRING>"),
            "MAP<name: STRING>"
        );
    }

    #[test]
    fn spellings_parse_to_shapes() {
        assert_eq!(
            parse_spelling("LIST<MAP>").expect("parses"),
            Spelled::Generic {
                name: "LIST".to_string(),
                args: vec![SpelledField {
                    name: None,
                    optional: false,
                    ty: Spelled::Named("MAP".to_string()),
                    default: None,

                    alias: None,
                }],
            }
        );
        assert_eq!(
            parse_spelling("MAP<name: STRING>").expect("parses"),
            Spelled::Generic {
                name: "MAP".to_string(),
                args: vec![SpelledField {
                    name: Some("name".to_string()),
                    optional: false,
                    ty: Spelled::Named("STRING".to_string()),
                    default: None,

                    alias: None,
                }],
            }
        );
        // Arity violations parse; resolution rejects them naming
        // the rule (`LIST` arity, `MAP` bare fields).
        assert!(parse_spelling("LIST<A, B>").is_ok());
        assert!(parse_spelling("MAP<STRING>").is_ok());
    }

    #[test]
    fn optional_field_spelling_parses_and_renders() {
        // A `?` suffix marks one field optional: it parses into the
        // flag, canonicalizes with the mark, and renders back.
        assert_eq!(
            parse_spelling("MAP<permit?: PERMIT>").expect("parses"),
            Spelled::Generic {
                name: "MAP".to_string(),
                args: vec![SpelledField {
                    name: Some("permit".to_string()),
                    optional: true,
                    ty: Spelled::Named("PERMIT".to_string()),
                    default: None,

                    alias: None,
                }],
            }
        );
        assert_eq!(
            canonicalize_spelling("MAP<permit ? : PERMIT>"),
            "MAP<permit?: PERMIT>"
        );
    }

    #[test]
    fn default_spellings_parse_render_and_materialize() {
        // `= literal` rides one field: ints, negative ints, floats,
        // bools, and quoted strings (spaces kept). Each parses into
        // the literal, canonicalizes back, and materializes the word.
        for (spelling, canonical, value) in [
            (
                "MAP<retries: INT = 2>",
                "MAP<retries: INT = 2>",
                Value::int(2),
            ),
            (
                "MAP<retries: INT=-2>",
                "MAP<retries: INT = -2>",
                Value::int(-2),
            ),
            (
                "MAP<ratio: FLOAT = 2.5>",
                "MAP<ratio: FLOAT = 2.5>",
                Value::float(2.5),
            ),
            (
                "MAP<on: BOOL = true>",
                "MAP<on: BOOL = true>",
                Value::bool(true),
            ),
            (
                "MAP<model: STRING = \"gpt-4\">",
                "MAP<model: STRING = \"gpt-4\">",
                Value::string("gpt-4".to_string()),
            ),
            (
                "MAP<msg: STRING = \"hello world\">",
                "MAP<msg: STRING = \"hello world\">",
                Value::string("hello world".to_string()),
            ),
        ] {
            let spelled = parse_spelling(spelling).expect("parses");
            assert_eq!(render_spelled(&spelled), canonical, "canonical: {spelling}");
            assert_eq!(canonicalize_spelling(spelling), canonical);
            match spelled {
                Spelled::Generic { args, .. } => {
                    let found = args.iter().find(|f| f.default.is_some()).expect("default");
                    let lit = found.default.as_ref().expect("lit");
                    assert_eq!(lit.to_value(), value);
                    assert!(
                        canonical.contains(&format!("= {}", lit.render())),
                        "canonical carries the default: {canonical}"
                    );
                }
                other => panic!("expected generic, got {other:?}"),
            }
        }
        // Rendered tags agree with rendered spellings on defaults.
        let tag = TypeTag::Record(Arc::from([Field {
            name: "retries".to_string(),
            ty: TypeTag::Int,
            docs: "",
            optional: false,
            default: Some(Value::int(2)),

            alias: None,
        }]));
        assert_eq!(
            render_structural(&tag),
            "MAP<retries: INT = 2>",
            "tag renders defaults canonically"
        );
        assert_eq!(
            render_default_value(&Value::float(2.5)),
            "2.5",
            "float keeps its point"
        );
        // Malformed defaults fail loudly: empty, non-literal,
        // unterminated strings, and bad numbers.
        for bad in [
            "MAP<a: INT = >",
            "MAP<a: INT = x>",
            "MAP<a: INT = 1.2.3>",
            "MAP<a: INT = \"x>",
            "MAP<a: INT = 9999999999999999999999>",
        ] {
            assert!(parse_spelling(bad).is_err(), "must fail: {bad}");
        }
    }

    #[test]
    fn unknown_map_spelling_parses_and_renders() {
        // `MAP<ANY>` is the explicit unknown map: it parses like any
        // other generic spelling and renders back to itself.
        assert_eq!(
            parse_spelling("MAP<ANY>").expect("parses"),
            Spelled::Generic {
                name: "MAP".to_string(),
                args: vec![SpelledField {
                    name: None,
                    optional: false,
                    ty: Spelled::Named("ANY".to_string()),
                    default: None,

                    alias: None,
                }],
            }
        );
        assert_eq!(canonicalize_spelling("MAP< ANY >"), "MAP<ANY>");
        assert_eq!(
            render_structural(&TypeTag::MapOf(Arc::new(TypeTag::Any))),
            "MAP<ANY>"
        );
    }

    #[test]
    fn map_of_checks_values_with_paths() {
        // Value shapes walk every entry with key paths; non-maps fail
        // naming the structural shape, never the coarse word.
        let tag = TypeTag::MapOf(Arc::new(TypeTag::Int));
        let good = map(&[("a", Value::int(1))]);
        assert!(check_value(&tag, &good).is_ok());
        let bad = map(&[("a", Value::string("x".to_string()))]);
        let err = check_value(&tag, &bad).expect_err("bad value must fail");
        assert!(format!("{err:#}").contains(".a"), "names the key: {err:#}");
        let not_map = Value::list(vec![Value::int(1)]);
        let err = check_value(&tag, &not_map).expect_err("non-map must fail");
        assert!(format!("{err:#}").contains("MAP<INT>"), "{err:#}");
    }

    #[test]
    fn spellings_reject_malformed_input() {
        for bad in [
            "",
            "LIST<>",
            "LIST<A,>",
            "MAP<name:>",
            "LIST<A",
            "list<MAP>",
            "LIST<A>>",
        ] {
            assert!(parse_spelling(bad).is_err(), "{bad:?} must fail");
        }
        // Depth cap counts syntactic nesting; deeper bails naming it.
        let mut deep = "INT".to_string();
        for _ in 0..MAX_GENERIC_DEPTH + 1 {
            deep = format!("LIST<{deep}>");
        }
        let err = parse_spelling(&deep).expect_err("over-deep spelling must fail");
        assert!(err.message().contains("generic depth"), "{err}");
        assert_eq!(err.key(), "dsl.spelling.max_depth");
    }

    #[test]
    fn second_alias_on_same_field_fails_with_both_names() {
        let err =
            parse_spelling("MAP<h: BOOL ALIAS help ALIAS h2>").expect_err("second ALIAS must fail");
        assert_eq!(err.key(), "dsl.alias.duplicate_on_field");
        assert!(err.message().contains("`h`"), "{err}");
        assert!(err.message().contains("`help`"), "{err}");
        assert!(err.message().contains("`h2`"), "{err}");
        assert!(err.message().contains("only one `ALIAS`"), "{err}");
        // No internal encodings in user output.
        assert!(!err.message().contains('\u{1f}'), "{err}");
    }

    #[test]
    fn second_alias_with_bad_name_is_bounded() {
        let err = parse_spelling("MAP<h: BOOL ALIAS help ALIAS Bad, x: INT>")
            .expect_err("second ALIAS with bad name must fail");
        assert_eq!(err.key(), "dsl.alias.duplicate_on_field");
        assert!(!err.message().contains('>'), "{err}");
        assert!(!err.message().contains("x: INT"), "{err}");
        assert!(!err.message().contains('\u{1f}'), "{err}");
    }

    #[test]
    fn identical_spellings_share_one_interned_payload() {
        let first = intern_rendered(
            "LIST<MAP>".to_string(),
            TypeTag::ListOf(Arc::new(TypeTag::Map)),
        );
        let second = intern_rendered(
            "LIST<MAP>".to_string(),
            TypeTag::ListOf(Arc::new(TypeTag::Map)),
        );
        let (TypeTag::ListOf(a), TypeTag::ListOf(b)) = (first, second) else {
            panic!("rendered tags keep their shape");
        };
        assert!(
            Arc::ptr_eq(&a, &b),
            "same canonical spelling shares one payload"
        );
    }

    #[test]
    fn structural_rendering_names_contents() {
        let fields: Arc<[Field]> = Arc::from([Field {
            name: "name".to_string(),
            ty: TypeTag::String,
            docs: "",
            optional: false,
            default: None,

            alias: None,
        }]);
        let record = TypeTag::Record(fields);
        let list = TypeTag::ListOf(Arc::new(record));
        assert_eq!(render_structural(&list), "LIST<MAP<name: STRING>>");
        assert_eq!(render_structural(&TypeTag::Int), "INT");
    }
}
