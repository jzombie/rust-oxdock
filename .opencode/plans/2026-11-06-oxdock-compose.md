# oxdock-compose — General-Purpose Composable Artifact Renderer

> **Tagline: Composed artifacts, byte-identical.**
> Status: planning (no code yet).
> Migration order (normative): there is exactly ONE generator at every
> commit. Step 1 renames `crates/docs-gen` → `crates/oxdock-compose`
> (pure move, no logic changes). Step 2 creates a NEW minimal private
> `crates/docs-gen` pointer crate that delegates to it. Step 3+ refactors
> inside `oxdock-compose` (plugin extraction, lifecycle, dispatcher).
> Never two coexisting implementations.
>
> Publish guardrails (normative — violated by accident before, never again):
> - `docs-gen` (old AND new pointer) is `publish = false`, ALWAYS. The old
>   crate already carries `publish = false` with the comment "Prototype
>   crate; don't publish with this name" — the rename MUST carry that line
>   over, and the new pointer crate MUST have it from its first commit.
>   Add a CI lint (or extend the existing publish-guard if one exists) that
>   fails if any crate named `docs-gen` is publishable.
> - `oxdock-compose` ships `publish = false` at rename time and KEEPS it
>   until a deliberate, separate "open for publish" decision (adopt the
>   workspace version via `version.workspace = true` — never `0.0.0` —
>   plus description review and docs pass). The rename commit must not
>   flip publish to true. Rationale: the name will be public API the moment
>   it ships; nothing in the refactor sequence requires crates.io.

## 1. What this is (no prior context assumed)

**One-sentence pitch:** `oxdock-compose` is a content-agnostic, deterministic
render pipeline: it composes separately-authored modules ("plugins") through
an ordered lifecycle to turn one context into byte-identical output artifacts
of any kind — markdown docs today, source code or anything else tomorrow.

**Vocabulary (normative for this crate):**

- Use: `compose / composition`, `artifact`, `hook`, `surface`, `context`,
  `format`, `output`.
- Do NOT use: `assembly/assembler` (reads as x86 in a Rust repo), `asset`
  (means static files / DAM holdings), `document` (over-fits to markdown).
  `generator` is banned from the name (generic); `renderer` describes only the
  last mile, not the composition.

**Why it exists:** today's `docs-gen` crate is a hardcoded pipeline that
renders this repo's README/CITATION/installer/etc. from templates. The
machinery underneath (manifest assembly, fragment expansion, deferred
pass-2, executable-bit fixups) is general-purpose, but it is fused to one
domain (OxDock Rust workspace docs). This plan extracts the general
machinery into `oxdock-compose` and pushes every domain-specific behavior —
Rust manifest reading, OxDock command reference, markdown helpers, deferred
TOC — out into registerable plugins with a lifecycle. `docs-gen` then becomes
one configuration (one plugin set + one script) over the general engine.

**Design motto (document inside the crate when built, per product decision):**
`oxdock-compose` literally uses composition to compose its own dispatcher
("composes itself like a macro"): the parent script assembles a child script
in memory (pipe, no file written) and runs it through a fresh interpreter,
and the child is what executes the lifecycle hooks. Composition all the way
down.

---

## 2. Current state inventory (what gets extracted)

All paths relative to repo root unless noted. Read these before designing.

### 2.1 Entry point — `crates/docs-gen/src/lib.rs`

- `run(repo_root)` → `run_with_plugins(repo_root, extra_modules)`.
- Key mechanism: `pipeline_modules!` macro — a **compile-time token list** of
  `(HeaderIdent, module_path)` pairs. One list fans out (via
  `pipeline_split!`) into three uses that must never drift (pinned by test
  `pipeline_registrations_cover_header_modules`):
  1. `oxdock! { modules: [...] }` script header (DSL-visible set),
  2. `engine.register_module(...)` calls (runtime set),
  3. header-names test helper.
