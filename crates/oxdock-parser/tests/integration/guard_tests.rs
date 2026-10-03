use oxdock_parser::{Guard, GuardExpr, Ns, parse_guard_expr_str};

#[test]
fn test_valid_guard_expressions() {
    let valid_guards = [
        "env:FOO",
        "eq(env:FOO, bar)",
        "ne(env:FOO, bar)",
        "bool:true",
        "family:unix",
        "not(family:windows)",
        "arch:x86_64",
        "not(arch:aarch64)",
        "any(env:A, env:B)",
        "any(eq(env:A, 1), os:linux)",
        "any(arch:x86_64, arch:aarch64)",
        "all(env:A, os:linux)",
        "all(arch:arm, os:linux)",
    ];

    for guard_str in valid_guards {
        let result = parse_guard_expr_str(guard_str);
        assert!(
            result.is_ok(),
            "Valid guard expression failed parsing: {guard_str}\n  error: {:?}",
            result.err()
        );
    }
}

#[test]
fn test_guards_reject_dollar_sign_runtime_variables() {
    let invalid_guards = ["bool:$match", "$dsl_var", "eq(env:$dsl_var, val)"];

    for guard_str in invalid_guards {
        assert!(
            parse_guard_expr_str(guard_str).is_err(),
            "Parser must reject '$' in guard positions: {guard_str}"
        );
    }
}

#[test]
fn test_eq_ne_guard_parsing() {
    // eq() produces GuardExpr::Predicate(EnvEquals)
    let expr = parse_guard_expr_str("eq(env:STAGE, prod)").unwrap();
    match &expr {
        GuardExpr::Predicate(Guard::Attr {
            ns: Ns::Env,
            key: Some(key),
            val: Some(value),
        }) => {
            assert_eq!(key, "STAGE");
            assert_eq!(value, "prod");
        }
        other => panic!("expected EnvEquals, got {other:?}"),
    }

    // ne() produces GuardExpr::Not(Predicate(EnvEquals))
    let expr = parse_guard_expr_str("ne(env:STAGE, prod)").unwrap();
    match &expr {
        GuardExpr::Not(inner) => match inner.as_ref() {
            GuardExpr::Predicate(Guard::Attr {
                ns: Ns::Env,
                key: Some(key),
                val: Some(value),
            }) => {
                assert_eq!(key, "STAGE");
                assert_eq!(value, "prod");
            }
            other => panic!("expected EnvEquals inside Not, got {other:?}"),
        },
        other => panic!("expected Not, got {other:?}"),
    }

    // Quoted value with spaces
    let expr = parse_guard_expr_str("eq(env:FOO, bar baz)").unwrap();
    match &expr {
        GuardExpr::Predicate(Guard::Attr {
            ns: Ns::Env,
            key: Some(key),
            val: Some(value),
        }) => {
            assert_eq!(key, "FOO");
            assert_eq!(value, "bar baz");
        }
        other => panic!("expected EnvEquals, got {other:?}"),
    }
}

#[test]
fn test_eq_guard_quoted_value_with_comma() {
    let expr = parse_guard_expr_str(r#"eq(env:LIST, "a,b")"#).unwrap();
    match &expr {
        GuardExpr::Predicate(Guard::Attr {
            ns: Ns::Env,
            key: Some(key),
            val: Some(value),
        }) => {
            assert_eq!(key, "LIST");
            assert_eq!(value, "a,b");
        }
        other => panic!("expected EnvEquals, got {other:?}"),
    }
}

#[test]
fn test_arch_guard_values_are_closed() {
    // Known targets parse; unknown or wrong-case values fail like
    // every other closed namespace.
    assert!(parse_guard_expr_str("arch:x86_64").is_ok());
    for bad in ["arch:commodore64", "arch:X86_64", "arch:x86-64"] {
        let err = parse_guard_expr_str(bad).expect_err("closed arch domain must reject");
        let text = format!("{err:#}");
        assert!(text.contains("unknown arch"), "{text}");
    }
    // Empty values never reach validation: the grammar rejects them.
    assert!(parse_guard_expr_str("arch:").is_err());
    let err = parse_guard_expr_str("cpuid:x86_64").expect_err("unknown namespace must fail");
    assert!(format!("{err:#}").contains("arch"));
}

#[test]
fn test_eq_guard_requires_env_prefix() {
    let invalid = ["eq(STAGE, prod)", "ne(STAGE, prod)", "eq(A, 1)"];
    for guard_str in invalid {
        assert!(
            parse_guard_expr_str(guard_str).is_err(),
            "eq()/ne() must require env: prefix: {guard_str}"
        );
    }
}

#[test]
fn test_eq_guard_display_roundtrip() {
    let guard = Guard::Attr {
        ns: Ns::Env,
        key: Some("STAGE".into()),
        val: Some("prod".into()),
    };
    assert_eq!(guard.to_string(), "eq(env:STAGE, prod)");

    // ne() displays as not(eq(...))
    let expr = GuardExpr::Not(Box::new(GuardExpr::Predicate(Guard::Attr {
        ns: Ns::Env,
        key: Some("STAGE".into()),
        val: Some("prod".into()),
    })));
    assert_eq!(expr.to_string(), "not(eq(env:STAGE, prod))");
}

#[test]
fn test_reject_old_equality_syntax() {
    let invalid = [
        "env:A==1",
        "env:A!=1",
        "env:A == 1",
        "env:A != 1",
        "!env:A==1",
        "!env:A!=1",
    ];
    for guard_str in invalid {
        assert!(
            parse_guard_expr_str(guard_str).is_err(),
            "Old equality syntax must be rejected: {guard_str}"
        );
    }
}

#[test]
fn test_reject_bare_operators_as_guards() {
    let invalid = ["==", "!=", "!", "!="];
    for guard_str in invalid {
        assert!(
            parse_guard_expr_str(guard_str).is_err(),
            "Bare operators must not parse as guards: {guard_str}"
        );
    }
}
