## Quick start

The following script is a complete OxDock script — it builds artifacts **and verifies them** with native assertions. Every fenced `oxdock` example in this README is executed against the implementation by [`crates/oxdock-logic-tests/tests/docs_conformance.rs`](./crates/oxdock-logic-tests/tests/docs_conformance.rs), so what you read here is guaranteed to match what the DSL actually does:

```oxdock
// Script-local variable: usable by templates and guards below.
ENV PROJECT=OxDock

// Creates the directory and any missing parents.
MKDIR dist

// Interpolate the variable into the file body via a template.
WRITE dist/hello.txt Built with {{ env:PROJECT }}

// Fail the script unless the artifact exists with exactly these bytes.
ASSERT_FILE dist/hello.txt Built with {{ env:PROJECT }}

// LS prints "<dir>:" then the entry names, sorted.
LS dist

// Assert stdout buffer of previous LS command is "hello.txt"
ASSERT_STDOUT hello.txt
```

Run it with the CLI:

```bash
cargo install --path oxdock-cli
oxdock --script Oxfile
```

Or embed the same script at compile time — the macro runs the script during `rustc` and generates a pure-Rust struct whose assets live in the binary's data section, readable at runtime with zero heap allocation:

```rust
use oxdock_buildtime_macros::embed;

embed! {
    // Embedded resources are mapped to `HelloAssets::get(resource)`
    name: HelloAssets,
    script: {
        ENV PROJECT=OxDock
        MKDIR dist
        WRITE dist/hello.txt Built with {{ env:PROJECT }}
        ASSERT_FILE dist/hello.txt Built with {{ env:PROJECT }}
    },
    // Generated assets land under target/, keeping the source tree clean
    out_dir: "target/prebuilt",
}

fn main() {
    // Verify we can read the resource we just created
    let file = HelloAssets::get("dist/hello.txt").expect("dist/hello.txt must be embedded");
    assert_eq!(file.data.as_ref(), b"Built with OxDock");
}
```
