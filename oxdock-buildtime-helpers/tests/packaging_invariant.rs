//! Packaging invariants for the build-script asset pipeline.
//!
//! The historical proc-macro pipeline refused to run without `.git` or
//! `CARGO_PRIMARY_PACKAGE` and needed `OXDOCK_EMBED_FORCE_REBUILD` to bust
//! cached artifacts. The build-script pipeline has no gates by construction;
//! these tests pin that behavior.

use std::collections::BTreeSet;

use anyhow::Result;
use oxdock_buildtime_helpers::{DslSource, EmbedSpec, execution_is_skipped_with, try_embed_assets};
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_sys_test_utils::TestEnvGuard;

/// Materialize a minimal package tree (build.rs + DSL + input file) under a
/// fresh tempdir and point the process env at it.
fn setup_package(tag: &str, with_git: bool) -> Result<oxdock_fs::GuardedTempDir> {
    let temp = GuardedPath::tempdir()?;
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new_guarded(root.clone(), root.clone())?;

    let manifest =
        format!("[package]\nname = \"fixture-{tag}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n");
    resolver.write_file(&root.join("Cargo.toml")?, manifest.as_bytes())?;
    resolver.write_file(&root.join("build.rs")?, b"fn main() {}\n")?;
    resolver.write_file(
        &root.join("assets.oxdock")?,
        b"COPY in.txt copied.txt\nWRITE out/generated.txt body\n",
    )?;
    resolver.write_file(&root.join("in.txt")?, b"payload\n")?;

    if with_git {
        resolver.create_dir_all(&root.join(".git")?)?;
        resolver.write_file(&root.join(".git/HEAD")?, b"ref: refs/heads/main\n")?;
    }

    Ok(temp)
}

fn run_embed(manifest_dir: &str, out_dir: &str) -> Result<(Vec<String>, String)> {
    let _manifest = TestEnvGuard::set("CARGO_MANIFEST_DIR", manifest_dir);
    let _out = TestEnvGuard::set("OUT_DIR", out_dir);
    let spec = EmbedSpec::new("DemoAssets", DslSource::File("assets.oxdock"))
        .subdir("prebuilt")
        .extra_input("in.txt");
    let directives = try_embed_assets(&spec)?;

    let out_root = GuardedPath::new_root_from_str(out_dir)?;
    let module = out_root.join("__oxdock_embed_DemoAssets.rs")?;
    let resolver = PathResolver::new_guarded(out_root.clone(), out_root.clone())?;
    let bytes = resolver.read_to_string(&module)?;
    Ok((directives, bytes))
}

fn digits_stripped(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_digit() { '0' } else { c })
        .collect()
}

#[test]
fn identical_behavior_without_git_or_primary_package() -> Result<()> {
    let with_git = setup_package("invariant_git", true)?;
    let without_git = setup_package("invariant_nogit", false)?;

    let out_a = with_git
        .as_guarded_path()
        .join("target-out-a")?
        .display()
        .to_string();
    let out_b = without_git
        .as_guarded_path()
        .join("target-out-b")?
        .display()
        .to_string();

    let (dirs_a, module_a) = run_embed(
        with_git.as_guarded_path().display().to_string().as_str(),
        &out_a,
    )?;
    let (dirs_b, module_b) = run_embed(
        without_git.as_guarded_path().display().to_string().as_str(),
        &out_b,
    )?;

    // Directives must match modulo the differing manifest roots.
    let normalize = |dirs: &[String], root: &str| -> Vec<String> {
        dirs.iter()
            .map(|d| d.replace(root, "<ROOT>"))
            .collect::<Vec<_>>()
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    assert_eq!(
        normalize(
            &dirs_a,
            with_git.as_guarded_path().display().to_string().as_str()
        ),
        normalize(
            &dirs_b,
            without_git.as_guarded_path().display().to_string().as_str()
        ),
        "directive sets must be identical regardless of .git presence"
    );

    // Generated modules must be structurally identical: strip each run's own
    // paths (sandbox + OUT_DIR differ per run) and timestamps before comparing.
    let normalize_module =
        |module: &str, out: &str| -> String { digits_stripped(&module.replace(out, "<OUT>")) };
    assert_eq!(
        normalize_module(&module_a, &out_a),
        normalize_module(&module_b, &out_b),
        "generated modules must be byte-identical modulo timestamps/paths"
    );
    // Marker infrastructure files must never leak into assets.
    assert!(
        !module_a.contains(".oxdock-tempdir"),
        "sandbox markers must not be embedded"
    );
    Ok(())
}

#[test]
fn directives_are_complete_and_ordered() -> Result<()> {
    let pkg = setup_package("directives_golden", false)?;
    let root_display = pkg.as_guarded_path().display().to_string();
    let out_dir = pkg
        .as_guarded_path()
        .join("out-golden")?
        .display()
        .to_string();
    let (dirs, _module) = run_embed(&root_display, &out_dir)?;

    assert!(!dirs.is_empty());
    assert!(
        dirs[0].starts_with("cargo:rerun-if-changed=") && dirs[0].ends_with("/build.rs"),
        "first directive must watch build.rs, got: {:?}",
        dirs[0]
    );
    assert!(
        dirs.iter().any(|d| d.ends_with("/assets.oxdock")),
        "DSL file must be watched: {dirs:?}"
    );
    assert!(
        dirs.iter().any(|d| d.ends_with("/in.txt")),
        "COPY source must be watched: {dirs:?}"
    );
    assert!(
        !dirs.iter().any(|d| d.contains("generated.txt")),
        "WRITE targets are outputs, not inputs: {dirs:?}"
    );
    Ok(())
}

#[test]
fn skip_predicate_branch_matrix() {
    let env = |key: &str| -> Option<String> {
        match key {
            "RUSTFLAGS" => Some("--cfg miri".into()),
            "VSCODE_PID" => Some("1234".into()),
            _ => None,
        }
    };
    assert!(execution_is_skipped_with(|_| Some("1".into()), || None));
    assert!(execution_is_skipped_with(env, || None));
    assert!(execution_is_skipped_with(
        |_| None,
        || Some("/path/to/rust-analyzer".into())
    ));
    // VSCODE_PID without TERM.
    assert!(execution_is_skipped_with(
        |k| (k == "VSCODE_PID").then(|| "1".into()),
        || None
    ));
    // Interactive terminal is not a skip.
    assert!(!execution_is_skipped_with(
        |k| match k {
            "VSCODE_PID" => Some("1".into()),
            "TERM" => Some("xterm-256color".into()),
            _ => None,
        },
        || None
    ));
    // RUSTFLAGS unrelated to Miri must not trigger the skip.
    assert!(!execution_is_skipped_with(
        |k| (k == "RUSTFLAGS").then(|| "-Dwarnings".to_string()),
        || None
    ));
    // Standard non-rust-analyzer executables must not trigger the skip.
    assert!(!execution_is_skipped_with(
        |_| None,
        || Some("/bin/cargo".into())
    ));
}
