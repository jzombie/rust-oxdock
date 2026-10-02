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

1. Load `docs-gen.json` (global values files, doc root scopes, generated
   inputs, merge policy, staging dir, version key, scope key aliases).
2. Render generated inputs through config-driven `GENERATED(key)`
   dispatch (command index and body, function references): the script
   only knows logical keys while the config owns the output paths, so
   generated content is shareable instead of trapped in one document.
3. Sync each member's `values.json` from its manifest, values only (see
   below). Target files, masters, and fragments are never touched by
   sync.
4. Read Cargo workspace content once (`WORKSPACE_MEMBERS`,
   `WORKSPACE_VERSION`, `WORKSPACE_PACKAGE`) for version stamps and
   citation metadata, carried on the expansion scope under the
   configured `workspace_pkg` key.
5. Assemble one `$files` manifest per target: glob every `fragments`
   pattern, expand each match once, and write the group-to-content map
   for the pipeline to `LOAD_JSON`.
6. Render every target: the `RENDER_TARGET` stage reads each
   `target.json`, loads its values and `$files` manifest, and expands
   the master template once through native `EXPAND` piped to `APPEND`.

The pipeline itself is OxDock, embedded in `src/lib.rs` via `oxdock!`
and parsed by the production dispatcher. Sequencing and all writes live
in the DSL as named `FUNC` stages; rendering, expansion, and encoding
helpers live in Rust only where the DSL has no primitives (registry
rendering, TOML metadata, strict JSON encoding, placeholder-safe
stems).

### Modules

Domain content arrives as in-crate plugins registered only by `run()`:
the core language never sees them.

- `DOCS_GEN_ENGINE` builtins: `FILE_STEM`, generic
  `EXPAND_TEMPLATE(raw, vars, env)`, `MERGE_VALUES`.
- `OXDOCK` registry introspection with config-driven `GENERATED(key)`
  dispatch.
- `RUST` Cargo workspace content: members, versions, and package
  metadata for stamps and citations.

Third-party plugins join through `run_with_plugins(root, extras)`,
which registers external modules so template placeholders resolve them
with no header changes. `pipeline_engine()` returns an engine carrying
exactly the `DOCS_GEN_ENGINE` default, for custom scripts that need
the engine primitives without the domain plugins.

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
Shared strings stay in the global values file and are referenced as
`{{ $docs_global.* }}`. Manifest-wide facts ride alongside under
their own scope keys: the workspace version (`version_key`, default
`CRATE_VERSION`, overridable per run through the environment) and the
workspace package record (`vars.workspace_pkg`, default
`workspace_pkg`) carrying author, license, and repository for
citations.

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

One values file can feed several masters: the generic example renders
a human `README.md`, a slim agent pointer `llms.txt`, and a full
carrier `llms-full.txt` from the same values through shared fragments.
Each master selects its sections, so the split between outputs is
structural (same values, same fragments) and can never drift into
copies.

## License

`docs-gen` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
