# oxdock-process

Process orchestration for OxDock environments.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

## Overview

Process orchestration layer. `CommandBuilder` constructs cross-platform
commands with env expansion, shell resolution, and background-mode support.
Under Miri the process manager is replaced by a synthetic implementation so
tests stay hermetic.

## Quick Start

See the [API documentation](https://docs.rs/oxdock-process).

## API

See the [API documentation](https://docs.rs/oxdock-process).

## License

`oxdock-process` is distributed under the terms of the Apache License (Version 2.0).