- `pipeline_engine()` is the **minimal engine**: registers ONLY
  `DOCS_GEN_ENGINE` (see §2.2). Everything else is per-pipeline choice.
  Downstream/custom pipelines are expected to build on `pipeline_engine()`
  + their own modules.
- The master pipeline is **one big `oxdock!` DSL script** (not Rust control
  flow): stages are named `FUNC`s (`REFRESH_GENERATED`, `SYNC_MEMBER`,
  `NOTE_TARGET`, `BUILD_MANIFEST`, `READ_TEXT`, `RENDER_TARGET`,
  `POST_TARGET`) and the top level reads as the stage list. Sequencing and
  all writes live in the script; Rust builtins supply only what the DSL has
  no primitives for.
- Invariants baked into the script worth preserving:
  - `WORKSPACE LOCAL` first (writes must hit the real tree, never a
    snapshot tempdir).
  - Version via `RUST::WORKSPACE_VERSION()` → `ENV CRATE_VERSION`.
  - `BUILD_MANIFEST` returns a JSON **string** today (`TO_JSON` at the end);
    known tech-debt: it should return a MAP and let the caller encode
    (keeps a codec decision out of the assembler).
  - Two-pass render: pass 1 `EXPAND` master template piped into `APPEND`;
    pass 2 reads output back, runs `DOCS::EXPAND_DEFERRED`, rewrites. The
    two passes live in separate `FUNC`s because declarations after
    `WITH_IO` steps don't bind in the same scope.
  - Executable-bit fixup (`apply_executable_bits`, `collect_target_manifests`,
    `executable_outs`): pure Rust post-pass, driven by `target.json`
    `"executable": true`. Strict: non-boolean flag or missing `out` fails
    naming the target.

### 2.2 Engine builtins — `crates/docs-gen/src/docs_gen_engine.rs`

Module name: `DOCS_GEN_ENGINE`. Registered unconditionally by
`pipeline_engine()`. Two functions only:

- `FILE_STEM(path) -> STRING` (pure): fragment key = filename up to first
  dot; charset `[A-Za-z0-9_-]`; anything else fails with a rename hint.
- `EXPAND_TEMPLATE(raw, vars: MAP<ANY>, env: MAP<STRING>) -> STRING`:
  caller-built scope (`vars` → `$name`, `env` → `{{ env:KEY }}`), placeholder
  calls dispatch through the **live pure-registry snapshot** (no hardcoded
  module names), one trailing newline stripped so master assembly equals
  plain concatenation.

This is the "file-set assembler's own primitives". Everything else
(`MAP_SET`, `TO_JSON`, `MERGE_MAPS`) lives in STD; all domain content comes
from plugins.

### 2.3 Internal plugins — `crates/docs-gen/src/plugins/`

Each exposes `pub fn module<P: ProcessManager>() -> HostModule<P>` built from
`#[oxdock_func]` registrations — **exactly the same shape as language plugins
under `crates/plugins/`** (SSH, NET, MARKDOWN). Internal only by
registration site (only `crate::run` registers them; the core language and
the `oxdock` facade never do).

