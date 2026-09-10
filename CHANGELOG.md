# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/) and this project adheres to
 (or is loosely based on) Semantic Versioning.

## [UNRELEASED]

### Added

- Unified `LET` output capture: `LET $x = <sync command>` runs the command to completion and binds its exact stdout bytes into `$x` (no newline stripping; commands with no stdout bind `""`; non-UTF8 stdout is an error), spilling to a guarded temp file past 8 MiB instead of buffering unboundedly in memory
- `LET $o = AWAIT $t` captures a background task's stdout into `$o`; bare `AWAIT $t` keeps its status semantics and now forwards the task's stdout to the parent stdout
- Pipe backlog and capture share one spillable sink backed by `GuardedPath::tempdir` (PID-lock GC) instead of `std::env::temp_dir`, with the same 8 MiB spill / 100 MiB backlog-cap behavior; spills stay memory-only under Miri
- `RUN ["exe", "arg", ...]` exec form: spawns the executable directly with no shell, so there is no shell expansion, globbing, redirection, or pipes; use it for portable commands. Elements accept quoted strings, bare words, `$var` / `$a.b`, and `CALL()`; quoted `{{ ... }}` templates interpolate per element while `\$` / `\{{` escapes pass through literally, and `;` / `//` inside elements stay literal. Guards and wrappers (`ASYNC`, `TIMEOUT`, `WITH_IO`) apply to both forms; `RUN []` is an error and shell `RUN <command...>` behavior is unchanged
- `ProcessManager::run_argv` / `spawn_argv` for direct executable spawning across the `Shell`, `Mock`, and Miri `Synthetic` backends, plus documented `INHERIT_STDOUT_ENV_VAR` / `PROCESS_DEBUG_ENV_VAR` constants replacing hardcoded environment variable names
- `WITH_IO` wrapping an `ASYNC` block whose body is a single `RUN`, guarded or not, now promotes the pipe to a zero copy OS kernel pipe: the producer child writes straight into the kernel and a concurrent `RUN` consumer reads straight out, with no copies through memory buffers. DSL consumers (`WRITE`, `READ`, ...) on a live name keep working through a bridged reader. All other shapes keep the in memory script pipe, so sequential fan in, keepers, DSL bodies, and host injected pipes behave exactly as before. Promotion is single producer single consumer by construction: a second producer or consumer on a live name fails deterministically instead of interleaving bytes. The consumer must run while the producer is alive, since output past the 64 KiB kernel buffer stalls until drained. Under Miri everything stays on script pipes with identical results for small payloads

### Changed

- `LET $x = WITH_IO [stdin=pipe:p] <sync command>` now captures instead of failing; combining capture with an explicit `WITH_IO [stdout=pipe:...]` is a parse error since the capture sink owns stdout
- Named `ASYNC` tasks no longer share the parent stdout writer: output is buffered per task and surfaces via `AWAIT` (forward), `LET $o = AWAIT $t` (bind), or end-of-pipeline reaping for tasks that are never awaited
- Host Rust API only, scripts are unaffected: `CommandOptions.stdin` is now a `CommandStdin` enum instead of `Option<SharedInput>`. Rust embedders replace `stdin: Some(x)` with `stdin: CommandStdin::Stream(x)` and `stdin: None` with `stdin: CommandStdin::Null`. `CommandStdout` and `CommandStderr` gain matching host only `OsPipe` variants for direct kernel pipe handoff

### Fixed

- `WITH_IO` docs describe both pipe modes: `ASYNC` single `RUN` pipelines use zero copy OS kernel pipes, sequential steps use script pipes (memory plus 8 MiB spill). The old "named pipes" and "without temp files" wording is removed.

## [0.10.0-alpha] - 2026-09-08

### Added

- Command reference examples demonstrating variable scoping: `ENV` and `LET` assignments inside a braced block revert when the block exits, and `EXPAND` `KEY=val` overrides shadow the environment for that call only
- `WORKDIR` description now documents relative-to-current-directory resolution, `/` reset to the workspace root, and the sandbox guarantee
- `ASSERT_*` descriptions now document abort-on-mismatch failure semantics, plus a new `ASSERT_FILE --hash` example
- Declared argument types are mechanically enforced: static literals type-check at lower time and variables/templates validate on their resolved values at runtime. `SLEEP`/`TIMEOUT` accept dynamic durations (`SLEEP $d`, `TIMEOUT $d`) instead of freezing literals at parse
- `COPY --from-current-workspace` reference example
- docs-gen exits non-zero when any target fails to render, with a regression test pinning the behavior

### Changed

- [docs-gen] READMEs are now assembled from a master template per document: the section order you see in the template file is the order you get in the README. Sections shared between documents live in one file and are pulled in by name, and a misspelled section name fails the build instead of silently rendering wrong docs. Two doc-only renames came along with it: every fragment now ends in `.md.tmpl`, and the `oxdock-build` fragment with parens in its name became `usage-build-rs.md`.

