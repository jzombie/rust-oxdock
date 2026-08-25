fn main() {
    // Relay Cargo's feature/cfg env into rustc-env space so the proc-macro
    // server executing the inline DSL sees CARGO_FEATURE_* / CARGO_CFG_*.
    oxdock_buildtime_helpers::emit_feature_and_cfg_envs()
        .expect("failed to emit feature/cfg envs");
}
