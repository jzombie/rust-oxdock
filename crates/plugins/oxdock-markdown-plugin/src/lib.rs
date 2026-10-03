//! Markdown rendering for OxDock scripts.
//!
//! This crate exposes a `MARKDOWN` host module through the [`Engine`]
//! facade: `MAP_TO_MD_TABLE` renders record data as an aligned Markdown
//! table, one row per MAP and one column per key (issue 170). It is callable as a DSL statement
//! after `IMPORT [MARKDOWN]` and inside placeholders as
//! `{{ MARKDOWN::MAP_TO_MD_TABLE($var) }}`. Table logic lives in
//! [`markdown`], the single source the host wrapper calls into.
//!
//! [`Engine`]: oxdock_core::Engine

mod funcs;
pub mod markdown;

pub use funcs::module_with;
use oxdock_core::HostModule;
pub use oxdock_process::DefaultProcessManager;

/// The `MARKDOWN` host module with the default process manager.
pub fn module() -> HostModule<DefaultProcessManager> {
    module_with()
}
