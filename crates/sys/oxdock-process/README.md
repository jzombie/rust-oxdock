# oxdock-process

Process orchestration for OxDock environments.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.
Process orchestration layer. `CommandBuilder` constructs cross-platform
commands with env expansion, shell resolution, and background-mode support.
Under Miri the process manager is replaced by a synthetic implementation so
tests stay hermetic.

## License

`oxdock-process` is distributed under the terms of the Apache License (Version 2.0).
