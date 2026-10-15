//! Closed-world locks behind the network isolation mandate.
//!
//! The pre-run check only inspects calls it recognizes
//! (`classify_service_call`), and only runs when a module constructor
//! stages it. Both are omission bugs waiting to happen: a new
//! service-taking function, or a new constructor that forgets the
//! check. These tests fail on either omission. No sockets, no threads,
//! no Tokio: pure parse plus inspection, so they run under Miri too.

use std::collections::HashSet;
use std::sync::Arc;

use indoc::indoc;
use oxdock_core::{HostRegistration, parse_script_with_modules, std_module_table};
use oxdock_net_plugin::{EndpointRegistry, classify_service_call};
use oxdock_parser::{ModuleFuncs, ModuleTable, base_name};
use oxdock_process::DefaultProcessManager;

/// Every service-taking function with every spelling the lowering can
/// produce for it: bare canonical, qualified canonical, qualified
/// short. A new service-taking function extends this table AND
/// `classify_service_call` together.
const ENFORCED: &[(&str, &str, &[&str])] = &[
    (
        "NET",
        "NET_LISTEN",
        &["NET_LISTEN", "NET::NET_LISTEN", "NET::LISTEN"],
    ),
    (
        "NET",
        "NET_CONNECT",
        &["NET_CONNECT", "NET::NET_CONNECT", "NET::CONNECT"],
    ),
    (
        "NET",
        "NET_PORT",
        &["NET_PORT", "NET::NET_PORT", "NET::PORT"],
    ),
    (
        "NET",
        "NET_ADDR",
        &["NET_ADDR", "NET::NET_ADDR", "NET::ADDR"],
    ),
    (
        "SSH",
        "SSH_SERVE",
        &["SSH_SERVE", "SSH::SSH_SERVE", "SSH::SERVE"],
    ),
    (
        "SSH",
        "SSH_CONNECT",
        &["SSH_CONNECT", "SSH::SSH_CONNECT", "SSH::CONNECT"],
    ),
];

/// Registered functions that take no service name, per module. A new
/// function lands here with justification, or in `ENFORCED` with
/// classifier coverage. Nowhere else to go.
const NON_SERVICE_NET: &[&str] = &["NET_ACCEPT", "NET_CLOSE", "NET_FETCH"];
const NON_SERVICE_SSH: &[&str] = &[
    "SSH_ACCEPT",
    "SSH_DEQUEUE",
    "SSH_PUMP_CHANNEL",
    "SSH_CLOSE",
    "SSH_PUMP",
    "SSH_PTY_RUN",
    "SSH_SEND_EXIT_STATUS",
    // Session-scoped like SSH_PTY_RUN: takes a session and pipes, never
    // a service name.
    "SSH_RUN",
];

fn registration_name(registration: &HostRegistration<DefaultProcessManager>) -> &str {
    match registration {
        HostRegistration::Stateful { name, .. } => name,
        HostRegistration::Pure { name, .. } => name,
        HostRegistration::Both { name, .. } => name,
    }
}

fn registration_base(registration: &HostRegistration<DefaultProcessManager>) -> &str {
    match registration {
        HostRegistration::Stateful { meta, .. } => base_name(&meta.name),
        HostRegistration::Pure { meta, .. } => base_name(&meta.name),
        HostRegistration::Both { meta, .. } => base_name(&meta.name),
    }
}

fn registered_names(module: &str) -> Vec<String> {
    let modules: Vec<oxdock_core::HostModule<DefaultProcessManager>> = vec![
        oxdock_net_plugin::module_with_endpoints(Arc::new(EndpointRegistry::new(false))),
        oxdock_ssh_plugin::module_with_endpoints(Arc::new(EndpointRegistry::new(false))),
    ];
    let found = modules
        .iter()
        .find(|candidate| candidate.name == module)
        .unwrap_or_else(|| panic!("{module} module missing from test harness"));
    found
        .funcs
        .iter()
        .map(registration_name)
        .map(str::to_string)
        .collect()
}

/// Every service-taking function is inspected under every spelling,
/// and the inspected set is exactly the enforced set: no missing
/// coverage, no spurious classification of benign functions (which
/// would demand runner mappings for non-service arguments).
#[test]
fn pre_run_inspector_covers_every_service_function() {
    for (module, canonical, spellings) in ENFORCED {
        for spelling in *spellings {
            assert_eq!(
                classify_service_call(module, spelling),
                Some(*canonical),
                "{module}::{spelling} must inspect as {canonical}"
            );
        }
    }
    for (module, allowlist) in [("NET", NON_SERVICE_NET), ("SSH", NON_SERVICE_SSH)] {
        let enforced: Vec<&str> = ENFORCED
            .iter()
            .filter(|(name, _, _)| *name == module)
            .map(|(_, canonical, _)| *canonical)
            .collect();
        let mut accounted: Vec<&str> = enforced
            .iter()
            .copied()
            .chain(allowlist.iter().copied())
            .collect();
        accounted.sort_unstable();
        accounted.dedup();
        let owned = registered_names(module);
        let mut registered: Vec<&str> = owned.iter().map(String::as_str).collect();
        registered.sort_unstable();
        assert_eq!(
            accounted, registered,
            "{module}: registered functions must be exactly enforced plus allowlisted"
        );
        for name in registered {
            let classified = classify_service_call(module, name).is_some();
            let expected = enforced.contains(&name);
            assert_eq!(
                classified, expected,
                "{module}::{name}: classification must match the enforced set"
            );
        }
    }
}

