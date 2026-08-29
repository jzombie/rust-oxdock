## Quick features

- Create an isolated, temporary workspace and run a script inside it.
- Drop into an interactive shell inside the temporary workspace with `oxdock --shell` (requires a TTY).
- Run a DSL script via `--script <path>` or by piping a script into stdin.
- Expose the real workspace to scripts via `WORKSPACE LOCAL` / `WORKSPACE SNAPSHOT` so steps can target either the temporary workspace or the live repo.
