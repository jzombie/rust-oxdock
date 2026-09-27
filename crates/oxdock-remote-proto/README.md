# oxdock-remote-proto

Contract for OxDock sealed remote execution: stable muxio method names, content-derived protocol digest, handshake types, and guarded tarball pack/unpack.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

Contract for OxDock sealed remote execution (`REMOTE` blocks). A small stable
method set carries the whole session at block granularity, with no per-command
RPC. A content-derived protocol digest gates the handshake, so contract edits
take effect without manual versioning. Control types cover session setup and
the result envelope. Tar helpers stream declared file transfers with
fail-closed path sanitation under the active root.

Dependency-light by design for use from the network plugin, the guest serve
loop, and test harnesses. Filesystem application stays with the caller under
guarded containment, and execution isolation stays with the operator and OS.

## License

`oxdock-remote-proto` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
