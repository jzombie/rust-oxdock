//! Template-expansion errors behind single constructors with stable keys.
//!
//! This module owns every expansion template and key (`proc.expand.*`).
//! It builds on the generic [`oxdock_diagnostics::Diagnostic`] transport
//! but owns its wording: changing expansion text happens here, never at
//! the call sites. Call sites pass structured data only, never
//! `format!` strings. Suggestion hints render inline in the message
//! (never via `with_hint`, avoiding double-print in `Display`); the
//! `did you mean` shape lives here, call sites pass only the scope.

use oxdock_diagnostics::Diagnostic;

/// Structured field keys. Single spelling for the whole module:
/// constructors reference these, never literals, so a key rename is
/// one edit.
const FIELD_KEY: &str = "key";
const FIELD_NAME: &str = "name";
const FIELD_MODULE: &str = "module";
const FIELD_FUNC: &str = "func";
const FIELD_ARG: &str = "arg";
const FIELD_ENTRY: &str = "entry";
const FIELD_ROOT: &str = "root";
const FIELD_SEGMENT: &str = "segment";
const FIELD_INDEX: &str = "index";
const FIELD_LEN: &str = "len";
const FIELD_LIMIT: &str = "limit";

/// Suggestion scope for an undefined `$var` miss: the name also lives
/// in the step environment or the override table.
pub enum ScriptSuggest {
    /// `env:{name}` holds a value.
    Env,
    /// A step override named `{name}` exists.
    StepOverride,
}

/// Suggestion scope for a missing step-override miss: the name also
/// lives in the script variables or the environment.
pub enum OverrideSuggest {
    /// `${name}` holds a value.
    ScriptVar,
    /// `env:{name}` holds a value.
    EnvVar,
}

/// Render one `did you mean` hint. Single owner for the shape; callers
/// pass the already-derived spelling and scope label.
fn did_you_mean(spelling: &str, scope: &str) -> String {
    format!("; did you mean '{spelling}' ({scope})?")
}

/// Undefined `env:KEY`. Takes the key plus whether a same-named script
/// variable exists; the hint shape lives here.
pub fn undefined_env_var(key: &str, suggest_script_var: bool) -> Diagnostic {
    let hint = suggest_script_var
        .then(|| did_you_mean(&format!("${key}"), "script variable"))
        .unwrap_or_default();
    let message = format!("undefined environment variable: '{key}'{hint}");
    Diagnostic::new(
        "proc.expand.undefined_env_var",
        message,
        Diagnostic::make_fields(&[(FIELD_KEY, key)]),
    )
}

/// Undefined `$var`. Takes the name plus an optional suggestion scope;
/// hinted and bare share one key and one mode.
pub fn undefined_script_var(rest: &str, suggest: Option<ScriptSuggest>) -> Diagnostic {
    let hint = match suggest {
        Some(ScriptSuggest::Env) => did_you_mean(&format!("env:{rest}"), "environment variable"),
        Some(ScriptSuggest::StepOverride) => did_you_mean(rest, "step override"),
        None => String::new(),
    };
    let message = format!("undefined script variable: '${rest}'{hint}");
    Diagnostic::new(
        "proc.expand.undefined_script_var",
        message,
        Diagnostic::make_fields(&[(FIELD_NAME, rest)]),
    )
}

/// Missing step-override argument. Takes the name plus an optional
/// suggestion scope; hinted and bare share one key and one mode.
pub fn missing_step_override(name: &str, suggest: Option<OverrideSuggest>) -> Diagnostic {
    let hint = match suggest {
        Some(OverrideSuggest::ScriptVar) => did_you_mean(&format!("${name}"), "script variable"),
        Some(OverrideSuggest::EnvVar) => {
            did_you_mean(&format!("env:{name}"), "environment variable")
        }
        None => String::new(),
    };
    let message = format!("missing required step override argument: '{name}'{hint}");
    Diagnostic::new(
        "proc.expand.missing_step_override",
        message,
        Diagnostic::make_fields(&[(FIELD_NAME, name)]),
    )
}

/// Placeholder in no resolvable shape. Takes the raw key; quoting
/// lives here.
pub fn invalid_placeholder_format(key: &str) -> Diagnostic {
    let message = format!(
        "invalid placeholder format '{key}': script variables must start with '$' and environment variables with 'env:'"
    );
    Diagnostic::new(
        "proc.expand.invalid_placeholder_format",
        message,
        Diagnostic::make_fields(&[(FIELD_KEY, key)]),
    )
}