| Module | File | Functions | Role |
|---|---|---|---|
| `DOCS` | `plugins/docs/mod.rs` | `DEFER(target, args...)`, `EXPAND_DEFERRED(full_text)` | Generic two-pass deferral. Pass 1 mints a nonce-framed base64url-JSON sentinel; pass 2 decodes + dispatches `target($full_text, ...args)`. Targets must be `MODULE::FUNC`; args must be JSON-safe (plain data only — handles fail at the boundary). No file I/O; text without sentinels round-trips unchanged. |
| `OXDOCK` | `plugins/oxdock/` | `COMMAND_INDEX`, `COMMAND_BODY`, `VALUE_TYPES`, `FUNCTION_REFERENCE`, `CONTEXT_REFERENCE`, `LOWER_MATH`, `PLUGIN_FUNCTION_REFERENCE(module)`, `PLUGIN_TYPE_REFERENCE(module)`, `ENV_CONTRACTS`, `GENERATED(key)` | Registry introspection rendered live from parser/registry metadata. `GENERATED` is the key→call dispatch current `docs-gen.json` loops over; unknown keys fail listing known keys. Rendering lives in `command_ref.rs` + `env_contracts.rs`. |
| `RUST` | `plugins/rust/` | `WORKSPACE_MEMBERS`, `WORKSPACE_VERSION`, `WORKSPACE_PACKAGE`, `CARGO_PACKAGE(member)`, `PACKAGE_VALUES_JSON(pkg, existing)` | Cargo workspace content. `docs_resolver` pins helpers to run cwd (= repo root). `PACKAGE_VALUES_JSON` merges manifest name/description with committed `name` override winning, stable key order, one trailing newline. |
| (external) `MARKDOWN` | `crates/plugins/oxdock-markdown-plugin` | e.g. `MAP_TO_MD_TABLE`, `TOC` | Domain helpers; any pure `fn(document, ...args)->STRING` can be a `DEFER` target. |

### 2.4 Config surface (stays config, not code)

- `docs-gen.json` (repo root): `{ global_values, scopes[], generated[{key,out}], … }`
  plus optional `merge_policy`, `staging_dir`, `version_key`, `vars.*`.
- `.oxdock/template/**/target.json`: `{ targets[{name, out, values, template, fragments{group:[globs]}, executable?}] }`.
  Rejects legacy `globs` key with a migration error; duplicate target names
  fail naming both manifests.
- `values.json` per target/scope; `fragments/*`, `shared/*`, `generated/*`
  template dirs.

### 2.5 Host extension pattern — `HostModule` (`crates/oxdock-core/src/exec/native.rs`)

```rust
pub struct HostModule<P: ProcessManager> {
    pub name: String,                    // e.g. "RUST"
    pub funcs: Vec<HostRegistration<P>>,// #[oxdock_func] registrations
    pub types: Vec<&'static TypeDescriptor>,
    pub record_schemas: Vec<RecordSchema>,
    pub states: Vec<ModuleState>,       // shared state (endpoint registries…),
                                        // incl. PreRunCheck steps inspectors
}
```

`Engine::register_module` stages these; runs expose `MODULE::NAME` calls.
`#[oxdock_func(pure, returns = …)]` generates the registration; `StepCtx`
gives bodies cwd/env/pure-function table/module state. Pure functions are
placeholder-callable (`{{ MOD::F($x) }}`); stateful ones are script-callable.
`PreRunCheck` (in `states`) runs against parsed steps before step 1 — the
fail-fast hook for "unmapped service" style validation.

---

## 3. Target architecture — `crates/oxdock-compose`

### 3.1 Crate shape

```
crates/oxdock-compose/
  Cargo.toml                 # name = "oxdock-compose"
  src/
    lib.rs                   # run(), run_with_plugins(), compose_engine()
    engine.rs                # COMPOSE_ENGINE builtins (FILE_STEM, EXPAND_TEMPLATE, …)
    config.rs                # compose.json / target.json loading + validation
    manifest.rs              # manifest assembly (MAP in, MAP out — NOT JSON strings)
    lifecycle.rs             # hook registry, ordering, dispatch table
    dispatcher.rs            # in-memory child-script composer (the self-composition)
    scanner.rs               # default file-discovery plugin (glob → fragment sets)
    writer.rs                # output writer + executable-bit applier
    plugins/
      mod.rs                 # re-exports; docs for authoring a compose plugin
```

`crates/docs-gen` becomes: `pub use oxdock_compose::*` (or a `run()`
that builds the docs plugin set and delegates) — no logic of its own.

### 3.2 What moves where (every current behavior becomes a plugin or core primitive)

