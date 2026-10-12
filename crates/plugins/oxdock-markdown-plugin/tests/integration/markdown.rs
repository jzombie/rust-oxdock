//! Integration coverage for the `MARKDOWN` host module.

#[test]
fn returns_gate_covers_markdown_functions() {
    // Same returns contract as STD builtins, over this module's own
    // registrations: sampled calls that reach `Ok` satisfy the slot,
    // and anything sampling cannot reach must carry examples. Pure
    // computation throughout, so this runs under Miri.
    let module = oxdock_markdown_plugin::module();
    let proc = oxdock_process::default_process_manager();
    oxdock_core::assert_module_returns_conformance(
        "MARKDOWN",
        &module.funcs,
        vec![module.clone()],
        proc,
    );
}
