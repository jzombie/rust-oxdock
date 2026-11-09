//! Compatibility pointer to [`oxdock_render`].
//!
//! All logic lives in `oxdock-render`; this crate only re-exports its
//! entry points so existing callers (`cargo run -p docs-gen`, `use
//! docs_gen::run`) keep working while the refactor moves forward. Never
//! add logic here: this stays a two-line shim until it is retired.
pub use oxdock_render::{pipeline_engine, run, run_with_plugins};
