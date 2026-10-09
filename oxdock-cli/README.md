# oxdock-cli

CLI tooling for executing OxDock's Dockerfile-inspired DSL on native platforms.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

## Quick features

- Create an isolated, temporary workspace and run a script inside it.
- Drop into an interactive shell inside the temporary workspace with `oxdock --shell` (requires a TTY).
- Run a DSL script via `--script <path>` or by piping a script into stdin.
- Map virtual service endpoints to physical interfaces: scripts declare logical ports or names (`NET_LISTEN("2251")`), and `--listen`, `-p`, or `--offline` decide what they bind to (defaults to loopback). Endpoint flags need the `net` Cargo feature (on by default; `--no-default-features` builds reject them).
- Expose the real workspace to scripts via `WORKSPACE LOCAL` / `WORKSPACE SNAPSHOT` so steps can target either the temporary workspace or the live repo.

## Notes

- When invoked from Cargo builds the CLI will normally detect the manifest dir; if you run the installed binary from other locations, you can influence where the workspace root is discovered with the `OXDOCK_WORKSPACE_ROOT` environment variable.
- The CLI uses the same DSL and runtime as the `oxdock-core` crate; it sets a separate `CARGO_TARGET_DIR` for nested cargo invocations to avoid collisions with the outer build.

Integration with compile-time macros
- For compile-time embedding, see `oxdock-macros::oxdock_embed!` which runs the same DSL during `build.rs`/proc-macro time and produces an `out_dir` whose files are embedded directly into a generated struct (no additional runtime dependencies).

Examples
See the repository `examples/` folder for sample scripts and an `embed_demo.rs` that demonstrates how the compile-time macro and CLI work together.

- Create an isolated, temporary workspace and run a script inside it.
- Drop into an interactive shell inside the temporary workspace with `oxdock --shell` (requires a TTY).
- Run a DSL script via `--script <path>` or by piping a script into stdin.
- Expose the real workspace to scripts via `WORKSPACE LOCAL` / `WORKSPACE SNAPSHOT` so steps can target either the temporary workspace or the live repo.

## Common usage

Run a script file (positional, same as `--script`):

```sh
oxdock ./build.oxfile
```

Run a script file (explicit flag form):

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

Expose a script service port on all interfaces:
```sh
oxdock --listen 0.0.0.0:2251 ./proxy.oxfile
```
Map an outer port to an inner service port or name (bare outer binds loopback):

```sh
oxdock -p 2222:2251 ./proxy.oxfile
```

Expose on all interfaces with an explicit host (bare outer never does this):

```sh
oxdock -p 0.0.0.0:2222:2251 ./proxy.oxfile
```

Run with no sockets at all (services stay handle-only):
```sh
oxdock --offline ./proxy.oxfile
```

Endpoint flags need the `net` Cargo feature (enabled by default).
Observe a mapped outer port in-script with `NET_PORT`/`NET_ADDR`
and pass it to inner commands through `ENV`.

## Command-line reference

The default build (with the `net` feature) prints the following for
`oxdock --help`. It is rendered from the same embedded usage body
the binary prints:

```text
oxdock 0.24.1-alpha — CLI tooling for executing OxDock's Dockerfile-inspired DSL on native platforms.

Usage: oxdock [OPTIONS] [SCRIPT]
  SCRIPT             script file path (same as `--script <file>`); `-` reads stdin
  --script <file|->  script file (relative resolves under the OxDock workspace root), or `-` for stdin
  --shell            run the script, then drop into an interactive shell (requires a TTY)
  --listen <addr>    expose a logical service port ([host:]port, repeatable)
  -p <[host:]outer:inner>  map outer port to an inner service port or name (repeatable; outer 0 is ephemeral; bare outer binds loopback, prefix 0.0.0.0: for all interfaces)
  --offline          open no sockets (conflicts with --listen/-p)
  --remote TARGET=CMD    bind a REMOTE target to a stdio transport command (repeatable)
  --help, -h         print this help and exit
  --version, -V      print the version and exit
With no script given, reads the script from stdin (must be piped unless `--shell`).
Scripts declare logical endpoints (a port like 2251); the flags above map them to interfaces.
```

## License

`oxdock-cli` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
