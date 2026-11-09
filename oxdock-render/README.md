# oxdock-render

Rendered artifacts, byte-identical: a general-purpose lifecycle renderer turning one context into deterministic outputs.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

## Overview

`oxdock-render` renders rendered artifacts from separately authored
inputs through an ordered lifecycle: one context in, byte-identical
outputs out. Markdown docs are the first pipeline, not the
boundary: the same engine renders any text artifacts a repo's
plugins describe.

Render this workspace:

```sh
cargo run -p oxdock-render
```

Render another checkout with its own pipeline:

```sh
oxdock-render --root /path/to/repo --config render.json
```

Inspect the output without running it:

```sh
oxdock-render --root /path/to/repo --print-child
```

Flags: `--root <path>` (workspace root, default: discovered),
`--config <path>` (pipeline config, default: `render.json`),
`--print-child` (print the rendered child script, skip the run).

Fail-closed throughout: duplicate target names, zero discovered
targets, misspelled placeholders, and unknown values fail the run
instead of rendering wrong outputs.

## Pipeline

Each run performs the same steps, in order:

1. Load `render.json` (global values, scopes, generated inputs,
   scope key aliases, version key, staging dir).
2. Provision the embedded pipeline files and check every plugin
   file exposes the same lifecycle methods, failing fast naming
   any outlier instead of mid-run at first use.
3. Assemble the child script: one qualified call per stage per
   plugin, in registration order (`SETUP` once up front, then the
   six stages per target), piped through a fresh interpreter that
   never touches disk.
4. Per target: validate the descriptor, expand fragments into a
   staging manifest, expand the master template, expand deferred
   sentinels, write the output, byte-compare disk against pipe.
   The docs plugin's `SETUP` additionally refreshes generated
   inputs, snapshots workspace metadata, and syncs member values.
5. Re-apply executable bits from `target.json` declarations, then
   report. Zero discovered targets fails the run.

The pipeline itself is OxDock: static `.oxfile` sources byte-copied
from the tree, entry plus renderer plus per-stage lifecycle files,
with the child rendered fresh every run and piped through a fresh
interpreter. Sequencing and all writes live in the DSL as named
`FUNC` stages; rendering, expansion, and encoding helpers live in
Rust only where the DSL has no primitives (registry rendering,
TOML metadata, strict JSON encoding, placeholder-safe stems).

### Modules

Plugin modules arrive linked into the binary and registered by `run()`:
the core language never sees them.

- `RENDER_ENGINE` builtins: `FILE_STEM` and generic
  `EXPAND_TEMPLATE(raw, vars, env)`. Layered values merge through
  the pure `STD::MERGE_MAPS` builtin instead of engine code.
- `OXDOCK` registry introspection with config-driven `GENERATED(key)`
  dispatch.
- `RUST` Cargo workspace content: members, versions, and package
  metadata for stamps and citations.

Lifecycle files are DSL, not plugin modules: one file per
pipeline participant under `src/`, every file exposing the same
lifecycle methods, registered by path in the config's `files`
list in execution order. Omit the list and the engine's own
`src/lifecycle/generic.oxfile` runs every stage alone.

Third-party plugin modules join through `run_with_plugins(root, extras)`,
which registers external modules so template placeholders resolve them
with no header changes.

### Fetch, parse, render

Remote values enter through the same stream shape scripts use:
`NET::NET_FETCH($url)` fetches a URL to text (Stateful, `https` with
loopback-only `http`), and the pure `STD::PARSE_JSON` /
`STD::PARSE_TOML` parsers turn that text into template values with the
exact conversion file loading uses. Fetching never touches the
filesystem and parsing never touches the network, so either side stays
testable alone.

## Values

Each target declares a `values` file holding its display `name` and
`description`. Every run re-derives both from the owning member's
manifest: `description` always flows from the manifest, while a
committed `name` wins as a display override (`OxDock` vs the package
name `oxdock`). Members without a manifest keep static values files.
Sync writes values only: masters, fragments, and target declarations
are the author's and are never rewritten.
Shared strings stay in the global values file and are referenced
under the configured scope keys (e.g. `{{ $docs_global.* }}`).
Manifest-wide facts ride alongside under their own scope keys:
the workspace version (`version_key`) and the workspace package
record (`vars.workspace_pkg`) carrying author, license, and
repository for citations. Both names come from the config and are
required whenever the workspace snapshot exists.

## Targets and assemblies

A target is declared by a `target.json` file with an output path, a
values file, a master template, and grouped discovery patterns (never
per-file lists):

```json
{"name": "readme", "out": "README.md", "values": ".../values.json", "template": ".../README.md.tmpl", "fragments": {"local": [".../fragments/*"], "shared": [".../shared/*"]}}
```

The master template is an ordinary Markdown file whose section
order is the output order. Wherever it names a section with a
`{{ $files.<group>.<stem> }}` placeholder, the matching fragment
renders there: `{{ $files.shared.intro }}` pulls in
`shared/intro.md.tmpl`. To add a section, drop the file in a
discovered directory and name it from the master with one placeholder
line. To reorder, move the placeholder lines.

Names are file stems (the name up to the first dot) limited to
letters, numbers, `_`, and `-`. A misspelled placeholder, two files
sharing one name, or an unreadable values file fails the run instead
of rendering wrong docs.

Two formatting rules keep assembly exact: keep fragments
newline-terminated with placeholders on their own lines, and escape
literal placeholder examples natively (`{{ ... }}`) so they pass
through untouched.

A target whose output executes directly declares `"executable":
true` beside `out`: the pipeline re-applies `0755` after every render,
so regenerating `install.sh` cannot silently drop the bit the smoke
workflow needs to execute it. Omitted everywhere else; a
present-but-not-boolean flag fails the run instead of rendering.

One values file can feed several masters: the generic example renders
a human `README.md`, a slim agent pointer `llms.txt`, and a full
carrier `llms-full.txt` from the same values through shared fragments.

## License

`oxdock-render` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
