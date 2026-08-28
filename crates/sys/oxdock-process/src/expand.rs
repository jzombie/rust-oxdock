use std::collections::HashMap;

use anyhow::Result;

use crate::contract::CommandContext;

/// Maximum bytes to buffer while scanning for closing delimiter.
/// If exceeded without finding closing delimiter, buffered bytes are flushed as literals.
const MAX_PLACEHOLDER_SCAN: usize = 1024;

/// Configurable delimiter syntax for template expansion.
pub struct TemplateDelimiters {
    pub open: &'static [u8],
    pub close: &'static [u8],
}

impl Default for TemplateDelimiters {
    fn default() -> Self {
        Self {
            open: b"{{",
            close: b"}}",
        }
    }
}

/// Streaming template expansion state machine.
///
/// Processes input bytes incrementally, expanding `{{ env:KEY }}` placeholders.
/// At most `MAX_PLACEHOLDER_SCAN` bytes are held in buffer. Plain text streams
/// flush immediately with zero buffering.
pub struct StreamingExpand {
    /// Bytes accumulated as key payload inside `{{ ... }}` (no open delimiter prefix).
    buffer: Vec<u8>,
    /// Explicit key=value overrides (take precedence over env).
    overrides: HashMap<String, String>,
    /// Environment variable lookup.
    env: HashMap<String, String>,
    /// State: are we currently inside a placeholder?
    in_placeholder: bool,
    /// Trailing opening byte from previous chunk — deferred across chunks.
    pending_brace: bool,
    /// Trailing closing byte from previous chunk — deferred across chunks.
    pending_close_brace: bool,
    /// Configurable delimiter syntax.
    delimiters: TemplateDelimiters,
}

impl StreamingExpand {
    /// Create with env vars and optional explicit overrides.
    /// Overrides take precedence over env vars.
    pub fn new(overrides: &[(String, String)], env: &HashMap<String, String>) -> Self {
        Self {
            buffer: Vec::with_capacity(256),
            overrides: overrides.iter().cloned().collect(),
            env: env.clone(),
            in_placeholder: false,
            pending_brace: false,
            pending_close_brace: false,
            delimiters: TemplateDelimiters::default(),
        }
    }

