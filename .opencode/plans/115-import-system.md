# Plan: #115 External script import system + dry-run preview

Source: https://github.com/jzombie/rust-oxdock/issues/115 (OPEN)
Plans dir per `SCRIPT::PLAN_DIR`: `CWD/.opencode/plans/*.md`.

## 1. Goal (from issue body)

Add `IMPORT` for OxDock files as composable, pure executable pipelines:
- Any file valid as CLI entry is valid as import target, any extension at all, especially `.oxdock` and `.oxfile`. No extension gate in loader.
- Import executes in function-call-like isolation with optional caller-supplied context.
- Static pre-execution preview so rogue scripts are visible before `RUN` effects.
- Chain: `#111 -> #114 -> #115`. Do not duplicate `#111` capture plumbing or `#114` scope/`Flow` mechanics. Rebase if `StepKind::AssignCapture`, `TaskEntry` sink, `ExecState::fork/push_scope/pop_scope`, or `Flow` changed.

Out of scope (stays in #153 MCP track, per 2026-10-21 comments):
- Single-file `EXPORT FOO` / `EXPORT [FOO, BAR]`: top-level only, validated against top-level `FUNC`s, forward refs allowed. Already shipped.
- #115 owns: full file-based module loading, cross-file visibility, purity.

## 2. Mandatory context: all comments (11)

1. **Pipes must not cross-talk** (`#115#issuecomment-5611863704`, linked at top of issue body as MUST-read). Flat `PipeRegistry` hazards (original comment used old `pipe:` names; now `$`-vars):
   - SPSC OS pipe collision if two modules use same `$data` name.
   - `ScriptPipe` accidental fan-in with common names (`$tmp`, `$out`) causing interleaving + broken `EOF`.
   - Directives: automatic module name mangling (`ModuleID::PipeName`) at AST lowering or frame push; explicit interface parameterization only (`IMPORT "./worker.oxdock" WITH_IO [stdin=$x, stdout=$y]`); frame-bound registry cleanup on import return. Note: issue comment used old `pipe:` syntax; current syntax is `$`-vars per `WITH_IO [stdout=$p]` in `exec/tests.rs`. Open question in comment: isolated sub-executors with own `ExecIo` vs shared `ExecState` stack. Plan below picks shared-stack + mangling (consistent with `call_func` / `execute_scoped_steps`), unless review prefers sub-executor.
2. **Dry-run relates to #82** (auditor/summarizer/permissions layer). Preview lists `file:line CMD args`, `RUN` payloads; cannot prove shell safety, only visibility. Reuse loader + parser only, no handlers/processes/FS writes.
3. **Remote URL gating update**: gate behind a feature, enabled by default, plus a `READ`-like command for pulling network requests into a stream. Supersedes older issue-body sketch (`--allow-remote-imports` flag + env + hash pin). Keep hash pin + cache under `GuardedPath::tempdir`; Miri always errors on remote path.
4. **Relates to #151** (initial `IMPORT` syntax integration). Check #151 before parser edits to avoid conflict.
5. **Mandatory `AS` alias** (`#issuecomment-5723818456`): file imports are `IMPORT ["some_file.oxdock" AS SOME_FILE]`. Alias mandatory, no filename inference. Alias is the namespace: imported `FUNC`s register as `SOME_FILE::FOO` via existing qualified-registry path with usual shadow/collision rules. Bare file `IMPORT` without `AS` is a parse error. Applies uniformly to local paths, future remote URLs, compile-time embedding.
6. **Modules exemption** (`#issuecomment-5723838136`): `modules: [...]` / `IMPORT [STD]`-style module names need no alias. Alias collisions with live module names (`STD`, `SCRIPT`, host, other alias) fail with explicit panic naming both parties, no shadowing/merging.
7. **`net` feature** (`#issuecomment-5752259628`, PR #160): all network comm rides through `oxdock-net-plugin`. Do not add direct fetch in core.
8./9. **EXPORT split** (two comments 2026-10-21, second corrects shell-mangled span): as above in section 1.
10. **MCP export scoping + pipeline reuse** (2026-11-01): only direct script `EXPORT` list becomes MCP tools; imported funcs stay private unless main script explicitly re-exports. Dogfood candidate: `gha-*.oxfile` changelog date gate shared between `gha-rust-validate.oxfile` and `gha-rust-release.oxfile`.
11. **Static analysis over closure** (2026-11-01): analyzer / `validate_script_types` must walk the full import graph, not entry file alone.

## 3. Current state (verified 2026-11-01)

- Module `IMPORT [STD]` / `IMPORT [STD, DOCS]` exists (`crates/oxdock-parser/src/parser.rs:4314`, `commands.rs:474`, `macro_input.rs:1594`). File `IMPORT "path" AS ALIAS` does not exist.
- `EXPORT FOO` / `EXPORT [FOO, ...]` exists single-file (`parser.rs:4411,4444,6462,7018`), consumed by `crates/oxdock-mcp`.
- `ExecState::fork` (`crates/oxdock-core/src/exec/state.rs:625`), `execute_scoped_steps` (`steps.rs:1705`), `call_func_value` in `handlers.rs` are the reuse targets. `FunctionRegistry` already qualified (`MODULE::BASE`, `native.rs:118`).
- No `StepKind::Import` yet. No `import_stack` / `import_cache` in `ExecState`.

## 4. Design

### 4.1 Syntax (parser)

```
IMPORT ["./lib.oxdock" AS LIB]
IMPORT ["./a.oxdock" AS A, "./b.oxdock" AS B]
IMPORT [STD,
        "./lib.oxdock" AS LIB, "./nested.oxdock" AS NEST]
IMPORT [[FOO, BAR FROM "./lib.oxdock"]]
IMPORT [FOO FROM "./lib.oxdock"]
IMPORT [[FOO FROM "./lib.oxdock" AS H], STD]
IMPORT ["./lib.oxdock" AS LIB] WITH_IO [stdin=$x, stdout=$y]
LET $r = IMPORT ["./lib.oxdock" AS LIB]
LET $t = ASYNC IMPORT ["./lib.oxdock" AS LIB]
```

- `dsl.pest`: new `import_file_statement`; wire into `element` before generic `command`. `IMPORT` keyword with `~ !(ALNUM|_)` guard (same as existing).
- Brackets hold comma-separated entries of either kind, across lines:
  each module word registers in order, each `"path" AS ALIAS` entry
  emits one `ImportFile` step in order. Without an alias a file entry
  is a parse error.
- Value positions (`LET` RHS, `ASYNC` bodies, `TIMEOUT`/`ISOLATED`/
  `WITH_IO` single inners) take exactly one file entry and no modules;
  anything else shaped like an import is a hard error there.
- Selective harvest `[[F, G FROM "path" (AS ALIAS)?]]` defines only the
  named top-level `FUNC`s (bare, or namespaced) without executing the
  file body; a single name needs no inner brackets (`[F FROM "path"]`,
  several keep `[[F, G FROM ...]]` so the comma stays grouped). The
  file's own nested imports still load. Missing names
  fail at import time. Produces no value: never valid in value
  positions. Bare names declare into the parse scope (duplicate and
  reserved rules mirror `FUNC`); aliases follow file-import rules.
- Paths resolve workspace-side against the importer dir (entry parent
  at top level, workspace root / build context for stdin). Script-made
  files land snapshot-side by default: generate-then-import needs
  `WORKSPACE LOCAL`.
- Lifecycle: every import joins `import_stack` for its duration
  (cycle/depth/base-dir correctness for nested loads, both forms) and
  clears `import_alias_frame` when the stack empties. Harvests push no
  scope (nothing executes), so only the stack unwinds there.
- Keep `IMPORT [STD]` module form untouched. Disambiguate per entry:
  `Word` means module, string literal means file form.
- `AS` mandatory per file entry; missing `AS` or non-uppercase alias = parse error. Alias must satisfy existing module-name rules (uppercase, checked where `qualify` validates).
- Guard rule: a top-level guard on a statement with module entries
  fails (`IMPORT cannot be guarded`, historical); file-only statements
  consume the guard like any other element; in-block mixed statements
  attach it to the file steps while modules register unconditionally.
- Optional trailing `KEY=value` overrides: defer per issue open decision; recommend single `Arg::Rest(KeyValue)` tail OR defer to phase 2. Phase 1: no overrides (isolation only), add overrides after isolation + pipes land.
- `WITH_IO` bindings on `IMPORT` callsite: explicit pipe parameterization only (per comment 1). No implicit pipe inheritance beyond what `CALL` already does; internal `WITH_IO` names inside callee get mangled.
- `LET $r = IMPORT ...` lowers to `AssignCapture{var, body: Import}` (same path as `LET $r = CALL`); `LET $t = ASYNC IMPORT ...` lowers to `AssignAsync`. Bare `IMPORT` discards return, keeps FS/pipe side effects.
- `commands.rs`: `structural [...] += ImportFile{path:Arg, alias:String, overrides?...}` + `all_structural_metadata()` + `structural_hint` + tripwire update.
- `ast.rs`: Display/coverage helpers; `IMPORT cannot be guarded` + `EXPORT`-style top-level checks as applicable (file IMPORT is executable so allowed nested, unlike `IMPORT [STD]` prelude rule; confirm against #151).
- `oxdock-macros`: inline `oxdock!` scripts with file `IMPORT` resolve against `CARGO_MANIFEST_DIR`; document limitation.

### 4.2 Loader + runtime

New `crates/oxdock-core/src/exec/import.rs` (or extend `args.rs`):
- `resolve_import(path_arg, caller_file_dir, alias) -> (canonical GuardedPath, text)`:
  - Local only in phase 1: `resolve_read(caller_dir, path)` via `PathResolver`; reject `..` escape; v1 search order relative-to-importer only. Accept any file extension (especially `.oxdock`, `.oxfile`); parse contents as script regardless of extension. Callee relative paths resolve against its own file dir (issue recommendation). Never `std::fs` directly; all through `oxdock-fs` (`GuardedPath`/`PathResolver`).
  - Cycle detection: `HashSet<canonical GuardedPath>` per top-level run + depth limit (e.g. 32). Error names full chain.
  - Cache: `HashMap<GuardedPath, Vec<Step>>` per run so diamond imports parse once. Share across `fork()` via `Arc<Mutex>` like `named_tasks`.
- Parse each file once with `parse_script`; retain `file:line` spans for errors.
- `handlers.rs::import_file_body`: mirror `call_func`: `push_scope` + `ScopeSnapshot{cwd,root,envs}`, bind overrides (phase 2), qualify callee `FUNC`s under alias (`ALIAS::FOO`), `execute_steps(body)`, catch `Return(Value)` as result, `pop_scope` always. Bare vs `AssignCapture` modes; `ASYNC IMPORT` via `fork()` with shared func/import registries.
- `steps.rs`: `StepKind::ImportFile` arm; `Break/Continue` cannot cross import boundary (step-numbered error); `Return` terminates import frame as its value; `Exit` aborts whole pipeline. `pre_register_assertions` accounts for imported `ASSERT_STDOUT`.
- `state.rs`: add `import_stack: Vec<GuardedPath>` + `import_cache` (or ctx struct to avoid bloating clone semantics).
- Pipes (comment 1, load-bearing): mangle internal pipe var keys as `AliasOrModuleID::VarName` during frame push; caller `WITH_IO` bindings (e.g. `[stdin=$x, stdout=$y]`) passed at callsite are the only cross-boundary pipes; drop namespaced entries on frame pop. Add collision tests: two imports using same internal `$data` must not collide; no implicit fan-in.
- Fingerprinting: extend `oxdock-build/src/track.rs:plan_input_directives` + `asset_input_fingerprint` to walk transitive local imports.
- Remote (phase 3): `net`-plugin fetch, feature-gated (enabled by default per comment), allowlist + hash pin, tempdir cache, Miri error. No new direct socket code; use `EndpointRegistry` / `acquire_listener` / `resolve_service_target` paths.

### 4.3 Preview / dry-run + auditor

- `oxdock --dry-run` (name TBD vs `--plan`): expand transitive import closure (local only in phase 1), print ordered `file:line CMD args` with `RUN` payloads verbatim/redacted, no handlers, no processes, no FS writes. Reuses parser + loader only.
- Coordinate with #82 auditor: same closure walker feeds permission summary (FS-outside-guard, URLs, `RUN`).
- Golden test: dry-run lists transitive steps without executing.

### 4.4 MCP + static check + docs

- MCP: only the entry `EXPORT` list becomes tools (comment 10).
  Re-exports (`EXPORT ALIAS::FOO`) promote imports to the entry
  surface and resolve through the registry at serve time; unknown
  re-exports fail tool setup instead of serving dead tools.
- `static_check.rs::validate_script_closure`: two-phase whole-closure validation (comment 11). Phase 1 expands the transitive import closure (`expand_import_closure`: resolve + parse + cache, cycle/depth guarded, missing files hard-fail); phase 2 collects every site's signatures (alias-qualified, or bare for alias-free harvests) into one checker and checks the entry plus every file body, so cross-file calls typecheck at their call sites. Everything present validates before the first step runs; nothing absent is skipped.
- Docs: structural metadata auto-flows; add `IMPORT` file-form fragments + preview + remote gating docs. `cargo run -p docs-gen`, never hand-edit READMEs.

### 4.5 Module semantics: shared state, single loading path

Files are modules with init-once semantics (Python shares this half:
import executes once, every importer sees the same state). Visibility
is deliberately stricter than Python: only `EXPORT`ed names cross the
boundary (unexported means private, and only entry `EXPORT`s become
MCP tools).

- One registry keyed by resolved path: parse once, validate once,
  initialize once. `import_completed` and alias+frame keys are gone;
  every alias is a view onto the same module, with the same state.
- Module top-level `LET`s drain into a persistent per-module store on
  first initialization. Module-owned `FUNC`s (owner-tagged at define
  time) resolve names as params, then call frame, then the owning
  module's store; `LET` inside a call stays call-local. Entry `FUNC`s
  stay params-only. Ambient run state (`ENV`/`WORKDIR`/`WORKSPACE`)
  still reverts at every scope boundary, including module init.
- Harvesting a function that touches module state fails explicitly
  (static probe plus runtime probe): harvests stay side-effect free
  by construction, and stateful helpers require a full import.
- One loading path: `load_file` parses, validates, and initializes.
  Expansion pre-warms it for files present at startup (fail fast
  before step 1); files generated mid-run go through the identical
  gate at first `IMPORT`. No separate "deferred" concept: same code,
  same guarantees, two timings, stated plainly in the reference.
- Cycles stay hard errors (no partial initialization).

## 5. Phasing (in scope: phases 1+2; phase 3 deferred to #270)

1. Phase 1: local file `IMPORT ["..." AS ALIAS]` (any extension, especially `.oxdock`/`.oxfile`; multi-entry and mixed module+file brackets, multiline included) + isolation + pipe mangling + `LET`/`ASYNC` capture + cycles/depth/cache + dry-run listing + tests.
2. Phase 2: override map (`KEY=value` vs `WITH $map`, one form), `modules:` exemption enforcement + collision panics. (Re-export rule for MCP: done — entry `EXPORT ALIAS::FOO` parses for visible aliases and resolves at serve time.)
3. Phase 3 (deferred to #270 `net: Run scripts from a remote URL`): remote URL imports via `net` plugin + feature gate + `READ`-like stream command + hash pin. Remote entry composes with #115 rules once fetched; see #270 for allowlist + hash-pin gating, `net`-feature gate, and cached downloads.

## 6. Tests

- Parser: file form with `AS`, multi-entry and mixed brackets, multiline, `FROM` harvests (bare + aliased), missing `AS` errors, bad alias errors, `LET $r = IMPORT`, `LET $t = ASYNC IMPORT`, module form unaffected, guarded `IMPORT` where applicable.
- Static closure: cross-file arity fails before any step with no side effects; nested file errors fail before any step; missing harvest names and cycles fail upfront. Only `EXPORT`ed names cross file boundaries (unexported is private); harvest of a defined-but-unexported name fails upfront with `does not export`.
- Follow-up (not this branch): unknown `ALIAS::F` calls (typo or unexported) pass the closure as opaque and fail at runtime when reached. Failing those upfront is sound (an uncollected alias call can never succeed) but is a separate strictness change.
- Core: isolation (`LET/ENV/WORKDIR/WORKSPACE` do not leak), `RETURN` capture vs bare discard, nested/transitive, cycle chain error, depth limit, diamond cache parses once, `BREAK/CONTINUE` cannot cross, `RETURN` outside import errors, `ASYNC IMPORT + AWAIT`.
- Pipes: two imports with same internal pipe name isolated; explicit `WITH_IO` passing works; no cross-talk without directive.
- FS/security: `..` escape + absolute-outside-root rejected; remote denied by default (phase 3); Miri ignores with reasons.
- Fixtures: `crates/oxdock-logic-tests/fixtures/.../cases/import_{basic,isolation,params,nested,cycle,remote_denied}/` + `coverage.toml` StepKind gate update.
- Preview: golden dry-run test (no files created, no process spawned).
- Dogfood (phase 2): shared date-gate import for `gha-*.oxfile`.

## 7. Verification

- `cargo test --workspace --tests` (min: `-p oxdock-parser -p oxdock-core -p oxdock-logic-tests -p oxdock-fs`).
- `cargo clippy --workspace -- -D warnings` (localized allows only, no `_`-prefix params, no `cfg!(miri)` outside `oxdock-fs`/`oxdock-process`).
- Manual: entry `IMPORT`ing lib with `RETURN`, `LET $r = IMPORT`, cycle file, `--dry-run` closure.
- `cargo run -p docs-gen` + commit regenerated READMEs.
- Guardrails: `oxdock-fs` abstractions only (no `tempfile` outside `oxdock-fs`), `oxdock-process` for spawning, `indoc!` raw blocks for test scripts, no em/en dashes in prose.

## 8. Open decisions to confirm before build

- Override syntax: trailing `KEY=value` vs `WITH $map` vs `ENV [...]` (recommend trailing `KEY=value`).
- Callee CWD base: own-file dir (recommended) vs caller CWD.
- Preview flag: `--dry-run` vs `--plan`.
- Sub-executor vs shared-stack for pipes (recommend shared-stack + mangling, matching `call_func`).
