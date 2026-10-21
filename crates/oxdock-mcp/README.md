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
live state. The script file is polled and the session hot-swaps on
change, so edits take effect on the next call with no restart; broken
edits keep serving the old tools. Exported signatures project to JSON
Schema, and only JSON-compatible types serve: opaque host types fail
fast before the reactor opens. Captured stdout and stderr ride the
result `content` block, so framing stays clean while `ECHO` output
stays visible.

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

## License

`oxdock-mcp` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