    /// Process a chunk of input bytes, writing expanded output to `out`.
    /// Returns early on empty input to preserve pending boundary state.
    pub fn process_bytes(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<usize> {
        if input.is_empty() {
            return Ok(0);
        }

        let start_len = out.len();
        let mut i = 0;

        // Handle pending close brace from previous chunk
        if self.pending_close_brace {
            self.pending_close_brace = false;
            if input[0] == self.delimiters.close[1] {
                // Confirmed close delimiter across boundary — extract key, lookup, emit
                let key = extract_key(&self.buffer);
                let value = lookup(&key, &self.overrides, &self.env);
                out.extend_from_slice(value.as_bytes());
                self.buffer.clear();
                self.in_placeholder = false;
                i = self.delimiters.close.len() - 1; // Skip input[0] (the second byte)
            } else {
                // Lone closing byte — treat as literal part of key
                // Push it to buffer, then let scan_placeholder process input[0]
                self.buffer.push(self.delimiters.close[0]);
                i = 0; // Do NOT skip input[0] — let scan_placeholder handle it
            }
        }

        // Handle pending open brace from previous chunk
        if self.pending_brace {
            self.pending_brace = false;
            if input[0] == self.delimiters.open[1] {
                // Confirmed `{{` across boundary — enter PlaceholderScan
                self.in_placeholder = true;
                self.buffer.clear();
                i = 1; // Skip the second open byte
            } else {
                // Single open byte was just a literal — flush it
                out.push(self.delimiters.open[0]);
            }
        }

        if self.in_placeholder {
            // We're inside a placeholder — scan for closing delimiter
            i = self.scan_placeholder(input, i, out);
        }

        // Normal state — scan for opening byte or flush literals
        while i < input.len() {
            if input[i] == self.delimiters.open[0] {
                if i + 1 < input.len() && input[i + 1] == self.delimiters.open[1] {
                    // Found open delimiter — enter PlaceholderScan
                    self.in_placeholder = true;
                    self.buffer.clear();
                    i += 2;
                    i = self.scan_placeholder(input, i, out);
                } else if i + 1 == input.len() {
                    // Opening byte is the LAST byte of chunk — defer
                    self.pending_brace = true;
                    i += 1;
                } else {
                    // Single opening byte in the middle — flush as literal
                    out.push(self.delimiters.open[0]);
                    i += 1;
                }
            } else {
                // Flush literal bytes until we find opening byte or end of chunk
                let start = i;
                while i < input.len() && input[i] != self.delimiters.open[0] {
                    i += 1;
                }
                out.extend_from_slice(&input[start..i]);
            }
        }

        Ok(out.len() - start_len)
    }

    /// Flush remaining buffer. Incomplete placeholders are treated as literals.
    pub fn flush(mut self, out: &mut Vec<u8>) -> Result<()> {
        // Emit deferred closing byte if present
        if self.pending_close_brace {
            self.buffer.push(self.delimiters.close[0]);
            self.pending_close_brace = false;
        }
        // Emit deferred opening byte if present
        if self.pending_brace {
            out.push(self.delimiters.open[0]);
            self.pending_brace = false;
        }
        // If inside placeholder, emit open delimiter prefix ONCE before buffer
        if self.in_placeholder {
            out.extend_from_slice(self.delimiters.open);
            self.in_placeholder = false;
        }
        // Flush remaining buffer as literal text
        out.extend_from_slice(&self.buffer);
        self.buffer.clear();
        Ok(())
    }

    /// Process a complete string (convenience for short command arguments).
    pub fn expand_string(self, input: &str) -> Result<String> {
        let mut out = Vec::with_capacity(input.len());
        let mut expander = self;
        expander.process_bytes(input.as_bytes(), &mut out)?;
        expander.flush(&mut out)?;
        Ok(String::from_utf8(out).unwrap_or_default())
    }

    /// Scan for closing delimiter starting at position `i`.
    /// Returns the next position to process after the placeholder.
    fn scan_placeholder(&mut self, input: &[u8], mut i: usize, out: &mut Vec<u8>) -> usize {
        while i < input.len() {
            if input[i] == self.delimiters.close[0] {
                if i + 1 < input.len() && input[i + 1] == self.delimiters.close[1] {
                    // Found closing delimiter — extract key, lookup, emit expansion
                    let key = extract_key(&self.buffer);
                    let value = lookup(&key, &self.overrides, &self.env);
                    out.extend_from_slice(value.as_bytes());
                    self.buffer.clear();
                    self.in_placeholder = false;
                    return i + 2;
                }
                if i + 1 == input.len() {
                    // Closing byte is the LAST byte — defer to next chunk
                    self.pending_close_brace = true;
                    return i + 1;
                }
            }
            self.buffer.push(input[i]);
            i += 1;

            // Buffer limit exceeded — flush as literal
            if self.buffer.len() > MAX_PLACEHOLDER_SCAN {
                out.extend_from_slice(self.delimiters.open);
                out.extend_from_slice(&self.buffer);
                self.buffer.clear();
                self.in_placeholder = false;
                return i;
            }
        }
        i
    }
}

/// Extract and trim key from buffer content (the bytes between delimiters).
fn extract_key(buffer: &[u8]) -> String {
    String::from_utf8_lossy(buffer).trim().to_string()
}

/// Lookup a key in overrides and env, stripping namespace prefixes.
///
/// `{{ env:CRATE_NAME }}` or `{{ script_env:CRATE_NAME }}` → lookup `"CRATE_NAME"`.
/// Overrides checked with both raw and normalized keys.
/// Keys WITHOUT `env:` or `script_env:` prefix are NOT looked up in env
/// (they expand to empty, matching the old behavior).
fn lookup(
    raw_key: &str,
    overrides: &HashMap<String, String>,
    env: &HashMap<String, String>,
) -> String {
    let key = raw_key.trim();
    // Only look up keys with env: or script_env: prefix
    let normalized = match key.strip_prefix("env:") {
        Some(k) => k,
        None => match key.strip_prefix("script_env:") {
            Some(k) => k,
            None => {
                // No namespace prefix — check overrides only, not env
                return overrides.get(key).cloned().unwrap_or_default();
            }
        },
    };

    // Check overrides with both raw and normalized keys
    overrides
        .get(key)
        .or_else(|| overrides.get(normalized))
        // Fall back to env with normalized key
        .or_else(|| env.get(normalized))
        .cloned()
        .unwrap_or_default()
}

// Legacy functions for backward compatibility

pub(crate) fn expand_with_lookup<F>(input: &str, mut lookup_fn: F) -> String
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
                        out.push_str(&lookup_fn(key).unwrap_or_default());
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
    use std::collections::HashMap;