/// Placeholder call with no registered resolver. Takes the module and
/// function names; quoting lives here.
pub fn call_without_registry(module: &str, name: &str) -> Diagnostic {
    let message = format!(
        "placeholder call '{module}::{name}' has no function registry; register one with with_call_resolver"
    );
    Diagnostic::new(
        "proc.expand.call_without_registry",
        message,
        Diagnostic::make_fields(&[(FIELD_MODULE, module), (FIELD_FUNC, name)]),
    )
}

/// Placeholder call argument nesting past the literal depth cap. Takes
/// the cap; rendering lives here.
pub fn call_arg_too_deep(limit: usize) -> Diagnostic {
    let message = format!("placeholder call argument nests too deep (limit {limit})");
    Diagnostic::new(
        "proc.expand.call_arg_too_deep",
        message,
        Diagnostic::make_fields(&[(FIELD_LIMIT, &limit.to_string())]),
    )
}

/// Empty `$` variable reference in a call argument. Parameterless: the
/// wording is fixed.
pub fn call_arg_empty_var() -> Diagnostic {
    Diagnostic::new(
        "proc.expand.call_arg_empty_var",
        "invalid placeholder call argument: empty variable reference".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Unresolvable placeholder call argument. Takes the raw source;
/// quoting lives here.
pub fn call_arg_invalid(src: &str) -> Diagnostic {
    let message = format!(
        "invalid placeholder call argument '{src}': use $var.path, a string, int, float, or bool literal, or a {{key: value}} or [item] literal"
    );
    Diagnostic::new(
        "proc.expand.call_arg_invalid",
        message,
        Diagnostic::make_fields(&[(FIELD_ARG, src)]),
    )
}

/// Nested calls inside placeholder arguments. Parameterless: the
/// wording is fixed.
pub fn call_arg_nested_calls() -> Diagnostic {
    Diagnostic::new(
        "proc.expand.call_arg_nested_calls",
        "nested calls are not supported in placeholder arguments".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Map literal entry without exactly one top-level `:`. Takes the raw
/// entry; quoting lives here.
pub fn map_entry_invalid(entry: &str) -> Diagnostic {
    let message = format!("invalid map literal entry '{entry}': use key: value");
    Diagnostic::new(
        "proc.expand.map_entry_invalid",
        message,
        Diagnostic::make_fields(&[(FIELD_ENTRY, entry)]),
    )
}

/// Duplicate key in a map literal. Takes the key; quoting lives here.
pub fn map_duplicate_key(key: &str) -> Diagnostic {
    let message = format!("duplicate key '{key}' in map literal");
    Diagnostic::new(
        "proc.expand.map_duplicate_key",
        message,
        Diagnostic::make_fields(&[(FIELD_KEY, key)]),
    )
}

/// Missing property on a map during key-path traversal. Takes the root
/// variable and the segment; quoting lives here.
pub fn key_path_missing_property(root: &str, segment: &str) -> Diagnostic {
    let message = format!("property '{segment}' not found on object '${root}'");
    Diagnostic::new(
        "proc.expand.key_path_missing_property",
        message,
        Diagnostic::make_fields(&[(FIELD_ROOT, root), (FIELD_SEGMENT, segment)]),
    )
}

/// Non-numeric list index during key-path traversal. Takes the root
/// variable and the segment; quoting lives here.
pub fn key_path_bad_index(root: &str, segment: &str) -> Diagnostic {
    let message = format!("invalid array index '{segment}' on list '${root}'");
    Diagnostic::new(
        "proc.expand.key_path_bad_index",
        message,
        Diagnostic::make_fields(&[(FIELD_ROOT, root), (FIELD_SEGMENT, segment)]),
    )
}

/// Out-of-bounds list index during key-path traversal. Takes the root
/// variable, the index, and the list length; rendering lives here.
pub fn key_path_index_out_of_bounds(root: &str, idx: usize, len: usize) -> Diagnostic {
    let message = format!("index {idx} out of bounds for list '${root}' (len: {len})");
    Diagnostic::new(
        "proc.expand.key_path_index_out_of_bounds",
        message,
        Diagnostic::make_fields(&[
            (FIELD_ROOT, root),
            (FIELD_INDEX, &idx.to_string()),
            (FIELD_LEN, &len.to_string()),
        ]),
    )
}

/// Property access on a primitive during key-path traversal. Takes the
/// root variable and the segment; quoting lives here.
pub fn key_path_on_primitive(root: &str, segment: &str) -> Diagnostic {
    let message = format!("cannot access property '{segment}' on primitive value of '${root}'");
    Diagnostic::new(
        "proc.expand.key_path_on_primitive",
        message,
        Diagnostic::make_fields(&[(FIELD_ROOT, root), (FIELD_SEGMENT, segment)]),
    )
}

/// `PIPE` value interpolated as text. Parameterless: the wording and
/// the `LET` capture guidance live here.
pub fn render_pipe() -> Diagnostic {
    Diagnostic::new(
        "proc.expand.render_pipe",
        "cannot render PIPE as STRING: pipes carry streams between steps, not text. Capture the producing step's output with LET $var: STRING = ... instead.".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `SEMAPHORE` value interpolated as text. Parameterless: the wording
/// lives here.
pub fn render_semaphore() -> Diagnostic {
    Diagnostic::new(
        "proc.expand.render_semaphore",
        "cannot render SEMAPHORE as STRING: semaphores coordinate task access between steps, not text.".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// `PERMIT` value interpolated as text. Parameterless: the wording
/// lives here.
pub fn render_permit() -> Diagnostic {
    Diagnostic::new(
        "proc.expand.render_permit",
        "cannot render PERMIT as STRING: permits grant scoped task access, not text.".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_has_a_distinct_key() {
        let keys = [
            undefined_env_var("FOO", false).key(),
            undefined_script_var("x", None).key(),
            missing_step_override("n", None).key(),
            invalid_placeholder_format("k").key(),
            call_without_registry("M", "F").key(),
            call_arg_too_deep(8).key(),
            call_arg_empty_var().key(),
            call_arg_invalid("s").key(),
            call_arg_nested_calls().key(),
            map_entry_invalid("e").key(),
            map_duplicate_key("k").key(),
            key_path_missing_property("r", "s").key(),
            key_path_bad_index("r", "s").key(),
            key_path_index_out_of_bounds("r", 5, 2).key(),
            key_path_on_primitive("r", "s").key(),
            render_pipe().key(),
            render_semaphore().key(),
            render_permit().key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn hinted_and_bare_share_one_key() {
        assert_eq!(
            undefined_env_var("FOO", false).key(),
            undefined_env_var("FOO", true).key()
        );
        assert_eq!(
            undefined_script_var("x", None).key(),
            undefined_script_var("x", Some(ScriptSuggest::Env)).key()
        );
        assert_eq!(
            missing_step_override("n", None).key(),
            missing_step_override("n", Some(OverrideSuggest::EnvVar)).key()
        );
    }

    #[test]
    fn messages_match_canonical_wording() {
        assert_eq!(
            undefined_env_var("FOO", false).message(),
            "undefined environment variable: 'FOO'"
        );
        assert_eq!(
            undefined_env_var("FOO", true).message(),
            "undefined environment variable: 'FOO'; did you mean '$FOO' (script variable)?"
        );
        assert_eq!(
            undefined_env_var("", false).message(),
            "undefined environment variable: ''"
        );
        assert_eq!(
            undefined_script_var("WHO", Some(ScriptSuggest::Env)).message(),
            "undefined script variable: '$WHO'; did you mean 'env:WHO' (environment variable)?"
        );
        assert_eq!(
            missing_step_override("PORT", Some(OverrideSuggest::ScriptVar)).message(),
            "missing required step override argument: 'PORT'; did you mean '$PORT' (script variable)?"
        );
        assert_eq!(
            missing_step_override("PORT", Some(OverrideSuggest::EnvVar)).message(),
            "missing required step override argument: 'PORT'; did you mean 'env:PORT' (environment variable)?"
        );
        assert_eq!(
            key_path_index_out_of_bounds("r", 5, 2).message(),
            "index 5 out of bounds for list '$r' (len: 2)"
        );
        assert_eq!(
            call_arg_too_deep(8).message(),
            "placeholder call argument nests too deep (limit 8)"
        );
    }
}
