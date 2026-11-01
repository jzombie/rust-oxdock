//! Multi-line `"""` string literals (#155): runtime conformance.
//!
//! Block payloads lower once in the parser (CRLF normalize, dedent,
//! trim) and expand exactly like double-quoted strings, so these
//! scripts assert behavior inside the DSL itself.

use anyhow::{Context, Result};
use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::GuardedPath;

fn run_script(script: &str) -> Result<()> {
    let steps = oxdock_core::parse_script(script).expect("block script parses");
    let workspace_temp = GuardedPath::tempdir().context("workspace tempdir")?;
    let context_temp = GuardedPath::tempdir().context("context tempdir")?;
    let fs_root = workspace_temp.as_guarded_path().clone();
    let context_root = context_temp.as_guarded_path().clone();
    run_steps_with_context_result_with_io(&fs_root, &context_root, &steps, ExecIo::new())
        .map(|_| ())
}

/// WRITE/READ round-trips block bytes exactly, matching `\n` escapes.
#[test]
fn block_string_write_read_round_trip() -> Result<()> {
    run_script(indoc! {r#"
        LET $prompt: STRING = """
            SELECT *
            FROM users
            """
        WRITE prompt.txt $prompt
        LET $back: STRING = READ prompt.txt
        ASSERT_EQ $back $prompt
        ASSERT_EQ $back "SELECT *\nFROM users"
    "#})
}

/// `{{ }}` interpolates inside blocks; indentation dedents first.
#[test]
fn block_string_interpolates() -> Result<()> {
    run_script(indoc! {r#"
        LET $name: STRING = "world"
        LET $greeting: STRING = """
            hello {{ $name }}
            bye {{ $name }}
            """
        ASSERT_EQ $greeting "hello world\nbye world"
    "#})
}

/// Relative indent survives dedent; blank lines keep shape.
#[test]
fn block_string_keeps_relative_indent() -> Result<()> {
    run_script(indoc! {r#"
        LET $cfg: STRING = """
            outer
                inner

            tail
            """
        ASSERT_EQ $cfg "outer\n    inner\n\ntail"
    "#})
}

/// Single and double quotes need no escaping; `\"\"\"` defers to runtime.
#[test]
fn block_string_quotes_and_escaped_delimiter() -> Result<()> {
    run_script(indoc! {r#"
        LET $quotes: STRING = """
            say "hi" and 'bye'
            """
        ASSERT_EQ $quotes "say \"hi\" and 'bye'"
        LET $fence: STRING = """
            code \"\"\" here
            """
        ASSERT_EQ $fence "code \"\"\" here"
    "#})
}
