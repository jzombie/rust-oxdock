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

    // In full-suite mode, pre-compile ast_commands once to avoid per-case cargo overhead
    #[allow(clippy::disallowed_macros)]
    let precompiled = if cfg!(feature = "slow-integration") {
        #[allow(
            clippy::disallowed_types,
            clippy::disallowed_methods,
            clippy::disallowed_macros
        )]
        {
            let template = fixtures_root
                .join("ast_commands")
                .expect("resolve ast_commands template");

            let mut build_cmd = std::process::Command::new("cargo");
            build_cmd.arg("build");
            build_cmd.arg("--message-format=json-render-diagnostics");
            build_cmd.current_dir(template.as_path());
            let target_dir = oxdock_fs::command_path(&shared_target).into_owned();
            build_cmd.env("CARGO_TARGET_DIR", target_dir);

            let output = build_cmd
                .output()
                .expect("failed to run cargo build for ast_commands");
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!("pre-compilation of ast_commands failed:\n{stderr}");
                std::process::exit(1);
            }
            let stdout = String::from_utf8_lossy(&output.stdout);

            // Parse Cargo JSON stream — use artifact.executable for cross-platform path resolution
            let binary = stdout
                .lines()
                .filter_map(|line| serde_json::from_str::<cargo_metadata::Message>(line).ok())
                .filter_map(|msg| match msg {
                    cargo_metadata::Message::CompilerArtifact(artifact)
                        if artifact.target.name == "ast_commands" =>
                    {
                        artifact.executable
                    }
                    _ => None,
                })
                .next()
                .map(|p| {
                    p.into_std_path_buf()
                        .canonicalize()
                        .expect("canonicalize binary path")
                })
                .unwrap_or_else(|| {
                    eprintln!("ast_commands executable not found in cargo build output");
                    std::process::exit(1);
                });

            eprintln!("pre-compiled ast_commands: {}", binary.display());
            Some(binary)
        }
    } else {
        None
    };

    let mut config = HarnessConfig::new("commands", fixtures_root);
    config.set_workspace_root_env = true;
    config.set_temp_target_dir = true;
    config.shared_target_dir = Some(shared_target);
    config.precompiled_binary = precompiled;
    config.case_config = Some(oxdock_logic_tests::harness::CaseConfig {
        fixture_name: "ast_commands".to_string(),
        cases_dir: "cases".to_string(),
        case_env: "OXDOCK_AST_CASE".to_string(),
        coverage_env: Some("OXDOCK_AST_ONLY_COVERAGE".to_string()),
        coverage_case_name: "coverage".to_string(),
        smoke_cases: vec!["write".to_string(), "with_io".to_string()],
    });

    let tests = build_trials(&resolver, &config).unwrap_or_else(|err| {
        eprintln!("commands harness failed to discover fixtures: {err:#}");
        std::process::exit(1);
    });

    let result = libtest_mimic::run(&args, tests);
    drop(temp_target);
    result.exit();
}
