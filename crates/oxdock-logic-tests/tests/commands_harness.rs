#![cfg(feature = "slow-integration")]

#[cfg(not(miri))]
use libtest_mimic::Arguments;
#[cfg(not(miri))]
use oxdock_fs::{GuardedPath, PathResolver};
#[cfg(not(miri))]
use oxdock_logic_tests::harness::{HarnessConfig, build_trials};

#[cfg(miri)]
fn main() {
    eprintln!(
        "Skipping commands fixture harness under Miri: requires cargo execution and fixture filesystem access."
    );
}

#[cfg(not(miri))]
fn main() {
    let mut args = Arguments::from_args();
    args.test_threads = Some(1);

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

    let temp_target = GuardedPath::tempdir().unwrap_or_else(|err| {
        eprintln!("commands harness failed to create temp target dir: {err:#}");
        std::process::exit(1);
    });
    let shared_target = temp_target.as_guarded_path().clone();

    let mut config = HarnessConfig::new("commands", fixtures_root);
    config.set_workspace_root_env = true;
    config.set_temp_target_dir = true;
    config.shared_target_dir = Some(shared_target);
    config.case_config = Some(oxdock_logic_tests::harness::CaseConfig {
        fixture_name: "ast_commands".to_string(),
        cases_dir: "cases".to_string(),
        case_env: "OXDOCK_AST_CASE".to_string(),
        coverage_env: Some("OXDOCK_AST_ONLY_COVERAGE".to_string()),
        coverage_case_name: "coverage".to_string(),
    });

    let tests = build_trials(&resolver, &config).unwrap_or_else(|err| {
        eprintln!("commands harness failed to discover fixtures: {err:#}");
        std::process::exit(1);
    });

    let result = libtest_mimic::run(&args, tests);
    drop(temp_target);
    result.exit();
}
