//! Keyed render failures (`render.*`). Wording lives here; call
//! sites pass structured data only, never `format!` strings.

use oxdock_diagnostics::Diagnostic;

/// Embedded pipeline bytes that are not UTF-8. Parameterless: the
/// wording is fixed (byte-identical to the historic anyhow context).
pub fn embedded_not_utf8() -> Diagnostic {
    Diagnostic::new(
        "render.embedded.not_utf8",
        "embedded pipeline is not UTF-8".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_has_a_distinct_key() {
        let keys = [embedded_not_utf8().key()];
        assert_eq!(keys, ["render.embedded.not_utf8"]);
    }

    #[test]
    fn messages_match_canonical_wording() {
        assert_eq!(
            embedded_not_utf8().message(),
            "embedded pipeline is not UTF-8"
        );
    }
}
