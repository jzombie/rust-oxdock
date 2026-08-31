You enforce this architectural boundary using three distinct test strategies across your parser, core engine, and compile-time API signatures.

**1. Negative Parser Tests (Grammar Enforcement)**

Assert that attempting to use DSL runtime variable syntax (`$var`, `bool:$var`, `env:$var`) inside guard brackets `[...]` returns a parse error.

```rust
// crates/oxdock-parser/tests/guard_tests.rs

#[test]
fn test_guards_reject_runtime_variables() {
    let invalid_scripts = [
        "[bool:$match] { WRITE out.txt 1 }",
        "[$dsl_var] { WRITE out.txt 1 }",
        "[env:$dsl_var == \"val\"] { WRITE out.txt 1 }",
        "[not($var)] { WRITE out.txt 1 }",
    ];

    for script in invalid_scripts {
        assert!(
            oxdock_parser::parse_script(script).is_err(),
            "Expected script to fail parsing: {}",
            script
        );
    }
}

```

**2. Signature Isolation Audit (Compile-Time Boundary)**

Verify that your guard evaluation functions in `oxdock-parser` / `oxdock-core` accept **only** static environment maps (`&HashMap<String, String>`) and OS metadata, making it impossible for them to receive `ExecState` or variable scopes.

```rust
// crates/oxdock-core/tests/guard_isolation.rs

#[test]
fn test_guard_evaluator_has_no_exec_state_dependency() {
    use std::collections::HashMap;
    use oxdock_parser::ast::Guard;
    use oxdock_core::exec::guards::guard_allows;

    let mut static_env = HashMap::new();
    static_env.insert("STAGE".to_string(), "prod".to_string());

    let guard = Guard::EnvEq("STAGE".to_string(), "prod".to_string());

    // This proves evaluation is pure static metadata matching:
    // guard_allows(&guard, &static_env) signature cannot take ExecState
    assert!(guard_allows(&guard, &static_env));
}

```

**3. Phase 1 vs. Phase 2 Boundary Test (Engine Execution)**

Write an integration test proving that steps pruned by Phase 1 guards are removed **before** any Phase 2 script variables are assigned or evaluated.

```rust
// crates/oxdock-core/tests/phase_boundary.rs

#[test]
fn test_phase_1_pruning_ignores_mid_script_variable_mutations() {
    // Attempting to dynamically enable a guard mid-script via LET must fail
    let script = r#"
        LET $STAGE = "prod"
        [env:STAGE == "prod"] {
            WRITE result.txt "should_not_run_if_host_env_lacks_STAGE"
        }
    "#;

    let steps = oxdock_parser::parse_script(script).unwrap();
    let env = oxdock_core::ExecIo::default(); // Empty ambient env (STAGE is missing)
    let repo_root = std::env::current_dir().unwrap();

    // Run with empty ambient env
    oxdock_core::run_steps(repo_root, steps, env).unwrap();

    // The guard [env:STAGE == "prod"] evaluated in Phase 1 against ExecIo (empty),
    // NOT Phase 2 ExecState ($STAGE = "prod"). Therefore, result.txt was never written.
    assert!(!std::path::Path::new("result.txt").exists());
}

```

Do you want to add a `cargo clippy` or lint check to fail the build if `ExecState` is ever added as a parameter to any function in `guards.rs`?
