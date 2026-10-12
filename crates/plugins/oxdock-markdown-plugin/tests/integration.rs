// Single integration test binary for oxdock-markdown-plugin: every
// `tests/<name>.rs` target lives here as a submodule so the crate links
// its test dependencies once instead of once per file.
#[path = "integration/markdown.rs"]
mod markdown;
