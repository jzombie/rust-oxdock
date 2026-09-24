#[cfg(not(miri))]
use libtest_mimic::Arguments;
#[cfg(not(miri))]
use oxdock_fs::PathResolver;
#[cfg(not(miri))]
use oxdock_logic_tests::harness::{HarnessConfig, build_trials, prefer_tmpfs_for_tempdirs};

#[cfg(miri)]
fn main() {
    eprintln!(
        "Skipping commands fixture harness under Miri: requires cargo execution and fixture filesystem access."
    );
}

#[cfg(not(miri))]
fn main() {
    // Process-global setup first: must precede the libtest-mimic worker pool.
    prefer_tmpfs_for_tempdirs();

    // No `test_threads` override: `ast_commands` trials run the shared
    // in-process runner against isolated tempdirs, so they scale across all
    // available cores. (The legacy per-trial `cargo run` fast path that
    // required serial execution has been removed.)
    let args = Arguments::from_args();

    let resolver = PathResolver::from_manifest_env().unwrap_or_else(|err| {
        eprintln!("commands harness failed to resolve manifest dir: {err:#}");
        std::process::exit(1);
    });

    let fixtures_root = resolver
        .root()
        .join("fixtures")
        .and_then(|root| root.join("commands"))
        .unwrap_or_else(|err| {
            eprintln!("commands harness failed to resolve fixtures root: {err:#}");
            std::process::exit(1);
        });

    let mut config = HarnessConfig::new("commands", fixtures_root);
    config.set_workspace_root_env = true;
    config.case_config = Some(oxdock_logic_tests::harness::CaseConfig {
        fixture_name: "ast_commands".to_string(),
        cases_dir: "cases".to_string(),
        case_env: oxdock_fs::env::AST_CASE.to_string(),
        coverage_env: Some(oxdock_fs::env::AST_ONLY_COVERAGE.to_string()),
        coverage_case_name: "coverage".to_string(),
        smoke_cases: vec!["write".to_string(), "with_io".to_string()],
    });

    let tests = build_trials(&resolver, &config).unwrap_or_else(|err| {
        eprintln!("commands harness failed to discover fixtures: {err:#}");
        std::process::exit(1);
    });

    libtest_mimic::run(&args, tests).exit();
}
