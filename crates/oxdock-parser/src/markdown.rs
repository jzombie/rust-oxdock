use anyhow::{Result, bail};
use line_ending::LineEnding;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// Runner configuration parsed from a fenced block's info-string.
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct BlockMetadata {
    pub env: Vec<(String, String)>,
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

/// Extract fenced code blocks whose info-string starts with `lang`.
///
/// Uses `pulldown-cmark` for reliable CommonMark fence parsing. The first
/// whitespace-delimited token of an opening fence's info-string selects the
/// language; remaining tokens are scanned as `key:value` metadata.
pub fn extract_fenced_blocks(markdown: &str, lang: &str) -> Result<Vec<FencedBlock>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    let offset_parser = Parser::new_ext(markdown, options).into_offset_iter();

    // Pre-compute newline byte offsets for line-number mapping.
    let mut line_starts: Vec<usize> = vec![0];
    for (i, _) in markdown.match_indices('\n') {
        line_starts.push(i + 1);
    }

    let md_len = markdown.len();
    let mut blocks = Vec::new();
    let mut in_block = false;
    let mut current_line_no = 0;
    let mut current_info = String::new();
    let mut current_body = String::new();

    for (event, range) in offset_parser {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
                let info_str = info.to_string();
                let first_token = info_str.split_whitespace().next().unwrap_or("");
                if first_token == lang {
                    in_block = true;
                    // Map byte offset to 1-based line number.
                    let line_no = line_starts.partition_point(|&start| start <= range.start);
                    current_line_no = line_no;
                    current_info = info_str;
                    current_body.clear();
                }
            }
            Event::Text(text) if in_block => {
                current_body.push_str(&text);
            }
            Event::End(TagEnd::CodeBlock) if in_block => {
                in_block = false;
                // pulldown-cmark auto-closes unclosed fences by emitting End
                // at EOF. Detect this: if the end offset reaches the document
                // end, the fence was never explicitly closed.
                if range.end >= md_len && !markdown.trim_end().ends_with("```") {
                    bail!("markdown:{current_line_no}: code fence is never closed");
                }
                let metadata = parse_metadata(&current_info, current_line_no)?;
                // Trim trailing newline that pulldown-cmark includes from the last line.
                let body = current_body.trim_end_matches('\n').to_string();
                blocks.push(FencedBlock {
                    line_no: current_line_no,
                    info_string: std::mem::take(&mut current_info),
                    metadata,
                    body: LineEnding::normalize(&body),
                });
            }
            _ => {}
        }
    }

    if in_block {
        bail!("markdown:{current_line_no}: code fence is never closed");
    }
    Ok(blocks)
}

fn skip_token(info: &str) -> &str {
    let first = info.split_whitespace().next().unwrap_or("");
    info.trim_start()[first.len()..].trim_start()
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
                    "unknown fence metadata key '{other}' (supported: env, roots, expect_error)"
                )));
            }
        }
    }
    Ok(metadata)
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
    fn parses_env_and_roots_metadata() {
        let md = concat!(
            "```oxdock env:A=1 env:B=two roots:unified\n",
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
