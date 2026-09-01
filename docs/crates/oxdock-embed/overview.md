Generates the runtime `EmbeddedFile` module that the `oxdock_embed!` macro emits.
At build time it hashes, stamps, and serialises every asset; at run time the
generated struct exposes `get(path) -> Option<EmbeddedFile>` and `iter()` over
all embedded filenames.
