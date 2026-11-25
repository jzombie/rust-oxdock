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
   AMEND 2026-11-26: both io ctors now attach the typed io error
   via with_source (kind plus text preserved) so Error-source
   and alternate-display chains keep it; PartialEq ignores source
   and Display are unchanged. New source-chain test pins the
   downcast kind and text for both ctors. Pipe 8 plus core 408
   plus process 119 green; clippy and fmt clean.
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