| Today (docs-gen) | Tomorrow (oxdock-compose) | Notes |
|---|---|---|
| `DOCS_GEN_ENGINE` (`FILE_STEM`, `EXPAND_TEMPLATE`) | `COMPOSE_ENGINE` core module, always registered by `compose_engine()` | Rename; keep semantics. Consider adding `MANIFEST_KEYS`/`SCAFFOLD` only if a second pipeline needs them — default no. |
| `lib.rs` master script stages | Lifecycle hooks (§4) + default script template in `dispatcher.rs` | Script owns sequencing; Rust owns hook registry. The default dispatcher must be generatable, not hand-written (see §5). |
| `BUILD_MANIFEST` (returns JSON string) | `manifest::assemble()` returning MAP; `TO_JSON` (STD codec) at the write edge | Fixes the known debt: codec stays out of the assembler. The JSON-string form remains available as a thin wrapper for back-compat during migration. |
| `DOCS` (`DEFER`/`EXPAND_DEFERRED`) | `compose::defer` core plugin (ships with crate, opt-in per pipeline) | It is generic (works for any `fn(document,…)->STRING` target); rename module `DOCS`→`DEFER` or `COMPOSE_DEFER`? Keep `DOCS::` aliases during migration, then retire. |
| `OXDOCK` introspection plugin | Moves to `crates/oxdock-compose/src/plugins/oxdock/` (or stays as the docs-pipeline's own plugin, NOT in core) | Decision: introspection is OxDock-domain, not compose-domain. Core must not depend on parser internals; the docs pipeline registers it like any domain plugin. |
| `RUST` cargo plugin | Same: becomes the docs-pipeline's (or a shared `compose-plugin-rust`) domain plugin | Core must not know Cargo. File-parsing per the product note belongs in plugins. |
| `MARKDOWN` usage | External plugin, unchanged, registered by pipelines that need it | No move. |
| `REFRESH_GENERATED` / `GENERATED(key)` dispatch | Generalize: each plugin declares `provides: [key]`; dispatcher builds the key→call table at compose time; unknown key still fails listing known keys | This is the "manifest discovery" piece: no hardcoded key list in core. |
| `SYNC_MEMBER` / `PACKAGE_VALUES_JSON` | Rust-domain plugin hooks (`PRE_RENDER` per member) | Values-file sync is one instance of "per-item context enrichment". |
| Executable-bit fixup | `writer` post-hook, driven by target manifest `executable` flag | Keep strictness (non-boolean fails naming the target). Make the flag name config-mappable, default `executable`. |
| `docs-gen.json` / `target.json` | `compose.json` + same `target.json` shape (superset-compatible) | Loader + validator in `config.rs`; unknown-key and duplicate-target errors preserved verbatim. |

### 3.3 Source/scanning as a plugin (per product note)

File discovery (`GLOB` over scopes, fragment-group assembly, stem
collision detection) must NOT be core logic. Shape:

- Core defines a `Source` trait (or hook contract): given a target's
  `fragments: {group: [globs]}`, produce `{group: {stem: expanded_text}}`.
- The **default implementation** (`scanner.rs`) is itself a plugin registering
  source hooks; pipelines replace it by registering their own source (e.g. an
  in-memory map source for tests, a git-tree source for CI, a code-AST
  source for source-code generation).
- The "lifecycle builds up its own working space" reading: each source plugin
  owns a **workspace** (staging dir / in-memory overlay) across the phases
  below — e.g. `PRE_SCAN` creates it, `SCAN` fills it, `ASSEMBLE` reads it,
  `POST_WRITE` evicts it. Core never touches the filesystem for sources
  except through the plugin's workspace handle.
- Stem-collision rule ("more than one file → fail naming the target") stays,
  but lives in the default scanner, not core — a code-gen pipeline may
  legitimately want last-wins or namespaced stems.

---

## 4. Hook lifecycle (normative)

### 4.1 Phases (in order, every run)

```
1. DISCOVER   find targets (target.json walk) and plugins providing keys
2. LOAD       read global values, per-target values, generated inputs
3. SCAN       source plugins build their workspaces (glob → files)
4. ASSEMBLE   build one manifest MAP per target (fragments → stems)
5. EXPAND     pass-1: expand master template over manifest scope
6. DEFERRED   pass-2: EXPAND_DEFERRED over rendered text (no-op if no sentinels)
7. WRITE      write outputs (+ executable bits)
8. VERIFY     determinism check (re-render in memory, byte-compare) + report
```

Each phase is a hook point. Plugins register any subset; unregistered phases
are no-ops. Ordering within a phase: registration order, except `DEFERRED`
targets which order by sentinel position in the document.

### 4.2 Hook signatures (OxDock-callable)

All hooks are plain `FUNC`s exported by plugin `.oxfile`/host modules and
invoked as bare qualified calls (`P0::PRE_RENDER($ctx)` — see §5.3 for why
bare, never `CALL`). Context is always a single MAP so the arity never
changes as the lifecycle grows:

```
FUNC PRE_DISCOVER($ctx: MAP)   # $ctx = {config, scopes}
FUNC POST_DISCOVER($ctx: MAP)  # $ctx += {targets}
FUNC PRE_LOAD($ctx: MAP)       # per-target values enrichment point
FUNC POST_LOAD($ctx: MAP)      # $ctx += {global, values}
FUNC PRE_SCAN($ctx: MAP)       # create workspace
FUNC POST_SCAN($ctx: MAP)      # $ctx += {files}
FUNC PRE_ASSEMBLE($ctx: MAP)
FUNC POST_ASSEMBLE($ctx: MAP)  # $ctx += {manifest}
FUNC PRE_RENDER($ctx: MAP)     # last chance to mutate scope
FUNC POST_RENDER($ctx: MAP)    # $ctx += {rendered}
FUNC PRE_WRITE($ctx: MAP)
FUNC POST_WRITE($ctx: MAP)    # $ctx += {written: [paths]}
FUNC VERIFY($ctx: MAP)         # return BOOL or EXIT 1 with reason
EXPORT PRE_DISCOVER POST_DISCOVER … VERIFY
```

Minimal useful plugin implements only `PRE_RENDER` (mutate scope) and/or
`POST_RENDER` (rewrite text). The docs pipeline's per-member values sync is
a `PRE_LOAD`; the executable-bit applier is a `POST_WRITE`; the Rust
introspection providers are `LOAD`-time `GENERATED`-key providers.

### 4.3 Hook contracts (fail-closed, deterministic)

1. **Unknown hook names fail the run naming the hook and plugin.**
   A plugin exporting `PRE_RENDRE` (typo) must never silently skip.
2. **Missing imports fail naming the violator.** If the dispatcher references
   a plugin that isn't registered, the child run's static gate fails before
   step 1 — never an empty render.
3. **Hooks are pure-ish:** they receive `$ctx` MAP, return a MAP (or STRING
   for text transforms). No hooks share mutable state except through the
   source workspace handle (§3.3). Nondeterministic hooks (timestamps, temp
   paths) MUST go through a `VERIFY`-visible seam: `VERIFY` re-renders in
   memory and byte-compares; any drift fails the run.
4. **Determinism:** equal inputs → byte-identical outputs, enforced by
   `VERIFY`, not by convention. `BTreeMap` ordering for all manifest maps;
   no hash-map iteration leaks into output.

---

## 5. The self-composition dispatcher (how hooks actually run)

This is the load-bearing mechanism. Do not hand-write the dispatcher; generate
it every run.

### 5.1 Pattern (proven — PR #276, commit `acba50ae`, executing doc fences)

Parent composes the child script **in memory** and streams it to a fresh
interpreter over a pipe. No file is ever written:

```text
IMPORT [STD]
WORKSPACE LOCAL                    # child must see plugin files (see §5.2)
LET $exe: STRING = INTERPRETER()
LET $child: PIPE
WITH_IO [stdout=$child] ECHO """
IMPORT ["plugins/hook.oxfile" AS P0]
LET $r: STRING = P0::PRE_RENDER($ctx)  # bare call — NOT "CALL ..."
ECHO "rendered=\{{ $r }}"              # escaped: child expands this
"""
LET $out: STRING = WITH_IO [stdin=$child] RUN [$exe]
```

Mechanism notes (all verified against `target/debug/oxdock`):

- `STD::INTERPRETER() -> STRING` returns the running binary; bare
  `RUN [$exe]` with piped stdin suffices (no `--script -`); the pipe
  delivers EOF when the producer step ends so the child reads exactly one
  script.
- Multiple producer steps may append into one pipe — the dispatcher can be
  assembled line-by-line in a `FOR` loop over registered modules.
- `ExecIo::set_interpreter(path)` override exists for embedding hosts and
  the doc-conformance harness (whose in-process binary is not a CLI).
  `INTERPRETER()` is intentionally NOT `pure` (it reads run state).
- SpillBuffer: 8 MiB memory → disk spill, 100 MiB cap — large dispatchers
  don't deadlock the producer.
- Child failure propagates: failed child `ASSERT`/nonzero exit bails the
  parent `RUN` with the child stderr in the error chain (this is what makes
  "strict errors block push" hold across the process boundary).

### 5.2 Workspace visibility (the one real gotcha)

`RUN [$exe]` spawns the child with cwd in an **isolated snapshot temp dir**
under `env -i`. A stdin child therefore resolves relative `IMPORT`s against
an empty dir and fails with *"must exist when the run starts (generated
imports are not supported)"*. Two verified fixes:

