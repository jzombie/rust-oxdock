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

### Unified handling contract (all `dsl.*`, no exceptions)

1. **Construction**: one named constructor per failure mode, owned by
   the domain module. Call sites pass structured data only: no
   `format!`, no prose fragments, no articles. `anyhow::Error`
   passthroughs take `&anyhow::Error` and render `{reason:#}` inline
   in the message; they do NOT also attach it via `with_source`
   (`Display` does not render `source`, and `anyhow`'s `{:#}` would
   print an attached chain twice).
2. **Keys**: `dsl.<family>.<mode>`, one per failure mode, never reused
   across modes. Distinct-key test in every owner module.
3. **Messages**: `COMMAND`-lead for validation errors, subject-lead
   plain sentences otherwise. Backticks for DSL tokens, `{got:?}`
   for found values. Never any line/column info; no trailing period.
4. **Spans**: `oxdock-parser` errors sync spans through
   `ParseError::from_diagnostic`/`with_span`; non-parser `dsl.*`
   errors (core typing, CLI) attach positions directly via
   `Diagnostic::with_span` when one is available. Location lives in
   `Span` only, never in message text.
5. **Chains**: `with_source` wherever an underlying error exists AND
   its text is not already interpolated in the message (typed
   io/sys/net errors wrapped in outer-context messages). Inline
   `{reason:#}` passthroughs are the other case, never both.
6. **Tests**: key test plus wording test per constructor, `indoc`
   blocks, never `\n`-joined scripts.

Normalization pass (after the remaining `commands.rs` sites are
over, before Phase B): bring every migrated constructor onto this
contract in one diff — strip the legacy `invalid syntax for command`
prefixes, unify quoting, update affected tests to the canonical
wording instead of preserving old strings.

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
2. `typing.rs`: done (2026-11-24). `resolve_named` (max_depth,
   bare_map/list/handle, alias cycle, unknown), `build_rendered`
   (max_depth_spelled), `build_generic` (no_args, handle/list arity,
   map_bare, default_mismatch), `finish_union` (empty_union),
   `collect_type_aliases` (already_defined, builtin/host/schema/func
   collisions) each have a `dsl.typing.*` / `dsl.alias.*` constructor
   in `exec/diagnostics.rs`. Wording preserved byte-identical;
   joining/quoting live in constructors, no `format!` at call sites.
   Left for Phase C: `check_options` unknown-option, `check_tag_known`
   (unregistered/VOID), `validate_function_type_tags`.
