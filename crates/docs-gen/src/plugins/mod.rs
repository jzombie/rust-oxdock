//! Internal plugins for the docs-gen render engine.
//!
//! Each plugin exposes a `module()` constructor over `#[oxdock_func]`
//! registrations, exactly like the language plugins under
//! `crates/plugins`, and is composed through `register_module`. They
//! stay internal by registration site: only `crate::run` registers
//! them, never the CLI default surface or the `oxdock` facade, so the
//! core language never sees them. Plugins are named for their domain,
//! not for docs-gen: any pipeline reuses the ones matching its project
//! (`OXDOCK` for OxDock projects, `RUST` for Cargo workspaces) plus the
//! engine builtins in [`crate::docs_gen_engine`], and registers nothing else.

pub mod oxdock;
pub mod rust;
