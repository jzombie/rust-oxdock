//! Shell-oracle parity for the next-release text migration (#203).
//!
//! Each test runs a workflow's shell pipeline verbatim through `sh`
//! and asserts the `TEXT_*` transcription answers identically, so the
//! eventual migration is a pure transcription. Unix-gated: the oracles
//! are POSIX pipelines with no Windows spelling, while the DSL side
//! already runs on every platform under the core suites and doc
//! conformance. `sh`, `awk`, `sed`, `grep`, `cut`, and `tr` are POSIX
//! givens on the unix runners, so no tool probing is needed.

use anyhow::{Context, Result};
use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::GuardedPath;

fn run_script(script: &str) -> Result<()> {
    let steps = oxdock_core::parse_script(script).expect("migration script parses");
    let workspace_temp = GuardedPath::tempdir().context("workspace tempdir")?;
    let context_temp = GuardedPath::tempdir().context("context tempdir")?;
    let fs_root = workspace_temp.as_guarded_path().clone();
    let context_root = context_temp.as_guarded_path().clone();
    run_steps_with_context_result_with_io(&fs_root, &context_root, &steps, ExecIo::new())
        .map(|_| ())
}

/// smoke-version `${tag#v}` (standing in for the setup-oxdock
/// `TrimStart('v')` bridge, same single-strip semantics on real tags).
#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns a real sh process")]
fn tag_prefix_strip_matches_shell_expansion() -> Result<()> {
    run_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $raw: STRING = RUN ["sh", "-c", "tag=v0.27.1-alpha; printf '%s' \"${tag#v}\""]
        LET $dsl: STRING = TEXT_TRIM_PREFIX("v0.27.1-alpha", "v")
        ASSERT_EQ $dsl $raw
    "#})
}

/// install-smoke `grep -q "^$hash "`: the anchored SHA256SUMS lookup,
/// including the substring guard (a hash that merely prefixes another
/// line must not match without the space).
#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns real sh and grep processes")]
fn checksum_line_lookup_matches_grep_anchor() -> Result<()> {
    run_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $line: STRING = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  oxdock-x86_64-unknown-linux-musl.tar.gz"
        LET $raw: STRING = RUN ["sh", "-c", "if printf '%s\n' 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  oxdock-x86_64-unknown-linux-musl.tar.gz' | grep -q '^e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 '; then echo true; else echo false; fi"]
        LET $shell: STRING = TEXT_TRIM($raw)
        LET $hit: BOOL = TEXT_MATCH($line, "^e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 ")
        ASSERT_EQ "{{ $hit }}" $shell

        # A hash that prefixes the line without the space still misses.
        LET $raw2: STRING = RUN ["sh", "-c", "if printf '%s\n' 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  oxdock-x86_64-unknown-linux-musl.tar.gz' | grep -q '^e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85 '; then echo true; else echo false; fi"]
        LET $shell2: STRING = TEXT_TRIM($raw2)
        LET $short: STRING = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85"
        LET $guard: BOOL = TEXT_MATCH($line, "^e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85 ")
        ASSERT_EQ "{{ $guard }}" $shell2
    "#})
}

/// `sha256sum`/`shasum | awk '{print $1}'`: the lockfile hash legs in
/// lint, tests, and miri read the first whitespace field.
#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns real sh and awk processes")]
fn checksum_field_split_matches_awk_first_field() -> Result<()> {
    run_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $raw: STRING = RUN ["sh", "-c", "printf '%s\n' 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  Cargo.lock' | awk '{print $1}'"]
        LET $shell: STRING = TEXT_TRIM($raw)
        LET $fields: LIST<STRING> = TEXT_FIELDS("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  Cargo.lock")
        ASSERT_EQ $fields.0 $shell
    "#})
}

/// smoke-version `grep '"tag_name"' | cut -d'"' -f4` plus `${tag#v}`:
/// the newest-release tag extraction and its prefix strip.
#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns real sh, grep, and cut processes")]
fn tag_extract_matches_grep_cut_chain() -> Result<()> {
    run_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $raw: STRING = RUN ["sh", "-c", "printf '%s\n' '  \"tag_name\": \"v0.27.1-alpha\",' | grep '\"tag_name\"' | cut -d'\"' -f4"]
        LET $shell: STRING = TEXT_TRIM($raw)
        LET $caps: LIST<STRING> = TEXT_EXTRACT("  \"tag_name\": \"v0.27.1-alpha\",", "\"tag_name\": \"([^\"]+)\"")
        LET $dsl: STRING = TEXT_TRIM_PREFIX($caps.1, "v")
        LET $bare: STRING = TEXT_TRIM_PREFIX($shell, "v")
        ASSERT_EQ $dsl $bare
    "#})
}

