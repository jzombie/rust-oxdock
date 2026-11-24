# Unified DSL-first error handling — plan of action

Source: issue #168 (`Centralize CLI user-facing errors behind constructors
with stable message keys`) plus the DSL follow-up comment on that issue
(second `ALIAS` leaking `WS_SENTINEL`, cross-field clash as the shape to copy).

User decisions (2026-11-23):
- Pilot scope: full DSL conversion now (not ALIAS-only pilot).
- Home: new diagnostics crate, and rip out `anyhow`.
- CLI relationship: CLI reuses DSL transport directly, owning its own templates.

Review verdict (2026-11-24): FAIL on God-crate, inverted message,
double-debug, key overloading, unbounded remainder, production panic,
missing chaining, gutter alignment. This revision addresses all six
directives.

## 1. Where we are (post-review refactor)

- `crates/oxdock-diagnostics` is transport-only (std plus `Arc`):
  - `Diagnostic { key, message, fields, span, hint, source }`,
    `Span`, `INTERNAL_SENTINEL`, `sanitize_internal_encoding`.
  - `pub fn new` + `pub fn make_fields` are the only construction
    entry; both sanitize `\u{1f}` to space and never panic.
  - `with_span`, `with_hint` (sanitized), `with_source`
    (`Arc<dyn Error + Send + Sync>`) with `Error::source` preserved.
  - `PartialEq` is manual (key, message, fields, span, hint; source
    ignored for equality). `Clone`, `Debug` derived.
  - `Display` gutter uses the line-number width so carets align for any
    width. `assert_no_internal_encodings` is a public test helper that
    panics on the marker; production paths sanitize and never call it
    on untrusted input. Downstream test suites may call it directly.
  - Tests: sanitization instead of panic, source chain, gutter
    alignment for 1/9/10/99/100/1234, equality ignoring source.
- `crates/oxdock-parser/src/diagnostics.rs` (new owner):
  - `alias_duplicate_on_field`, `alias_needs_field/name/name_lowercase`,
    `spelling_trailing_text` (fixed wording: unexpected, not expected),
    `spelling_max_depth(max, got)`, `spelling_expected_type_name`,
    `spelling_expected_uppercase(found)`,
    `spelling_expected_delimiter`, `spelling_unterminated_string`,
    `spelling_expected_literal`, `spelling_invalid_float/int`.
  - Each key distinct; no `format!` at call sites; quoting owned by the
    constructor. Test pins distinct keys.
  - `decode_spelling_fragment` lives here; `WS_SENTINEL` is
    `pub(crate)` in `tag.rs`.
- `crates/oxdock-parser/src/tag.rs`:
  - Returns `Diagnostic` transport; calls into `diagnostics::`.
  - Fixed double-debug (`&first.to_string()`), fixed inverted trailing
    message, split overloaded `dsl.spelling.expected` into dedicated
    keys, bounded `peek_second_alias` to one token.
  - Tests: updated max-depth key, kept
    `second_alias_on_same_field_fails_with_both_names`, added
    `second_alias_with_bad_name_is_bounded`.
- `crates/oxdock-core/src/exec/diagnostics.rs` (new owner):
  - `map_duplicate_field`, `alias_self`, `alias_clash`.
- `crates/oxdock-core/src/exec/typing.rs`: calls typing diagnostics;
  no longer imports `Diagnostic` constructors.
- `crates/oxdock-cli/src/errors.rs`: owns `cli.*` templates and keys
  directly on the transport (no passthrough to leaf constructors).
  Added untrusted-input test proving sanitization.
- Docs: diagnostics overview rewritten as transport-only plus owner
  list; `cargo run -p oxdock-render` regenerated.
- Green: `cargo test --workspace --tests`, `cargo clippy
  -p oxdock-diagnostics/parser/core/cli --all-targets`, CLI default /
  `--no-default-features` / `--features ssh`.

## 2. Why staged, not one-shot

- `grep -r anyhow`: 867 usages, 779 `bail!` sites, 163
  `anyhow::Result/Error` sites. Per-crate: `oxdock-core` 359,
  `oxdock-logic-tests` 137, plugins 106, sys 93, parser 30, CLI 25.
- Public API returns `anyhow::Result` (`Engine`, `HostModule`,
  `NativeFn`, `OxDockFn`, parser entrypoints). Removing `anyhow`
  changes every downstream signature at once.
- Direction holds (DSL first, new transport crate, CLI owns its
  templates on that transport), but the cutover stays incremental:
  `anyhow::Error::new(diag)` as the interop shim, one owner per
  diagnostic, one area per change.

## 3. Design constraints (load bearing)

- Decentralized ownership: transport in `oxdock-diagnostics`, templates
  with owners (parser spelling, core typing, CLI errors). Leaf crate
  never mentions `ALIAS`, `MAP`, `--listen`, or endpoint semantics.
- Architectural single-ownership: every rule lives in exactly one
  function. Spelling parser owns `ALIAS` arity; typing owns uniqueness;
  token skipper never restates.
- Call sites pass structured fields, never `format!`. Template lives
  once in the owning constructor.
- Constructors take decoded fields, never raw `rest:?`. Transport
  sanitizes `\u{1f}`; tests assert absence via the public helper.
  Untrusted input can never panic release.
- Stable dotted keys (`dsl.*`, `cli.*`), unused for rendering, asserted
  in tests so rewording cannot silently fork a site. No overloaded
  keys: one key per failure mode.
- `oxdock-diagnostics` stays std-only, never depends on workspace
  crates. `source: Option<Arc<dyn Error + Send + Sync>>` preserves
  chains now, before Phase D.
- No `debug_assert!` / `std::cfg!` macro (clippy `disallowed_macros`).
  Test-only code uses `#[cfg(test)]` attributes, which are allowed.
