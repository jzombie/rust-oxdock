//! Language-level constants: the module separator, system module names,
//! and reserved keywords.
//!
//! Single source of truth shared by the parser (`parser`), the
//! macro token walker (`macro_input.rs`), and the runtime registry
//! (`oxdock-core`). Changing any of these must update exactly one
//! definition here; the backend's structural dispatch carries the same
//! spellings, and the keyword transparency test locks the two together.

/// Separator between module and function name (`STD::GLOB`).
pub const MODULE_SEPARATOR: &str = "::";

/// Variable sigil (`$NAME`, `$ALIAS::NAME`): the `$` prefix marking
/// a variable read, binding head, or mutation target. Single owner
/// for the sigil spelling, so diagnostics and renderers never
/// restate it.
pub const VAR_SIGIL: &str = "$";

/// Module holding the compiled-in builtins.
pub const STD_MODULE_NAME: &str = "STD";

/// Module holding in-script `FUNC` definitions.
pub const SCRIPT_MODULE_NAME: &str = "SCRIPT";

/// Bare-word statement starters parsed by PEG rules rather than command
/// lowering. Each is referenced by [`crate::STRUCTURAL_KEYWORDS`]; spelling
/// lives here exactly once.
pub const KEYWORD_LET: &str = "LET";
pub const KEYWORD_CONST: &str = "CONST";
pub const KEYWORD_FOR: &str = "FOR";
pub const KEYWORD_IF: &str = "IF";
pub const KEYWORD_ELSE: &str = "ELSE";
pub const KEYWORD_SWITCH: &str = "SWITCH";
pub const KEYWORD_CASE: &str = "CASE";
pub const KEYWORD_DEFAULT: &str = "DEFAULT";
pub const KEYWORD_ASYNC: &str = "ASYNC";
pub const KEYWORD_AWAIT: &str = "AWAIT";
pub const KEYWORD_CANCEL: &str = "CANCEL";
pub const KEYWORD_FUNC: &str = "FUNC";
pub const KEYWORD_RETURN: &str = "RETURN";
pub const KEYWORD_WHILE: &str = "WHILE";
pub const KEYWORD_BREAK: &str = "BREAK";
pub const KEYWORD_CONTINUE: &str = "CONTINUE";
pub const KEYWORD_REMOTE: &str = "REMOTE";
pub const KEYWORD_TYPE: &str = "TYPE";
pub const KEYWORD_WITH_IO: &str = "WITH_IO";
pub const KEYWORD_TIMEOUT: &str = "TIMEOUT";
pub const KEYWORD_ISOLATED: &str = "ISOLATED";
pub const KEYWORD_RUN: &str = "RUN";
pub const KEYWORD_INHERIT_ENV: &str = "INHERIT_ENV";
pub const KEYWORD_ENV: &str = "ENV";
pub const KEYWORD_EXPAND: &str = "EXPAND";
pub const KEYWORD_IN: &str = "IN";
pub const KEYWORD_AS: &str = "AS";
pub const KEYWORD_FROM: &str = "FROM";

/// Plain command spellings. `Command::as_str` and `Command::parse` own
/// no literals: every arm references one of these, so a rename touches
/// exactly one definition here.
pub const KEYWORD_WORKDIR: &str = "WORKDIR";
pub const KEYWORD_WORKSPACE: &str = "WORKSPACE";
pub const KEYWORD_ECHO: &str = "ECHO";
pub const KEYWORD_COPY: &str = "COPY";
pub const KEYWORD_COPY_GIT: &str = "COPY_GIT";
pub const KEYWORD_SYMLINK: &str = "SYMLINK";
pub const KEYWORD_MKDIR: &str = "MKDIR";
pub const KEYWORD_INSTALL: &str = "INSTALL";
pub const KEYWORD_SET_EXECUTABLE: &str = "SET_EXECUTABLE";
pub const KEYWORD_ARCHIVE: &str = "ARCHIVE";
pub const KEYWORD_UNARCHIVE: &str = "UNARCHIVE";
pub const KEYWORD_LS: &str = "LS";
pub const KEYWORD_READ: &str = "READ";
pub const KEYWORD_READ_LINE: &str = "READ_LINE";
pub const KEYWORD_PROMPT: &str = "PROMPT";
pub const KEYWORD_WRITE: &str = "WRITE";
pub const KEYWORD_APPEND: &str = "APPEND";
pub const KEYWORD_APPEND_LINE: &str = "APPEND_LINE";
pub const KEYWORD_REMOVE: &str = "REMOVE";
pub const KEYWORD_ASSERT_EQ: &str = "ASSERT_EQ";
pub const KEYWORD_ASSERT_NE: &str = "ASSERT_NE";
pub const KEYWORD_ASSERT_CONTAINS: &str = "ASSERT_CONTAINS";
pub const KEYWORD_EXIT: &str = "EXIT";
pub const KEYWORD_FAIL: &str = "FAIL";
pub const KEYWORD_SLEEP: &str = "SLEEP";
pub const KEYWORD_LIST_APPEND: &str = "LIST_APPEND";

/// Uppercase `CONST` spelling: leading `A-Z`, then `A-Z`, digits,
/// or `_`. Single owner for the definition gate and the bare-read
/// gate so both agree on what counts as a const word.
pub fn is_const_name(name: &str) -> bool {
    !name.is_empty()
        && name.as_bytes()[0].is_ascii_uppercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Variable inspection: a dedicated AST node, never a registry entry, so it
/// needs no import and accepts no qualifier.
pub const KEYWORD_INSPECT: &str = "INSPECT";

/// Bare-call scope directive (a lowering directive, not a runtime step).
pub const KEYWORD_IMPORT: &str = "IMPORT";

/// Tool-surface directive: marks script `FUNC`s, qualified host
/// functions, and `TYPE` aliases for export; membership is validated
/// against top-level items and imports at lowering.
pub const KEYWORD_EXPORT: &str = "EXPORT";

/// Interpolation placeholder prefix (`__OXDOCK_INJECT_<n>__`): guard
/// keys carrying this shape read as env keys.
pub const INJECT_PREFIX: &str = "__OXDOCK_INJECT_";
/// Trailing marker closing an interpolation placeholder.
pub const INJECT_SUFFIX: &str = "__";

/// Canonical qualified form: `MODULE::BASE`.
pub fn qualify(module: &str, base: &str) -> String {
    format!("{module}{MODULE_SEPARATOR}{base}")
}

/// Split `MODULE::BASE` into its parts; `None` for bare names.
pub fn split_qualified(name: &str) -> Option<(&str, &str)> {
    name.split_once(MODULE_SEPARATOR)
}

/// Base of `MODULE::BASE`, or the name itself when bare. Used for
/// human-facing step errors; listings (`FUNCTIONS()`, `DESCRIBE`) keep the
/// qualified form.
pub fn base_name(qualified: &str) -> &str {
    split_qualified(qualified)
        .map(|(_, base)| base)
        .unwrap_or(qualified)
}

/// `$NAME` rendering for diagnostics and `Display` impls.
pub fn sigil_var(name: &str) -> String {
    format!("{VAR_SIGIL}{name}")
}

/// `$ALIAS::NAME` rendering for diagnostics and `Display` impls.
pub fn sigil_qualified(alias: &str, name: &str) -> String {
    format!("{VAR_SIGIL}{alias}{MODULE_SEPARATOR}{name}")
}
