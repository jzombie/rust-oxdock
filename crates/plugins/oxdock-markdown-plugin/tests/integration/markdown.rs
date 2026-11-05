//! Integration coverage for the `MARKDOWN` host module.

#[test]
#[cfg_attr(
    miri,
    ignore = "60s wall-clock watchdog cannot survive Miri's execution speed on pulldown samples plus full-script examples"
)]
fn returns_gate_covers_markdown_functions() {
    // Same returns contract as STD builtins, over this module's own
    // registrations: sampled calls that reach `Ok` satisfy the slot,
    // and anything sampling cannot reach must carry examples. Natively
    // pure throughout; under Miri the gate's wall-clock watchdog
    // expires before interpreted samples complete, so coverage comes
    // from the native run only.
    let module = oxdock_markdown_plugin::module();
    let proc = oxdock_process::default_process_manager();
    oxdock_core::assert_module_returns_conformance(
        "MARKDOWN",
        &module.funcs,
        vec![module.clone()],
        proc,
    );
}
