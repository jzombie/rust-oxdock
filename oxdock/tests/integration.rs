// Single integration test binary for oxdock: every `tests/integration/*`
// target lives here as a submodule so the crate links its test
// dependencies once instead of once per file.
#[cfg(feature = "cli")]
#[path = "integration/cli_args.rs"]
mod cli_args;
#[cfg(feature = "cli")]
#[path = "integration/cli_bin.rs"]
mod cli_bin;
#[path = "integration/facade_parity.rs"]
mod facade_parity;
#[cfg(feature = "cli")]
#[path = "integration/hello_cli.rs"]
mod hello_cli;
#[cfg(feature = "cli")]
#[path = "integration/readme_quickstart.rs"]
mod readme_quickstart;
