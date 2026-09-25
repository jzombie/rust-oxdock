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

Map an outer port to an inner service port or name:
```sh
oxdock -p 2222:2251 ./proxy.oxfile
```

Run with no sockets at all (services stay handle-only):
```sh
oxdock --offline ./proxy.oxfile
```

Endpoint flags need the `net` Cargo feature (enabled by default).
Observe a mapped outer port in-script with `NET_PORT`/`NET_ADDR`
and pass it to inner commands through `ENV`.

## License

`oxdock-cli` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