/// `rustc -Vv | sed -n 's/^commit-hash: //p'`: the toolchain hash legs
/// read one labeled line out of the version report.
#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns real sh and sed processes")]
fn commit_hash_extract_matches_sed_line_pick() -> Result<()> {
    run_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $raw: STRING = RUN ["sh", "-c", "printf '%s\n' 'rustc 1.99.0 (5f94df478 2026-08-27)' 'binary: rustc' 'commit-hash: 5f94df478b8a7f5e0078f8e85d14300e5814e9b6' | sed -n 's/^commit-hash: //p'"]
        LET $shell: STRING = TEXT_TRIM($raw)
        LET $report: STRING = "rustc 1.99.0 (5f94df478 2026-08-27)\nbinary: rustc\ncommit-hash: 5f94df478b8a7f5e0078f8e85d14300e5814e9b6"
        LET $caps: LIST<STRING> = TEXT_EXTRACT($report, "(?m)^commit-hash: (\\S+)")
        ASSERT_EQ $caps.1 $shell
    "#})
}

/// `awk '/^TOTAL/ {print $10}' | tr -d '%'`: the coverage leg reads the
/// tenth field of the TOTAL row and strips the percent sign.
#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns real sh, awk, and tr processes")]
fn coverage_total_parse_matches_awk_row_pick() -> Result<()> {
    run_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $raw: STRING = RUN ["sh", "-c", "printf '%s\n' 'TOTAL 100 0 100% 50 0 100% 200 0 85.71%' | awk '/^TOTAL/ {print $10}' | tr -d '%'"]
        LET $shell: STRING = TEXT_TRIM($raw)
        LET $fields: LIST<STRING> = TEXT_FIELDS("TOTAL 100 0 100% 50 0 100% 200 0 85.71%")
        LET $dsl: STRING = TEXT_TRIM_SUFFIX($fields.9, "%")
        ASSERT_EQ $dsl $shell

        # Detail rows never select, like awk skipping non-TOTAL lines.
        LET $skip: BOOL = TEXT_MATCH("src/main.rs 10 0 100%", "^TOTAL")
        ASSERT_EQ $skip false

        # No TOTAL row defaults to zero, like the -z fallback.
        LET $found: BOOL = TEXT_MATCH("src/main.rs 10 0 100%", "(?m)^TOTAL")
        LET $pct: STRING = "0"
        IF $found {
            $pct = "unreachable"
        }
        ASSERT_EQ $pct "0"
    "#})
}

/// `sed -n '2,3p'`: the line-window read over a text body.
#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns real sh and sed processes")]
fn text_lines_range_matches_sed_window() -> Result<()> {
    run_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $raw: STRING = RUN ["sh", "-c", "printf '%s\n' 'one' 'two' 'three' 'four' | sed -n '2,3p'"]
        LET $shell: STRING = TEXT_TRIM($raw)
        LET $window: LIST<STRING> = TEXT_LINES_RANGE("one\ntwo\nthree\nfour", 2, 3)
        LET $dsl: STRING = TEXT_JOIN($window, "\n")
        ASSERT_EQ $dsl $shell

        # Past-the-end clamps on both sides of the transcription.
        LET $raw2: STRING = RUN ["sh", "-c", "printf '%s\n' 'one' 'two' | sed -n '2,9p'"]
        LET $shell2: STRING = TEXT_TRIM($raw2)
        LET $tail: LIST<STRING> = TEXT_LINES_RANGE("one\ntwo", 2, 9)
        LET $dsl2: STRING = TEXT_JOIN($tail, "\n")
        ASSERT_EQ $dsl2 $shell2
    "#})
}

/// rust-release `endsWith`/`startsWith` runner expressions have no shell
/// spelling, so the mapping proof is the truth table itself: moving a
/// branch into an oxdock leg preserves every answer. Platform-free.
#[test]
fn target_predicates_match_runner_expressions() -> Result<()> {
    run_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        # endsWith(matrix.target, 'linux-musl'): musl legs go static.
        LET $musl: BOOL = TEXT_ENDS_WITH("x86_64-unknown-linux-musl", "linux-musl")
        ASSERT_EQ $musl true
        LET $gnu: BOOL = TEXT_ENDS_WITH("x86_64-unknown-linux-gnu", "linux-musl")
        ASSERT_EQ $gnu false

        # startsWith(matrix.os, 'windows'): windows legs set crt-static.
        LET $win: BOOL = TEXT_STARTS_WITH("windows-latest", "windows")
        ASSERT_EQ $win true
        LET $nix: BOOL = TEXT_STARTS_WITH("ubuntu-latest", "windows")
        ASSERT_EQ $nix false
    "#})
}