- `WORKSPACE LOCAL` in the parent before composing (preferred — child
  inherits the local root), or
- `ENV OXDOCK_WORKSPACE_ROOT=/abs/path` (escape hatch when the parent must
  stay hermetic).

Doc fences use a third form, `roots:unified` fence metadata (unified
workspace/context roots in the harness). The dispatcher builder must set one
of these; "child can't see plugin files" is otherwise the first failure
every new pipeline hits.

For **file children** (`RUN [$exe, "main.oxfile"]`), run by path so
entry-relative `IMPORT` resolves. For **self-contained children**, ride the
stdin pipe. The dispatcher uses the pipe (self-contained); file mode exists
for the `MAKE_GREETER`-style "compose a module file, then a child imports
it" pattern.

### 5.3 Two-phase expansion rule (the composition primitive)

`WITH_IO [stdout=$child] ECHO` runs its argument through parent-side
`expand_string` FIRST. Therefore:

- **Parent-side values** (plugin paths, aliases the generator invents) →
  **unescaped** `{{ }}`: baked in at compose time.
- **Child-side values** (things the child computes, e.g. `$r` from a hook) →
  **escaped** `\{{ }}` in the composed source: stays literal through the
  pipe, expanded by the child.

Proven by the executing `INTERPRETER()` doc example ("dynamic composition"):
parent bakes `{{ $who }}` → `Ada`; child expands `\{{ $who }}` → `Grace`
from its own binding; both asserted. If this example ever breaks, the
dispatcher primitive is broken — it is the canary.