    #[test]
    fn basic_expansion() {
        let mut env = HashMap::new();
        env.insert("NAME".into(), "World".into());
        let expander = StreamingExpand::new(&[], &env);
        let result = expander.expand_string("Hello {{ env:NAME }}").unwrap();
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn multiple_vars() {
        let mut env = HashMap::new();
        env.insert("A".into(), "X".into());
        env.insert("B".into(), "Y".into());
        let expander = StreamingExpand::new(&[], &env);
        let result = expander
            .expand_string("{{ env:A }} and {{ env:B }}")
            .unwrap();
        assert_eq!(result, "X and Y");
    }

    #[test]
    fn missing_var() {
        let env = HashMap::new();
        let expander = StreamingExpand::new(&[], &env);
        let result = expander.expand_string("{{ env:MISSING }}").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn no_placeholders() {
        let env = HashMap::new();
        let expander = StreamingExpand::new(&[], &env);
        let result = expander.expand_string("plain text").unwrap();
        assert_eq!(result, "plain text");
    }

    #[test]
    fn empty_input() {
        let env = HashMap::new();
        let expander = StreamingExpand::new(&[], &env);
        let result = expander.expand_string("").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn override_precedence() {
        let mut env = HashMap::new();
        env.insert("KEY".into(), "envval".into());
        let overrides = vec![("KEY".into(), "override".into())];
        let expander = StreamingExpand::new(&overrides, &env);
        let result = expander.expand_string("{{ env:KEY }}").unwrap();
        assert_eq!(result, "override");
    }

    #[test]
    fn override_with_namespace() {
        let mut env = HashMap::new();
        env.insert("CRATE".into(), "envval".into());
        let overrides = vec![("CRATE".into(), "override".into())];
        let expander = StreamingExpand::new(&overrides, &env);
        let result = expander.expand_string("{{ env:CRATE }}").unwrap();
        assert_eq!(result, "override");
    }

    #[test]
    fn override_raw_key_match() {
        let mut env = HashMap::new();
        env.insert("CRATE".into(), "envval".into());
        let overrides = vec![("env:CRATE".into(), "override".into())];
        let expander = StreamingExpand::new(&overrides, &env);
        let result = expander.expand_string("{{ env:CRATE }}").unwrap();
        assert_eq!(result, "override");
    }

    #[test]
    fn unclosed_placeholder() {
        let env = HashMap::new();
        let expander = StreamingExpand::new(&[], &env);
        let result = expander.expand_string("{{ env:KEY").unwrap();
        assert_eq!(result, "{{ env:KEY");
    }

    #[test]
    fn unclosed_with_prefix() {
        let env = HashMap::new();
        let expander = StreamingExpand::new(&[], &env);
        let result = expander.expand_string("Hello {{ env:KEY").unwrap();
        assert_eq!(result, "Hello {{ env:KEY");
    }

    #[test]
    fn buffer_limit_exceeded() {
        let env = HashMap::new();
        let expander = StreamingExpand::new(&[], &env);
        // Create input with `{{` followed by >1024 bytes without `}}`
        let mut input = b"{{ ".to_vec();
        input.extend(std::iter::repeat_n(b'x', 2000));
        let result = expander
            .expand_string(&String::from_utf8_lossy(&input))
            .unwrap();
        // Should flush as literal with `{{` prefix
        assert!(result.starts_with("{{ "));
        assert!(result.len() > 1024);
    }

    #[test]
    fn partial_across_chunks() {
        let mut env = HashMap::new();
        env.insert("NAME".into(), "World".into());
        let mut expander = StreamingExpand::new(&[], &env);
        let mut out = Vec::new();

        // Split `{{ env:NA` / `ME }}` across chunks
        expander.process_bytes(b"{{ env:NA", &mut out).unwrap();
        expander.process_bytes(b"ME }}", &mut out).unwrap();
        expander.flush(&mut out).unwrap();

        assert_eq!(String::from_utf8_lossy(&out), "World");
    }

    #[test]
    fn trailing_brace_across_chunks() {
        let mut env = HashMap::new();
        env.insert("NAME".into(), "World".into());
        let mut expander = StreamingExpand::new(&[], &env);
        let mut out = Vec::new();

        // Split `...{` / `{env:NAME}}` across chunks
        expander.process_bytes(b"...", &mut out).unwrap();
        expander.process_bytes(b"{", &mut out).unwrap();
        expander.process_bytes(b"{env:NAME}}", &mut out).unwrap();
        expander.flush(&mut out).unwrap();

        assert_eq!(String::from_utf8_lossy(&out), "...World");
    }

    #[test]
    fn trailing_brace_at_eof() {
        let env = HashMap::new();
        let mut expander = StreamingExpand::new(&[], &env);
        let mut out = Vec::new();

        expander.process_bytes(b"hello{", &mut out).unwrap();
        expander.flush(&mut out).unwrap();

        assert_eq!(String::from_utf8_lossy(&out), "hello{");
    }

    #[test]
    fn immediate_flush_guarantee() {
        let env = HashMap::new();
        let mut expander = StreamingExpand::new(&[], &env);
        let mut out = Vec::new();

        // 1MB of plain text with no placeholders
        let input = std::iter::repeat_n(b'x', 1024 * 1024).collect::<Vec<_>>();
        expander.process_bytes(&input, &mut out).unwrap();
        expander.flush(&mut out).unwrap();

        assert_eq!(out.len(), 1024 * 1024);
    }

    #[test]
    fn nested_braces() {
        let mut env = HashMap::new();
        env.insert("KEY{1}".into(), "val".into());
        let expander = StreamingExpand::new(&[], &env);
        let result = expander.expand_string("{{ env:KEY{1} }}").unwrap();
        assert_eq!(result, "val");
    }

    #[test]
    fn split_close_delimiter_across_chunks() {
        let mut env = HashMap::new();
        env.insert("NAME".into(), "World".into());
        let mut expander = StreamingExpand::new(&[], &env);
        let mut out = Vec::new();

        // Split `}}` across chunks: `{{ env:NAME` / `}}`
        expander.process_bytes(b"{{ env:NAME", &mut out).unwrap();
        expander.process_bytes(b"}}", &mut out).unwrap();
        expander.flush(&mut out).unwrap();

        assert_eq!(String::from_utf8_lossy(&out), "World");
    }

    #[test]
    fn split_close_delimiter_with_trailing_content() {
        let mut env = HashMap::new();
        env.insert("NAME".into(), "World".into());
        let mut expander = StreamingExpand::new(&[], &env);
        let mut out = Vec::new();

        // Split `}}` across chunks with content after
        expander.process_bytes(b"{{ env:NAME", &mut out).unwrap();
        expander.process_bytes(b"}} rest", &mut out).unwrap();
        expander.flush(&mut out).unwrap();

        assert_eq!(String::from_utf8_lossy(&out), "World rest");
    }

    #[test]
    fn empty_input_preserves_pending_state() {
        let mut env = HashMap::new();
        env.insert("NAME".into(), "World".into());
        let mut expander = StreamingExpand::new(&[], &env);
        let mut out = Vec::new();

        // End chunk with closing byte, then empty input, then confirm
        expander.process_bytes(b"{{ env:NAME", &mut out).unwrap();
        expander.process_bytes(b"", &mut out).unwrap(); // empty — should preserve state
        expander.process_bytes(b"}}", &mut out).unwrap();
        expander.flush(&mut out).unwrap();

        assert_eq!(String::from_utf8_lossy(&out), "World");
    }
}