- No `std::fs` / `std::process` / `tempfile` outside `oxdock-fs` /
  `oxdock-process`. Tests use `GuardedPath::tempdir` and `indoc`
  raw blocks, never `\n`-joined scripts.
- Every crate ships a pipeline README. Template or crate changes
  require `cargo run -p oxdock-render` and committed outputs.

## 4. Remaining work, in order

### Phase A — DSL spelling + typing (close the pilot)

1. `tag.rs`: done for the pilot set. Any new spelling case gets a new
   dedicated key, never reuse of `spelling_expected`.
2. `typing.rs`: `build_rendered` / `build_generic` / `resolve_named` /
   alias-cycle / depth / arity errors. Each gets a `dsl.typing.*` or
   `dsl.alias.*` constructor in `exec/diagnostics.rs` with field,
   expected, found.
3. `parser.rs`: audit `ParseError::validation/structural/invalid_syntax`
   call sites in `commands.rs`, `command.rs`, `strip_flags.rs`,
   `lib.rs`. Route each through the owning `Diagnostic` constructor
   plus `with_span` / `with_source`. Keep `ParseError` as the span
   carrier during migration, with `Diagnostic` as payload (or
   `From<Diagnostic> for ParseError`).
4. Tests per diagnostic: constructor key test plus doc fence each for
   second-`ALIAS`-on-field, alias-equals-own-field,
   alias-clashes-across-fields, plus bounded-remainder and
   sanitize tests already added.

Acceptance A: `cargo test -p oxdock-parser -p oxdock-core` green;
no `\u{1f}` in output (sanitized plus test asserts);
`MAP<h: BOOL ALIAS help ALIAS h2>` fails with
`dsl.alias.duplicate_on_field`; `MAP<h: BOOL ALIAS help ALIAS Bad>`
reports bounded `Bad`, not the tail.

### Phase B — CLI per #168 (finish the issue)

Scope: `oxdock-cli` only, NET-touched lines, fold `usage()` ownership
into `errors.rs`. No flag-shape, script-read, or shell-runner changes.

1. `errors.rs`: add the rest of the NET set (`--listen` / `-p` shape,
   `-p` inner invalid, `usage()` text, known-but-unlinked module
   message). Keys under `cli.*`, owned here.
2. Delete remaining inline `bail!` on NET-touched lines; keep pure
   validators compiled in lean builds with the feature error winning.
3. Tests across default / `--features ssh` / `--no-default-features`
   green; net-required text from all call sites via the single
   constructor; duplicates and offline assert keys.

Acceptance B: the exact acceptance block in #168.

### Phase C — per-area follow-ups (no blanket conversion)

One issue per area, each owning its keys and templates, using
`with_source` where an underlying `io` / `fs` / `process` / `net`
error exists:

- CLI flag-shape, script-read, shell-runner, `serve.rs`, `mcp.rs`.
- Core runtime: `native.rs`, `mcp.rs`, `fs_ops.rs`, `pipeline.rs`,
  `static_check.rs`, `import.rs`.
- Parser: `markdown.rs`, `template.rs`, `macro_input.rs`.
- Plugins and sys crates last.

Never a repo-wide `bail!` sweep.

### Phase D — retire `anyhow` as the carrier

Only after Phases A through C. `source` chaining is already in the
transport, so this phase is mechanical:

1. Introduce `pub type DiagnosticResult<T> = Result<T, Diagnostic>`
   in `oxdock-diagnostics` (context via `with_hint` / `with_source`).
2. Migrate public signatures leaf to root, keeping
   `From<Diagnostic> for anyhow::Error` until the last caller flips.
3. Delete `anyhow` per crate only when no `anyhow::` path remains.

Acceptance D: `grep -r anyhow --include='*.rs' crates/ oxdock/` empty
except shims listed, workspace tests green, no public
`anyhow::Result`.

## 5. Test plan (every phase)

- `cargo test -p oxdock-diagnostics -p oxdock-parser --lib`.
- `cargo test -p oxdock-core --lib`.
- `cargo test -p oxdock-cli --lib`, plus `--no-default-features`
  and `--features ssh`.
- `cargo clippy -p oxdock-diagnostics -p oxdock-parser -p oxdock-core
  -p oxdock-cli --all-targets`.
- `cargo run -p oxdock-render` after template or crate changes; commit
  outputs.
- Final gate: `cargo test --workspace --tests`.

## 6. Risks and guards

- God-crate relapse: any domain wording in `oxdock-diagnostics`
  reintroduces the bottleneck. Guard: leaf owns transport only;
  review rejects domain constructors there.
- Grammar drift: skipper versus spelling parser. Guard: skipper covers
  consecutive `ALIAS`; spelling owns arity; bounded remainder; tests
  pin both.
- Sentinel crash: raw `rest:?` in a message. Guard: decode owner plus
  transport sanitize plus public assert helper.
- Key overloading: one generic key for many failures blinds tooling.
  Guard: distinct keys per mode, test pins uniqueness.
- Public API break: flipping `anyhow::Result` early. Guard: Phase D
  last, `with_source` first.