### 5.4 `CALL` is gone — use bare calls

The CHANGELOG-era `CALL GREET("ada")` syntax no longer exists:

- `LET $r = CALL F(...)` silently binds the literal string `"CALL"` (exit 0).
- Statement `CALL F(...)` fails with `unknown command: CALL`.

Dispatcher MUST emit bare qualified calls: `LET $r: STRING = P0::PRE_RENDER($ctx)`
and statement `P0::PRE_RENDER()` for void hooks. (There is a pending
CHANGELOG/doc fix for the stale `CALL` documentation — out of scope for this
crate, but do not copy the stale form into new docs.)

### 5.5 Same-run `WRITE` + `IMPORT` is impossible (why the pipe exists)

The import closure is a **pre-execution analysis pass** ("expansion is the
analysis pass, and analysis needs bytes" — `import.rs`). A file written by
step N cannot be imported by step N+1 in the same run, with or without
`WORKSPACE LOCAL`. Parent-WRITE → child-IMPORT (fresh run) works; same-run
does not. The pipe is therefore not an optimization — it is the only
same-run composition path, and the dispatcher must use it.

### 5.6 What `dispatcher.rs` builds

Given the registered plugin list `[(alias, path, hooks[])]`, emit (via pipe
producers, one `ECHO` per line or per block):

```
IMPORT ["<path>" AS P<idx>]            # one per plugin
LET $ctx: MAP<ANY> = <initial-manifest-as-literal>
LET $ctx = P0::PRE_RENDER($ctx)        # only hooks the plugin exports;
LET $ctx = P1::PRE_RENDER($ctx)        # dispatcher knows the export list
…                                      # (from module metadata, not by guessing)
ECHO "<final-marker>=\\{{ $ctx }}"     # or WRITE outputs directly in child
```

The dispatcher builder reads each plugin's **export list from module
metadata** (the `HostModule` funcs / `.oxfile` `EXPORT` line), never by
trying calls. Unknown hook → fail naming hook+plugin (§4.3.1).

---

## 6. Migration plan (rename first — one generator at all times)

0. **Precondition**: `main` green, `docs-fresh` passing. The rename must be
   drift-free: rendered outputs byte-identical before and after.
1. **Rename the crate** `crates/docs-gen` → `crates/oxdock-compose`
   (pure `git mv` + `Cargo.toml` `[package] name` + workspace `members`
   entry + `use docs_gen::` → `use oxdock_compose::` import paths).
   NO logic changes in this commit. Crate-level doc header is updated to
   the new name + tagline ("Composed artifacts, byte-identical."), but
   module names (`DOCS_GEN_ENGINE`, `pipeline_engine`, `run`) stay as-is
   until step 3. **Keep `publish = false` verbatim** (with its "don't
   publish with this name" comment, reworded to the new name). On the
   publish decision the crate adopts the workspace version
   (`version.workspace = true`) — never `0.0.0`. Verify: full workspace
   build + render produces zero drift, full test suite green.
2. **Create the NEW private `crates/docs-gen` pointer crate**: minimal
   `lib.rs` (+ `main.rs` shim if the old binary entry lived there) that
   does nothing but delegate —
   `pub use oxdock_compose::{run, run_with_plugins}` (and any other items
   external callers import), with a doc comment stating it is a
   compatibility pointer and all logic lives in `oxdock-compose`.
   **`publish = false` from its first commit, no exceptions.**
   Re-add it to workspace `members`. Point `docs.yml` / CI at whichever
   binary name the pipeline keeps (prefer `oxdock-compose`; keep a
   `docs-gen` alias only if external callers exist). Verify: `docs-fresh`
   green through the pointer.
3. **Refactor inside `oxdock-compose`** (steps 3–7 of the old plan, now
   renames-within-one-crate, each its own commit, `docs-fresh` green at
   each):
   - `docs_gen_engine.rs` → `engine.rs` (rename module `DOCS_GEN_ENGINE`
     → `COMPOSE_ENGINE`; fix `BUILD_MANIFEST`-returns-string debt → MAP).
   - `plugins/docs` → core `defer` plugin (`DOCS` → `DEFER`, alias or
     major-note per §8.4).
   - `plugins/oxdock`, `plugins/rust` → domain plugins out of core (§3.2).
   - `config.rs` (`compose.json` loader accepting `docs-gen.json` shape),
     `manifest.rs`, `lifecycle.rs`, `dispatcher.rs`, `scanner.rs`,
     `writer.rs` extracted from the `lib.rs` master script.
4. **Retire the pointer** (separate ticket, not this plan): once no caller
   imports `docs-gen`, delete the pointer crate. Until then it stays a
   two-line shim — never a second implementation.

---

## 7. Testing (every guarantee gets a fence or a test)

- **Executing doc fences** (the PR-#276 pattern): every composition example
  is a ```oxdock fence executed by `docs_conformance` with a staged
  interpreter binary — never ```text. New fences needed: full dispatcher
  skeleton (compose → pipe → child imports plugin → hook runs → parent
  asserts), `roots:unified` where the child needs files.
- **`conformance.rs` skip rule**: reinvocation examples
  (`INTERPRETER()` + `RUN [$exe`) return `Skipped` under the recording mock
  (parse still validates; never counts for/against coverage). New examples
  must match the skip detector's string shape or extend it — a renamed
  variable silently falling through to the mock gate is the known footgun.
- **Unit tests**: hook ordering (registration order honored), unknown-hook
  failure names hook+plugin, stem-collision failure names target, duplicate
  target names fail naming both manifests, non-boolean `executable` fails
  naming target, `VERIFY` byte-compare catches injected nondeterminism,
  `GENERATED`-style unknown-key error lists known keys.
- **Determinism suite**: render twice (second in-memory), byte-compare;
  fixtures under `parser/tests/fixtures/` only (raw `logs/` are append-only
  production data — never copy live log content into tests; see AGENTS.md).
- **AGENTS.md checklist applies**: `cargo fmt --check`, `cargo clippy -- -D
  warnings`, `cargo test` (fixtures only), plus the plugin-change test list.

---

## 8. Open questions (do NOT resolve by guessing — ask or ticket)

1. **Lifecycle granularity**: per-module hooks (`PRE_RENDER` per plugin, as
   in §4.2) vs per-stage hooks vs hybrid? The dispatcher sketch assumes
   per-module-per-phase; a per-stage design would invert it.
2. **Source-plugin trait shape**: Rust trait vs hook-convention vs both?
   (How does a custom source advertise its workspace handle to core?)
3. **Where domain plugins live**: inside `oxdock-compose/src/plugins/` vs
   separate `compose-plugin-*` crates vs next to the docs pipeline.
4. **Name of the defer module**: keep `DOCS` for compat or rename to
   `DEFER`/`COMPOSE_DEFER` now while the rename window is open.
5. **Self-composition doc**: the "composes itself like a macro" property
   (§1 motto) needs one precise paragraph in the crate docs with the
   mechanism (pipe + fresh interpreter, never hand-written / never on disk)
   — marketing without the mechanism sentence will read as hype.
6. **Stale `CALL` docs** (CHANGELOG:720 + any command reference copies):
   separate fix, but the new crate's docs must never show the `CALL` form.

---

## 9. Glossary for implementers (terms as used in this plan)

- **Compose**: assemble many inputs into one output via lifecycle hooks.
  Never "assemble".
- **Artifact**: the thing a build produced (README, llms.txt, .rs file).
  Format-neutral by definition.
- **Hook**: a named lifecycle entry point (`PRE_RENDER`, `POST_WRITE`, …).
  Mechanism, not outcome — never the crate name.
- **Surface**: a place an artifact lands for an audience (human README vs
  machine llms.txt vs compiler .rs).
- **Context**: the input MAP (global values + per-target values + manifest).
- **Dispatcher**: the generated child script; never hand-written, never on
  disk; composed into a pipe and executed by a fresh interpreter run.
- **Two-phase expansion**: parent bakes unescaped `{{ }}` at compose time;
  `\{{ }}` survives the pipe for the child to expand.
- **Fail-closed**: missing hooks/imports/keys fail naming the violator;
  never render empty or default.
- **Byte-identical**: equal inputs → byte-identical outputs, enforced by
  VERIFY re-render comparison.