### Fixed

- `EXIT` with a non-integer code is now an error instead of silently exiting 0
- Trailing positionals beyond a command's declared arity fail lowering instead of being silently discarded; tail-joining commands (`ECHO`, `WRITE`/`APPEND` contents, `ASSERT_FILE` expected text, `ASSERT_STDOUT`, `EXPAND` overrides, `INHERIT_ENV` keys) declare variadic `Rest` args so legitimate multi-word use keeps working
- Command reference argument/flag tables escape `|` in type strings (e.g. `SNAPSHOT|LOCAL`), which previously split the WORKSPACE row into extra columns on strict Markdown renderers
- `SLEEP` summary reworded from "Sleep without spawning a shell" to "Pause execution for a duration"

## [0.9.0-alpha] - 2026-09-07

### Added

- `docs-gen` rebuilt as a general-purpose doc engine: ordered `template` / `read` / `glob` / `text` stages executed as pure OxDock DSL (`$var` bindings only, no hand-built AST), config-driven targets discovered from each crate's `.oxdock/template` directory, and plugin data providers (`command-ref`, `cargo-metadata`) with per-target value overrides
- Sparse `target.json` files (just `name`/`out`) synthesize stages from the target directory layout (`header.tmpl`, verbatim `fragments/*.md`, expanded `fragments/*.tmpl`, `footer.tmpl`); bespoke targets declare full stages
- Generated command reference shared three ways from one provider: root README, `oxdock` README, and rustdoc includes (`oxdock/docs/command_reference.md`, `crates/oxdock-parser/docs/command_reference.md`)
- Shared embed example consumed by both the root and `oxdock` READMEs from a single canonical file
- Master-template targets: order comes from `{{> path }}` positions in an `output.tmpl` document instead of a managed JSON stage list (verbatim unless `.tmpl`, which expands); literal document prose expands with the values context

### Fixed

