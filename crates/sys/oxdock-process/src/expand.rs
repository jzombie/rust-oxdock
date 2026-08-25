use std::collections::HashMap;

use crate::contract::CommandContext;

pub(crate) fn expand_with_lookup<F>(input: &str, mut lookup: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            if let Some(&'{') = chars.peek() {
                chars.next(); // consume second '{'
                let mut content = String::new();
                let mut closed = false;
                // Look ahead for closing }}
                let mut inner_chars = chars.clone();
                while let Some(ch) = inner_chars.next() {
                    if ch == '}'
                        && let Some(&'}') = inner_chars.peek()
                    {
                        closed = true;
                        break;
                    }
                    content.push(ch);
                }

                if closed {
                    // Advance main iterator past content and closing braces.
                    // Count chars, not bytes: content may contain multi-byte
                    // UTF-8 (e.g. non-ASCII placeholder names).
                    for _ in 0..content.chars().count() {
                        chars.next();
                    }
                    chars.next(); // first }
                    chars.next(); // second }

                    let key = content.trim();
                    if !key.is_empty() {
                        out.push_str(&lookup(key).unwrap_or_default());
                    }
                } else {
                    out.push('{');
                    out.push('{');
                }
            } else {
                out.push('{');
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn expand_script_env(input: &str, script_envs: &HashMap<String, String>) -> String {
    expand_with_lookup(input, |name| {
        if let Some(key) = name.strip_prefix("env:") {
            script_envs
                .get(key)
                .cloned()
                .or_else(|| std::env::var(key).ok())
        } else {
            None
        }
    })
}

pub fn expand_command_env(input: &str, ctx: &CommandContext) -> String {
    expand_with_lookup(input, |name| {
        if let Some(key) = name.strip_prefix("env:") {
            ctx.envs().get(key).cloned()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxdock_fs::{GuardedPath, PolicyPath};
    use oxdock_sys_test_utils::TestEnvGuard;
    use std::collections::HashMap;

    #[test]
    fn expand_script_env_prefers_script_values() {
        let mut script_envs = HashMap::new();
        script_envs.insert("FOO".into(), "from-script".into());
        script_envs.insert("ONLY".into(), "only".into());
        let _env_guard = TestEnvGuard::set("FOO", "from-env");
        let rendered = expand_script_env(
            "{{ env:FOO }}:{{ env:ONLY }}:{{ env:MISSING }}",
            &script_envs,
        );
        assert_eq!(rendered, "from-script:only:");
    }

    #[test]
    fn expand_script_env_supports_colon_separator() {
        let mut script_envs = HashMap::new();
        script_envs.insert("FOO".into(), "val".into());
        let rendered = expand_script_env("{{ env:FOO }}", &script_envs);
        assert_eq!(rendered, "val");
    }

    #[test]
    fn expand_command_env_handles_var_forms() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let guard = temp.as_guarded_path().clone();
        let cwd: PolicyPath = guard.clone().into();
        let mut envs = HashMap::new();
        envs.insert("FOO".into(), "bar".into());
        envs.insert("PCT".into(), "percent".into());
        envs.insert("CARGO_TARGET_DIR".into(), guard.display().to_string());
        envs.insert("HOST_ONLY".into(), "host".into());

        let ctx = CommandContext::from_map(&cwd, &envs, &guard, &guard, &guard);

        // Valid syntax: {{ env:VAR }}
        let rendered = expand_command_env(
            "{{ env:FOO }}-{{ env:PCT }}-{{ env:HOST_ONLY }}-{{ env:CARGO_TARGET_DIR }}",
            &ctx,
        );
        assert_eq!(rendered, format!("bar-percent-host-{}", guard.display()));

        // Invalid/Legacy syntax: treated as literal text
        // %FOO% -> %FOO%
        // {CARGO_TARGET_DIR} -> {CARGO_TARGET_DIR}
        // $$ -> $$
        let input_literal = "%FOO%-{CARGO_TARGET_DIR}-$$";
        let rendered_literal = expand_command_env(input_literal, &ctx);
        assert_eq!(rendered_literal, input_literal);
    }

    #[test]
    fn expand_command_env_does_not_fallback_to_host() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let guard = temp.as_guarded_path().clone();
        let cwd: PolicyPath = guard.clone().into();
        let envs = HashMap::new();
        let _env_guard = TestEnvGuard::set("HOST_ONLY", "host");

        let ctx = CommandContext::from_map(&cwd, &envs, &guard, &guard, &guard);
        let rendered = expand_command_env("{{ env:HOST_ONLY }}", &ctx);
        assert_eq!(rendered, "");
    }

    #[test]
    fn expand_with_lookup_handles_multibyte_placeholder_names() {
        // Regression: advancement used to count bytes instead of chars, so a
        // multi-byte placeholder name swallowed characters after `}}`.
        let rendered = expand_with_lookup("{{ env:héllo }}X", |name| {
            if name == "env:héllo" {
                Some("value".to_string())
            } else {
                None
            }
        });
        assert_eq!(rendered, "valueX");
    }

    #[test]
    fn expand_with_lookup_preserves_multibyte_text_outside_placeholders() {
        let rendered = expand_with_lookup("héllo wörld {{ env:A }} ✓", |name| {
            if name == "env:A" {
                Some("1".to_string())
            } else {
                None
            }
        });
        assert_eq!(rendered, "héllo wörld 1 ✓");
    }

    #[test]
    fn expand_with_lookup_keeps_unclosed_double_brace_literal() {
        let rendered = expand_with_lookup("a {{ b", |_| -> Option<String> {
            panic!("input without any closing braces must not produce lookups")
        });
        assert_eq!(rendered, "a {{ b");
    }

    #[test]
    fn expand_with_lookup_binds_first_open_to_next_close_across_text() {
        // Pins current greedy behavior: the first `{{` binds to the next `}}`
        // even across an interior `{{`, and the entire span is trimmed into a
        // single lookup key. With no resolver entry for that composite key,
        // the whole placeholder renders as empty text.
        let seen = std::cell::RefCell::new(None);
        let rendered = expand_with_lookup("a {{ b {{ env:X }} c", |name| {
            *seen.borrow_mut() = Some(name.to_string());
            if name == "env:X" {
                Some("V".to_string())
            } else {
                None
            }
        });
        assert_eq!(rendered, "a  c");
        assert_eq!(seen.borrow().as_deref(), Some("b {{ env:X"));
    }

    #[test]
    fn expand_with_lookup_skips_empty_and_blank_keys() {
        let rendered = expand_with_lookup("x{{}}y", |_| -> Option<String> {
            panic!("empty key must not be looked up")
        });
        assert_eq!(rendered, "xy");

        let rendered_blank = expand_with_lookup("x{{   }}y", |_| -> Option<String> {
            panic!("blank key must not be looked up")
        });
        assert_eq!(rendered_blank, "xy");
    }

    #[test]
    fn expand_with_lookup_passes_through_stray_braces() {
        let rendered_close = expand_with_lookup("a }} b", |_| -> Option<String> {
            panic!("stray closing braces must not be looked up")
        });
        assert_eq!(rendered_close, "a }} b");

        let rendered_single = expand_with_lookup("{ alone {", |_| -> Option<String> {
            panic!("single brace must not be looked up")
        });
        assert_eq!(rendered_single, "{ alone {");
    }

    #[test]
    fn expand_with_lookup_supports_adjacent_placeholders() {
        let rendered = expand_with_lookup("{{ env:A }}{{ env:B }}", |name| match name {
            "env:A" => Some("a".to_string()),
            "env:B" => Some("b".to_string()),
            _ => None,
        });
        assert_eq!(rendered, "ab");
    }
}
