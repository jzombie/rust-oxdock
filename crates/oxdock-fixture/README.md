# oxdock-fixture

Test-fixture harness that materializes reproducible Cargo workspaces for OxDock's compile-time, Dockerfile-inspired DSL.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.
Materializes a Cargo project template inside a temporary, auto-cleaned
directory for integration tests. Workspace dependencies are patched to local
paths at runtime so the fixture builds against the current checkout.

## License

`oxdock-fixture` is distributed under the terms of the Apache License (Version 2.0).
