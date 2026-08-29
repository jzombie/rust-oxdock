Parity fixtures live under `fixtures/parity/<case>/` and compare string DSL to token DSL.

- `dsl.txt` holds the string-based DSL.
- `tokens.rs` holds the braced-token version (the contents of a `script: { ... }` block).
- Errors are defined in `case.toml` under `[expect.error]` (supports `contains` or `equals`).

The parity harness parses both and asserts their ASTs match.
