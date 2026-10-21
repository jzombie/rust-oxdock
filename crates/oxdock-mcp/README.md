# oxdock-mcp

MCP tool hosting for exported OxDock script functions over stdio JSON-RPC.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

## Overview

MCP tool hosting for exported script functions. A script marks its
callable surface with `EXPORT [ADD, GREET]` (single-file scope), and
`oxdock --mcp script.oxdock` serves each entry as an MCP tool over
stdio JSON-RPC instead of running linear steps. The reactor keeps one
initialized session: linear steps run once at startup (ambient `ENV`,
function definitions), then every `tools/call` dispatches through the
live state. Tool descriptions come from the `#` comment block directly
above each `FUNC`; `//` and `/* */` comments are valid script but never
feed descriptions, and an undocumented function serves with its registry
summary or an empty description. The script file is polled and the session hot-swaps on
change, so edits take effect on the next call with no restart; broken
edits keep serving the old tools. Successful reloads also emit
`notifications/tools/list_changed` (advertised via the tools
`listChanged` capability), so clients re-fetch instead of showing a
frozen list. Exported signatures project to JSON
Schema, and only JSON-compatible types serve: opaque host types fail
fast before the reactor opens. Captured stdout and stderr ride the
result `content` block, so framing stays clean while `ECHO` output
stays visible. A synthesized `MCP_STATUS` tool rides alongside every
table: it reports the load generation (1 at startup, plus one per
successful reload), the served tool names, the script content hash,
and the last reload outcome, so agents can detect tools changing
underneath them and broken edits stay visible instead of
stderr-only.

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
`#` doc block as its description. Edits to the script file reload
live with no restart, and the server emits
`notifications/tools/list_changed` so compliant clients re-fetch
instead of showing a frozen list. Call `MCP_STATUS` any time to check
the load generation, the served tool names, and whether the last
reload succeeded.

## License

`oxdock-mcp` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
