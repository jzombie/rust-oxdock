# oxdock-fs

Guarded filesystem and workspace utilities for OxDock environments.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.
Guarded filesystem and workspace utilities. `GuardedPath` guarantees that
every operation stays within a declared root; `PathResolver` abstracts read,
write, copy, and directory-creation behind a trait so host syscalls are
isolated in one crate. Tempdir PID-lock GC keeps stale `oxdock-*` directories
clean across runs.

## License

`oxdock-fs` is distributed under the terms of the Apache License (Version 2.0).
