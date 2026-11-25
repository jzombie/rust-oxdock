//! Canonical DSL names of the compiled-in STD functions.
//!
//! Single source of truth for every user-visible function name that
//! appears in a diagnostic message, a hand-built [`HostRegistration`],
//! or an argument-prefix string. `#[oxdock_func]` derives the registry
//! name from the Rust function name by uppercasing (`path_type` becomes
//! `PATH_TYPE`, overridable with `name = "..."` for `BOOL`), so a rename
//! of the Rust function silently moves the registry while a string
//! literal in `diagnostics.rs` rots. Every constructor message and every
//! hand-built registration below interpolates one of these constants
//! instead of restating the spelling; the registry lock test in
//! `tests.rs` (`function_names_match_registry`) asserts each constant
//! against `builtin_registrations()`, so a rename fails loudly instead
//! of drifting.
//!
//! Only functions actually named in diagnostics or hand-built
//! registrations live here. Macro-registered functions whose names never
//! surface in user-facing text need no constant.

/// `BOOL`: explicit override on `bool_convert` (`bool` is a Rust keyword).
pub const FUNC_BOOL: &str = "BOOL";
/// `BOOL_OR`: disjunction with `BOOL` coercion.
pub const FUNC_BOOL_OR: &str = "BOOL_OR";
/// `CONTAINS`: substring / element membership.
pub const FUNC_CONTAINS: &str = "CONTAINS";
/// `FORMAT_BYTE_COUNT`: human-readable byte counts.
pub const FUNC_FORMAT_BYTE_COUNT: &str = "FORMAT_BYTE_COUNT";
/// `FORMAT_DURATION`: human-readable durations.
pub const FUNC_FORMAT_DURATION: &str = "FORMAT_DURATION";
/// `FLOAT`: float conversion.
pub const FUNC_FLOAT: &str = "FLOAT";
/// `INT`: integer conversion.
pub const FUNC_INT: &str = "INT";
/// `IS_EXEC`: executable probe.
pub const FUNC_IS_EXEC: &str = "IS_EXEC";
/// `IS_TERMINAL`: terminal probe.
pub const FUNC_IS_TERMINAL: &str = "IS_TERMINAL";
/// `LEN`: length of strings, lists, and maps.
pub const FUNC_LEN: &str = "LEN";
/// `PARSE_ARGS`: flags-plus-positionals parsing.
pub const FUNC_PARSE_ARGS: &str = "PARSE_ARGS";
/// `PARSE_BYTE_COUNT`: byte-count token parsing.
pub const FUNC_PARSE_BYTE_COUNT: &str = "PARSE_BYTE_COUNT";
/// `PARSE_DURATION`: duration token parsing.
pub const FUNC_PARSE_DURATION: &str = "PARSE_DURATION";
/// `PARSE_FLAGS`: flags-only parsing.
pub const FUNC_PARSE_FLAGS: &str = "PARSE_FLAGS";
/// `PARSE_POSITIONALS`: positionals-only parsing.
pub const FUNC_PARSE_POSITIONALS: &str = "PARSE_POSITIONALS";
/// `PATH_BASE`: file-name extraction.
pub const FUNC_PATH_BASE: &str = "PATH_BASE";
/// `PATH_REL`: relative-path computation.
pub const FUNC_PATH_REL: &str = "PATH_REL";
/// `PATH_TYPE`: filesystem entry description.
pub const FUNC_PATH_TYPE: &str = "PATH_TYPE";
/// `PATH_WRITABLE`: writability probe.
pub const FUNC_PATH_WRITABLE: &str = "PATH_WRITABLE";
/// `SEMAPHORE_NEW`: semaphore construction.
pub const FUNC_SEMAPHORE_NEW: &str = "SEMAPHORE_NEW";
/// `SHELL_SPLIT_ARGS`: shell-word splitting.
pub const FUNC_SHELL_SPLIT_ARGS: &str = "SHELL_SPLIT_ARGS";
/// `TO_JSON`: value-to-JSON encoding.
pub const FUNC_TO_JSON: &str = "TO_JSON";