- Quoted values with spaces parse identically in every command: `ENV SET_FORTH="outer scope"` stores `outer scope` instead of truncating, and `EXPAND tmpl KEY="a b"` no longer fails with "accepts at most one path"
- `KEY=$var` env values and `EXPAND` overrides evaluate the variable (parity with `ECHO $var`); `KEY="{{ $var }} tail"` interpolates with the literal tail kept
- Multi-assignment lines split uniformly (`EXPAND K1=$x K2=$y` yields two overrides); `ENV` with more than one assignment is a precise error instead of silently merging or dropping values
- `$var` mixed into `ECHO` / `RUN` / `WRITE` tails is preserved instead of silently dropped (`ECHO $x hello` keeps the value)
- Both `"` and `'` quotes strip in `ENV` values (previously `"` only), and values split on the first `=` (`KEY=a=b` stores `a=b`)
- `GLOB()` patterns containing `..` match nothing instead of traversing outside the sandbox root; every glob result is validated against the workspace boundary
- `GLOB()` lists sandbox contents on Windows (verbatim `\\?\`-prefixed roots no longer yield empty results)
- Windows PID liveness probe treats `ERROR_ACCESS_DENIED` as alive (parity with Unix `EPERM`), so tempdir cleanup never reaps another live process's directories

### Changed

- `ENV` / `EXPAND` reference docs rewritten: what a template is, placeholder namespaces and precedence, override value rules, and runnable proof examples for every value form
- `FOR` / `LET` / `ECHO` reference docs enriched (`GLOB("*")` quoting rule and end-to-end example, expression-only `LET` right-hand side, variable `ECHO` forms, piped-stdin `EXPAND`)

## [0.8.0-alpha] - 2026-09-05

### Added

- `EXPAND` command for template expansion of a file or stdin to stdout, with `KEY=val` overrides alongside `{{ env:KEY }}` interpolation
- `ASYNC` / `AWAIT` / `CANCEL` for background tasks, including block form and `LET $t = ASYNC { ... }` handles
- `TIMEOUT <duration> <command|block>` and `SLEEP <duration>` for deadline control and delays
- `READ_LINE $var` for line-oriented reads into a variable
- `FOR` (value and key-value forms), `IF` / `ELSE IF` / `ELSE`, and `LET` / `ASSIGN` with expression support (`==` / `!=`, `!` negation, `&&` / `||`, `GLOB(...)`)
- Guard expressions: `!` / `not(...)`, `any(...)` / `all(...)`, `eq(...)` / `neq(...)`, `bool:<val>`; `[guard]` prefixes on `LET` / `ENV` / `WORKDIR` / `WORKSPACE` blocks
- Unified block scoping for braced blocks (`IF`, `FOR`, `TIMEOUT`, `ASYNC`, `WITH_IO`) with scope unwind on nested `EXIT`
- `oxdock` facade crate as the canonical entry point; bare `cargo run` launches the CLI
- CLI `--help` / `-h` usage output, positional script paths, and `-` / `--script -` stdin handling
- `oxdock!` proc-macro for inline DSL with `#var` host interpolation, including `FOR` / `LET` blocks and `GLOB(#var)`
- `WorkspaceFs::open_read` / `open_write` / `open_append` streaming file I/O across Host, Miri, and Mock backends

### Changed

- Data pipeline handlers (`WRITE` / `APPEND`, `EXPAND`, `HASH_SHA256`, `ASSERT_STDOUT`) stream in fixed-size chunks instead of buffering whole inputs
- `ASSERT_STDOUT` uses bounded per-step matching instead of an unbounded stdout log; windows are re-expanded on `ENV` / `INHERIT_ENV` mutations
- `RUN_BG` semantics superseded by `ASYNC` task handles with `AWAIT` / `CANCEL`; `RAW_WRITE` superseded by `{{ env:KEY }}` interpolation
- Per-arg quoting tracked via `Arg::String` / `Arg::Expr` (quoted `--flags` stay positional)
- `Guard` inversion replaced by composable `not()` / `!` and `eq` / `neq` / `bool` guards
- Single-site command registry generating step kinds, lowering, and metadata; unknown-command errors include structural and casing hints
- Crate renames: `oxdock-buildtime-helpers` to `oxdock-build`, `oxdock-buildtime-macros` to `oxdock-macros`; `embed!` / `prepare!` to `oxdock_embed!` / `oxdock_prepare!`
- Docs generated from the command registry; `pulldown-cmark` replaces the custom markdown parser
- `spawn_interactive_shell` moved to `oxdock-process`; CLI runner is a thin delegate
- Test layout collapsed to single integration binaries per crate with a `slow-integration` feature gate
- `oxdock-fs` path handling normalized for Windows CI parity
- Bump `syn` 2.0.119 → 3.0.4

### Removed

- `RUN_BG` command (use `ASYNC`); `RAW_WRITE` command (use interpolation)
- Old `oxdock-buildtime-helpers` / `oxdock-buildtime-macros` crate names and bare `embed!` / `prepare!` macro names
- Unbounded `ExecState::stdout_log`; legacy `expand_with_lookup`; custom markdown parser; `DocSpec` in `docs-gen`; orphaned `oxdock-process` `test_utils` module

### Fixed

- Template `}}` split across chunk boundaries is detected; empty input preserves pending expansion state
- `open_append` on the Miri backend appends instead of overwriting from position 0
- `ASSERT_STDOUT` falls through to step-scope matching only on empty stdin; error output includes buffered content for debugging
- Long-needle assertions no longer truncate history prematurely
- Unknown-command diagnostics suggest structural statements and correct casing
- Nested `EXIT` unwinds `LET` / `ENV` / `WORKDIR` / `WORKSPACE` scopes without leaking
- `CANCEL` on completed tasks and double-`CANCEL` succeed without affecting unrelated tasks; `TIMEOUT` kills only the timed-out task

## [0.7.0-alpha] - 2026-08-27

### Refactoring

- **oxdock-core**: Split `exec.rs` into focused modules: `handlers`, `fs_ops`, `io`, `pipe`, `state`, `steps`, and `tests` for better maintainability
- **oxdock-process**: Decomposed `lib.rs` into `builder`, `shell`, `child`, `contract`, `expand`, `shell_manager`, `synthetic`, and `builtin_env` modules

### Added

- `APPEND` command for cross-platform append-only file writes (ideal for GitHub Actions `$GITHUB_OUTPUT`, `$GITHUB_ENV`, `$GITHUB_STEP_SUMMARY`)
- GitHub Actions Integration section in README documenting `ECHO`, `RUN`, and `APPEND` patterns for workflow commands
- Markdown DSL parsing support (`oxdock-parser/src/markdown.rs`)
- `OXDOCK_EMBED_FINGERPRINT_SALT` environment variable for cache busting
- `ASSERT_STDOUT` and `ASSERT_ABSENT` command prototypes
- Docs conformance tests and packaging invariant tests
- Expanded README with comprehensive documentation

### Fixed

- Fuzz parity test failure: filter out strings that fail `proc_macro2` lexing instead of panicking

### Dependencies

- Bump `anyhow` 1.0.100 → 1.0.104
- Bump `libc` 0.2.178 → 0.2.189
- Bump `libtest-mimic` 0.8.1 → 0.8.2
- Bump `line-ending` 1.5 → 1.5.1
- Bump `pest`/`pest_derive` 2.8.4 → 2.9.0
- Bump `proc-macro2` 1.0.103 → 1.0.107
- Bump `proptest` 1.9.0 → 1.11.0
- Bump `quote` 1.0.42 → 1.0.47
- Bump `sha2` 0.10.9 → 0.11.0 (with API migration)
- Bump `syn` 2.0 → 2.0.119
- Bump `tempfile` 3.24.0 → 3.27.0
- Bump `toml_edit` 0.24.0 → 0.25.13
- Update all transitive dependencies via `cargo update`
