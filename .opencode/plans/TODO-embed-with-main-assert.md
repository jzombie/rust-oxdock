This README example would be better off if it had a Rust assertion that the written asset is readable via Rust... showing how it can be embedded into the binary.

```rust
use oxdock_buildtime_macros::embed;

embed! {
    name: HelloAssets,
    script: {
        ENV PROJECT=oxdock
        MKDIR dist
        WRITE dist/hello.txt Built with {{ env:PROJECT }}
        ASSERT_FILE dist/hello.txt Built with {{ env:PROJECT }}
    },
    out_dir: "prebuilt",
}
```