3. `parser.rs`: bridge landed (2026-11-24). `ParseError` carries an
   optional owning [`Diagnostic`] payload via
   `ParseError::from_diagnostic(kind, diag, ctx)` plus
   `diagnostic()` / `key()` accessors; `Display` unchanged so legacy
   assertions keep passing. `diagnostics.rs` owns five
   `dsl.validation.*` constructors (flag_missing_value, unknown_flag,
   too_many_positionals, missing_argument, arg_type_mismatch) with a
   distinct-key test and a legacy-prefix test. Migrated: all of
   `strip_flags.rs` (2 sites) and `command.rs`
   (`validate_positionals_against_meta` + `check_one`, 4 sites); no
   `format!` at those call sites. `arg_type_mismatch` takes
   `&anyhow::Error` and renders `{reason:#}` inside the constructor so
   the full chain survives. Span is synced both ways: `from_diagnostic`
   attaches the `ctx` span to the embedded diagnostic, and
   `with_span` enrichment propagates to it (only while it has none),
   each pinned by a test. ENV/EXPAND group done (2026-11-24):
   `join_value`, `lower_env_assignment`, `lower_env_tokens`,
   `lower_expand_tokens` (~15 sites) through eight more
   `dsl.validation.*` constructors; error passthroughs take
   `&anyhow::Error` with `{reason:#}` inside. WORKDIR/WORKSPACE/COPY
   group done (2026-11-24): WORKDIR/WORKSPACE/COPY/COPY_GIT/SYMLINK
   lowers (14 sites) through twelve parameterless `dsl.validation.*`
   constructors (wording owned by constructors, no prose at call
   sites); `unknown_workspace_source` is shared by COPY and SYMLINK
   (same failure mode, one key) and records the invoking command.
   Filesystem group done (2026-11-24): MKDIR/INSTALL/SET_EXECUTABLE/
   ARCHIVE/UNARCHIVE/WRITE/APPEND/APPEND_LINE/REMOVE path-missing
   lowers plus RUN empty-argv (reuses `needs_value`, 13 sites) through
   eleven more parameterless constructors. READ_LINE/PROMPT group
   done (2026-11-24): variable/message/sigil lowers (8 sites)
   through five `dsl.validation.*` constructors; the `found` value
   now renders the argument instead of the internal `Arg` debug
   shape (first canonical improvement: legacy substring assertions
   still hold). ASSERT/EXIT/SLEEP/LIST_APPEND group done (2026-11-24):
   value/needle/code/duration/item/arity lowers (9 sites) through nine
   `dsl.validation.*` constructors. ASSERT_EQ/NE equality group done
   (2026-11-24): shared `lower_equality_args` (3 sites) through three
   `dsl.validation.assert_equality_*` constructors taking the invoking
   command (same failure mode, one key). UNARCHIVE/EXPAND leftovers
   done (2026-11-24): --max-bytes passthrough through new
   `dsl.validation.unarchive_bad_max_bytes`, EXPAND split/extra-path
   through existing constructors (3 sites). Still open: the rest of
   the `commands.rs` bulk (validation fully migrated: zero
   `ParseError::validation` sites remain; EXPORT group done
   (2026-11-24): func/type/const lowers through six
   `dsl.validation.export_*` constructors; structural set done
   (2026-11-24): LET/TTY/WITH_IO/REMOTE/expr lowers (10 sites)
   through ten `dsl.structural.*` constructors carrying the rule
   in the structured field; `invalid_syntax` set done (2026-11-24):
   `invalid_syntax_error` through `dsl.invalid_syntax.malformed`
   (found/expected preserved via new `with_found`/`with_expected`
   builders), `unknown_command_error` through
   `dsl.unknown_command.unknown`. `parser.rs` rewrap done
   (2026-11-24): `TokenError` carries `Option<Box<Diagnostic>>`
   (boxed: cold path, `TokenError` travels by value);
   `lower_err` plus 25 ParseError-wrapping sites carry verbatim,
   two kind-dropping sites (`validate_remote_copy_placement`,
   `validate_exports`) now preserve kind too; `token_error`
   reattaches via `from_diagnostic` when present, string rewrap
   otherwise (lexer/parser-native errors unchanged). `lib.rs` mock
   done (2026-11-24): all 16 sites through canonical constructors,
   `ENV` passes production errors through directly.)
   Key survival pinned by `stable_keys_survive_the_token_round_trip`
   (write/missing-arg, unknown-command, invalid-syntax, export
   keys through the string path with `Display` unchanged).
   Normalization pass done (2026-11-24): all migrated constructors onto the contract in one diff: legacy prefixes stripped, quoting unified (backticks/`{got:?}`), trailing periods dropped, em dash replaced; hints, build mocks, and docs aligned; tests updated to canonical wording. Phase B (CLI NET set) is next.
4. Tests per diagnostic: constructor key test plus doc fence each for
   second-`ALIAS`-on-field, alias-equals-own-field,
   alias-clashes-across-fields, plus bounded-remainder and
   sanitize tests already added.

Acceptance A: `cargo test -p oxdock-parser -p oxdock-core` green;
no `\u{1f}` in output (sanitized plus test asserts);
`MAP<h: BOOL ALIAS help ALIAS h2>` fails with
`dsl.alias.duplicate_on_field`; `MAP<h: BOOL ALIAS help ALIAS Bad>`
reports bounded `Bad`, not the tail.

### Phase B — CLI per #168 (done 2026-11-24)

Scope was `oxdock-cli` only, NET-touched lines, `usage()` folded into
`errors.rs`. No flag-shape, script-read, or shell-runner changes
(--remote shape and missing-value messages left as-is).

