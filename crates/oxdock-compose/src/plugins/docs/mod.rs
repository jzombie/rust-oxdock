//! Compatibility alias for the core [`crate::defer`] plugin.
//!
//! Registers the same two functions under the old `DOCS` module name
//! so existing templates (`DOCS::DEFER`, `DOCS::EXPAND_DEFERRED`) keep
//! working while the migration moves callers to `DEFER::`. Both entry
//! points share the canonical logic, so behavior cannot drift between
//! the names. Never add logic here; retire this module once no caller
//! references `DOCS::`.

use anyhow::Result;
use oxdock_core::{HostModule, OxDockFn, StepCtx, TypeTag, Value};
use oxdock_func_macro::oxdock_func;
use oxdock_process::ProcessManager;

/// Defer a domain call to pass 2. Alias of `DEFER::DEFER`; see
/// [`crate::defer`] for the contract.
#[oxdock_func(pure, returns = TypeTag::String)]
fn defer(
    /// Target deferred call in `MODULE::FUNC` form.
    target: String,
    /// Remaining arguments forwarded to the target after the document text.
    #[union(String, Int, Float, Bool, List, Map)]
    args: Vec<Value>,
) -> Result<Value> {
    Ok(Value::string(crate::defer::mint_deferred(&target, &args)?))
}

/// Expand every deferred sentinel in fully rendered text. Alias of
/// `DEFER::EXPAND_DEFERRED`; see [`crate::defer`] for the contract.
#[oxdock_func(returns = TypeTag::String)]
fn expand_deferred<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    /// Fully rendered document text possibly holding sentinels.
    full_text: String,
) -> Result<Value> {
    Ok(Value::string(crate::defer::expand_text(
        &cx.pure_functions(),
        &full_text,
    )?))
}

/// The `DOCS` compat alias. Registered alongside `DEFER` by
/// `crate::run` until no caller references the old name.
pub fn module<P: ProcessManager>() -> HostModule<P> {
    HostModule {
        name: "DOCS".to_string(),
        funcs: vec![Defer::registration(), ExpandDeferred::registration()],
        types: vec![],
        record_schemas: vec![],
        states: Vec::new(),
    }
}
