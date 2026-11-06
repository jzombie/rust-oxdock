//! Internal host modules for the compose engine.
//!
//! Each module exposes a `module()` constructor over `#[oxdock_func]`
//! registrations, exactly like the language modules under
//! `crates/plugins`, and is composed through `register_module`. They
//! stay internal by registration site: only `crate::run` registers
//! them, never the CLI default surface or the `oxdock` facade, so the
//! core language never sees them. Modules are named for their domain:
//! any pipeline reuses the ones matching its project (`OXDOCK` for
//! OxDock projects, `RUST` for Cargo workspaces) plus the engine
//! builtins in [`crate::engine`], and registers nothing else.
//!
//! Lifecycle behavior lives separately, in DSL plugin files under
//! `plugins/`: one file per plugin, every file exposing the same
//! lifecycle methods.

pub mod docs;
pub mod oxdock;
pub mod rust;
