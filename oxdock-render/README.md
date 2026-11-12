# oxdock-render

Generate text files from templates and data: READMEs, docs, installers, and other release artifacts, rendered deterministically from a single pipeline config.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

## Overview

`oxdock-render` generates text files from templates and data files, so generated files never drift from their sources. Its first job is this workspace's own docs: every `README.md`, the install scripts, and the CI action are rendered from templates under `.oxdock/template/`, and CI fails if a generated file does not match a fresh render. The same engine generates any text artifacts, not just Markdown: one config lists targets, and each target names its output file, its template, its data, and its fragment folders.

Render this workspace:

```sh
cargo run -p oxdock-render
```

Render another checkout with its own pipeline:

```sh
oxdock-render --root /path/to/repo --config render.json
```

See what would run without writing anything:

```sh
oxdock-render --root /path/to/repo --print-child
```

Flags: `--root <path>` (workspace root, default: discovered),
`--config <path>` (pipeline config, default: `render.json`),
`--print-child` (print the generated script, skip the run).

Fail-closed throughout: duplicate target names, zero discovered
targets, misspelled placeholders, and unknown values fail the run
instead of writing wrong outputs.

## Pipeline

Each run performs the same steps, in order:

1. Load `render.json`: global values, which files to scan,
   generated inputs, and naming rules.
2. Serve the binary's embedded pipeline scripts from memory and
   check every plugin implements the same stages, failing fast with
   the offender named instead of halfway through a target.
3. Build the run script: one step per stage per plugin, in
   registration order (`SETUP` once up front, then each stage per
   target), piped through a fresh interpreter that never touches
   disk.
4. Per target: check the target declaration, expand the fragments
   into the master template, and write the output file. The docs
   plugin first refreshes generated inputs and re-reads workspace
   metadata.
5. Restore executable bits declared by targets (so regenerating
   `install.sh` keeps its executable bit), then report. Zero
   discovered targets fails the run.

The pipeline itself runs on OxDock scripts shipped inside the
binary: static `.oxfile` sources plus one lifecycle file per
pipeline participant, with the run script built fresh every run.
Step order and all file writes live in those scripts as named
stages; Rust only handles what the scripts cannot (workspace
metadata, strict encoding, filename rules).

### Modules

Behavior arrives through plugin modules linked into the binary:

- `RENDER_ENGINE`: filename and template-expansion helpers.
- `OXDOCK`: command and function reference data used by the
  generated docs.
- `RUST`: Cargo workspace facts (members, versions, and package
  metadata) used for version stamps and citations.

Each pipeline participant also ships one lifecycle script
(`src/lifecycle/*.oxfile`) implementing the same stages; the
config lists them in run order. Leave the list out and a generic
default runs every stage alone.

External modules join through `run_with_plugins(root, extras)`,
so templates can call them with no other changes.

### Fetch, parse, render

Remote data enters through the same calls scripts use:
`NET::NET_FETCH($url)` fetches a URL to text, and the pure
`STD::PARSE_JSON` / `STD::PARSE_TOML` parsers turn that text into
template values exactly as file loading would. Fetching never
touches the filesystem and parsing never touches the network, so
each side stays testable alone.

## Values

Each target names a values file holding its display `name` and
`description`. Every run re-reads both from the owning crate's
manifest: `description` always comes from the manifest, while a
committed `name` wins as a display override (`OxDock` vs the
package name `oxdock`). Crates without a manifest keep static
values files. Sync writes values only: templates, fragments, and
target declarations are the author's and are never rewritten.
Shared strings live in the global values file under scope keys
(e.g. `{{ $docs_global.* }}`).
Workspace-wide facts travel alongside under their own keys: the
workspace version and the workspace package record (author,
license, repository) used for citations.

## Targets

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

Two formatting rules keep output exact: keep fragments
newline-terminated with placeholders on their own lines, and escape
literal placeholder examples natively (`{{ ... }}`) so they pass
through untouched.

A target whose output executes directly declares `"executable":
true` beside `out`: the pipeline re-applies `0755` after every render,
so regenerating `install.sh` cannot silently drop the executable
bit. Omitted everywhere else; a present-but-not-boolean flag fails
the run instead of rendering.

One values file can feed several masters: the generic example renders
a human `README.md`, a slim agent pointer `llms.txt`, and a full
carrier `llms-full.txt` from the same values through shared fragments.

## License

`oxdock-render` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
