//! Internal plugins for the render engine.
//!
//! Each plugin exposes a `module()` constructor over `#[oxdock_func]`
//! registrations, exactly like the language plugins under
//! `crates/plugins`, and is rendered through `register_module`. They
//! stay internal by registration site: only `crate::run` registers
//! them, never the CLI default surface or the `oxdock` facade, so the
//! core language never sees them. Plugins are named for their domain:
//! any pipeline reuses the ones matching its project (`OXDOCK` for
//! OxDock projects, `RUST` for Cargo workspaces) plus the engine
//! builtins in [`crate::engine`], and registers nothing else.
//!
//! Lifecycle behavior lives separately, in DSL lifecycle files
//! beside the core scripts in `src/`: one file per pipeline
//! participant, every file exposing the same lifecycle methods.

pub mod oxdock;
pub mod rust;
