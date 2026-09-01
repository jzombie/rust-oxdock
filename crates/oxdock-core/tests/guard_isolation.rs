use std::collections::HashMap;

use oxdock_parser::{Guard, guard_allows};

#[test]
fn test_guard_evaluator_accepts_only_script_env_map() {
    let guard = Guard::EnvEquals {
        key: "STAGE".to_string(),
        value: "prod".to_string(),
    };

    let ambient_env: HashMap<String, String> = HashMap::new();
    let allowed: bool = guard_allows(&guard, &ambient_env);
    assert!(!allowed, "empty env should not match STAGE==prod");

    let mut prod_env = HashMap::new();
    prod_env.insert("STAGE".to_string(), "prod".to_string());
    assert!(guard_allows(&guard, &prod_env), "STAGE=prod should match");
}
