# oxdock-cli

CLI tooling for executing OxDock's Dockerfile-inspired DSL on native platforms.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

## Overview

- Create an isolated, temporary workspace and run a script inside it.
- Drop into an interactive shell inside the temporary workspace with `oxdock --shell` (requires a TTY).
- Run a DSL script via `--script <path>` or by piping a script into stdin.
- Expose the real workspace to scripts via `WORKSPACE LOCAL` / `WORKSPACE SNAPSHOT` so steps can target either the temporary workspace or the live repo.

## Quick Start

## Common usage

Run a script file:
```sh
oxdock --script ./build.oxfile
```

Pipe a script into the CLI:
```sh
cat my-script.oxfile | oxdock
```

Drop into a shell inside the temporary workspace (interactive):
```sh
oxdock --shell
```

## API

See the [API documentation](https://docs.rs/oxdock-cli).

## License

`oxdock-cli` is distributed under the terms of the Apache License (Version 2.0).
