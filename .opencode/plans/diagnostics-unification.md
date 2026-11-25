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
- Core runtime: `exec/mcp.rs` done (6 `dsl.mcp.*` ctors) and
  `exec/import.rs` done (21 `dsl.import.*` ctors) and
  `native.rs` done 2026-11-24 (46 `dsl.func.*` / `dsl.std.*`
  ctors: registry, PARSE_ARGS group, byte/duration,
  BOOL/CONTAINS/LEN/PATH/SHELL builtins; only test-code `ECHO`
  stub left; lib 407 + integration 629 green; clippy/doc/fmt clean).
  `static_check.rs` done 2026-11-25 (~129 sites through ~110
  `dsl.check.*` ctors plus 2 `dsl.import.*` additions
  (`not_in_closure`, `export_type_mismatch`): helpers, var/keypath,
  template group, capture/await/async, return/flow, resolve_call,
  const group, call arity, arithmetic, RPN; zero inline
  `bail!`/`anyhow!` left; only test-code ECHO stub remains in
  `native.rs`; lib 408 + integration 629 green; clippy/doc clean;
  zero fmt drift).
  `fs_ops.rs` and `pipeline.rs` verified clean (no inline
  failures; `fs_ops` only re-exports `Result`).
- Parser: `markdown.rs` done 2026-11-25 (10 `dsl.markdown.*`
  ctors: unclosed fence + 9 metadata shapes, line-prefixed
  byte-identical), `template.rs` done (6 `dsl.template.*` ctors:
  quoted/block trailing/unterminated, escape, map-key),
  `macro_input.rs` done (1 `dsl.macro_input.*` ctor); zero inline
  `bail!`/`anyhow!` left in all three; parser lib 266 +
  integration 301 green; core lib 408 + integration 629 green;
  clippy/doc clean; zero fmt drift.
- Plugins and sys crates last. MCP crate done 2026-11-25
  (new `oxdock-mcp/src/diagnostics.rs` owning 15 `mcp.*` ctors:
  tool duplicate/opaque-return/opaque-param, schema opaque-type,
  args not-object/missing-required/count-out-of-range/unknown,
  export status-reserved/no-entries, session lock-poisoned
  reload/call, serve stdio-serve/stdio-service/runtime-build
  passthroughs; `tools.rs`/`schema.rs`/`server.rs`/`lib.rs` zero
  inline `bail!`/`anyhow!`; non-anyhow errors wrapped with
  `anyhow::Error::new` at call sites; lib 11 + integration 39
  green; clippy/doc clean; zero fmt drift).
- Sys crates: `oxdock-process` execution errors done 2026-11-25
  (17 `proc.run.*` ctors appended to `src/diagnostics.rs`:
  interpreter/exec/interactive/background-trio/spawn/unguarded/
  argv-unimplemented/spawn-anchors/stdin+cap locks, plus a generic
  `Debug` `run_command_failed` shared by all four backends;
  `shell.rs`/`synthetic.rs`/`builder.rs`/`contract.rs`/
  `env_policy.rs`/`mock.rs`/`shell_manager.rs` zero inline
  failures; messages byte-identical; Miri-only call sites need no
  test gating (ctors ungated); test-only macro imports moved into
  `mod tests`; lib 119 + core lib 408 + integration 629 green;
  clippy/doc clean; zero fmt drift).
- Sys crates: `oxdock-process` `expand.rs` done 2026-11-25 (new
  `src/diagnostics.rs` owning 18 `proc.expand.*` ctors: env/script-var
  misses with inline `did you mean` hints via `ScriptSuggest` /
  `OverrideSuggest` scopes (hinted/bare share one key), missing
  override, invalid format, no-registry, depth cap, empty/invalid/
  nested call args, map entry/duplicate-key, key-path
  missing-property/bad-index/out-of-bounds/on-primitive, render
  PIPE/SEMAPHORE/PERMIT; all messages byte-identical; test stubs
  left as-is; lib 119 + core lib 408 + integration 629 green;
  clippy/doc clean; zero fmt drift).
- Build crate: `oxdock-build` `assets.rs` done 2026-11-25 (new
  `src/diagnostics.rs` owning 11 `build.*` ctors: assets collect,
  out-dir invalid, module path rejected, DSL escapes-manifest, parse
  (shared by both sites), execution, workdir missing/not-dir, out-dir
  target not-dir/create-failed, stage copy; `OUT_DIR` spelling derives
  from the declared env contract, never literals; passthroughs render
  `{reason:#}` inline with no `with_source`; `.context` chains left
  as-is (already proper chaining); test-only `ParseError` stubs left
  as-is; messages byte-identical; lib 27 + integration 4 green;
  clippy/doc clean; zero fmt drift).
