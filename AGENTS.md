# OxDock Workspace Guardrails

## Linting

Do not override linter rules in any crate. The only permitted narrow, crate-local `#[allow(...)]` exceptions are the system crates located under `crates/sys/*` (for example, `oxdock-fs`, `oxdock-process`, `oxdock-sys-test-utils`). Never add workspace-wide or blanket overrides.

- **Naming**: Do not prefix parameter names with `_` to silence unused warnings. Prefer using the value (e.g., `let _ = param;`) or a localized `#[allow(unused_variables)]` if absolutely necessary.

- **Strategy**: When addressing clippy warnings, prefer localized `#[allow(...)]` on specific items (functions/impls/structs) instead of crate-level `#![allow(...)]`.
    - Avoid blanket overrides on traits or impl blocks if the lint only applies to a subset of methods; apply the suppression to the individual methods instead.
    - For imports, split the `use` statement so that `#[allow(...)]` only applies to the specific disallowed item.
    - For enums/structs, apply `#[allow(...)]` to the specific variant or field that uses the disallowed type, not the whole type definition.
- **Scope**: Do not perform repository-wide or file-wide blanket changes to satisfy lints; limit edits to the minimal, justified scope.
- **Enforcement**:
    - `clippy.toml` is used to `deny` `disallowed_macros` (like `std::cfg`), `disallowed_methods`, and `disallowed_types`.
    - `[workspace.lints.clippy]` in `Cargo.toml` enforces these rules workspace-wide.
    - Crates should not override these defaults unless absolutely necessary (rare).

## Miri & Isolation

Prefer explicit, test-only skips over runtime detection.

- **Principle**: Avoid runtime Miri detection in implementation code. Tests *can* be skipped under Miri only when necessary, and they *MUST* include the reason.
- **Implementation**:
    - Do not use `cfg!(miri)` or `#[cfg(miri)]` outside of `crates/oxdock-fs` and `crates/oxdock-process`, except for test-only `#[cfg_attr(miri, ignore = "...")]` skips that include a reason.
- **Exceptions**:
    - Implementation crates (`oxdock-fs`, `oxdock-process`) may legitimately use `cfg!(...)` or `#[cfg(...)]` internally.
    - Use narrow, localized `#[allow(clippy::disallowed_macros)]` or `#[allow(clippy::disallowed_methods, clippy::disallowed_types)]` on specific functions/items to keep the rest of the crate provably conformant.

## Filesystem & Process

- **Filesystem**: Use the `oxdock-fs` abstractions (`GuardedPath`/`UnguardedPath`, `PathResolver`, `GuardedPath::tempdir`) instead of raw `std::fs` for guarded paths; keep paths under their guards.
    - Do not use `tempfile` directly in any crate other than `oxdock-fs`.
    - Normalize paths via the shared helpers (`to_forward_slashes`, `normalized_path`, `PathResolver::parse_env_path`) instead of ad-hoc `.replace` chains or string comparisons.
- **Process Execution**: Use `oxdock-process` abstractions instead of raw `std::process::Command`.
    - Logic handling process execution differences (e.g. Miri vs Native) belongs in `oxdock-process`.

## Cross-Platform Compatibility

- **Consistency**: All features and DSL commands must behave identically across platforms (Linux, macOS, Windows) as much as possible.
- **Exceptions**: Only `RUN` and `ASYNC` commands are expected to differ, as they execute arbitrary shell commands specific to the host OS.
- **Testing**: Tests must ensure parity. If platform-specific setup is required (e.g. creating symlinks in test fixtures), ensure both Unix and Windows paths are covered.

## Testing & Layout

- **Testing**: Prefer `cargo test --workspace --tests` to cover all crates; fixtures for the macros live under `crates/oxdock-logic-tests/fixtures/integration/buildtime_macros`.
- **Workspace layout**: Internal crates live under `crates`; the CLI & build-time macros sit at the workspace root.

## Code Style

- **Punctuation**: do not use em dashes or en dashes as punctuation in code comments, doc comments, or any other prose. Use periods or colons instead. Hyphens inside code, identifiers, crate names, CLI flags, and versions remain allowed. This extends the README prose-style rule below to all written text in the repo.

## Documentation (generated READMEs)