1. `errors.rs` owns the NET set: `listen_requires_address`,
   `listen_port_numeric`, `listen_port_zero`,
   `listen_address_invalid`, `publish_requires_shape`,
   `publish_outer_invalid`, net-gated `publish_inner_invalid`
   (with `{reason:#}` passthrough), and
   `module_unlinked(module, feature)`. Keys under `cli.*`.
   `UsageAssets` + `usage()` moved from `lib.rs` to `errors.rs`
   (re-exported, `oxdock_cli::usage()` unchanged).
2. `endpoints.rs` has zero inline `bail!`/`anyhow!` left:
   `parse_listen_arg`/`parse_publish_arg`/`build_registry` go
   through the constructors; pure validators stay compiled in lean
   builds with the feature error winning (unchanged pattern).
3. Lean `parse_cli_script` maps a parse failure to
   `cli.module_unlinked` when the script actually `IMPORT`s an
   unlinked module (bare words inside `IMPORT [...]` only, so strings
   and comments never misfire; full-feature builds map nothing).
   Lean NET/SSH rejection tests pin the typed key; endpoint,
   registry-conflict, offline, and duplicate tests pin theirs.
   Green: default (44), `--no-default-features` (39),
   `--no-default-features --features ssh` (44); clippy clean
   (pre-existing warnings only); doc clean; no new fmt drift.

Acceptance B: the exact acceptance block in #168.

### Phase C — per-area follow-ups (no blanket conversion)

One issue per area, each owning its keys and templates, using
`with_source` where an underlying `io` / `fs` / `process` / `net`
error exists. Never a repo-wide `bail!` sweep.

#### C.1 CLI remainder (done 2026-11-24)

Flag-shape, script-read, shell-runner, `serve.rs`, `mcp.rs` through
25 new `cli.*` constructors in `errors.rs` (feature-gated where the
call site is: net-gated serve/remote-serve, mcp-gated MCP,
lean-only remote-shape, windows-only shell-temp).
`endpoints.rs`/`serve.rs`/`mcp.rs` have zero inline `bail!`/`anyhow!`
left; `lib.rs` keeps only help/version surfacing (not failures) and
pre-existing `.context` io chains. Dynamic io read chains
(`read_script`, tempdir/resolver setup) intentionally untouched:
already proper chained contexts, no static template to own.
Green: default (48), `--no-default-features` (42),
`--no-default-features --features ssh` (47); clippy/doc clean; no
fmt drift.

#### C.2 core runtime (in progress)

Done 2026-11-24: exec/`mcp.rs` (6 ctors: `dsl.mcp.*` incl. tail-carrying
call_failed) and exec/`import.rs` (21 ctors: `dsl.import.*` incl.
const-group, flow-crossing, unified type-collision detail). Both files
have zero inline `bail!`/`anyhow!` left; lib 406 + integration 629
green; clippy/doc clean; no fmt drift.

Remaining:
- Core runtime: `native.rs` done 2026-11-24 (46 `dsl.func.*` /
  `dsl.std.*` ctors: registry, PARSE_ARGS group, byte/duration,
  BOOL/CONTAINS/LEN/PATH/SHELL builtins; only test-code `ECHO`
  stub left; lib 407 + integration 629 green; clippy/doc/fmt clean).
  `static_check.rs` next (~129 sites). `fs_ops.rs` and
  `pipeline.rs` verified clean (no inline failures; `fs_ops`
  only re-exports `Result`).
- Parser: `markdown.rs`, `template.rs`, `macro_input.rs`.
- Plugins and sys crates last.

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
- Speculative LSP fields: `Span` gains file ids / byte offsets only
  when a symbol table needs them. Guard: downstream crates build
  spans only through `Span` constructors (`line_only` / `line_col` /
  `full`), never struct literals. `Step::span()` /
  `defined_symbol()` (TYPE/FUNC, in `ast.rs`) derive from stamped
  line/col with zero new data; positions stay additive later.
