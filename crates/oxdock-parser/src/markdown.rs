use anyhow::{Result, bail};
use line_ending::LineEnding;

/// Runner configuration parsed from a fenced block's info-string.
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct BlockMetadata {
    pub env: Vec<(String, String)>,
    pub stdin: Option<String>,
    pub unified_roots: bool,
    pub expect_error: Option<String>,
}

/// A fenced code block extracted from a Markdown document.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FencedBlock {
    /// 1-based line number of the opening fence in the source document.
    pub line_no: usize,
    pub info_string: String,
    pub metadata: BlockMetadata,
    pub body: String,
}

fn fence_ticks(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if trimmed.len() - trimmed.trim_start_matches('`').len() < 3 {
        return None;
    }
    let indent = line.len() - trimmed.len();
    (indent <= 3).then(|| trimmed.len() - trimmed.trim_start_matches('`').len())
}

/// Extract fenced code blocks whose info-string starts with `lang`.
///
/// The first whitespace-delimited token of an opening fence's info-string
/// selects the language; remaining tokens are scanned as `key:value` metadata.
/// Values may be double-quoted to contain spaces. Unknown keys and duplicate
/// single-occurrence keys are hard errors so typos cannot silently change
/// runner behavior.
pub fn extract_fenced_blocks(markdown: &str, lang: &str) -> Result<Vec<FencedBlock>> {
    let mut blocks = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    let mut info_string: Option<String> = None;
    let mut body: Vec<&str> = Vec::new();

    for (idx, line) in markdown.lines().enumerate() {
        let line_no = idx + 1;
        match (open, fence_ticks(line)) {
            (None, Some(ticks)) => {
                let trimmed = line.trim_start();
                open = Some((line_no, ticks));
                info_string = Some(trimmed[ticks..].trim().to_string());
                body.clear();
            }
            (Some((open_no, open_ticks)), Some(ticks)) => {
                let trimmed = line.trim_start();
                if ticks >= open_ticks && trimmed[ticks..].trim().is_empty() {
                    if let Some(info) = info_string.take()
                        && first_token(&info) == lang
                    {
                        blocks.push(FencedBlock {
                            line_no: open_no,
                            metadata: parse_metadata(&info, open_no)?,
                            info_string: info,
                            body: LineEnding::normalize(&body.join("\n")),
                        });
                    }
                    open = None;
                    info_string = None;
                } else {
                    body.push(line);
                }
            }
            (Some(_), None) => body.push(line),
            (None, None) => {}
        }
    }

    if let Some((open_no, _)) = open {
        bail!("markdown:{open_no}: code fence is never closed");
    }
    Ok(blocks)
}

fn first_token(info: &str) -> &str {
    info.split_whitespace().next().unwrap_or("")
}

fn parse_metadata(info: &str, line_no: usize) -> Result<BlockMetadata> {
    let fail = |msg: String| anyhow::anyhow!("markdown:{line_no}: {msg}");

    let rest = skip_token(info);
    let mut metadata = BlockMetadata::default();
    let mut scanner = rest.chars().peekable();

    loop {
        while matches!(scanner.peek(), Some(c) if c.is_whitespace()) {
            scanner.next();
        }
        let Some(first_char) = scanner.peek().copied() else {
            break;
        };
        if !first_char.is_ascii_alphanumeric() && first_char != '_' {
            return Err(fail(format!(
                "unexpected character '{first_char}' in fence metadata"
            )));
        }
        let mut key = String::new();
        let mut saw_equals = false;
        while let Some(&c) = scanner.peek()
            && c != ':'
            && c != '='
            && !c.is_whitespace()
        {
            key.push(c);
            scanner.next();
        }
        match scanner.peek() {
            Some(':') => {
                scanner.next();
            }
            Some('=') => {
                saw_equals = true;
            }
            other => {
                let _ = other;
                return Err(fail(format!(
                    "fence metadata item '{key}' is missing ':' after its key"
                )));
            }
        }
        if saw_equals {
            return Err(fail(format!(
                "fence metadata item '{key}' must use 'key:value' syntax"
            )));
        }

        while matches!(scanner.peek(), Some(' ')) {
            scanner.next();
        }
        let value = if scanner.peek() == Some(&'"') {
            scanner.next();
            let mut value = String::new();
            loop {
                match scanner.next() {
                    Some('"') => break,
                    Some(c) => value.push(c),
                    None => {
                        return Err(fail(
                            "unterminated double quote in fence metadata".to_string(),
                        ));
                    }
                }
            }
            value
        } else {
            let mut value = String::new();
            while let Some(&c) = scanner.peek()
                && !c.is_whitespace()
            {
                value.push(c);
                scanner.next();
            }
            value
        };

        match key.as_str() {
            "env" => {
                let (name, val) = value
                    .split_once('=')
                    .ok_or_else(|| fail("env metadata must be 'env:KEY=VALUE'".to_string()))?;
                metadata.env.push((name.to_string(), val.to_string()));
            }
            "stdin" => {
                if metadata.stdin.replace(value.clone()).is_some() {
                    return Err(fail("stdin metadata specified more than once".to_string()));
                }
            }
            "roots" => {
                if value != "unified" {
                    return Err(fail(format!(
                        "unsupported roots metadata '{value}' (only 'unified' is defined)"
                    )));
                }
                if metadata.unified_roots {
                    return Err(fail("roots metadata specified more than once".to_string()));
                }
                metadata.unified_roots = true;
            }
            "expect_error" => {
                if metadata.expect_error.replace(value.clone()).is_some() {
                    return Err(fail(
                        "expect_error metadata specified more than once".to_string(),
                    ));
                }
            }
            other => {
                return Err(fail(format!(
                    "unknown fence metadata key '{other}' (supported: env, stdin, roots, expect_error)"
                )));
            }
        }
    }
    Ok(metadata)
}

