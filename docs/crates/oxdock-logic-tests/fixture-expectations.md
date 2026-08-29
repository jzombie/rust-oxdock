Fixtures define expectations in `case.toml`. This keeps error handling and output
assertions consistent across all harnesses. When present, the harness runs one
test per case file. Without it, the harness defaults to `cargo run --quiet` and
expects success.

`case.toml` format:

```
name = "failure"
args = ["run"]

[expect]
status = "failure"

[expect.stderr]
contains = ["failed to parse manifest"]

[expect.error]
contains = "failed to parse manifest"
```

Multiple cases can be defined under `cases/` as either `cases/<case>.toml` or
`cases/<case>/case.toml`. Each case produces its own test invocation.
