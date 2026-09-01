use indoc::indoc;
use oxdock_fs::GuardedPath;

fn run_script(root: &GuardedPath, script: &str) -> Result<(), anyhow::Error> {
    let steps = oxdock_core::parse_script(script).expect("parse script");
    oxdock_core::run_steps(root, &steps).map_err(|e| anyhow::anyhow!("{e}"))
}

#[test]
fn test_guard_evaluates_at_runtime_against_script_env() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    // ENV STAGE=prod mutates the script-level env map.
    // The guard [eq(env:STAGE, prod)] evaluates at runtime against that map
    // (not during static AST analysis), so the guarded block executes.
    let script = indoc! {r#"
        ENV STAGE=prod
        [eq(env:STAGE, prod)] {
            WRITE out.txt "guarded"
        }
    "#};

    run_script(&root, script).unwrap();

    // out.txt must exist because the guard was satisfied at runtime.
    assert!(
        root.join("out.txt").unwrap().exists(),
        "guarded block should execute when env matches at runtime"
    );
}

#[test]
fn test_guard_skips_block_when_env_does_not_match() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();

    // STAGE is set to "dev", but the guard requires "prod".
    let script = indoc! {r#"
        ENV STAGE=dev
        [eq(env:STAGE, prod)] {
            WRITE out.txt "guarded"
        }
    "#};

    run_script(&root, script).unwrap();

    // out.txt must not exist because the guard failed at runtime.
    assert!(
        !root.join("out.txt").unwrap().exists(),
        "guarded block should be skipped when env does not match"
    );
}