fn skip_token(info: &str) -> &str {
    info.trim_start()[first_token(info).len()..].trim_start()
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn extracts_matching_language_blocks_with_line_numbers() {
        let md = indoc! {r#"
            before

            ```bash
            RUN not extracted
            ```

            ```oxdock
            ECHO first
            ```

            ```text oxdock-like
            ignored because the first token is text
            ```

            ```oxdock
            ECHO second
            ```
        "#};
        let blocks = extract_fenced_blocks(md, "oxdock").expect("extract");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].line_no, 7);
        assert_eq!(blocks[0].body, "ECHO first");
        assert_eq!(blocks[1].line_no, 15);
        assert_eq!(blocks[1].body, "ECHO second");
        assert_eq!(blocks[0].metadata, BlockMetadata::default());
    }

    #[test]
    fn closing_fence_requires_at_least_as_many_ticks() {
        let md = indoc! {r#"
            ````oxdock
            ECHO stays-open-here
            ``` not a closer
            ECHO still-body
            ````
            ```oxdock
            ECHO next
            ```
        "#};
        let blocks = extract_fenced_blocks(md, "oxdock").expect("extract");
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            blocks[0].body,
            "ECHO stays-open-here\n``` not a closer\nECHO still-body"
        );
    }

    #[test]
    fn unclosed_fence_is_an_error() {
        let err =
            extract_fenced_blocks("```oxdock\nECHO x\n", "oxdock").expect_err("unclosed fence");
        assert!(err.to_string().contains("never closed"));
    }

    #[test]
    fn parses_quoted_values_containing_spaces() {
        let md = "```oxdock expect_error:\"EXIT requested with code 42\"\nEXIT 42\n```\n";
        let block = &extract_fenced_blocks(md, "oxdock").expect("extract")[0];
        assert_eq!(
            block.metadata.expect_error.as_deref(),
            Some("EXIT requested with code 42")
        );
    }

    #[test]
    fn parses_env_stdin_and_roots_metadata() {
        let md = concat!(
            "```oxdock env:A=1 env:B=two stdin:hello roots:unified\n",
            "ECHO x\n",
            "```\n"
        );
        let block = &extract_fenced_blocks(md, "oxdock").expect("extract")[0];
        assert_eq!(
            block.metadata.env,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "two".to_string())
            ]
        );
        assert_eq!(block.metadata.stdin.as_deref(), Some("hello"));
        assert!(block.metadata.unified_roots);
    }

    #[test]
    fn unknown_and_duplicate_keys_are_errors() {
        let md = "```oxdock bogus:x\nECHO x\n```\n";
        let err = extract_fenced_blocks(md, "oxdock").expect_err("unknown key");
        assert!(
            err.to_string()
                .contains("unknown fence metadata key 'bogus'")
        );

        let dup = "```oxdock stdin:a stdin:b\nECHO x\n```\n";
        let err = extract_fenced_blocks(dup, "oxdock").expect_err("duplicate key");
        assert!(err.to_string().contains("more than once"));

        let roots = "```oxdock roots:squashed\nECHO x\n```\n";
        let err = extract_fenced_blocks(roots, "oxdock").expect_err("bad roots");
        assert!(err.to_string().contains("only 'unified'"));

        let env = "```oxdock env:NOVALUE\nECHO x\n```\n";
        let err = extract_fenced_blocks(env, "oxdock").expect_err("env without '='");
        assert!(err.to_string().contains("env:KEY=VALUE"));
    }

    #[test]
    fn unterminated_quote_is_an_error() {
        let md = "```oxdock expect_error:\"oops\nECHO x\n```\n";
        let err = extract_fenced_blocks(md, "oxdock").expect_err("unterminated quote");
        assert!(err.to_string().contains("unterminated double quote"));
    }

    #[test]
    fn equals_separator_is_rejected_with_guidance() {
        let md = "```oxdock expect_error=\"oops\"\nECHO x\n```\n"
            .replace("expect_error:", "expect_error=");
        let err = extract_fenced_blocks(&md, "oxdock").expect_err("equals separator");
        assert!(err.to_string().contains("must use 'key:value' syntax"));
    }

    #[test]
    fn crlf_bodies_are_normalized_to_lf() {
        let md = "```oxdock\r\nECHO a\r\nECHO b\r\n```\r\n";
        let block = &extract_fenced_blocks(md, "oxdock").expect("extract")[0];
        assert_eq!(block.body, "ECHO a\nECHO b");
    }

    #[test]
    fn bare_cr_bodies_are_normalized_to_lf() {
        let md = "```oxdock\nECHO a\rECHO b\r\n```\n";
        let block = &extract_fenced_blocks(md, "oxdock").expect("extract")[0];
        assert_eq!(block.body, "ECHO a\nECHO b");
    }
}
