#![allow(rustdoc::invalid_codeblock_attributes)]
#![doc = include_str!("../docs/crate_docs.md")]

// Re-exported unconditionally so `oxdock!` expansions (which emit absolute
// `oxdock_parser::...` paths) resolve for consumers that depend only on `oxdock`,
// including `--no-default-features` (macros-only) builds.
pub use oxdock_build;
pub use oxdock_core;
pub use oxdock_parser;

pub use oxdock_macros::{oxdock, oxdock_embed, oxdock_prepare};

pub use oxdock_func_macro::{oxdock_func, oxdock_type};

// CLI runner logic stays in `oxdock-cli`; this re-export is the identical entry point.
#[cfg(feature = "cli")]
pub use oxdock_cli::{
    Engine, EngineOutput, ExecState, ExecutionResult, FuncKind, FuncMeta, FuncParam, Guard,
    HostModule, HostRegistration, NativeFn, Options, OxDockFn, OxDockType, PureFn, ScriptSource,
    Step, StepCtx, StepKind, TypeDescriptor, Value, execute, execute_with_result, parse_script,
    parse_script_with_modules, run, run_script, run_steps, run_steps_with_context,
    run_steps_with_context_result, run_steps_with_manager_with_modules, shell_program, usage,
};