- Plugin crate: `oxdock-ssh-plugin` done 2026-11-26 (new
  `src/diagnostics.rs` owning 45 `ssh.*` ctors: key escapes/not-Ed25519/
  insecure-permissions, pump output-lock/input-read/worker-panicked/
  interrupted/source-read/target-lock/func-input-read, auth rejected
  (shared EXEC/CONNECT key), session pump-finished/queue-saturated/
  writer-gone/already-pumped, channel data-unknown/pump-gone/wire-gone/
  request-unknown/already-announced, run child-read/output-lock/
  needs-program/spawn-lost-stderr/interrupted, pty output-lock/
  interrupted, dequeue task-cancelled/server-closed (shared
  DEQUEUE/ACCEPT keys) + 6 requires-async + serve option-empty/
  required (shared keys) + exec needs-program/username-empty/offline/
  option-positive (shared key) + connect requires-async/offline/
  username-empty + send-exit-status negative/out-of-range;
  `keys.rs`/`bridge.rs`/`runtime.rs`/`types.rs`/`auth.rs`/`run.rs`/
  `pty.rs`/`funcs.rs` zero inline failures (only
  `anyhow::Error::new` + `.context` chains remain); `no_cli_mapping`
  / `acquire_listener` / `resolve_service_target` / `check_options`
  propagation untouched; messages byte-identical (existing
  `contains(...)` pins hold); lib 13 green, integration 67 passed +
  1 pre-existing failure (`non_map_serve_options_bails` fails
  identically on the clean tree: `check_options` renders
  "expected `MAP`" with backticks vs the test's "expected MAP");
  clippy/doc clean; zero fmt drift).
- Sys crate: `oxdock-pipe` done 2026-11-26 (new
  `src/diagnostics.rs` owning 8 `pipe.*` ctors: slot
  os-reader-lock/os-writer-lock/os-handle-consumed (shared
  reader/writer key)/handle-lock, peek unbound/os-materialized/failed,
  spill captured-stdout-invalid-UTF-8; `slot.rs`/`backend.rs`/`spill.rs`
  zero inline failures (only `anyhow::Error::new` remains);
  I/O failures take `&std::io::Error`, UTF-8 takes
  `&std::string::FromUtf8Error`; messages byte-identical; lib 5 green
  (incl. 2 new key+wording pins); clippy/doc clean; zero fmt drift).
- Sys crate: `oxdock-pty` done 2026-11-26 (new
  `src/diagnostics.rs` owning 6 `pty.*` ctors: pump
  master-read/output-lock (shared stream+tail key)/input-read/
  sink-lock, run needs-program, worker panicked (shared input/output
  key with `worker` field); `run.rs` zero inline failures (only
  `anyhow::Error::new` + proper `.context` chains remain);
  I/O failures take `&std::io::Error`, panics take `&impl Debug`;
  messages byte-identical; lib 18 + new pins green, ssh-plugin lib 13
  green; clippy/doc clean; zero fmt drift).
- Leaf crate: `oxdock-utils` done 2026-11-26 (new
  `src/diagnostics.rs` owning 9 `tar.*` ctors: sanitize empty/
  backslash/absolute/escapes-root/empty-normalized, verify
  sha256-mismatch, unpack archive-empty/archive-truncated/
  unsupported-type; `tar.rs` zero inline failures (only
  `anyhow::Error::new` remains); raw names take `&str`, entry type
  takes `&impl Debug`, byte count takes `usize`; messages
  byte-identical (existing `truncated`/`empty`/`escapes` substring pins
  hold); lib 9 green (incl. 2 new key+wording pins), remote-proto 14
  green; clippy/doc clean; zero fmt drift; leaf doc notes the
  dependency-free diagnostics transport exception).
- Plugin crate: `oxdock-markdown-plugin` done 2026-11-26 (new
  `src/diagnostics.rs` owning 10 `md.*` ctors: table
  unrenderable-type (shared cell/nested-JSON key)/non-finite-float/
  expects-map-or-list (shared loop/top-level key)/no-columns, toc
  invalid-format/level-out-of-range (shared min/max key via `OPT_*`
  option param)/min-exceeds-max/inline-requires-single-level (shared
  resolve/generate key, generation casts `u8` to `i64`), section
  no-heading/offsets-out-of-bounds; `markdown.rs` zero inline failures
  (only `anyhow::Error::new` remains; `funcs.rs` was already clean);
  type names/floats/needles take structured values, `Debug` quoting
  lives in ctors; messages byte-identical (existing
  `requires max_level == min_level`/needle pins hold); lib 26
  (incl. 3 new key+wording pins) + integration + doc green;
  clippy/doc clean; zero fmt drift).
- Plugin crate: `oxdock-net-plugin` done 2026-11-26 (new
  `src/diagnostics.rs` owning 69 `net.*` ctors: pump
  worker-panicked/socket-shutdown/stdout-lock (shared socket+memory
  key)/socket-read/interrupted (shared supervisors key)/memory-lock/
  memory-read, fetch url-missing-scheme/url-not-https (distinct keys)/
  cleartext/scheme/redirects/missing-location/status, endpoint
  no-cli-mapping (kept `-> anyhow::Error` signature for the SSH
  caller, body built from the ctor)/duplicate/not-bound/
  virtual-channel/service-not-mapped/never-bound (shared serve/dial/
  introspect/pre-run key)/ambiguous/unknown/already-claimed/dial-only/
  connect-unknown/connect-full/literal-names/offline (shared key),
  validate port-numeric/qualifier/ref-invalid/empty/host-port/
  bad-chars/port-parse/port-zero/bad-shape/reserved, func
  listen-qualified/accept-requires-async/interrupted (shared loops
  key)/listener-closed (shared paths key)/accept-failed/connect-
  requires-async/connect-offline/fetch-option-negative (shared key via
  `OPT_*`)/fetch-exceeds-u32, remote transport-empty/cancelled/
  unknown-tag/no-staged-tarball/handshake-rejected (reason defaults
  inside)/proto-mismatch/language-mismatch/response-failed (chain
  inline)/no-frame/target-failed (reason defaults inside)/script-sha/
  stdout-sha/no-tarball/push-sha/undeclared-path (shared entries key)/
  flag malformed/target-invalid/command-empty/tokenize-failed/
  inventory duplicate/tokenize-failed/no-executable/unknown-target
  (declared list renders inside); all 8 files zero inline failures
  (only `anyhow::Error::new` + proper `.context` chains remain);
  concrete `&io::Error` / `&shell_words::ParseError` /
  `&anyhow::Error` params per contract; messages byte-identical
  (existing `already claimed`/`listener is closed`/`requires ASYNC`/
  `is full`/`--offline mode`/`cleartext`/`is not mapped`/`ambiguous`
  pins hold); lib 38 (incl. 3 new key+wording pins) + integration 38
  green, ssh-plugin lib 13 green (sole `no_cli_mapping` consumer);
  clippy/doc clean; zero fmt drift).
- Sys crate: `oxdock-fs` done 2026-11-26 (new
  `src/diagnostics.rs` owning 63 `fs.*` ctors: guard
  access-escapes (shared canonical/lexical key)/snapshot-not-
  materialized, link refuses-symlink-final/parent-reference/
  link-ancestor, remove-link no-file-name (shared modes key)/
  escapes-root, miri metadata-unguarded/open-file-unguarded/spill
  (shared create/open/truncate key)/private/symlink/temp-lock/
  unguarded-read-dir, symlink dest-exists (shared host+Miri key)/
  self-link/unsupported-platform, file missing (shared Miri-probe +
  mock-reads key), copy unsupported-type (shared classifier + both
  entry-kind probes key)/source-missing (shared host+synthetic key)/
  source-unsupported-kind/no-workspace-root/snapshot-not-
  materialized, backend workdir-not-dir/fd-path-failed (shared
  unix+windows key)/opened-outside-tree, synthetic state-missing
  (root/build)/dir-missing/stat-failed (shared metadata+member key)/
  archive-symlink/entry-kind-missing/missing-file/remove-missing/
  remove-not-empty/metadata-unsupported, path script-not-found/
  base-empty (shared modes key)/rel-empty/rel-dotdot/rel-mixed/
  rel-not-prefix/scratch-collided, cache ensure-failed, snapshot
  cwd-outside-root, mock join-failed (chain inline)/
  metadata-unsupported (shared key)/unguarded-unsupported (shared
  ~13-method key)/read-not-utf8/read-dir/copy (shared 4-direction
  key)/symlink/open/perms/missing-path/remove-no-file/remove-no-dir
  (shared recursive+empty key)/remove-not-empty/git-copy, git
  identity-miri/config-get-failed/config-set-failed (exit status
  interpolated, never hardcoded)/copy-outside-context/ls-tree-failed/
  path-not-in-rev/show-failed/submodule-unsupported; all 13 files
  zero inline failures (only `anyhow::Error::new` + proper `.context`
  chains remain); render-only `&Path` params carry localized
  `#[allow(clippy::disallowed_types)]` per repo convention; messages
  byte-identical (existing `script not found` pin holds); lib 71
  (incl. 3 new key+wording pins) + integration suites green,
  downstream core lib 408 green; clippy/doc clean; zero fmt drift).

### Phase D — retire `anyhow` as the carrier

Only after Phases A through C. `source` chaining is already in the
transport, so this phase is mechanical:

1. Transport foundation done 2026-11-26: `pub type
   DiagnosticResult<T> = Result<T, Diagnostic>` in
   `oxdock-diagnostics` (context via `with_hint` / `with_source`);
   no new dependency (leaf stays dependency-free) because
   `anyhow`'s blanket `From<E: StdError + Send + Sync + 'static>`
   already covers `Diagnostic`, so `?`/`.into()` carry keyed
   diagnostics into unmigrated `anyhow::Result` signatures with
   message and chain intact (pinned by transport tests).
   `Diagnostic` is internally boxed (`Box<DiagnosticInner>`,
   pointer-sized, pinned by a `size_of` test), so
   `DiagnosticResult` keeps the `anyhow::Error` happy-path
   footprint with no `clippy.toml` tuning and no per-item allows;
   the public API (`new`, accessors, builders, `Clone`,
   `PartialEq`, `Error`) is unchanged.
2. Migrate public signatures leaf to root, keeping
   `From<Diagnostic> for anyhow::Error` until the last caller flips.
   Pipe step done 2026-11-26: take x2 and peek now return
   DiagnosticResult with no Error-new wrappers; create_os_pipe and
   OsPipeEntry-new now return io Result as pure kernel creation.
   One boundary fix in core peek_pipe_content via Ok wrap with
   question-mark coercion. Materialize and drain_string_strict stay
   on anyhow Result until dedicated ctors land. Pipe 5 plus core
   408 plus process 119 green; clippy and fmt clean.
   Pipe completion done 2026-11-26: two new ctors for the mixed
   io-plus-diagnostic sites (slot os_pipe_creation_failed,
   spill drain_failed, both with key plus wording pins);
   materialize and drain_string_strict now return
   DiagnosticResult with map_err at the boundary. Crate is now
   anyhow-free (dep removed from Cargo toml, lock entry dropped).
   Pipe 7 plus core 408 plus process 119 green; clippy doc and
   fmt clean.
   AMEND 2026-11-26: the io ctors now attach the typed io error
   via with_source (kind plus text preserved) so Error-source
   and alternate-display chains keep it; PartialEq ignores source
   and Display are unchanged. New source-chain test pins the
   downcast kind and text for each io ctor. Pipe 8 plus core 408
   plus process 119 green; clippy and fmt clean.
   Follow-up 2026-11-26: peek_failed gets the same with_source
   treatment (same format-without-source pattern, now covered by
   the source-chain test too).
   Utils step 2026-11-26: sanitize_rel and verify_sha256 now
   return DiagnosticResult with bare Err ctors; hash_stream now
   returns io Result as a pure stream read. One boundary fix in
   net-plugin hash_and_len via Ok wrap with question-mark
   coercion; core with_context and serve question-mark callers
   hold unchanged. Pack and unpack stay on anyhow Result until
   io ctors plus callback migration land. Utils 9 plus core 408
   green; clippy and fmt clean.
   Pty step 2026-11-26: pump_stdin_to_master now returns
   DiagnosticResult with bare Err ctors; the input-worker
   boundary maps back via Error from into the anyhow ChildDone
   channel. Both io ctors (master_read_failed, input_read_failed)
   now attach the typed io error via with_source; new
   source-chain test pins kind and text. Pump_master_out and
   run_pty_session stay on anyhow Result until spawn and write
   ctors land. Pty 19 plus core 408 green; clippy and fmt clean.
   Mcp step 2026-11-26: build_tool_table, input_schema_for,
   output_schema_for, and schema_for_tag now return
   DiagnosticResult with bare Err ctors; server question-mark
   callers and test expect callers hold unchanged. Map_json_args
   and value_to_json_result stay on anyhow Result until the core
   json boundary flips. Mcp 50 green; clippy and fmt clean.
   Mcp serve step 2026-11-26: serve_stdio and
   serve_stdio_blocking now return DiagnosticResult with bare
   Err ctors (third-party rmcp and tokio errors still ride in
   through the existing anyhow passthrough adapters, messages
   byte-identical); reject_reserved_metas now returns
   DiagnosticResult with a bare Err. One boundary fix in cli
   mcp via Ok wrap with question-mark coercion. Init_handler,
   build_inner, and reload paths stay on anyhow Result until
   core session errors flip. Mcp 50 green; clippy and fmt clean.
   Pty output step 2026-11-26: new output_write_failed and
   output_flush_failed ctors replace the two with_context
   format-string wraps (typed io error via with_source, distinct
   keys); pump_master_out now returns DiagnosticResult with bare
   Err ctors, output-worker boundary maps back via Error from
   into the anyhow ChildDone channel. Run_pty_session spawn and
   alloc paths stay on anyhow Result until spawn ctors land.
   Pty 19 plus core 408 green; clippy and fmt clean.
   Pty spawn step 2026-11-26: four new passthrough ctors
   (pty_alloc_failed, pty_master_reader_failed,
   pty_master_writer_failed, pty_spawn_failed) replace the
   with_context format-string wraps; third-party portable-pty
   errors (already anyhow) ride `{reason:#}` inline with no
   source, messages byte-identical. Run_pty_session keeps its
   anyhow Result (worker accumulation still anyhow-native);
   Context import dropped. Pty 19 plus core 408 green;
   clippy and fmt clean.
   Process step 2026-11-26: format_value_for_string now returns
   DiagnosticResult with three bare Err ctors (pipe,
   semaphore, permit); recursive collect flips to
   DiagnosticResult. Four anyhow boundary fixes via Ok wrap
   with question-mark coercion (lookup x2, key-path tail,
   core resolve_arg). Lookup, resolver type, and remaining
   expand fns stay on anyhow Result until call resolution
   flips. Process 119 plus core 408 green; clippy and fmt
   clean.
   Process vars-path step 2026-11-26: resolve_value_path and
   resolve_key_path_strict now return DiagnosticResult with
   bare Err ctors; two anyhow boundary fixes via Ok wrap with
   question-mark coercion (lookup dollar arm,
   resolve_call_value dollar arm). The call-arg family
   (resolve_placeholder_arg, resolve_call_arg,
   resolve_call_value, resolve_call_map, resolve_call_list)
   stays on anyhow Result: resolve_call_map calls parser
   parse_map_key (anyhow return), so the family flips when
   the parser template boundary does. Process 119 plus core
   408 green; clippy and fmt clean (one pre-existing
   obfuscated_if_else in diagnostics.rs untouched).
   Process spawn-anchors step 2026-11-26: check_spawn_anchors
   now returns DiagnosticResult with two bare Err ctors
   (spawn_requires_path, spawn_requires_system_root); all
   five question-mark callers (core handlers, shell_manager,
   ssh-plugin run and pty, net-plugin session) hold unchanged
   via blanket From; dropped the now-unused anyhow Result
   import. Process 119 plus core 408 green; clippy and fmt
   clean.
   Process direct-cmd step 2026-11-26: direct_cmd now returns
   DiagnosticResult with two bare Err ctors
   (run_exec_requires_argv, run_exec_requires_program); the
   shell_manager run_argv caller and the expect-based unit
   tests hold unchanged; redundant closure simplified per
   clippy. Interactive paths stay on anyhow Result until
   the CommandBuilder boundary flips.
   Process 119 plus core 408 green; clippy and fmt clean.
   Process shell-io step 2026-11-26: three new io ctors
   (run_current_exe_failed, run_failed shared by the
   status-wait and piped-output paths, spawn_failed) with
   typed io source chains plus pins; interpreter_exe and
   ShellLauncher run, run_with_output, and spawn now return
   DiagnosticResult with bare Err ctors, messages
   byte-identical; Context import gated to windows for the
   one remaining windows-only use. Core native
   interpreter_exe caller holds unchanged via blanket From.
   Process 120 plus core 408 green; clippy and fmt clean
   (one pre-existing obfuscated_if_else untouched).
   Process builder-boundary step 2026-11-26: two new
   passthrough ctors (job_assign_failed for the windows
   job-object failure, synthetic_failed for unkeyed Miri
   fallout) plus a from_anyhow helper reclaiming
   already-keyed payloads via downcast so both Miri arms
   preserve keys; CommandBuilder status, output, and spawn
   plus ChildHandle::new now return DiagnosticResult,
   reusing run_failed/spawn_failed/run_spawn_unsupported_miri
   with byte-identical messages. Shell_manager holds via
   one Ok-wrap boundary; build, ssh, cli, and core callers
   hold unchanged. Spawn_piped variants and
   spawn_interactive_shell stay until their own increment.
   Process 121 plus core 408 green; clippy and fmt clean
   (only pre-existing baseline warnings).
   Process piped-spawn step 2026-11-26: two new ctors
   (spawn_pipe_half_missing shared by all five stdin/stdout/
   stderr take sites with the stream name as a structured
   field, spawn_interactive_window_failed preserving the
   windows outer label byte-identically over the inner
   spawn_failed payload); CommandBuilder spawn_piped and
   spawn_piped_with_stderr plus spawn_interactive_shell and
   the test-only shell-command hook chain now return
   DiagnosticResult with bare Err ctors. Ssh-plugin holds
   unchanged via anyhow Context blanket; cli run_shell
   takes one Ok-wrap boundary. Process 121 plus core 408
   plus ssh 13 green; clippy and fmt clean (only
   pre-existing baseline warnings).
   Process manager-helpers step 2026-11-26: no new ctors;
   apply_ctx and spawn_child_with_streams now return
   DiagnosticResult, reusing check_spawn_anchors and
   spawn_failed with byte-identical messages. Trait impls
   and run_prepared hold unchanged via blanket From; the
   handle.wait site keeps run_prepared on anyhow until the
   trait-boundary increment (needs wait io ctors plus the
   BackgroundHandle/ProcessManager trait flip across
   shell_manager, child, contract, mock, and miri
   synthetic). Process 121 plus core 408 green; clippy and
   fmt clean (only pre-existing baseline warnings).
   Process wait-failure step 2026-11-26: one new io ctor
   (wait_failed, shared by the try_wait poll and the blocking
   wait: one key, one mode, typed io source); ChildHandle
   maps both wait io errors through it at the anyhow
   boundary with the outer run_prepared desc context
   unchanged. Trait flip deliberately deferred: flipping
   BackgroundHandle/ProcessManager to DiagnosticResult
   pulls in seven impls across three crates, including
   core ThreadJoinHandle whose re-emit preserves EXIT
   codes via ExitRequest context that has no Diagnostic
   transport yet; that needs its own core-side design
   first. Process 121 plus core 408 green; clippy and
   fmt clean (only pre-existing baseline warnings).
   Process 121 plus core 408 green; clippy and fmt clean
   (only pre-existing baseline warnings).
   Core json-boundary step 2026-11-26: six new ctors
   (std_to_json_non_finite_float, std_to_json_unencodable,
   json_handle_needs_object, json_handle_exact_shape,
   json_handle_task_id_positive,
   json_handle_encode_needs_handle) replace the five bare
   bail sites; value_to_json, json_to_value_typed, and
   value_to_json_typed now return DiagnosticResult.
   To_json_from_value stays on anyhow Result and holds
   unchanged via blanket From. Mcp map_json_args and
   value_to_json_result now return DiagnosticResult with
   bare Err ctors; server map_err boundaries hold
   unchanged. Core 408 plus mcp 50 green; clippy and fmt
   clean (only pre-existing baseline warnings).
   Parser template-boundary plus process call-arg step
   2026-11-26: no new ctors; parse_string_literal,
   parse_quoted_literal, parse_block_literal,
   unescape_literal, and parse_map_key now return
   DiagnosticResult with bare Err ctors, and the process
   call-arg family (resolve_placeholder_arg,
   resolve_call_arg, resolve_call_value, resolve_call_map,
   resolve_call_list) follows with bare Err ctors. The
   literal chain needed no mapping (single-owner errors
   throughout); the one needless Ok-wrap simplified per
   clippy. Lookup, PlaceholderCall, static_check, and core
   args callers hold unchanged via blanket From. Parser
   267 plus integration 303 plus process 121 plus core 408
   green; clippy and fmt clean (only pre-existing
   baseline warnings).
   Process lookup plus streaming-expansion step
   2026-11-26: no new ctors; lookup, process_bytes, flush,
   expand_string, and scan_placeholder now return
   DiagnosticResult with bare Err ctors. The anyhow
   PlaceholderCall resolver boundary maps through the
   existing from_anyhow reclaim, so keyed resolver errors
   keep their keys; PlaceholderCall itself stays on anyhow
   Result until the core native plus handlers resolver
   layer flips. Core EXPAND streaming callers hold
   unchanged via blanket From. Three needless Ok-wraps
   simplified per clippy. Process 121 plus core 408
   green; clippy and fmt clean (only pre-existing
   baseline warnings).
   Core MCP dispatch step 2026-11-26: no new ctors;
   McpSession::call now returns DiagnosticResult with
   bare Err ctors (unknown_tool plus the call_failed
   passthrough, which keeps its &anyhow::Error param and
   inline {reason:#} rendering). Init plus tool_metas
   stay on anyhow Result: new_state, check_pre_run,
   validate_closure, execute_steps, and exported_metas
   are still anyhow, so they hold the session-assembly
   side until those flip. The rmcp call_tool closure
   follows with one bare-Err line (lock-poisoned path);
   its McpError mapping via format {err:#} holds
   unchanged for Diagnostic. Core 408 plus mcp session
   26 plus mcp unit suites green; clippy and fmt clean
   (only pre-existing baseline warnings).
   Export-metadata leaf step 2026-11-26: no new ctors;
   resolve_file_call_key plus exported_metas plus
   McpSession::tool_metas now return DiagnosticResult
   with bare Err ctors (ambiguous_alias plus
   export_unknown, both already keyed). McpSession::init
   plus resolve_call_entry plus the rmcp build_inner path
   hold unchanged via blanket From (Diagnostic into
   anyhow). Core 408 plus mcp session 26 plus mcp unit
   suites green; clippy and fmt clean (only pre-existing
   baseline warnings).
   Endpoint-validation leaf step 2026-11-26: no new
   ctors; parse_port, split_protocol_qualifier,
   parse_endpoint_ref, and parse_virtual_endpoint now
   return DiagnosticResult with bare Err ctors (all
   net.validate.* already keyed). The two chaining
   ctors (endpoint_ref_invalid, endpoint_port_parse)
   keep their &anyhow::Error params per the passthrough
   precedent, so the two intra-file sites wrap the
   Diagnostic inner with anyhow::Error::new. Net-plugin
   plus ssh-plugin host-func callers hold unchanged via
   blanket From; the two CLI publish_inner_invalid sites
   wrap with anyhow::Error::new to match that ctor's
   &anyhow::Error param; ssh parse_serve_endpoint takes
   one Ok(...?) wrap at the anyhow boundary (direct
   return does not coerce). Net 38 plus net-lib 38 plus
   ssh-unit 13 plus cli-lib 49 green; ssh integration
   shows only the pre-existing non_map_serve_options
   failure (identical on clean tree); clippy and fmt
   clean.
   Zero-value leaf step 2026-11-26: no new ctors;
   optional_zero_value plus check_script_param_tags now
   return DiagnosticResult with bare Err ctors
   (zero_value_missing already keyed; optional_param
   keeps its &anyhow::Error param per the passthrough
   precedent, so the two intra-file sites wrap the
   Diagnostic inner with anyhow::Error::new).
   resolve_script_params stays on anyhow Result: its
   resolve_tag plus check_declaration_tag callees are
   still anyhow, so its optional_zero_value site takes
   the same shim and holds. The two register paths plus
   handlers callers hold unchanged via blanket From
   (including the with_context fill site, since anyhow
   Context covers Diagnostic). Core 408 green; clippy
   and fmt clean.
   Define-script leaf step 2026-11-26: no new ctors;
   define_script plus define_file_func now return
   DiagnosticResult with bare Err ctors (shadow_reserved
   plus duplicate_in_scope, both already keyed; the
   check_script_param_tags call holds directly now that
   it is Diagnostic). The infallible param-vec collect
   re-targets its turbofish to DiagnosticResult.
   Handlers plus import callers (four bare-? sites, all
   still anyhow) hold unchanged via blanket From. Core
   408 green; clippy and fmt clean.
   Union-finish leaf step 2026-11-26: no new ctors;
   finish_union now returns DiagnosticResult with a bare
   empty_union Err (already keyed). Its build_rendered
   caller stays on anyhow Result, so the tail call takes
   one Ok(...?) wrap (direct return does not coerce).
   Core 408 green; clippy and fmt clean.
   Alias-collect leaf step 2026-11-26: no new ctors;
   collect_type_aliases now returns DiagnosticResult
   with five bare Err ctors (already keyed:
   already_defined plus builtin/host/schema/func
   collisions). All callers (handlers map-transpose,
   six static_check sites, five import sites, mod
   validate_closure, tests if-let-Ok) stay on anyhow
   and hold via blanket From. Core 408 green; clippy
   and fmt clean.
   Tag-resolution cluster step 2026-11-26: two new
   ctors (typing_unregistered plus typing_void_position,
   wording byte-identical to the replaced bail!s, both
   pinned by key plus message tests); resolve_tag plus
   resolve_named plus resolve_spelling_text plus
   build_rendered plus build_generic plus check_tag_known
   plus check_declaration_tag plus check_return_tag now
   return DiagnosticResult with bare Err ctors (all other
   keys pre-existing). parse_spelling already returns
   Diagnostic so its map_err shim drops; check_value keeps
   its map_err (anyhow into Diagnostic, same-type ?).
   validate_function_type_tags stays anyhow (three bare
   bail!s need their own ctors later) and its
   check_tag_known call holds via blanket From, as do all
   bare-? callers. Sixteen map_err-with-ctor sites
   (native 2, static_check 10 incl. check_return_tag and
   check_declaration_tag wraps, import 4 with Some) take
   the &anyhow::Error::new inner shim per the passthrough
   precedent; the handlers anyhow!-format site compiles
   unchanged (Display). Core 408 plus integration 629
   plus parser 267 green; clippy (only pre-existing
   baseline) and fmt clean.
   Script-params leaf step 2026-11-26: no new ctors;
   resolve_script_params now returns DiagnosticResult.
   The func_param_type map_err drops its outer
   anyhow::Error::new (the ctor keeps its &anyhow::Error
   param, inner shim stays); check_declaration_tag holds
   directly and the func_optional_param site holds
   unchanged (both already Diagnostic). All six callers
   (handlers 2, static_check 2, import 2) stay anyhow and
   hold via blanket From. Core 408 plus integration 629
   green; clippy and fmt clean.
   Func-tags validation step 2026-11-26: three new ctors
   (func_param_values_without_type plus
   func_optional_before_required plus
   func_optional_without_map, wording byte-identical to
   the replaced bail!s, all pinned by key plus message
   tests); validate_function_type_tags now returns
   DiagnosticResult with bare Err ctors, and its
   check_tag_known calls hold directly. The mod
   validate_closure caller and all seven tests.rs sites
   hold unchanged (blanket From for ?, Display plus
   expect_err/expect are error-type generic). Core 408
   plus integration 629 plus parser 267 green; clippy
   (only pre-existing baseline) and fmt clean.
   Options-check leaf step 2026-11-26: two new ctors
   (check_unknown_option plus check_option_value,
   wording byte-identical to the replaced bail! and the
   check_value_at path failure, both pinned by key plus
   message tests); check_options now returns
   DiagnosticResult. The unknown-key bail becomes a bare
   Err with the joined slice passed in; the shape check
   maps the parser anyhow failure through the verbatim
   passthrough (its path prefix already names func, key,
   expected, and actual, so no wrapper is added, per
   single-ownership). The anyhow import drops from
   typing.rs entirely. Macro-generated wrappers (anyhow
   Result) plus the markdown boundary test hold via
   blanket From. Core 408 plus integration 629 plus
   markdown 26 green; clippy and fmt clean.
   CLI-parse cluster step 2026-11-26: one new ctor
   (std_parse_args_arg_shape, verbatim passthrough of
   the check_value_at path failure, pinned by key plus
   message test); coerce_cli_flag plus string_list_arg
   plus tokenize_raw_args (including its utf8 closure)
   plus coerce_flags plus resolve_flags_specs now return
   DiagnosticResult with bare Err ctors (all other keys
   pre-existing; lex_error keeps its &lexopt::Error
   param per the concrete-borrowed precedent).
   int_from_value/float_from_value stay anyhow (their
   own bare messages are a separate leaf); their
   map_err sites discard into keyed ctors unchanged.
   The three NativeFn closures (PARSE_ARGS plus
   PARSE_FLAGS plus PARSE_POSITIONALS) stay anyhow and
   hold via blanket From. Core 408 plus integration 629
   green; clippy and fmt clean.
   Int-float converter step 2026-11-26: seven new ctors
   (std_int_non_integer_float plus std_int_bad_string
   plus std_int_bad_type plus std_float_non_finite_float
   plus std_float_bad_string plus
   std_float_non_finite_string plus std_float_bad_type,
   wording byte-identical to the replaced bail!s, all
   pinned by key plus message tests, Debug rendering
   inside the ctors per the spelled precedent);
   int_from_value plus float_from_value now return
   DiagnosticResult with bare Err ctors. The
   coerce_cli_flag discarding map_errs hold unchanged;
   the INT()/FLOAT() macro fns stay anyhow and take one
   Ok(...?) wrap each at the boundary (direct return
   does not coerce). Core 408 plus integration 629
   green; clippy and fmt clean.
   Arithmetic-eval step 2026-11-26: seven new ctors
   (eval_math_stack_underflow plus
   eval_arith_overflow_div_zero plus
   eval_arith_needs_numeric plus eval_float_div_zero
   plus eval_non_finite_result plus eval_neg_overflow
   plus eval_neg_needs_numeric, wording byte-identical
   to the replaced bail!s, all pinned by key plus
   message tests, Debug rendering inside the ctors;
   non-finite shared by arith and neg so one key owns
   the failure mode). pop_stack plus apply_arith plus
   apply_neg now return DiagnosticResult; the
   evaluate_expr arith arm plus the
   evaluate_compiled_math tail take one Ok(...?) wrap
   each at the anyhow boundary, while the interior
   `?` sites hold via blanket From. Core 408 plus
   integration 629 green; clippy and fmt clean.
   Condition-int resolve step 2026-11-26: three new
   ctors (eval_condition_needs_bool plus
   eval_expected_int plus eval_int_arg_resolve,
   wording byte-identical, all pinned by key plus
   message tests, Debug rendering inside the ctors);
   is_truthy plus resolve_arg_as_int now return
   DiagnosticResult. The resolve_arg dependency stays
   anyhow, so its failure maps through the verbatim
   passthrough (message already names the argument
   and cause, per the parse_args precedent). All five
   is_truthy plus three resolve_arg_as_int call sites
   sit in anyhow fns and hold via blanket From. Core
   408 plus integration 629 green; clippy and fmt
   clean.
   Compare-boundary step 2026-11-26: three new ctors
   (eval_compare_equality_shape plus
   eval_ordering_needs_numeric plus
   eval_unstaged_int_boundary, wording byte-identical,
   all pinned by key plus message tests, Debug
   rendering inside the ordering ctor).
   apply_compare now returns DiagnosticResult; the
   Compare arm takes one Ok(...?) wrap at the anyhow
   boundary, while the u64 boundary arm keeps its
   anyhow::Error::new wrap (direct Err return does
   not coerce; needless_return avoided with a bare
   tail). Core 408 plus integration 629 green; clippy
   and fmt clean.
   Value-read step 2026-11-26: five new ctors
   (eval_undefined_var plus eval_key_not_found plus
   eval_invalid_array_index plus
   eval_index_out_of_bounds plus eval_traverse_scalar,
   wording byte-identical, all pinned by key plus
   message tests; the runtime traverse/sigil keys stay
   distinct from the static check_* backticked keys).
   load_var_binding plus descend_value now return
   DiagnosticResult; the Var plus KeyPath arms take
   one Ok(...?) wrap each at the anyhow boundary,
   while all `?` sites (AST, RPN, templates,
   handlers pipe-guard) hold via blanket From. Core
   408 plus integration 629 green; clippy and fmt
   clean.
   Path-predicate step 2026-11-26: eight new ctors
   (eval_path_type_needs_arg plus
   eval_path_type_needs_string plus
   eval_is_exec_needs_arg plus
   eval_is_exec_needs_string plus
   eval_path_writable_needs_arg plus
   eval_path_writable_needs_string plus
   eval_path_resolve_failed plus
   eval_workdir_resolve_failed, wording byte-identical,
   all pinned by key plus message tests; the two
   identical resolve-path texts share one key per the
   same-failure-mode rule, while per-command arg
   shapes keep per-command keys since IS_EXEC says
   `name` where the others say `path`).
   path_type_from_value plus is_exec_from_value plus
   path_writable_from_value now return
   DiagnosticResult; the three NativeFn wrappers stay
   anyhow and take one Ok(...?) wrap each. Core 408
   plus integration 629 green; clippy and fmt clean.
   Name-constant step 2026-11-26: new
   `exec/function_names.rs` with 22 `FUNC_*` constants
   (every STD function named in a diagnostic message
   or hand-built registration); all core `std_*` /
   `eval_*` constructor messages interpolate the
   constants plus existing `*_TYPE_NAME` constants
   instead of restating spellings, byte-identical
   output proven by the unchanged literal message
   pins. The three hand-built registrations
   (`PARSE_ARGS`, `PARSE_FLAGS`, `PARSE_POSITIONALS`)
   plus their arg-prefix strings plus the
   `BOOL`/`BOOL_OR` coerce call sites use the same
   constants, ending the triple-sourcing. New
   `HostRegistration::name()` plus a
   `function_names_match_registry` lock test asserts
   every constant against `builtin_registrations()`,
   so a Rust-fn rename (which moves the
   macro-derived name) fails loudly. Parser's three
   `("key", key)` leaks now use a new `FIELD_KEY`.
   Deliberately unchanged: message-test expectations
   stay literal (they lock user-visible bytes; the
   registry test locks constant-to-registry
   agreement), and keyword literals (`IMPORT`, `LET`,
   `RETURN`, ...) plus core `FIELD_*` plus the
   static-vs-runtime wording pairs stay for defined
   follow-ups (keyword sweep via existing `KEYWORD_*`;
   core field-key registry per the 11-crate precedent;
   wording convergence as a behavior-changing
   decision with its own review). Core 409 plus
   integration 629 plus parser 267 green; clippy and
   fmt clean.
   Base64-codec step 2026-11-26: four new ctors
   (std_base64_encode_needs_string plus
   std_base64_decode_needs_string plus
   std_base64_decode_failed plus
   std_base64_decode_not_utf8, wording byte-identical,
   all pinned by key plus message tests; the decode
   failure takes the concrete `&base64::DecodeError`
   per the borrowed-error precedent, with `Debug`
   rendering of the input inside the ctor).
   base64_encode_from_value plus
   base64_decode_from_value now return
   DiagnosticResult; the two `#[oxdock_func]` wrappers
   stay anyhow and take one Ok(...?) wrap each, while
   the args.rs round-trip unit tests hold unchanged.
   Two new `FUNC_*` constants
   (`FUNC_BASE64_ENCODE`, `FUNC_BASE64_DECODE`)
   covered by the registry lock test. Core 409 plus
   integration 629 green; clippy and fmt clean.
   Toml-load step 2026-11-26: seven new ctors
   (std_load_toml_needs_arg plus
   std_load_toml_needs_string plus
   std_load_toml_resolve_failed plus
   std_load_toml_read_failed plus
   std_load_toml_not_utf8 plus std_toml_parse_failed
   plus std_parse_toml_needs_string, wording
   byte-identical, all pinned by key plus message
   tests; resolve/read failures take
   `&anyhow::Error`, the UTF-8 failure the concrete
   `&Utf8Error`, the parse failure the concrete
   `&toml::de::Error`, each per the borrowed-error
   precedent). load_toml_from_value plus
   load_toml_value plus parse_toml_from_value now
   return DiagnosticResult; the two NativeFn wrappers
   stay anyhow with one Ok(...?) wrap each, while the
   internal tail calls use direct returns (same error
   type both sides, per needless_question_mark).
   The test helper builds the Utf8Error at runtime
   through a Vec to avoid the invalid_from_utf8
   lint on literal bytes. Two new FUNC_* constants
   (`FUNC_LOAD_TOML`, `FUNC_PARSE_TOML`) covered by
   the lock test. Core 409 plus integration 629
   green; clippy and fmt clean.
   Json-load step 2026-11-26: seven new ctors
   (std_load_json_needs_arg plus
   std_load_json_needs_string plus
   std_load_json_resolve_failed plus
   std_load_json_read_failed plus
   std_load_json_not_utf8 plus std_json_parse_failed
   plus std_parse_json_needs_string, wording
   byte-identical, all pinned by key plus message
   tests; the JSON parse failure takes the concrete
   `&serde_json::Error`, mirroring the TOML step's
   `&toml::de::Error`). load_json_from_value plus
   load_json_value plus parse_json_from_value now
   return DiagnosticResult; the two NativeFn wrappers
   stay anyhow with one Ok(...?) wrap each. Two new
   FUNC_* constants (`FUNC_LOAD_JSON`,
   `FUNC_PARSE_JSON`) covered by the lock test. Core
   409 plus integration 629 green; clippy and fmt
   clean.
   Glob step 2026-11-26: four new ctors
   (std_glob_needs_arg plus std_glob_needs_string
   plus std_glob_resolve_failed plus
   std_glob_list_failed, wording byte-identical, all
   pinned by key plus message tests; the two failure
   passthroughs stay verbatim with no prefix, matching
   the bare `?` propagation they replace, and the
   listing failure takes the concrete
   `&std::io::Error` per the borrowed-error
   precedent). glob_from_value now returns
   DiagnosticResult; the `#[oxdock_func]` wrapper
   stays anyhow with one Ok(...?) wrap. One new
   FUNC_* constant (`FUNC_GLOB`) covered by the lock
   test; test-sample construction uses
   `io::Error::other` per the clippy lint. Core 409
   plus integration 629 green; clippy and fmt clean.
   Map-helpers step 2026-11-26: six new ctors
   (std_has_key_needs_map plus
   std_map_set_needs_map plus std_map_set_duplicate
   plus std_merge_maps_needs_list plus
   std_merge_maps_needs_map plus
   std_merge_maps_duplicate, wording byte-identical,
   all pinned by key plus message tests; the element
   position rides a `usize` index formatted through
   `to_string`, per the eval-index precedent).
   has_key_from_value plus map_set_from_value plus
   merge_maps_from_value now return DiagnosticResult;
   the three NativeFn wrappers stay anyhow with one
   Ok(...?) wrap each. Three new FUNC_* constants
   (`FUNC_HAS_KEY`, `FUNC_MAP_SET`,
   `FUNC_MERGE_MAPS`) covered by the lock test; also
   removed a stray duplicate `FUNC_LOAD_TOML` entry
   the TOML step left in the lock list. Core 409 plus
   integration 629 green; clippy and fmt clean.
   To-json-encode step 2026-11-26: one new ctor
   (std_to_json_encode_failed, wording byte-identical
   `encode JSON: ...`, pinned by key plus prefix
   message tests; takes the concrete
   `&serde_json::Error` per the borrowed-error
   precedent, mirroring the parse-failure ctor).
   to_json_from_value now returns DiagnosticResult
   (value_to_json was already flipped, so only the
   serialization tail needed mapping); the NativeFn
   wrapper stays anyhow with one Ok(...?) wrap. No
   new FUNC_* constant (`FUNC_TO_JSON` already
   covered). Core 409 plus integration 629 green;
   clippy and fmt clean.
   Resolve-arg-state step 2026-11-26: two new ctors
   (eval_arg_expr_needs_step_ctx plus
   eval_arg_expand_failed, wording byte-identical,
   both pinned by key plus message tests). The Expr
   sites share one key: the ctor takes `&Expr` and
   renders `Debug` inside, per the value-rendering
   precedent. Expansion failures ride a verbatim
   `{reason:#}` passthrough mirroring
   eval_int_arg_resolve, with its own key since the
   failure context differs (string expansion versus
   int resolution). resolve_arg_state now returns
   DiagnosticResult; its two steps.rs callers
   (pre_register_assertions,
   sync_iteration_assert_needles) stay anyhow and
   hold via the blanket Diagnostic-into-anyhow
   conversion, so no caller edits. No new FUNC_*
   constant (internal resolution, not stdlib
   surface). Core 409 plus integration 629 green;
   clippy and fmt clean.
   Text-helpers step 2026-11-26: five new ctors
   (std_text_invalid_pattern plus
   std_text_needs_search plus std_text_needs_needle
   plus std_text_needs_one_based_start plus
   std_text_needs_ordered_range, wording
   byte-identical, all pinned by key plus message
   tests). Shared helpers take the invoking `func`
   as a structured field, so one key covers every
   caller (TEXT_MATCH, TEXT_GREP, TEXT_COUNT_MATCHES
   and TEXT_EXTRACT share the pattern key; the empty
   search sites stay distinct wordings, each its own
   key). The pattern failure takes the concrete
   `&regex::Error` per the borrowed-error precedent;
   the prefix assertion only pins the stable head
   since the regex backend owns the tail. Eight
   functions flip (text_compile_pattern plus
   text_replace_literal_str plus
   text_replace_regex_str plus text_count_str plus
   text_grep_str plus text_count_matches_str plus
   text_lines_range_str plus text_extract_str);
   TextPump::apply_str holds its anyhow signature
   with one Ok(...?) wrap. Eight new FUNC_TEXT_*
   constants wired through all production call sites
   and covered by the lock test; TRIM/TO_UPPER/
   TO_LOWER family literals stay for their own
   mirror-step later. Test helper builds the
   regex::Error at runtime through a String to
   avoid the invalid_regex lint on literal patterns.
   Core 409 plus integration 629 green; clippy and
   fmt clean.
   Pipe-refusal step 2026-11-26: one new ctor
   (std_pipe_needs_step_io, wording byte-identical,
   pinned by key plus message test). The anyhow
   helper is deleted: both call sites invoke the
   ctor directly, so the constructor is the single
   owner of the message. mirror_string converts
   with `.into()` and text_replace_regex_string
   with the same, each holding its anyhow signature
   via the blanket Diagnostic-into-anyhow
   conversion. No new FUNC_* constant (the
   TEXT_REPLACE call site rides the existing
   constant). Core 409 plus integration 629 green;
   clippy and fmt clean.
   Coercion step 2026-11-26: three new ctors
   (coerce_type_mismatch plus coerce_string_to_pipe
   plus coerce_pipe_to_string, wording
   byte-identical, all pinned by key plus message
   tests). The generic ctor takes `&TypeTag` plus
   `&Value` and renders the structural tag plus the
   type word inside, per the value-rendering
   precedent; the pipe ctors interpolate
   PIPE_TYPE_NAME plus STRING_TYPE_NAME instead of
   literals. Both anyhow helpers (type_mismatch,
   mismatch) are deleted: all 15 construction sites
   across coerce_value, coerce_scalar,
   string_or_path_text, mirror_stream, and
   read_pipe_eager invoke the ctors directly with
   `.into()`, holding their anyhow signatures. One
   signature flips as a clean leaf:
   string_or_path_text now returns DiagnosticResult
   (single ctor site, no inner anyhow dependency);
   its ~15 NativeFn callers hold unchanged via the
   blanket conversion. coerce_value plus
   coerce_scalar stay anyhow for now: the parser
   check_value error they propagate is still a raw
   anyhow bail, and flipping ahead of the parser
   would need a downcast. New `dsl.coerce.*`
   namespace; no FUNC_* constants (slot contexts
   like "coercion" ride the existing call-site
   strings). Core 409 plus integration 629 green;
   clippy and fmt clean.
   Const-call-arg step 2026-11-26: two new ctors
   (eval_const_arg_keypath plus
   eval_const_arg_bare_sigil, wording byte-identical,
   both pinned by key plus message tests). The
   parser template helpers stay the single wording
   owners: the ctors call qualified_keypath_error
   plus bare_sigil_error directly and only add the
   key, so every phase keeps agreeing word for word
   by construction. The bare-sigil ctor takes the
   trimmed source; the keypath ctor is parameterless.
   resolve_const_call_arg holds its anyhow signature
   with two `.into()` conversions: its success arm
   rides resolve_const_ref, which still returns
   anyhow, so the flip waits on the import layer per
   leaf-to-root order. No FUNC_* constants (template
   internals, not stdlib surface). Core 409 plus
   integration 629 green; clippy and fmt clean.
   Mirror-pipe step 2026-11-26: three new ctors
   (std_pipe_read_failed plus std_pipe_create_failed
   plus std_pipe_spawn_failed, wording byte-identical,
   all pinned by key plus message tests; read/create
   take `&anyhow::Error`, spawn the concrete
   `&std::io::Error`, each per the borrowed-error
   precedent). The empty-search preflight reuses the
   existing std_text_needs_search key instead of
   minting a duplicate: same failure mode, func as
   the structured field. mirror_stream holds its
   anyhow signature with explicit
   anyhow::Error::from conversions: bare `.into()`
   inside map_err closures is ambiguous (E0283,
   reflexive From<Diagnostic> versus the blanket
   into-anyhow), while `Err(... .into())` in tail
   position infers cleanly from the return type.
   Identical read/create wordings in read_pipe_eager
   plus write_completed_pipe stay for their own
   step, which will reuse these keys. No FUNC_*
   constants (call sites ride the existing func
   params). Core 409 plus integration 629 green;
   clippy and fmt clean.
   Eager-pipe step 2026-11-26: six new ctors
   (pipe_reader_lock_poisoned plus
   pipe_writer_lock_poisoned plus
   pipe_chunk_read_failed plus pipe_input_not_utf8
   plus pipe_spill_failed plus pipe_write_failed,
   wording byte-identical, all pinned by key plus
   message tests). The outer open/create failures
   reuse the existing pipe_read_failed plus
   pipe_create_failed keys with explicit
   anyhow::Error::from conversions (bare `.into()`
   in map_err closures stays ambiguous per E0283).
   The inner loop read gets its own key because it
   reports `{e}` Display while the outer open
   reports `{e:#}`: pipe_chunk_read_failed takes
   `&std::io::Error` where the outer takes
   `&anyhow::Error`. The two UTF-8 sites share one
   func-only key (no displayable detail); the two
   spill sites share one func-plus-io key; write_all
   plus flush share one func-plus-io key. Locks take
   func only. Both fns hold their anyhow signatures
   per leaf-to-root order. No FUNC_* constants
   (call sites ride the existing func params). Core
   409 plus integration 629 green; clippy and fmt
   clean.
3. Delete `anyhow` per crate only when no `anyhow::` path remains.
4. Plugin boundary stays optional for external authors: the
   `#[oxdock_func]` / host-function boundary keeps accepting plain
   error returns (`anyhow` / boxed errors) and maps them to a
   `plugin.custom` fallback key, so third-party plugins are never
   required to depend on `oxdock-diagnostics` or hand-write
   per-constructor modules. The strict constructor pattern
   (`diagnostics.rs`, `FIELD_*`, key + wording pins) applies to
   first-party crates only. If that boilerplate drags first-party
   velocity, a follow-up (separate issue, not this one) may add a
   declarative/`derive` macro generating constructors, `FIELD_*`
   constants, and uniqueness checks from an enum, preserving the
   wire format and single-owner wording.

Acceptance D: `grep -r anyhow --include='*.rs' crates/ oxdock/` empty
except shims listed, workspace tests green, no public
`anyhow::Result`, and an external plugin with no
`oxdock-diagnostics` dependency still compiles and runs through the
fallback key.

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
- Plugin-authoring friction: per-constructor boilerplate plus key
  taxonomy could push external authors away. Guard: the host-function
  boundary keeps the `anyhow`/boxed-error fallback (`plugin.custom`
  key); strict `diagnostics.rs` is first-party only; any
  constructor-generating macro is a separate follow-up issue, never a
  blocker for Phase D.
- Speculative LSP fields: `Span` gains file ids / byte offsets only
  when a symbol table needs them. Guard: downstream crates build
  spans only through `Span` constructors (`line_only` / `line_col` /
  `full`), never struct literals. `Step::span()` /
  `defined_symbol()` (TYPE/FUNC, in `ast.rs`) derive from stamped
  line/col with zero new data; positions stay additive later.
- Error-carrier choice: `anyhow` stays only as the Phase D interim
  carrier (its blanket `From` lets unmigrated `anyhow::Result`
  callers accept `Diagnostic` with no glue). Guard: internal
  execution paths terminate on `DiagnosticResult`, never on a
  second error vocabulary. `thiserror` fits only at explicit
  external boundaries (plugin/host-function adapters, wire-error
  mapping) where typed foreign variants must cross; adopting it is
  a scoped post-Phase-D follow-up per boundary, never a blanket
  replacement of `DiagnosticResult`.
