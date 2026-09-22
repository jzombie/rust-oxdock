//! Thin wrapper around [`oxdock_logic_tests::ast_runner`].
//!
//! The case-spec loading, execution, and verification logic lives in the
//! `oxdock-logic-tests` library so the `commands_harness` trials can run
//! cases in-process. This binary preserves the historical `cargo run`
//! behavior (env-var dispatch, `fixture failed:` stderr format) for manual
//! debugging.
use anyhow::{Context, Result};
use oxdock_fs::PathResolver;
use oxdock_fs::env as oxdock_env;
use oxdock_logic_tests::{ast_runner, format_fixture_stderr};

fn main() {
    if let Err(err) = run() {
        eprintln!("{}", format_fixture_stderr(&err));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let resolver = PathResolver::from_manifest_env().context("resolve fixture manifest dir")?;
    let case_filter = std::env::var(oxdock_env::AST_CASE).ok();
    let only_coverage = std::env::var_os(oxdock_env::AST_ONLY_COVERAGE).is_some();
    ast_runner::run_ast_suites(resolver.root(), case_filter.as_deref(), only_coverage)
}
