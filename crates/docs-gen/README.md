# docs-gen

Renders every README in the workspace from shared templates.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

## Overview

`docs-gen` builds every `README.md` in this workspace from templates,
so shared sections are written once and reused everywhere. Run it with
`cargo run -p docs-gen` after changing anything under a
`.oxdock/template` directory, then commit the regenerated outputs.

Concretely, each run:

- assembles every README from a master template whose section order
  is the order you see in that file,
- fills in sections shared between documents (the project intro,
  install instructions, embed example) from single canonical files,
- stamps the workspace version into every version reference from one
  source (`CRATE_VERSION`, overridable per run),
- regenerates the command reference from the parser command registry,
  so the docs can never list a removed command or miss a new one,
- re-derives each member document display name and description from that
  member's `Cargo.toml` (documents without a member manifest keep static
  values files),
- and fails the whole run on a misspelled section or unknown value
  instead of rendering wrong docs.

It accepts one flag:

- `--root <path>`: which workspace root to render. Without it, the
  runner discovers the enclosing workspace from the current
  directory.

## Pipeline

Each run performs the same steps, in order:

1. Load `docs-gen.json` (global values file, doc root scopes, generated
   input destinations).
2. Render the command metadata (index and body from `oxdock-parser`) and
   the function registry (function reference from `builtin_function_metas`)
   into the three generated inputs declared by `docs-gen.json`, so
   generated content is shareable instead of trapped in one document.
   The script only knows the three logical inputs; the config owns the
   paths, so the pipeline works for any workspace layout.
3. Sync each member's `values.json` from its manifest (see below).
4. Assemble one `$files` manifest per target: glob every `fragments`
   pattern, expand each match once, and write the group-to-content map
   for the pipeline to `LOAD_JSON` (see below).
5. Render every target: the `RENDER_TARGET` stage reads each `target.json`,
   loads its values and `$files` manifest, and expands the master
   template once through native `EXPAND` piped to `APPEND`.

The pipeline itself is OxDock, embedded in `src/lib.rs` via `oxdock!`
and parsed by the production dispatcher. Sequencing and all writes live
in the DSL as named `FUNC` stages; rendering, expansion, and encoding
helpers live in Rust only where the DSL has no primitives (registry
rendering, TOML metadata, strict JSON encoding, placeholder-safe
stems).

## Values

Each target declares a `values` file holding its display `name` and
`description`. Every run re-derives both from the owning member's
manifest: `description` always flows from the manifest, while a
committed `name` wins as a display override (`OxDock` vs the package
name `oxdock`). Members without a manifest keep static values files.
Shared strings stay in the global values file and are referenced as
`{{ $docs_global.* }}`.

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

## License

`docs-gen` is distributed under the terms of the Apache License (Version 2.0).
