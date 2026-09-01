# oxdock-embed

OxDock binary embedding layer.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.
Generates the runtime `EmbeddedFile` module that the `oxdock_embed!` macro emits.
At build time it hashes, stamps, and serialises every asset; at run time the
generated struct exposes `get(path) -> Option<EmbeddedFile>` and `iter()` over
all embedded filenames.

## License

`oxdock-embed` is distributed under the terms of the Apache License (Version 2.0).