/// A registered function outside both lists fails here with the fix
/// spelled out: classify it, or allowlist it with justification.
#[test]
fn unlisted_functions_fail_closed() {
    for (module, allowlist) in [("NET", NON_SERVICE_NET), ("SSH", NON_SERVICE_SSH)] {
        for name in registered_names(module) {
            let enforced = ENFORCED
                .iter()
                .any(|(module_name, canonical, _)| *module_name == module && *canonical == name);
            let allowed = allowlist.contains(&name.as_str());
            assert!(
                enforced || allowed,
                "{module}::{name} is neither enforced nor allowlisted: \
                 if it takes a service name, add it to ENFORCED and \
                 classify_service_call; otherwise add it to the module \
                 allowlist with justification"
            );
        }
    }
}

/// Parse table seeded from the modules under test plus STD: probes
/// use `IMPORT` with bare calls, exactly like production scripts, so
/// the lowered names are the real ones the checker inspects.
fn probe_table() -> ModuleTable {
    let mut table = std_module_table();
    for module in [
        oxdock_net_plugin::module_with_endpoints(Arc::new(EndpointRegistry::new(false))),
        oxdock_ssh_plugin::module_with_endpoints(Arc::new(EndpointRegistry::new(false))),
    ] {
        let functions: HashSet<String> = module
            .funcs
            .iter()
            .map(|registration| registration_base(registration).to_string())
            .collect();
        table
            .modules
            .insert(module.name.clone(), Some(ModuleFuncs { functions }));
    }
    table
}

/// Service calls nested in function bodies, loops, branches, and
/// async blocks are inspected exactly like top-level ones, in both
/// statement and expression position. One probe per shape so a
/// regression names the shape that went blind: each must fail
/// inspection naming its own unmapped service.
#[test]
fn nested_service_calls_are_inspected() {
    let table = probe_table();
    let module: oxdock_core::HostModule<DefaultProcessManager> =
        oxdock_net_plugin::module_with_endpoints(Arc::new(EndpointRegistry::new(false)));
    let probes = [
        (
            "funcdef statement",
            "lock-probe-funcdef",
            indoc! {r#"
                FUNC PROBER() {
                    NET_LISTEN("lock-probe-funcdef", {})
                }
            "#},
        ),
        (
            "loop statement",
            "lock-probe-loop",
            indoc! {r#"
                FOR $x: STRING IN ["a"] {
                    NET_LISTEN("lock-probe-loop", {})
                }
            "#},
        ),
        (
            "branch expression",
            "lock-probe-branch",
            indoc! {r#"
                IF true {
                    LET $l: STRING = NET_LISTEN("lock-probe-branch", {})
                }
            "#},
        ),
        (
            "async statement",
            "lock-probe-async",
            indoc! {r#"
                LET $t: HANDLE = ASYNC {
                    NET_LISTEN("lock-probe-async", {})
                }
            "#},
        ),
    ];
    for (shape, service, body) in probes {
        let script = format!("IMPORT [STD, NET]\n{body}");
        let steps =
            parse_script_with_modules(&script, table.clone()).expect("{shape} probe parses");
        let err = module.check_pre_run(&steps);
        assert!(
            err.is_err(),
            "{shape} must stage inspection of nested calls"
        );
        let err = err.unwrap_err();
        assert!(
            err.to_string().contains(service),
            "{shape}: expected inspection to name '{service}', got {err:#}"
        );
    }
}
/// Every public constructor stages its pre-run check: an unmapped
/// service call fails inspection with the mapping hint, while benign
/// steps pass. Covers the default, registry-taking, and concrete
/// constructors per module, since engines use `module()` and harnesses
/// use the other two.
#[test]
fn every_module_constructor_stages_its_pre_run_check() {
    let table = probe_table();
    let benign = parse_script_with_modules(
        indoc! {r#"
            IMPORT [STD]
            ASSERT_EQ 1 1
        "#},
        table.clone(),
    )
    .expect("benign steps parse");
    let net_probe = parse_script_with_modules(
        indoc! {r#"
            IMPORT [STD, NET]
            NET_LISTEN("lock-probe-net", {})
        "#},
        table.clone(),
    )
    .expect("net probe parses");
    let ssh_probe = parse_script_with_modules(
        indoc! {r#"
            IMPORT [STD, SSH]
            SSH_SERVE("lock-probe-ssh", {username: "u", password: "p"})
        "#},
        table,
    )
    .expect("ssh probe parses");
    let net_modules = [
        oxdock_net_plugin::module(),
        oxdock_net_plugin::module_with(),
        oxdock_net_plugin::module_with_endpoints(Arc::new(EndpointRegistry::new(false))),
    ];
    for (index, module) in net_modules.iter().enumerate() {
        let staged = module.check_pre_run(&net_probe);
        assert!(
            staged.is_err(),
            "net constructor {index} must stage its check"
        );
        let err = staged.unwrap_err();
        assert!(
            err.to_string().contains("has no CLI port mapping"),
            "net constructor {index}: expected the mapping hint, got {err:#}"
        );
        assert!(
            module.check_pre_run(&benign).is_ok(),
            "net constructor {index} must pass benign steps"
        );
    }
    let ssh_modules = [
        oxdock_ssh_plugin::module(),
        oxdock_ssh_plugin::module_with(),
        oxdock_ssh_plugin::module_with_endpoints(Arc::new(EndpointRegistry::new(false))),
    ];
    for (index, module) in ssh_modules.iter().enumerate() {
        let staged = module.check_pre_run(&ssh_probe);
        assert!(
            staged.is_err(),
            "ssh constructor {index} must stage its check"
        );
        let err = staged.unwrap_err();
        assert!(
            err.to_string().contains("has no CLI port mapping"),
            "ssh constructor {index}: expected the mapping hint, got {err:#}"
        );
        assert!(
            module.check_pre_run(&benign).is_ok(),
            "ssh constructor {index} must pass benign steps"
        );
    }
}
