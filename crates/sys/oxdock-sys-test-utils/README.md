# oxdock-sys-test-utils

Shared system test utilities for OxDock workspace tests.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.
Shared test helpers for environment-variable guarding. `TestEnvGuard` sets or
removes an env var, serialises access per key across threads, and restores the
prior state on drop.

## License

`oxdock-sys-test-utils` is distributed under the terms of the Apache License (Version 2.0).
