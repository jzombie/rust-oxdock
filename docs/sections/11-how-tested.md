## How these examples are tested

Every ```` ```oxdock ```` fence in this document is extracted with [`oxdock_parser::extract_fenced_blocks`](./crates/oxdock-parser/src/markdown.rs) and executed by [`crates/oxdock-logic-tests/tests/docs_conformance.rs`](./crates/oxdock-logic-tests/tests/docs_conformance.rs) against the real parser and interpreter, so the documentation cannot drift from the implementation. Enforcement layers:

- **Parse & execute:** every snippet must parse and run clean (or fail with its declared `expect_error:` message) on Linux, macOS, and Windows CI.
- **Coverage gates:** every parser command must appear in at least one executable example, and key structural features (`any(`, `not(`, `{{ env:`, `[env:`) must be demonstrated.
- **Compile-time parity:** a [build-time fixture](./crates/oxdock-logic-tests/fixtures/integration/buildtime_macros/assert_verification/) runs this README's quick-start script through `oxdock_embed!`, assertions included.
- **Real-binary check:** the quick start is additionally executed through the actual `oxdock` binary exactly as documented (`--script Oxfile`).
- **Doctest execution:** the Rust quick start is wired into [`crates/oxdock-doc-tests`](./crates/oxdock-doc-tests/) and compiled *and* run by `cargo test --doc` on every CI OS.
- **Reference integrity:** every relative Markdown link target and every repo path referenced from a ```` ```bash ```` fence must exist.

Snippets contain nothing but OxDock — copy any of them straight into an `Oxfile` or an `oxdock_embed!` macro. Runner-specific configuration lives in the fence info-string, which Markdown renders as inert metadata:

```text
```oxdock                                    plain snippet, must parse and run clean
```oxdock env:KEY=value                      inject an environment value (visible to INHERIT_ENV/guards)
```oxdock roots:unified                      run with workspace root == build context (COPY/COPY_GIT demos)
```oxdock expect_error:"message substring"   snippet must fail with this text in its error
```

Everything else you see inside the fences — including the `ASSERT_*` commands — is part of the DSL itself and executes identically in your own pipelines.

If you change the DSL, update this reference in the same commit — CI will hold you to it.