/// Reusable-workflow ref parsing (`${VAR##*@}`, `${ref#refs/tags/}`,
/// `${ref#refs/heads/}` in docs.yml and rust-release.yml): the
/// longest-prefix strip takes everything through the last `@`, then
/// the tag and head prefixes fall away in order.
#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns a real sh process")]
fn workflow_ref_parse_matches_shell_expansion() -> Result<()> {
    run_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $raw: STRING = RUN ["sh", "-c", "ref='octo/repo/.github/workflows/ci.yml@refs/tags/v0.27.1'; ref=\"${ref##*@}\"; ref=\"${ref#refs/tags/}\"; ref=\"${ref#refs/heads/}\"; printf '%s' \"$ref\""]
        LET $full: STRING = "octo/repo/.github/workflows/ci.yml@refs/tags/v0.27.1"
        LET $at: LIST<STRING> = TEXT_EXTRACT($full, "@([^@]+)$")
        LET $dsl: STRING = TEXT_TRIM_PREFIX(TEXT_TRIM_PREFIX($at.1, "refs/tags/"), "refs/heads/")
        ASSERT_EQ $dsl $raw

        # A branch ref keeps its name once the heads prefix falls away.
        LET $raw2: STRING = RUN ["sh", "-c", "ref='octo/repo/.github/workflows/ci.yml@refs/heads/main'; ref=\"${ref##*@}\"; ref=\"${ref#refs/tags/}\"; ref=\"${ref#refs/heads/}\"; printf '%s' \"$ref\""]
        LET $full2: STRING = "octo/repo/.github/workflows/ci.yml@refs/heads/main"
        LET $at2: LIST<STRING> = TEXT_EXTRACT($full2, "@([^@]+)$")
        LET $dsl2: STRING = TEXT_TRIM_PREFIX(TEXT_TRIM_PREFIX($at2.1, "refs/tags/"), "refs/heads/")
        ASSERT_EQ $dsl2 $raw2
        ASSERT_EQ $dsl2 "main"
    "#})
}

/// rust-release `tr '-' '_'`: the target-to-env-var translation is a
/// single-character literal replacement.
#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns real sh and tr processes")]
fn target_env_translation_matches_tr() -> Result<()> {
    run_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $raw: STRING = RUN ["sh", "-c", "printf '%s' 'x86_64-unknown-linux-musl' | tr '-' '_'"]
        LET $dsl: STRING = TEXT_REPLACE("x86_64-unknown-linux-musl", "-", "_")
        ASSERT_EQ $dsl $raw
    "#})
}

/// `${#var}`: tag and digest sizing without shell arithmetic.
#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns a real sh process")]
fn text_length_matches_shell_count() -> Result<()> {
    run_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $raw: STRING = RUN ["sh", "-c", "tag=v0.27.1-alpha; printf '%s' \"${#tag}\""]
        LET $shell: STRING = TEXT_TRIM($raw)
        LET $n: INT = TEXT_LENGTH("v0.27.1-alpha")
        ASSERT_EQ "{{ $n }}" $shell
    "#})
}

/// `grep -o 'aa' | wc -l`: non-overlapping occurrence counting.
#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns real sh, grep, and wc processes")]
fn text_count_matches_grep_o_wc() -> Result<()> {
    run_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $raw: STRING = RUN ["sh", "-c", "printf '%s' 'aaa' | grep -o 'aa' | wc -l"]
        LET $shell: STRING = TEXT_TRIM($raw)
        LET $n: INT = TEXT_COUNT("aaa", "aa")
        ASSERT_EQ "{{ $n }}" $shell
    "#})
}

/// `grep -c ': test'`: the miri-badge test counting shape.
#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns real sh and grep processes")]
fn text_count_matches_matches_grep_c() -> Result<()> {
    run_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $raw: STRING = RUN ["sh", "-c", "printf '%s\n' 'test a: test' 'test b: nope' | grep -c ': test'"]
        LET $shell: STRING = TEXT_TRIM($raw)
        LET $n: INT = TEXT_COUNT_MATCHES("test a: test\ntest b: nope", ": test")
        ASSERT_EQ "{{ $n }}" $shell
    "#})
}

/// plain `grep '^one'`: matching lines out, misses print nothing.
#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns real sh and grep processes")]
fn text_grep_matches_grep_lines() -> Result<()> {
    run_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $raw: STRING = RUN ["sh", "-c", "printf '%s\n' 'one' 'two' 'one more' | grep '^one'"]
        LET $shell: STRING = TEXT_TRIM($raw)
        LET $hits: LIST<STRING> = TEXT_GREP("one\ntwo\none more", "^one")
        LET $dsl: STRING = TEXT_JOIN($hits, "\n")
        ASSERT_EQ $dsl $shell
    "#})
}

/// `grep -v '^one'` via `{invert: true}`: non-matching lines out.
#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns real sh and grep processes")]
fn text_grep_invert_matches_grep_v() -> Result<()> {
    run_script(indoc! {r#"
        IMPORT [STD]
        INHERIT_ENV [PATH]
        LET $raw: STRING = RUN ["sh", "-c", "printf '%s\n' 'one' 'two' 'one more' | grep -v '^one'"]
        LET $shell: STRING = TEXT_TRIM($raw)
        LET $kept: LIST<STRING> = TEXT_GREP("one\ntwo\none more", "^one", {invert: true})
        LET $dsl: STRING = TEXT_JOIN($kept, "\n")
        ASSERT_EQ $dsl $shell
    "#})
}
