// TODO: This likely can be replaced entirely with the oxdock crate

// Included only while rustdoc collects doctests, keeping normal `cargo doc`
// free of README-relative link warnings.
//
// README fence info-strings (`oxdock env:KEY=…`, `oxdock roots:unified`, …)
// are runner metadata, not Rust attributes; silencing rustdoc's attribute
// parser for this documentation-only crate.
#![cfg_attr(doctest, allow(rustdoc::invalid_codeblock_attributes))]
#![cfg_attr(doctest, doc = include_str!("../../../README.md"))]