- **Do not edit `README.md` files by hand.** All READMEs are rendered by the native OxDock pipeline in `crates/docs-gen/src/main.rs` (`cargo run -p docs-gen` uses the workspace version; `CRATE_VERSION=<version>` overrides it); any manual additions will be overwritten on the next run. Each `target.json` declares its `out` path, `values` file, master `template`, and grouped `fragments` discovery patterns (discovered via the `scopes` in `docs-gen.json`); order lives in the master template as `{{ $files.group.stem }}` placeholders. See `crates/docs-gen/README.md` for the pipeline.
- **Edit the templates instead**: per-crate sources live under `<crate>/.oxdock/template/` (a master `<out-basename>.md.tmpl` plus `header.md.tmpl`, `fragments/`, `footer.md.tmpl`); the workspace and `oxdock` READMEs compose shared and generated sections through the same placeholders. Placeholder keys are file stems (the name up to the first dot). This repo standardizes on `*.md.tmpl` for markdown so the output type is visible. Shared strings stay single-sourced in `.oxdock/template/_global/values.json` and are referenced as `{{ $docs_global.* }}`. Per-target `values.json` files are auto-synced from member manifests on every run: `description` always flows from the manifest, while a committed `name` wins as a display override. Shared lead copy lives once in `.oxdock/template/shared/intro.md.tmpl` and is staged by both README targets. Re-run `docs-gen` after changing templates and commit the regenerated outputs.
- **Prose style**: do not use em dashes, en dashes, or hyphens as punctuation in README prose and templates. Use periods or colons instead. Hyphens inside code, crate names, CLI flags, and versions remain allowed. Fragments documenting DSL `{{ ... }}` examples escape them natively (`\{{ ... }}`) so they pass through strict expansion byte-identical.
- **Doc examples narrate**: every multi-stanza `oxdock` fence in templates and READMEs carries `#` comment lines stating what each stanza demonstrates. Whole-line hash comments read cleaner than `//` in DSL scripts. Prefer one continuous fence per narrative over many small fragments: fragments hide the cross-stanza context (setup, scope entry and exit) that makes the behavior legible. A bare run of repeated WRITE/READ/ASSERT lines with no explanation is an unscannable blob: one comment line per stanza, naming the behavior shown. Separate the stanzas with blank lines so each comment, action, and assertion reads as one visual unit. Comments are ordinary DSL comments, so annotated fences keep executing unchanged under `docs_conformance`.

## Process-Manager Boundary (decision record)

`oxdock-process` and term-wm's PTY stack stay **independent**: batch script-to-completion orchestration vs. interactive PTY/vt100 streaming are disjoint mechanics with conflicting constraints (sync/std/Miri-clean vs. thread+channel event loops). The only genuine overlap — `shell_program()` shell resolution and `is_pid_alive()` liveness probing in `oxdock-fs` (backing the tempdir PID-lock GC) — is behavior-pinned by tests, so if duplication ever hurts, extraction into a small shared micro-crate is a mechanical follow-up. Do not introduce cross-dependencies between these projects without revisiting that assessment.

## Workflow

- **Autonomy**: When test failures are reported or observed, proceed to investigate and fix them without asking for confirmation unless there are multiple viable options or the change is risky/behavior-altering.
- **Formatting**: Test and fixture scripts are always `indoc` raw blocks (`indoc::indoc! {r#"..."#}`), never `\n`-joined string continuations. Single-line scripts stay plain string literals. `\n` joins are unreadable in review and hide the script's shape; there is no exception for tests.
- **Doc examples execute**: every Rust fence anywhere in the repo (doc templates, rustdoc comments, READMEs) must compile and run as a doctest. Never mark a fence `ignore`/`text` to dodge a non-compiling sketch. If an example cannot stand alone, inline what it needs or delete it. If an example cannot compile in its own crate (e.g. a proc-macro crate demonstrating downstream types), relocate it to a crate where it compiles and reference it; never keep an uncompiled duplicate alongside the runnable original. Duplicated examples diverge; there is exactly one runnable copy.
- **DSL scripts read as DSL**: runnable doc examples build scripts with the `oxdock!` macro (compile-time DSL tokens), never `\n`-joined string continuations. Exceptions, each with a reason: `oxdock-core`'s own doctests cannot use `oxdock!` (dependency cycle: the macro crate depends on core), and parse-error demos must feed strings to the parser (a compile-time macro cannot produce a runtime parse failure). Everywhere else, multi-line strings go in `indoc` raw blocks (`indoc::indoc! {r#"..."#}`).
