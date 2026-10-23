# oxdock-mcp

MCP tool hosting for exported OxDock script functions over stdio JSON-RPC.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

## Overview

MCP tool hosting for exported script functions.

A script marks its callable surface with `EXPORT [ADD, GREET]`
(script `FUNC`s), or names a host function directly with
`EXPORT [STD::TEXT_JOIN]`: registry summaries and docs carry over
as the tool description, so host tools serve with no wrapper.
Cross-file `EXPORT` stays out of scope.

`oxdock --mcp script.oxdock` serves each entry as an MCP tool over
stdio JSON-RPC instead of running linear steps. The reactor keeps
one initialized session: linear steps run once at startup (ambient
`ENV`, function definitions), then every `tools/call` dispatches
through the live state.

Tool descriptions come from the `#` comment block directly above
each `FUNC`; `//` and `/* */` comments are valid script but never
feed descriptions, and an undocumented function serves with its
registry summary or an empty description.

The script file is polled and the session hot-swaps on change, so
edits take effect on the next call with no restart; broken edits
keep serving the old tools. Successful reloads also emit
`notifications/tools/list_changed` (advertised via the tools
`listChanged` capability), so clients re-fetch instead of showing
a frozen list.

Exported signatures project to JSON Schema, and only
JSON-compatible types serve: opaque host types fail fast before
the reactor opens. Default-routed stdout and stderr stream to the
client live as notifications while the call runs, so `ECHO` output
stays visible without touching the process wire. The final result
carries only the return value, plus a drop notice if a slow client
overflowed the bounded channel.

A synthesized `MCP_STATUS` tool rides alongside every table: it
reports the load generation (1 at startup, plus one per successful
reload), the served tool names, the script content hash, and the
last reload outcome, so agents can detect tools changing underneath
them and broken edits stay visible instead of stderr-only.

Long calls detach instead of blocking: wrap the slow steps in an
`ASYNC` block and `RETURN` the task handle. The call returns at
once with `{"task_id": N}`, and the exported `STD::TASK_POLL`
builtin snapshots the job by handle: running state, output tails
so far, and the final value or exit once finished. Handles are
ordinary words, so `CANCEL` and `AWAIT` keep working on them
in-script, and `TASK_POLL` never consumes: repeated polls and a
later `AWAIT` all see the same outcome.

```oxdock
IMPORT [STD]
# Detach a slow command and hand back its handle.
FUNC SLOW_OP() {
  LET $t: HANDLE = ASYNC { RUN ["sleep", "30"] }
  RETURN $t
}
# Poll the handle: running with tails so far, then finished.
FUNC POLL_WRAP($t: HANDLE) {
  RETURN TASK_POLL($t)
}
# The export marks the tools without changing linear calls.
EXPORT [SLOW_OP, POLL_WRAP, STD::TASK_POLL]
```


```oxdock
# Define an exported tool and call it directly.
FUNC ADD($a: INT, $b: INT) {
  RETURN $a + $b
}
EXPORT [ADD]

# The export marks the tool without changing linear calls.
LET $sum: INT = ADD(2, 3)
ASSERT_EQ $sum 5
```

## OpenCode setup

To drive a script from OpenCode, build with the feature and register
the server in `opencode.jsonc`. Any key under `mcp` becomes the tool
prefix, so name it after the toolset it serves.

```jsonc
{
  "mcp": {
    "oxdock-demo": {
      "type": "local",
      "command": [
        "/path/to/oxdock",
        "--mcp",
        "/path/to/tools.oxdock"
      ],
      "enabled": true
    }
  }
}
```

Build once with `cargo build -p oxdock --features mcp`, then restart
OpenCode (or resync its MCP servers) so the client connects and lists
tools. Each `EXPORT`ed function appears as `<name>_<FUNC>` with the
`#` doc block as its description.

```jsonc
// tools/list advertises each entry with its schema.
{
  "name": "oxdock-demo_ADD",
  "description": "Adds two integers. Returns the sum.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "a": { "type": "integer" },
      "b": { "type": "integer" }
    },
    "required": ["a", "b"]
  }
}
// tools/call dispatches by tool name with a JSON argument object.
{
  "name": "oxdock-demo_ADD",
  "arguments": { "a": 2, "b": 3 }
}
// The result carries the JSON return value under `result`.
{
  "result": 5
}
```

`ECHO` output never lands in the result: it streams as log
notifications while the call runs, then the value follows. A call
to `GREET` streams `greeting requested` mid-call and resolves
`{"result": "Hello, Ada"}` at the end.

Edits to the script file reload live with no restart, and the server
emits `notifications/tools/list_changed` so compliant clients
re-fetch instead of showing a frozen list. Call `MCP_STATUS` any time
to check the load generation, the served tool names, and whether the
last reload succeeded.

Long calls should detach: wrap the slow steps in an `ASYNC` block
and `RETURN` the task handle, which crosses the wire as
`{"task_id": N}`. Export `STD::TASK_POLL` alongside and poll the
handle until `state` reads `finished`. Foreground calls hold the
session, so a minutes-long run starves every other call until it
returns. Retained finished tasks are capped, so abandoned polls
cannot grow the session without bound.

```jsonc
// Detach by returning a handle; the call answers immediately.
{
  "name": "CARGO_TEST",
  "arguments": { "options": {} }
}
// Poll the handle: `state` reads `running` with tails so far,
// then `finished` with the return value under `result`.
{
  "name": "TASK_POLL",
  "arguments": { "task": { "task_id": 1 } }
}
```

## License

`oxdock-mcp` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
