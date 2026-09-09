// Single integration test binary for oxdock-parser: every former
// `tests/<name>.rs` target now lives here as a submodule so the crate
// links its test dependencies once instead of once per file. No test was
// added, removed, or altered in the move — each module is byte-identical
// to its original file, except `mod common;` collapsing to this root.
#[path = "integration/cancel.rs"]
mod cancel;
#[path = "integration/common/mod.rs"]
mod common;
#[path = "integration/env_display.rs"]
mod env_display;
#[path = "integration/guard_tests.rs"]
mod guard_tests;
#[path = "integration/inherit_env.rs"]
mod inherit_env;
#[path = "integration/invalid_env_inversion.rs"]
mod invalid_env_inversion;
#[path = "integration/let_async_with_io.rs"]
mod let_async_with_io;
#[path = "integration/let_capture.rs"]
mod let_capture;
#[path = "integration/macro_input_parens.rs"]
mod macro_input_parens;
#[path = "integration/platform_display.rs"]
mod platform_display;
#[path = "integration/timeout_block.rs"]
mod timeout_block;
#[path = "integration/unified_values.rs"]
mod unified_values;
#[path = "integration/with_io_block.rs"]
mod with_io_block;
