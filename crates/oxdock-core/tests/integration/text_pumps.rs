//! Streaming `TEXT_*` pipe pumps: mirror ops run on pump threads with
//! O(chunk) memory, so payloads crossing chunk reads (and the old eager
//! cap) stream end to end instead of refusing.

use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, GuardedTempDir};

fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
    temp.as_guarded_path().clone()
}

fn run_script(root: &GuardedPath, script: &str) -> Result<(), anyhow::Error> {
    let steps = oxdock_core::parse_script(script).expect("parse script");
    run_steps_with_context_result_with_io(root, root, &steps, ExecIo::new()).map(|_| ())
}

#[test]
fn text_pumps_mirror_small_pipes() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        LET $cap: PIPE
        WITH_IO [stdout=$cap] {
            ECHO "  hello world  "
        }
        LET $trimmed: PIPE = TEXT_TRIM($cap)
        LET $loud: PIPE = TEXT_TO_UPPER($trimmed)
        LET $swapped: PIPE = TEXT_REPLACE($loud, "WORLD", "THERE")
        WITH_IO [stdin=$swapped] READ_LINE $line
        ASSERT_EQ $line "HELLO THERE"
    "#};
    run_script(&root, script).expect("chained mirror pumps");
}

#[test]
fn text_pumps_stream_past_chunk_reads() {
    // 20KB single line: every 8KB chunk read cuts mid-content, and the
    // payload sails past the old eager cap with O(chunk) memory.
    let payload = "ab12".repeat(5000);
    let expected = payload.to_uppercase();
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let template = indoc! {r#"
        IMPORT [STD]
        LET $cap: PIPE
        WITH_IO [stdout=$cap] {
            ECHO "@PAYLOAD@"
        }
        LET $trimmed: PIPE = TEXT_TRIM($cap)
        LET $out: PIPE = TEXT_TO_UPPER($trimmed)
        WITH_IO [stdin=$out] READ_LINE $line
        ASSERT_EQ $line "@EXPECTED@"
    "#};
    let script = template
        .replace("@PAYLOAD@", &payload)
        .replace("@EXPECTED@", &expected);
    run_script(&root, &script).expect("20KB streams through pumps");
}

#[test]
fn text_pump_replace_rewrites_needles_across_chunks() {
    // 20000 a's: chunk cuts land inside "aa" needles throughout, and
    // the sliding window still rewrites every occurrence in order.
    let payload = "a".repeat(20_000);
    let expected = "b".repeat(10_000);
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let template = indoc! {r#"
        IMPORT [STD]
        LET $cap: PIPE
        WITH_IO [stdout=$cap] {
            ECHO "@PAYLOAD@"
        }
        LET $trimmed: PIPE = TEXT_TRIM($cap)
        LET $out: PIPE = TEXT_REPLACE($trimmed, "aa", "b")
        WITH_IO [stdin=$out] READ_LINE $line
        ASSERT_EQ $line "@EXPECTED@"
    "#};
    let script = template
        .replace("@PAYLOAD@", &payload)
        .replace("@EXPECTED@", &expected);
    run_script(&root, &script).expect("cross-chunk needles rewrite");
}

#[test]
fn text_pump_prefix_suffix_strip_over_pipes() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        LET $cap: PIPE
        WITH_IO [stdout=$cap] {
            ECHO "v1.2.3"
        }
        LET $trimmed: PIPE = TEXT_TRIM($cap)
        LET $bare: PIPE = TEXT_TRIM_PREFIX($trimmed, "v")
        WITH_IO [stdin=$bare] READ_LINE $line
        ASSERT_EQ $line "1.2.3"

        LET $arc: PIPE
        WITH_IO [stdout=$arc] {
            ECHO "archive.tar.gz"
        }
        LET $trimmed_arc: PIPE = TEXT_TRIM($arc)
        LET $base: PIPE = TEXT_TRIM_SUFFIX($trimmed_arc, ".gz")
        WITH_IO [stdin=$base] READ_LINE $name
        ASSERT_EQ $name "archive.tar"
    "#};
    run_script(&root, script).expect("prefix and suffix pumps");
}

#[test]
fn text_replace_rejects_empty_search_at_call_site() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        LET $fixed: STRING = TEXT_REPLACE("hello", "", "x")
    "#};
    let err = run_script(&root, script).expect_err("empty search must fail");
    assert!(
        format!("{err:#}").contains("non-empty"),
        "error names the rule: {err:#}"
    );
}

#[test]
fn text_regex_single_backslash_is_a_digit_class() {
    // A single `\d` in a DSL string reaches the regex engine as a
    // digit class: "abc123" holds no `d`, so a literal `d+` reading
    // would miss and this assertion would fail.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        LET $hit: BOOL = TEXT_MATCH("abc123", "\d+")
        ASSERT_EQ $hit true
        LET $caps: LIST<STRING> = TEXT_EXTRACT("abc123", "([a-z]+)(\d+)")
        ASSERT_EQ $caps.2 "123"
    "#};
    run_script(&root, script).expect("single-backslash classes match");
}

#[test]
fn template_paths_agree_string_and_expand() {
    // The same placeholder through an ordinary string and through
    // EXPAND resolves identically: one grammar, one table, one
    // renderer. The WRITE escapes its braces so the file keeps the
    // literal placeholder for EXPAND to resolve.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        WRITE "tpl.txt" 'result:\{{ STD::TEXT_TRIM("  padded  ") }}'
        LET $via_expand: STRING = EXPAND "tpl.txt"
        LET $via_string: STRING = 'result:{{ STD::TEXT_TRIM("  padded  ") }}'
        ASSERT_EQ $via_string $via_expand
        ASSERT_EQ $via_expand "result:padded"
    "#};
    run_script(&root, script).expect("string and EXPAND templates agree");
}

#[test]
fn template_calls_reject_stateful_functions() {
    // Placeholder resolution serves the pure table only: stateful
    // entries like FUNCTIONS never appear there, in any template.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD]
        WRITE "tpl.txt" 'names:\{{ STD::FUNCTIONS() }}'
        LET $out: STRING = EXPAND "tpl.txt"
    "#};
    let err = run_script(&root, script).expect_err("stateful call in template must fail");
    assert!(
        format!("{err:#}").contains("unknown placeholder function 'STD::FUNCTIONS'"),
        "error names the missing registry entry: {err:#}"
    );
}
