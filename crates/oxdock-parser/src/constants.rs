//! Language-level constants: the module separator, system module names,
//! and reserved keywords.
//!
//! Single source of truth shared by the grammar lowerer (`parser.rs`), the
//! macro token walker (`macro_input.rs`), and the runtime registry
//! (`oxdock-core`). Changing any of these must update exactly one
//! definition here; `dsl.pest` carries the same spellings by necessity (a
//! grammar file cannot reference Rust constants) and the bidirectional
//! keyword conformance test locks the two together.

/// Separator between module and function name (`STD::GLOB`).
pub const MODULE_SEPARATOR: &str = "::";

/// Module holding the compiled-in builtins.
pub const STD_MODULE_NAME: &str = "STD";

/// Module holding in-script `FUNC` definitions.
pub const SCRIPT_MODULE_NAME: &str = "SCRIPT";

/// Bare-word statement starters parsed by PEG rules rather than command
/// lowering. Each is referenced by [`crate::STRUCTURAL_KEYWORDS`]; spelling
/// lives here exactly once.
pub const KEYWORD_LET: &str = "LET";
pub const KEYWORD_FOR: &str = "FOR";
pub const KEYWORD_IF: &str = "IF";
pub const KEYWORD_ELSE: &str = "ELSE";
pub const KEYWORD_ASYNC: &str = "ASYNC";
pub const KEYWORD_AWAIT: &str = "AWAIT";
pub const KEYWORD_CANCEL: &str = "CANCEL";
pub const KEYWORD_FUNC: &str = "FUNC";
pub const KEYWORD_RETURN: &str = "RETURN";
pub const KEYWORD_WHILE: &str = "WHILE";
pub const KEYWORD_BREAK: &str = "BREAK";
pub const KEYWORD_CONTINUE: &str = "CONTINUE";

/// Variable inspection: a dedicated AST node, never a registry entry, so it
/// needs no import and accepts no qualifier.
pub const KEYWORD_INSPECT: &str = "INSPECT";

/// Bare-call scope directive (a lowering directive, not a runtime step).
pub const KEYWORD_IMPORT: &str = "IMPORT";

/// Reserved for future script-module support; rejected at lowering.
pub const KEYWORD_EXPORT: &str = "EXPORT";

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
