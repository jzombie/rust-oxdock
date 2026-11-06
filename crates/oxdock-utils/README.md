# oxdock-utils

Shared leaf utilities for OxDock: streaming tarball pack/unpack, SHA-256 hex digests, and other dependency-light helpers used across crates.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

Small shared leaf utilities for OxDock: dependency-light helpers used
by more than one crate, kept in one place so a single helper never
costs a new crate. New one-off utilities land here as modules; the
crate stays a leaf (third-party deps only, never workspace crates), so
anything can depend on it without a cycle.

- `tar`: streaming tarball pack/unpack over caller-held handles, with
  sanitized member paths and write-through integrity. Backs local
  `ARCHIVE` / `UNARCHIVE` and remote declared transfers from the same
  code, with no filesystem access of its own.
- Digests: canonical hex over raw bytes (`sha256_hex`), over
  already-finalized digests (`hex_encode`), and over re-read streams
  (`hash_stream`, yielding hex plus length), so the encoding cannot
  drift between one-shot hashes, streaming hashers, and tests.

See rustdoc for the item-level API.

## License

`oxdock-utils` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
