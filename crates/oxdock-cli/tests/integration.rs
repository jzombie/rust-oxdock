// Single integration test binary for oxdock-cli: every former
// `tests/<name>.rs` target now lives here as a submodule so the crate
// links its test dependencies once instead of once per file. No test was
// added, removed, or altered in the move : each module is byte-identical
// to its original file.
//
// Binary-driven tests (`cli_bin`, `readme_quickstart`) moved to the `oxdock`
// facade crate alongside the `oxdock` binary.
#[path = "integration/parse.rs"]
mod parse;
#[path = "integration/script.rs"]
mod script;
