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
   Template-expansion step 2026-11-26: seven new
   ctors (template_missing_key plus
   template_empty_placeholder plus
   template_invalid_key plus template_empty_env
   plus template_keypath plus template_bare_sigil
   plus template_placeholder_miss, wording
   byte-identical, all pinned by key plus message
   tests). The template helpers stay the single
   wording owners for keypath plus bare-sigil: the
   new template ctors call qualified_keypath_error
   plus bare_sigil_error directly and only add keys,
   mirroring the const-arg pair (duplicate wording
   across phases keeps separate keys per the
   existing check-vs-eval precedent). Missing keys
   share one key across both sites via the closure:
   the ctor takes `env_hint: bool` and builds the
   `env:` versus `$` suggestion inside, so call
   sites pass decoded values. Empty `env:` plus
   `script_env:` share one parameterless key. The
   lazy placeholder miss is a verbatim passthrough
   taking `&str` with no source attached.
   expand_string holds its anyhow signature with
   explicit anyhow::Error::from conversions: its
   success arms ride resolve_const_ref plus
   resolve_const_call_arg, both still anyhow, so the
   flip waits on the import layer per leaf-to-root
   order. Only one key-list entry for the missing
   variant (true/false share the key); both wordings
   stay pinned by message tests. Core 409 plus
   integration 629 green; clippy and fmt clean.
   Math-imbalance step 2026-11-26: one new ctor
   (eval_math_stack_len taking `usize`, wording
   byte-identical, pinned by key plus message test).
   This was the last `bail!` in args.rs, so the
   `bail` import is trimmed, leaving only
   `anyhow::Result`. evaluate_compiled_math holds
   its anyhow signature: its arms ride expand_string
   plus resolve_const_ref plus handlers dispatch,
   all still anyhow, so the signature flip waits
   per leaf-to-root order. Core 409 plus integration
   629 green; clippy and fmt clean.
   Pre-run chain step 2026-11-26: no new ctors, all
   wordings already pinned. Four signatures flip to
   DiagnosticResult across two crates: core
   PreRunInspector plus PreRunCheck::new plus run
   plus HostModule::check_pre_run, and net
   assert_services_bound plus check_service_bound
   plus EndpointRegistry::resolve_ref. resolve_ref
   internals drop six anyhow::Error::new wrappers
   (virtual_channel_no_physical, service_not_mapped,
   never_bound, ambiguous_endpoint) since
   parse_endpoint_ref was already DiagnosticResult.
   check_service_bound calls net_diag::no_cli_mapping
   directly at its two sites; the endpoints.rs
   no_cli_mapping anyhow wrapper stays for the
   call-time acquire paths plus SSH, which hold
   anyhow and flip in their own steps. Callers hold
   via the blanket conversion (core mod.rs plus
   mcp.rs check_pre_run sites unchanged); tests
   using to_string plus contains plus {:#} pass
   unchanged since Display renders the message.
   Core 409 plus integration 629 plus net 38 plus
   service_enforcement 4 green; clippy and fmt
   clean.
   Session-assembly step 2026-11-26: no new ctors,
   both fns infallible. Two signatures flip to
   DiagnosticResult: fs reserve_cargo_scratch plus
   core new_state. reserve_cargo_scratch is pure
   lazy reservation with no I/O, so the body stays
   a bare Ok; the flip only retires the anyhow
   Result alias at the boundary. new_state holds a
   single `?` on reserve, now direct with no
   conversion. Callers hold: the anyhow session
   entry rides the blanket Diagnostic-into-anyhow
   conversion with no change, and the `.unwrap()` /
   `.expect()` test harnesses work unchanged since
   Diagnostic is Debug. Core 409 plus integration
   629 plus fs 71 green; clippy and fmt clean.
   Import-closure step 2026-11-26: no new ctors,
   all wordings already pinned. Eight signatures
   flip to DiagnosticResult in import.rs:
   resolve_top_level_with plus resolve_top_level
   plus load_file plus expand_load plus expand_walk
   plus expand_walk_top plus expand_walk_kind plus
   expand_import_closure. Bodies drop ten
   anyhow::Error::new wrappers; the two
   resolve_tag sites keep their inner
   anyhow::Error::new(e) reason wrap (the callee is
   already Diagnostic, the ctor takes
   &anyhow::Error) and only shed the outer wrap.
   read_import_bytes holds anyhow: the caller maps
   it into import_cannot_read with reason chaining,
   so no intermediate conversion is lost. Holders
   convert at the boundary with explicit
   map_err(anyhow::Error::from): dry_run_resolve
   plus load_import stay anyhow for the dry-run and
   runtime paths. Core 409 plus integration 629
   green; clippy and fmt clean.
   Harvest-merge step 2026-11-26: no new ctors,
   all wordings already pinned. Four signatures
   flip to DiagnosticResult: import
   insert_imported_type plus gate_harvested_name
   plus static_check merge_harvest plus
   merge_harvests_in. Bodies drop seven
   anyhow::Error::new wrappers; the resolve_tag
   site keeps its inner anyhow::Error::new(err)
   reason wrap (the callee is already Diagnostic,
   the ctor takes &anyhow::Error) and only sheds
   the outer wrap. Callers hold via the blanket
   Diagnostic-into-anyhow conversion: anyhow
   import_funcs and validate_script_closure plus
   Checker sites call the flipped fns with no
   change. Core 409 plus integration 629 green;
   clippy shows only the pre-existing
   needless_borrow warnings elsewhere in the file;
   fmt clean.
   Func-registry step 2026-11-26: no new ctors,
   all wordings already pinned. Two signatures
   flip to DiagnosticResult in static_check.rs:
   collect_func_harvest_edges plus
   extend_funcs_checked. Bodies drop two
   anyhow::Error::new wrappers; both are pure
   leaves (closure-key probe plus map insert, no
   fallible callees). Callers hold via the blanket
   Diagnostic-into-anyhow conversion with no
   change. Core 409 plus integration 629 green;
   clippy shows only the pre-existing
   needless_borrow warnings elsewhere in the file;
   fmt clean.
   Arith-rule step 2026-11-26: no new ctors, all
   wordings already pinned. Three signatures flip
   to DiagnosticResult in static_check.rs:
   numeric_pair plus neg_rule plus ordering_pair.
   Bodies drop five anyhow::Error::new wrappers;
   all three are pure leaves (StaticTy match plus
   loc prefix, no fallible callees). The one
   anyhow boundary that returns a leaf directly
   (infer_expr Arithmetic arm) uses Ok(f(...)?)
   since Result error types do not coerce on
   direct return; the `?` call sites ride the
   blanket conversion unchanged. Core 409 plus
   integration 629 green; clippy shows only the
   pre-existing needless_borrow warnings elsewhere
   in the file; fmt clean.
   Unify-traverse step 2026-11-26: no new ctors,
   all wordings already pinned. Two signatures
   flip to DiagnosticResult in static_check.rs:
   unify plus traverse_static_keypath. Bodies drop
   five anyhow::Error::new wrappers; both are pure
   leaves (tags_equal match plus field/index
   probes, no fallible callees). Callers hold via
   the blanket Diagnostic-into-anyhow conversion:
   anyhow infer_expr plus walk sites call with `?`
   and no change. Core 409 plus integration 629
   green; clippy and fmt clean.
   Exact-arg step 2026-11-26: no new ctors, all
   wordings already pinned. Three signatures flip
   in static_check.rs: exact_mismatch returns
   Diagnostic plus Checker check_exact plus
   check_arg return DiagnosticResult. Bodies drop
   five anyhow::Error::new wrappers; the cluster
   is self-contained (tags_equal plus
   coercible_pair plus is_compatible are
   infallible bool, recursion stays inside the
   flipped set). Callers hold via the blanket
   Diagnostic-into-anyhow conversion: anyhow walk
   plus call sites use `?` or is_ok with no
   change. Core 409 plus integration 629 green;
   clippy and fmt clean.
   Module-lets step 2026-11-26: no new ctors, all
   wordings already pinned. Two signatures flip
   in static_check.rs: module_lets plus
   module_lets_indexed return DiagnosticResult.
   Body drops one anyhow::Error::new wrapper; the
   resolve_tag site keeps its inner
   anyhow::Error::new(err) reason wrap (the callee
   is already Diagnostic, the ctor takes
   &anyhow::Error) and only sheds the outer wrap.
   Callers hold via the blanket
   Diagnostic-into-anyhow conversion with no
   change. Core 409 plus integration 629 green;
   clippy and fmt clean.
   Binding-read step 2026-11-26: no new ctors,
   all wordings already pinned. Three signatures
   flip in static_check.rs: check_var_binding_access
   plus Env read_guarded plus read_entry_guarded
   return DiagnosticResult. Bodies drop five
   anyhow::Error::new wrappers; the cluster is
   self-contained (is_declared plus guards_overlap
   plus guard_satisfiable are infallible bool, and
   unify already flipped). The one anyhow boundary
   that returns a leaf directly (check_name_read)
   uses Ok(f(...)?) since Result error types do
   not coerce on direct return; the `?` call sites
   ride the blanket conversion unchanged. Core 409
   plus integration 629 green; clippy and fmt
   clean.
   Env-gate step 2026-11-26: no new ctors, all
   wordings already pinned. One signature flips
   in static_check.rs: Checker check_env_name
   returns DiagnosticResult. Body drops one
   anyhow::Error::new wrapper; a pure leaf
   (env_declared plus step_pos are infallible, no
   fallible callees). Callers hold via the blanket
   Diagnostic-into-anyhow conversion: anyhow walk
   plus template sites use `?` unchanged, and the
   one map_err(|_| ...) site discards the error so
   the type change is invisible. Core 409 plus
   integration 629 green; clippy and fmt clean.
   Declare step 2026-11-26: no new ctors, all
   wordings already pinned. Three signatures flip
   in static_check.rs: Env declare plus
   declare_const plus declare_inner return
   DiagnosticResult. Bodies drop two
   anyhow::Error::new wrappers; the cluster is
   self-contained (guards_overlap is infallible
   bool, recursion stays inside the flipped set).
   Callers hold via the blanket
   Diagnostic-into-anyhow conversion with no
   change. Core 409 plus integration 629 green;
   clippy and fmt clean.
   Static-check bulk step 2026-11-26 (per user
   request for whole-file efficiency over
   one-leaf increments): no new ctors, all
   wordings already pinned. All remaining
   anyhow::Result signatures in static_check.rs
   flip to DiagnosticResult in one pass
   (validate_import_body_types, harvest probes,
   Checker collect plus walk plus infer plus
   call plus return plus flow families, template
   plus placeholder plus map plus RPN helpers,
   with_scope_aliases closure bound,
   validate_script_closure outcome holder).
   Bodies drop ~97 anyhow::Error::new wrappers by
   balanced-paren unwrap; the 12 surviving
   anyhow::Error::new(err) sites are inner reason
   wraps for ctors taking &anyhow::Error and stay
   per contract. Option<anyhow::Error> failure
   holders become Option<Diagnostic>. Two
   now-needless Ok(f(...)?) sites shed the
   wrapper after both sides flipped. Cross-file
   fallout in import.rs: three
   import_failed_validation sites gain the inner
   anyhow::Error::new(e) wrap since the callee
   now returns Diagnostic while the ctor takes
   &anyhow::Error. Outside callers hold via the
   blanket Diagnostic-into-anyhow conversion with
   no change. Core 409 plus integration 629
   green; clippy shows only the three
   pre-existing needless_borrow warnings; fmt
   clean.
   Closure-validation step 2026-11-26: no new
   ctors, all wordings already pinned. Two
   signatures flip in exec/mod.rs:
   validate_steps_with_fs_modules_and_entry
   plus validate_closure_for_run return
   DiagnosticResult. Bodies need no unwrap
   (all callees already DiagnosticResult:
   collect_type_aliases,
   validate_function_type_tags,
   collect_top_funcs, expand_import_closure,
   validate_script_closure, new_state). Outside
   callers hold via the blanket
   Diagnostic-into-anyhow conversion with no
   change (mcp init, finish_run, CLI dry-run,
   expect_err tests). Core 409 plus integration
   629 green; clippy and fmt clean.
   Import dry-run bulk step 2026-11-26 (per user
   file-sized batches): no new ctors, all
   wordings already pinned. Eight signatures flip
   in exec/import.rs: dry_run_closure plus
   dry_run_walk plus dry_run_load plus
   dry_run_resolve plus dry_run_walk_top_imports
   plus dry_run_walk_kind plus read_and_parse_import
   plus load_import return DiagnosticResult.
   Bodies drop outer anyhow::Error::new wrappers
   (import_cycle, import_depth_limit,
   import_cannot_read, import_bad_utf8,
   import_parse_failed keep inner
   anyhow::Error::new(e) reason wraps per
   contract); dry_run_resolve plus load_import
   shed map_err(anyhow::Error::from) after both
   sides flipped. read_import_bytes stays anyhow
   (raw io without path context; callers add it
   via import_cannot_read). Outside callers hold
   via the blanket conversion (CLI dry-run,
   import_funcs/import_file runtime paths,
   dry-run tests). Core 409 plus integration 629
   green; clippy and fmt clean.
   Own-type-harvest bulk step 2026-11-26: no new
   ctors, all wordings already pinned. Three
   signatures flip in exec/import.rs:
   resolve_own_type_harvests plus
   resolve_own_type_harvests_inner plus
   find_script_site return DiagnosticResult.
   Bodies drop two outer anyhow::Error::new
   wrappers (import_unknown_type with and without
   reason; inner anyhow::Error::new(e) reason
   wraps stay per contract). find_script_site is
   an infallible body (match plus frame walk).
   Callees already Diagnostic (load_import,
   collect_type_aliases, gate_harvested_name,
   insert_imported_type, resolve_tag); callers
   hold via the blanket conversion
   (import_file/ensure_module_init runtime paths,
   handlers call dispatch). Core 409 plus
   integration 629 green; clippy and fmt clean.
   Pipe-handle bulk step 2026-11-26 (per user
   file-sized batches): three new ctors in
   exec/diagnostics.rs with key plus wording
   pins (std_pipe_handle_spent parameterless for
   the take-twice remedy shared by all five take
   sites; std_pipe_needs_pipe(func, actual) for
   the three host needs-PIPE checks; 
   std_pipe_close_needs_script parameterless for
   the single force-close site). Sixteen
   signatures flip: io.rs spent_handle returns
   Diagnostic plus PipeRegistry
   ensure_handle/peek_pipe_content/pin_keeper/
   resolve_stdin/resolve_stdout/resolve_stderr
   plus the six ExecState wrappers return
   DiagnosticResult; steps.rs pipe_reader/
   pipe_writer/close_pipe return
   DiagnosticResult. Bodies drop bare anyhow!
   sites; take sites keep map_err shape against
   the new spent ctor; materialize/peek already
   Diagnostic so ensure/peek flips shed one
   needless Ok(?). Cross-file fallout in args.rs:
   four std_pipe_read/create_failed sites gain
   the inner anyhow::Error::new(e) wrap since the
   callee now returns Diagnostic while the ctor
   takes &anyhow::Error. resolve_assert_target
   needs no change (anyhow! formats the
   Diagnostic source inline). Outside callers
   hold via the blanket conversion (handlers io
   paths, SSH pipe bridges, test harnesses).
   Core 409 plus integration 629 green; clippy
   and fmt clean.
   Assert-lock bulk step 2026-11-26: one new
   ctor in exec/diagnostics.rs with key plus
   wording pin (std_assert_lock_poisoned(lock)
   shares one key across all three locks per the
   same-failure-mode rule; messages stay
   byte-identical). Three signatures flip in
   exec/steps.rs: pre_register_assertions plus
   sync_iteration_assert_needles plus
   cleanup_assertion_generation return
   DiagnosticResult (resolve_arg_state already
   Diagnostic; retain/insert/update_needle
   infallible). Bodies drop eight bail! sites
   for return Err with the shared ctor.
   Handlers keeps three anyhow-outer sites on
   the same key via anyhow::Error::new wrap
   (exact_stdout_bytes plus assert_contains
   stdout/stderr windows). Callers hold via the
   blanket conversion (execute_steps plus
   execute_steps_inner). Core 409 plus
   integration 629 green; clippy and fmt clean.
   Inspect/value-core bulk step 2026-11-26: no
   new ctors, reuses the single undefined-var
   owner per the same-failure-mode rule. Four
   signatures flip: args.rs
   evaluate_inspect_var plus type_of_from_value
   plus rand_float_from_value return
   DiagnosticResult (infallible bodies for the
   latter two; inspect delegates to the handler);
   handlers.rs inspect_var_map returns
   DiagnosticResult (single bail! site becomes
   return Err(diag::eval_undefined_var);
   message consolidates from "variable 'x' is
   not defined" to the pinned "undefined
   variable $x", matching load_var_binding and
   the integration expectations). Anyhow
   boundaries use Ok(f(...)?): evaluate_expr
   Inspect arm plus the two #[oxdock_func]
   macro wrappers (TYPE_OF, RAND) stay anyhow
   per the macro guard. Compiled-math Inspect
   op holds via the blanket conversion. Core
   409 plus integration 629 green; clippy and
   fmt clean.
   Coercion bulk step 2026-11-26: two new
   verbatim passthrough ctors in
   exec/diagnostics.rs with key plus wording
   pins (coerce_check_failed for shaped-type
   shape failures inside coercion;
   eval_host_return_failed for host return
   shape failures; both render {reason:#}
   inline with no with_source per contract).
   Three signatures flip in exec/args.rs:
   check_host_return_value plus coerce_value
   plus coerce_scalar return DiagnosticResult.
   Bodies drop eleven .into() conversions on
   the already-keyed mismatch arms
   (coerce_type_mismatch, string_to_pipe,
   pipe_to_string); parser shape paths map
   via the new passthroughs. Callers hold via
   the blanket conversion (state declare /
   mutate / push paths, handlers call
   dispatch, conformance lying-returns check).
   This unblocks the state declare/mutate
   cluster next. Core 409 plus integration 629
   green; clippy and fmt clean.
   State declare/mutate bulk step 2026-11-26:
   ten new ctors in exec/diagnostics.rs with
   key plus wording pins (eval_scope_underflow
   parameterless; eval_redeclare_var /
   eval_redeclare_const with key; 
   eval_no_var_scope parameterless shared by
   both declare paths; eval_const_mutation
   single wording owner with CONST from the
   shared constant; eval_undeclared_declare
   with key plus expected shape covering both
   TYPE and LIST hints; eval_undeclared bare;
   eval_list_not_list with key plus actual;
   eval_list_value_not_list shared by both
   stores; eval_list_element_failed verbatim
   passthrough). Six signatures flip in
   exec/state.rs: pop_scope plus declare_var
   plus declare_const plus declare_binding plus
   mutate_var plus push_into_list return
   DiagnosticResult (coerce already Diagnostic;
   check_value_at maps via the new
   passthrough). const_mutation_error keeps
   its anyhow return for the func-macro
   boundary but delegates to
   eval_const_mutation, so one wording owner
   serves both. Messages stay byte-identical.
   Cross-file fallout: seven pop_scope tuple-
   match holders gain explicit
   map_err(anyhow::Error::from) (five in
   handlers, one in import, one in steps)
   since Diagnostic cannot mix with anyhow in
   one tuple. Callers otherwise hold via the
   blanket conversion. Core 409 plus
   integration 629 green; clippy and fmt
   clean.
   State tails bulk step 2026-11-26: closes
   state.rs, two new ctors in
   exec/diagnostics.rs with key plus wording
   pins (eval_scratch_materialize_failed
   verbatim passthrough shared by both spawn
   choke points; eval_task_cap_exceeded
   taking the configured maximum from the
   shared constant at the call site). Three
   signatures flip: state.rs command_ctx plus
   alloc_task_id plus steps.rs
   ensure_scratch_materialized return
   DiagnosticResult. command_ctx reuses the
   existing eval_workdir_resolve_failed for
   the resolve leg; the scratch leg maps via
   the new passthrough; the cap maps via the
   new max-taking ctor (test expectation on
   the concurrency prefix still holds).
   Drops the now-unused anyhow::Result import
   from state.rs (const_mutation_error keeps
   fully-qualified anyhow::Error for the
   macro boundary). All callers (handlers
   spawn paths, SSH bridges, alloc tests)
   hold via ? and expect. Core 409 plus
   integration 629 green; clippy and fmt
   clean.
   Mirror/eager bulk step 2026-11-26: no new
   ctors, all wordings already pinned. Five
   signatures flip in exec/args.rs:
   TextPump::apply_str plus mirror_string plus
   mirror_stream plus read_pipe_eager plus
   write_completed_pipe return
   DiagnosticResult. Bodies drop outer
   anyhow::Error::from and .into() wrappers
   on the already-keyed arms; pipe
   reader/writer legs keep the inner
   anyhow::Error::new(err) reason wraps since
   those ctors take &anyhow::Error while the
   callees now return Diagnostic. Pump-thread
   internals (SpillBuffer push/flush/write/
   drain, push_case/trim/prefix/suffix/
   replace) deliberately stay anyhow: the
   pump body discriminates clean
   downstream-close via downcast_ref::<
   PumpDownstreamClosed>, which has no
   Diagnostic equivalent. Thirteen
   #[oxdock_func] macro tails in native.rs
   gain the Ok(f(...)?) firewall; the one ?
   caller (read_pipe_eager in replace_regex)
   holds. Callers outside native hold via the
   blanket conversion. Core 409 plus
   integration 629 green; clippy and fmt
   clean.
   Const-ref bulk step 2026-11-26: unblocks
   the template/arg cluster. One new ctor
   (import_const_forward_failed verbatim
   passthrough of the ForwardError Display,
   which stays the single wording owner for
   unknown/broken/split/cycle/ambiguous
   hops; no wrapper, no source) with key
   plus wording pins. Two signatures flip:
   import.rs resolve_const_ref plus args.rs
   resolve_const_call_arg return
   DiagnosticResult. resolve_const_ref drops
   all anyhow::Error::new wrappers; the
   chase_const_forward leg maps via the new
   ctor keeping the inner anyhow::Error::new
   (err) reason wrap since the ctor takes
   &anyhow::Error. All four
   resolve_const_ref callers (evaluate_expr,
   resolve_const_call_arg, compiled-math
   LoadConstRef, expand_string placeholder)
   are still anyhow and hold via the blanket
   conversion with zero edits except one
   Ok(? ) firewall on the evaluate_expr
   ConstRef match tail (other arms already
   use that shape). Core 409 plus
   integration 629 green; clippy and fmt
   clean.
   Template/arg cluster bulk step 2026-11-26:
   eight signatures flip in exec/args.rs
   (expand_string, evaluate_expr,
   resolve_arg, resolve_arg_opt,
   evaluate_assert_operand,
   resolve_overrides, evaluate_compiled_math,
   expand_dsl_vars) plus two holder fixes in
   already-Diagnostic fns. One new ctor
   (eval_dispatch_failed verbatim fallback)
   with key plus wording pins. The
   anyhow-to-Diagnostic direction gets one
   owner: lift_dispatch_error downcasts
   keyed diagnostics riding in the chain so
   placeholder-unknown listings and handler
   errors keep their keys, and wraps only
   the rare unkeyed tail verbatim; all six
   dispatch sites (placeholder resolve,
   block/call tails, RPN
   resolve/gate/execute) route here. Bodies
   drop all anyhow::Error::from/new wraps.
   The two reverse-direction holders
   (resolve_arg_state expand legs,
   resolve_arg_as_int resolve leg) gain the
   inner anyhow::Error::from(err) wrap since
   those ctors take &anyhow::Error. One
   needless Ok(? ) shed at the compiled-math
   tail. Deliberately held: resolve_arg_as_
   duration (parse_duration needs
   parser-side duration keys, separate
   slice) and the pump-thread internals
   (downcast discrimination, permanent).
   New lift round-trip test pins key
   preservation plus the verbatim fallback.
   Core 410 plus integration 629 green;
   clippy and fmt clean.
   Step helper-cluster bulk step 2026-11-26:
   five new dsl.eval.* ctors in
   exec/diagnostics.rs with key plus wording
   pins (pipe_assertion_read_failed verbatim
   passthrough, with_io_unexpanded
   parameterless, async_cancelled
   parameterless, async_exit_status with
   task Option<u64> plus platform
   ExitStatus, flow_crosses_remote with step
   plus word). Two signatures flip in
   exec/steps.rs (resolve_assert_target,
   restore_scopes) to DiagnosticResult; the
   WITH_IO arm plus ASYNC cancelled plus
   reaper plus named-task plus REMOTE
   crossing arms re-key through the new
   ctors with anyhow::Error::new inner wraps
   where the ctor takes &anyhow::Error. The
   three REMOTE call-site words ride
   KEYWORD_BREAK/CONTINUE/RETURN (constants
   sweep, bytes identical). Clippy
   needless_return shed on the three REMOTE
   tails. Core 410 plus integration 629
   green; clippy and fmt clean.
   Constants sweep bulk step 2026-11-26 (per
   user whole-codebase direction): every
   user-facing message template outside
   oxdock-fs interpolates the shared
   KEYWORD_*/FUNC_*/*_TYPE_NAME constants
   instead of restating spellings; bytes
   stay identical (constants render the
   same text) and pinning tests hold.
   Touched: core exec/diagnostics.rs (95
   templates plus VOID import),
   steps/import/state/static_check
   call-site kinds and spellings,
   handlers/remote/mod/native message
   sites, exit.rs Display, parser
   diagnostics/commands/command/tag/
   literal/ast/parser messages and
   renderers, mcp plus process plus net
   plus ssh plus markdown diagnostics,
   net remote/session.rs contexts.
   Mechanical guards: plain "..."
   .to_string() sites become format! with
   brace escaping; .context("...") sites
   become .with_context(|| format!(...));
   eprintln DEBUG, option specs,
   #[oxdock_*] attrs, docs:/summary:/
   syntax:/description: (all &'static
   str, uninterpolatable), internal
   .expect(...) panics, test pins and
   scripts, and plugin registry names
   (no shared FUNC_ owner) stay literal.
   with_io_syntax/with_io_bindings become
   fns (concat! rejects const refs).
   Deliberate exception: oxdock-fs keeps
   literals (adding oxdock-parser there is
   a dependency cycle: parser -> pipe ->
   fs). Pre-existing failures unchanged
   (ssh escaping_key_path plus
   non_map_serve_options fail on clean
   tree; snapshot_hash_parity green after
   the sweep). Core 410 plus integration
   629, parser 267 plus 303, net 38,
   process 121, fs 71 green; clippy at
   baseline, fmt clean on all touched
   files.
   Stdin failure-cluster bulk step 2026-11-26
   (file-sized batch): two new dsl.eval.*
   ctors with key plus wording pins
   (eval_stdin_lock_failed with func field
   covers all 7 poison sites: READ,
   READ_LINE, WRITE, APPEND, EXPAND plus
   the two verb-generic assert drains;
   eval_stdin_read_failed with optional
   func plus io reason covers all 7 read
   sites, mirroring the
   eval_async_exit_status Option-field
   precedent: bare READ/READ_LINE/EXPAND
   paths pass None, WRITE/APPEND/drain/
   assert paths pass Some(verb)). All 14
   sites re-keyed through anyhow::Error::new
   shims (enclosing fns stay anyhow::Result
   until the execute_steps chain flips;
   matches the std_assert_lock_poisoned
   precedent). Io ctors render {reason}
   inline with no with_source (text
   inlined per contract); lock ctor takes
   no source (poison flag carries no
   detail). Bytes identical (no test pins
   the strings; anyhow chain Display
   matches the inlined rendering). Core
   410 plus integration 629 green; clippy
   at baseline (core 3 pre-existing),
   fmt clean on both touched files.
   Exact-buffer trio bulk step 2026-11-26
   (file-sized batch): three new dsl.eval.*
   ctors with key plus wording pins
   (eval_assert_stdin_overflow covers the
   drain_stdin_bounded cap breach,
   eval_assert_stdout_overflow covers the
   overflowed generation entry,
   eval_assert_no_exact_capture covers the
   missing generation entry; each takes
   the invoking verb as its only field, no
   source). All 3 sites re-keyed through
   anyhow::Error::new shims (enclosing fns
   stay anyhow::Result). The drain if-block
   keeps explicit return Err (non-tail
   position must diverge); the match arms
   use bare Err tails (no needless_return).
   Bytes identical. Core 410 plus
   integration 629 green; clippy at
   baseline, fmt clean on both touched
   files.
   Assert-equality bulk step 2026-11-26
   (file-sized batch): thirteen new
   dsl.eval.assert_* ctors with key plus
   wording pins (hash_mismatch, hash_match,
   hash_needs_string, stream_mismatch for
   Option-Value expected, stream_mismatch_text
   for str expected, stream_unexpected_equal,
   stderr_unsupported via shared
   KEYWORD_ASSERT_CONTAINS, requires_expected,
   value_mismatch, value_unexpected_equal,
   pipe_mismatch for Option-Value expected,
   pipe_mismatch_text for str expected,
   pipe_unexpected_equal; verb is the invoker
   field throughout, Debug rendering lives in
   the ctors). All 19 bail sites in
   assert_equality re-keyed through
   anyhow::Error::new shims (enclosing fn
   stays anyhow::Result); the two pipe
   UTF-8 with_context sites deliberately held
   for a later io-ctor batch. Let-else and
   if-statement positions keep explicit
   return Err (must diverge); the Stderr tail
   arm uses bare Err (avoids needless_return);
   fmt normalized the shared hash indent.
   Bytes identical (integration only checks
   substrings). Core 410 plus integration 629
   green; clippy at baseline (3 pre-existing
   static_check needless_borrow), fmt clean
   on both touched files.
   Assert-contains bulk step 2026-11-26
   (file-sized batch): six new
   dsl.eval.assert_contains_* ctors with key
   plus wording pins (str_mismatch for string
   haystacks, value_mismatch for list/whole
   values, missing for map-key and absent
   windows, needs_haystack for bad haystack
   types, emitted_mismatch for window detail
   with trim_end living in the ctor,
   pipe_mismatch for pipe buffers; verb is
   the invoker field throughout, Debug
   rendering lives in the ctors). All 10
   bail sites in assert_contains re-keyed:
   early misses keep explicit return Err
   (must diverge past later checks), the
   Value-tail needs_haystack and all window
   match arms plus the pipe if-else use bare
   Err tails (avoids needless_return). The
   pipe UTF-8 with_context site deliberately
   held for a later io-ctor batch. Fixed a
   new unnecessary_to_owned (emitted field
   takes trimmed directly). Bytes identical
   (integration only checks substrings).
   Core 410 plus integration 629 green;
   clippy back at baseline (3 pre-existing),
   fmt clean on both touched files.
   Loop-shape bulk step 2026-11-26
   (file-sized batch): four new
   dsl.eval.* ctors with key plus wording
   pins (break_outside_loop and
   continue_outside_loop parameterless;
   capture_flow_outside_loop with step plus
   word covering BREAK/CONTINUE, mirroring
   check_flow_outside_loop; capture_return_
   outside_function with step). Eight sites
   re-keyed: top_level_flow trio reuses the
   existing check_* ctors (no new keys,
   single wording ownership, same-crate
   key sharing per the std_assert_lock_
   poisoned precedent); capture trio uses
   explicit return Err (match is a statement
   before drain, must diverge); top_level
   trio and bare dispatch pair use bare Err
   tails (match/fn tail, avoids
   needless_return). Bytes identical. Core
   410 plus integration 629 green; clippy at
   baseline (3 pre-existing), fmt clean on
   both touched files.
   RUN transport bulk step 2026-11-26
   (file-sized batch): nine new dsl.eval.run_*
   ctors with key plus wording pins
   (run_cancelled with command covering shell
   cmd plus pty what; run_argv_cancelled with
   argv debug; run_unexpected_capture and
   run_argv_unexpected_capture; run_exit_status
   and run_argv_exit_status with platform
   ExitStatus Display, pins use starts_with;
   run_needs_program; run_pty_exit_status with
   numeric code; run_exec_requires_arg
   parameterless). Thirteen sites re-keyed
   across run/run_argv/run_pty/resolve_run_exec_
   argv: pre/post-spawn cancellation and exit
   checks keep explicit return Err (statement
   positions must diverge); Captured arms and
   Completed/Cancelled match tails use bare
   or return Err per tail versus statement
   shape, avoiding needless_return. Spawn
   with_context trio deliberately held for a
   later io-ctor batch; exec-form element
   type errors and SLEEP held. Bytes
   identical. Core 410 plus integration 629
   green; clippy at baseline (3 pre-existing),
   fmt clean on both touched files.
   Pipe plus spawn transport bulk step
   2026-11-26 (file-sized batch): three new
   dsl.eval.* ctors with key plus wording
   pins (pipe_not_utf8 with verb plus
   FromUtf8Error covering ASSERT equality and
   ASSERT_CONTAINS, one key mirroring
   check_flow_outside_loop; run_spawn_failed
   with command covering shell cmd plus pty
   what, mirroring eval_run_cancelled;
   run_argv_spawn_failed with argv debug).
   Spawn ctors take &anyhow::Error passthrough
   (outer-context RUN prefix plus {reason:#}
   inline, no with_source per contract) because
   spawn_command/spawn_argv/run_pty_session
   return anyhow Result; pipe ctor takes
   &FromUtf8Error concrete (text inlined, no
   source, per pipe/fs precedent). Five sites
   re-keyed via map_err plus anyhow Error new
   (spawn trio drops with_context; pipe pair
   drops with_context for direct FromUtf8
   mapping). Bytes identical (context colon
   plus source rendering preserved). Core 410
   plus integration 629 green; clippy at
   baseline (3 pre-existing), fmt clean on
   both touched files.
   Exec-form plus SLEEP bulk step 2026-11-26
   (file-sized batch): five new dsl.eval.*
   ctors with key plus wording pins
   (run_exec_pipe_arg, run_exec_semaphore_arg,
   and run_exec_permit_arg parameterless;
   run_exec_element_needs_string with got
   covering map plus task handle task#N, one
   key mirroring check_flow_outside_loop;
   sleep_cancelled parameterless). Six sites
   re-keyed across flatten_exec_value plus
   sleep: pipe/semaphore/permit bails become
   bare Err tails (fn tail, avoids needless_
   return); map plus handle use explicit return
   Err with got string (map literal versus
   formatted task handle); SLEEP keeps explicit
   return Err (loop statement must diverge).
   Unused KEYWORD_SLEEP plus
   PERMIT/SEMAPHORE imports dropped from
   handlers (messages now owned downstream).
   Bytes identical. Core 410 plus integration
   629 green; clippy at baseline (3
   pre-existing), fmt clean on both touched
   files.
   COPY transport bulk step 2026-11-26
   (file-sized batch): four new dsl.eval.*
   ctors with key plus wording pins
   (copy_failed with from plus to covering all
   five resolve/place/copy sites, one key;
   copy_git_failed with rev plus from plus to
   covering both sites; copy_no_file_name with
   display for the Option None arm;
   copy_destination_escapes with display plus
   anyhow reason for the join guard). Nine
   sites re-keyed across copy/copy_git/
   place_file_in_dir via map_err plus anyhow
   Error new (bare tail for the join guard,
   no trailing question-mark). The join guard
   keeps its join error text (outer-context
   colon plus source rendering preserved);
   the no-file-name arm stays source-free
   (Option context, no underlying error).
   Unused KEYWORD_COPY plus KEYWORD_COPY_GIT
   imports dropped from handlers. Bytes
   identical. Core 410 plus integration 629
   green; clippy at baseline (3 pre-existing),
   fmt clean on both touched files.
   SYMLINK transport bulk step 2026-11-26
   (file-sized batch): two new dsl.eval.*
   ctors with key plus wording pins
   (symlink_failed with from plus to covering
   all six resolve/place/link sites, one key;
   symlink_no_file_name with from for the
   Option None arm). Seven sites re-keyed
   across symlink/link_base_name via map_err
   plus anyhow Error new (link_base_name uses
   ok_or_else, source-free like the COPY
   no-file-name arm). Chain finding: the
   snapshot renderer (error_with_snapshot)
   builds step N (chain[1]) plus causes from
   chain layers, and
   symlink_errors_report_underlying_cause pins
   step 2 (SYMLINK taken.txt taken.txt) with
   the cause under causes. Inlining the reason
   into the message flattens the chain and
   moves the paren, so symlink_failed keeps
   the bare outer-context message and carries
   the failure as source. anyhow has no std
   Error impl, so the full-chain text rides an
   Other-kind io error, collapsing deeper
   layers the same way the process io ctors
   collapse to kind plus text. Contract point
   5 compliant (no interpolation, source
   attached). Known inconsistency flagged for
   a later normalization pass: copy_failed,
   copy_git_failed, run_spawn_failed, and
   run_argv_spawn_failed inline {reason:#}
   with no source, which flattens snapshot
   chains the same way; no test pins their
   rendering, so they stay as-is in this
   batch. Unused KEYWORD_SYMLINK import
   dropped from handlers. Bytes identical
   (snapshot output verified via the pinning
   test). Core 410 plus integration 629 green;
   clippy at baseline (3 pre-existing), fmt
   clean on both touched files.
   WORKDIR/MKDIR/SET_EXECUTABLE/LS bulk step
   2026-11-26 (file-sized batch): six new
   dsl.eval.* ctors with key plus wording pins
   (workdir_failed with path; mkdir_resolve_failed
   with path plus mkdir_create_failed with display
   keeping the lowercase failed to create dir
   wording byte-identical; set_executable_failed
   with path covering both resolve plus chmod
   sites, one key; ls_resolve_failed with
   Option path covering the explicit plus bare
   forms, one key; ls_read_failed with display).
   Eight sites re-keyed across workdir/mkdir/
   set_executable/ls via map_err plus anyhow
   Error new. All six keep the bare outer-context
   message plus chained source (Other-kind io
   error carrying the full-chain text), matching
   the symlink_failed chain shape so the snapshot
   renderer keeps step N (chain[1]) plus causes.
   Key-pin list carries ls_resolve_failed once
   (Some form); the None branch shares the key,
   mirroring stdin_read_failed/async_exit_status.
   Unused KEYWORD_MKDIR plus KEYWORD_SET_EXECUTABLE
   plus KEYWORD_WORKDIR imports dropped from
   handlers. Bytes identical. Core 410 plus
   integration 629 green; clippy at baseline (3
   pre-existing), fmt clean on both touched files.
   READ/READ_LINE bulk step 2026-11-26
   (file-sized batch): seven new dsl.eval.*
   ctors with key plus wording pins
   (read_resolve_failed with path;
   read_open_failed with display keeping the
   lowercase failed to open wording
   byte-identical; read_file_failed plus
   read_write_failed taking &io::Error with
   static wordings living in the ctors, one key
   each covering both branches; read_needs_stdin
   plus read_line_needs_stdin parameterless with
   shared keyword constants; read_line_not_utf8
   taking &FromUtf8Error). Eight sites re-keyed
   across read/read_line via map_err plus anyhow
   Error new (stdin-missing bails become explicit
   return Err, utf8 uses map_err on from_utf8).
   All failure ctors keep the bare outer-context
   message plus chained source (Other-kind io
   error), matching the symlink chain shape; the
   missing-file chain test (failed to open plus
   leaf under causes) and the requires-stdin pin
   both pass. Message pins use literal
   READ/WITH_IO/READ_LINE spellings (test pins
   out of scope for the constants sweep); ctor
   bodies use the shared constants. Bytes
   identical. Core 410 plus integration 629
   green; clippy at baseline (3 pre-existing),
   fmt clean on both touched files.
   WRITE bulk step 2026-11-26 (file-sized batch):
   six new dsl.eval.* ctors with key plus wording
   pins (write_resolve_failed with path;
   ensure_parent_failed with display, one key
   designed to cover APPEND plus APPEND_LINE
   later since the outer text names no keyword;
   write_file_failed with display;
   write_needs_stdin with path, no source;
   write_open_failed with display for writing;
   write_stream_failed with display for the
   streamed chunk). Six sites re-keyed across
   write via map_err plus anyhow Error new
   (stdin-missing bail becomes explicit return
   Err). All failure ctors keep the bare
   outer-context message plus chained source
   (Other-kind io error). Type note: the
   streamed write_all yields std io Error, so
   that site wraps with anyhow Error new at
   the call site for the ref-anyhow-Error param,
   per the interop rule. Bytes identical. Core
   410 plus integration 629 green; clippy at
   baseline (3 pre-existing), fmt clean on both
   touched files.
   APPEND bulk step 2026-11-26 (file-sized batch):
   four new dsl.eval.* ctors with key plus wording
   pins (append_resolve_failed with path;
   append_failed with display, one key covering
   both the whole-file and streamed-chunk sites
   since the outer text is identical, mirroring
   set_executable_failed; append_needs_stdin with
   path, no source; append_open_failed with display
   for appending). Six sites re-keyed across append
   via map_err plus anyhow Error new (stdin-missing
   bail becomes explicit return Err). The parent-dir
   site reuses ensure_parent_failed from the WRITE
   batch, single ownership holding. The streamed
   write_all site wraps its std io Error with anyhow
   Error new at the call site. All failure ctors keep
   the bare outer-context message plus chained source.
   Bytes identical. Core 410 plus integration 629
   green; clippy at baseline (3 pre-existing), fmt
   clean on both touched files.
   APPEND_LINE/REMOVE bulk step 2026-11-27
   (file-sized batch): six new dsl.eval.* ctors
   with key plus wording pins
   (append_line_resolve_failed with path; the
   append_line write plus parent sites reuse
   eval_append_failed plus eval_ensure_parent_failed;
   remove_failed with path, one key covering all four
   transport sites with identical outer text;
   remove_protected, remove_missing, and
   remove_link_changed with path and no source for the
   fail-closed validation arms; remove_not_empty_failed
   with path for the recursive hint). Eleven sites
   re-keyed across append_line/remove via map_err plus
   anyhow Error new (non-tail bails become explicit
   return Err; tail-position match arms use bare Err
   tails for needless_return, keeping clippy at
   baseline). Unused KEYWORD_APPEND_LINE plus
   KEYWORD_REMOVE imports dropped from handlers.
   Bytes identical (workspace-root guard pin passes).
   Core 410 plus integration 629 green; clippy at
   baseline (3 pre-existing), fmt clean on both
   touched files.
   EXPAND bulk step 2026-11-27 (file-sized batch):
   four new dsl.eval.* ctors with key plus wording pins
   (expand_resolve_failed with path;
   expand_read_failed with display; expand_write_failed
   taking ref-io-Error with the static wording living in
   the ctor, one key covering both streaming loops plus
   the final flush; expand_needs_stdin parameterless with
   shared keyword constants). Seven sites re-keyed across
   replace/expand via map_err plus anyhow Error new
   (stdin-missing bail becomes explicit return Err).
   The file-open site reuses eval_read_open_failed,
   single ownership holding for the shared failed to open
   wording. The chunk-read site wraps its std io Error
   with anyhow Error new at the call site. Bytes identical.
   Core 410 plus integration 629 green; clippy at baseline
   (3 pre-existing), fmt clean on both touched files.
   INSTALL/PROMPT bulk step 2026-11-27 (file-sized batch):
   six new dsl.eval.* ctors with key plus wording pins
   (install_failed with from plus to, one key covering
   all three resolve plus atomic-install sites;
   prompt_no_answer plus prompt_eof with message and no
   source for the pre-I/O guards; prompt_write_failed,
   prompt_flush_failed, and prompt_read_failed with
   message plus ref-io-Error for the stderr and stdin
   failures). Eight sites re-keyed across install/prompt
   via map_err plus anyhow Error new (non-tail bails
   become explicit return Err). Unused KEYWORD_INSTALL
   import dropped from handlers; KEYWORD_PROMPT stays
   for the bind_prompt_var sites in a later batch.
   Bytes identical (non-TTY pin passes). Core 410 plus
   integration 629 green; clippy at baseline
   (3 pre-existing), fmt clean on both touched files.
   PROMPT-bind/LIST_APPEND bulk step 2026-11-27
   (file-sized batch): two new dsl.eval.* ctors with key
   plus wording pins (prompt_bind_failed with var, one key
   covering the mutate plus declare arms; list_append_failed
   with var). Three sites re-keyed across bind_prompt_var
   plus push_into via map_err plus anyhow Error new.
   Type note: mutate_var, declare_var, and push_into_list
   return DiagnosticResult, so the sites convert with
   anyhow Error from at the call site for the ref-anyhow
   param, per the args.rs precedent. PROMPT is now fully
   migrated: unused KEYWORD_PROMPT plus KEYWORD_LIST_APPEND
   imports dropped from handlers. Bytes identical. Core 410
   plus integration 629 green; clippy at baseline
   (3 pre-existing), fmt clean on both touched files.
   AMEND 2026-11-27 (review-directed, SEV-2): eval_prompt_bind_failed
   plus eval_list_append_failed now take the inner Diagnostic by
   value instead of ref-anyhow-Error. The reason field carries the
   inner message and the concrete diagnostic rides as the chained
   source with key, fields, and chain intact (no anyhow round-trip,
   no Other-kind flattening). Call sites pass e directly. First
   by-value precedent in the exec diagnostics; applies wherever the
   callee already returns DiagnosticResult. Gates stay green.
   WITH_IO validation bulk step 2026-11-27 (file-sized batch):
   five new dsl.eval.* ctors with key plus wording pins
   (geometry_needs_tty plus tty_no_stderr plus tty_needs_run
   parameterless; duplicate_stream with stream, one key covering
   all six stdin/stdout/stderr/tty/rows/cols sites;
   pipe_handle_mismatch with actual plus Value, one key covering
   both match arms with Debug rendering in the ctor). Thirteen
   sites re-keyed: with_io_block reuses eval_with_io_unexpanded;
   resolve_pipe_handle None arm reuses eval_undeclared(var).
   Unused PIPE_TYPE_NAME import dropped from handlers. Bytes
   identical (only contains pins on tty and duplicate-stream
   wordings). Tail discipline: bare Err tails where the match is
   the tail value, explicit return Err in diverging if/for arms.
   Core 410 plus integration 629 green; clippy at baseline
   (3 pre-existing), fmt clean on both touched files.
   Call/loop dispatch bulk step 2026-11-27 (file-sized batch):
   thirteen new dsl.eval.* ctors with key plus wording pins.
   FOR trio: for_needs_key_value parameterless, for_map_key_type
   with actual, for_needs_list_or_map with Value and Debug in the
   ctor. Gate trio: call_recursion_limit with base name,
   call_arity_exact plus call_arity_range with counts.
   Resolve pair: call_unknown with name plus hint (one key for the
   registry plus script-site arms), call_unknown_rpn separate key
   for the backtick-less RPN spelling. Return-tag:
   call_return_tag_failed takes the Diagnostic by value and inlines
   its text flat with no source (the original anyhow bang built a
   flat string, so the chain stays flat and no anyhow round-trip
   is needed). Boundary: call_missing_param with param plus base,
   call_zero_value_failed takes the Diagnostic by value with bare
   outer plus chained source (the original with_context was
   already two layers, so chain depth is unchanged),
   call_flow_crosses_func with step plus word plus func covering
   BREAK and CONTINUE. Wrapper: call_failed with ref-anyhow-Error
   and bare outer plus chained source (matching
   eval_symlink_failed, not the copy_failed inline shape, per
   review). Fifteen sites re-keyed across for_loop,
   build_script_entry, resolve_call_entry, gate_call, and
   execute_call_entry. Unused KEYWORD_FOR, KEYWORD_FUNC,
   KEYWORD_TYPE, MAP_TYPE_NAME, and STRING_TYPE_NAME imports
   dropped from handlers. Bytes identical (only contains pins on
   arity, unknown-function, recursion, and boundary wordings).
   Core 410 plus integration 629 green; clippy at baseline
   (3 pre-existing), fmt clean on both touched files.
   AMEND 2026-11-27 (review-directed, SEV-2): eval_call_failed
   corrected to bare outer plus chained source; entry moved below
   WITH_IO so PROMPT-bind stays adjacent to its AMEND. Gates stay
   green.
   Archive/unarchive bulk step 2026-11-27 (file-sized batch):
   twelve new dsl.eval.* ctors with key plus wording pins.
   Pack side: archive_failed bare outer plus chained source (one
   key for every fallible op under the pack path), no-file-name
   with display, refuses-symlink-source and refuses-symlink with
   Debug in ctor, destination-is-source and destination-inside
   guards parameterless over spellings. Unpack side:
   unarchive_failed bare outer plus source, member_failed bare
   member outer plus source (one key for the whole apply
   callback, Diagnostic sanitize inners ride via anyhow
   round-trip per the func_param_type precedent),
   bad-max-bytes with raw plus source, expansion-cap with cap
   plus path, refuses-symlink with path plus target,
   restore-mode-failed with source. resolve_archive_src now takes
   (src, dst) and append_archive_tree takes (src, dst) instead of
   a preformatted tag; unpack closure drops entry_tag and captures
   spellings. Forty-plus sites re-keyed across resolve, collect,
   append, archive, and unarchive. Bytes identical (tests pin via
   err.chain() needles: cannot be the source itself, cannot
   overwrite inside, expansion cap, refuses to pack symlink,
   refuses symlink member). Unused KEYWORD_ARCHIVE and
   KEYWORD_UNARCHIVE imports dropped from handlers. Core lib 410
   plus integration 629 green; clippy at baseline
   (3 pre-existing), fmt clean on both touched files.
   Flow plus capture bulk step 2026-11-27 (file-sized batch):
   six new dsl.eval.* ctors with key plus wording pins plus one
   reuse. Block pair: block_flow_crosses_block with step plus
   word (one key for BREAK and CONTINUE escaping an inline
   block value). Async quartet: async_flow_crosses_async with
   step plus word (one key for BREAK, CONTINUE, and RETURN
   across both spawn sites: the task closure and the thread
   worker), entry_unavailable parameterless (the original site
   discards the channel error), plus while_loop cancel reuses
   async_cancelled. Capture pair: capture_with_io_stdout with kw
   (one key for the IMPORT and CALL stdout-pipe arms),
   capture_not_utf8 with kw plus var plus by-value Diagnostic
   (bare outer plus chained source; the original flat anyhow
   over a Diagnostic was already two layers). TTY pair:
   with_io_tty_live_pipe with stream (one key for stdin and
   stdout live-OS-pipe guards). Fourteen sites re-keyed across
   block_value, while_loop, assign_capture, both async spawns,
   and the tty resolver. Bytes identical (only contains pins on
   block-boundary wording). Unused anyhow macro plus
   KEYWORD_ASYNC and KEYWORD_WITH_IO imports dropped from
   handlers. Tail discipline: bare Err tails in match-value
   positions (needless_return fix keeps the no-new-warning
   gate). Core lib 410 plus integration 629 green; clippy at
   baseline (3 pre-existing), fmt clean on both touched files.
   Await plus cancel plus timeout plus isolated bulk step
   2026-11-27 (file-sized batch): fourteen new dsl.eval.* ctors
   with key plus wording pins. Task trio: task_var_undefined with
   var (one key for single-AWAIT, list-AWAIT, and CANCEL
   lookups), task_not_handle with var (one key for AWAIT and
   CANCEL resolvers), task_not_found with label (one key for the
   id lookup and the double-await guard; the guard passes
   sigil_var(var) so both render one `$`). AWAIT quintet:
   await_cancelled with var (one key for claim-time and
   poll-loop observations), await_member_not_handle with label
   plus got, await_timeout with var, await_exit_status with var
   plus platform status (prefix-pinned like async_exit_status),
   await_fell_off with kw plus out plus task. CANCEL pair:
   cancel_gone with var (one key for missing-entry and
   completed arms), cancel_already with var. TIMEOUT plus
   ISOLATED trio: timeout_deadline with budget (bare diagnostic
   on the success path, typed Diagnostic as the with_context
   layer over the body error on the failure path, so the key,
   the fields, and the original chain depth are all preserved),
   inside_isolated parameterless (same typed-layer chain trick;
   SEV-2 amend: an earlier draft stringified the diagnostic
   with `.to_string()`, which would have discarded the key and
   fields into an untyped String layer), isolated_outside_allowlist parameterless (runtime
   twin of check_isolated_allowlist, which keeps its static step
   prefix). Placeholder: placeholder_miss_text verbatim &str
   passthrough with no source (quoting lives in the describe
   renderer). Twenty-one sites re-keyed across resolve, await,
   capture, cancel, deadline, isolated, and EXPAND placeholder
   paths. Bytes identical (contains pins on task-handle,
   failed-with-status, and fell-off wordings). Dropped the now-
   unused anyhow bail macro plus KEYWORD_AWAIT, KEYWORD_CANCEL,
   KEYWORD_ISOLATED, and KEYWORD_TIMEOUT imports from handlers;
   Context stays (both with_context sites keep the anyhow
   chain). Core lib 410 plus integration 629 green; clippy at
   baseline (3 pre-existing), fmt clean on both touched files.
   REMOTE bulk step 2026-11-27 (file-sized batch): fourteen new
   dsl.eval.* ctors with key plus wording pins. Guest guard:
   remote_copy_outside_guest parameterless. Transfer: rewind_failed
   with io error inline, fetch_missing with rel plus
   Option-cause (one key for the fast-fail and probe-cause arms),
   fetch_entry_cap with rel plus cap (call site passes
   MAX_TRANSFER_ENTRIES, mirroring unarchive_expansion_cap),
   fetch_no_file_name with rel (Option context becomes
   ok_or_else). Header: remote_unknown_var and remote_unknown_env
   (runtime twins of the check ctors, which keep backticks and
   step prefixes), unrenderable_var with name plus type plus
   target (typed Diagnostic with_context layer over the renderer
   error, never stringified). Session: no_session with target,
   exit_status with target plus code and transport_failed with
   target (both typed Diagnostic context layers over runner
   errors, per the eval_inside_isolated precedent). Push:
   push_destination_escapes plus push_target_outside (one key
   for the absolute, nested, and bare shapings) with Debug path
   plus inline reason, push_symlink_over_dir with path.
   Seventeen sites re-keyed across copy_transfer, resolve_header,
   resolve_fetch_entries, dispatch_remote, and apply_push_symlink.
   Bytes identical (contains pins on fetch-source-missing and
   transport-failed plus unexpected-EOF chain). Dropped the now-
   unused anyhow bail macro plus KEYWORD_COPY, KEYWORD_REMOTE,
   and KEYWORD_WITH_IO imports from remote; Context stays (three
   typed-layer sites). Core lib 410 plus integration 629 green
   (including 19 remote); clippy at baseline (3 pre-existing),
   fmt clean on all three touched files.
   write-live-pipe bulk 2026-11-27 (file-sized batch): one new
   dsl.eval.write_live_pipe ctor with key plus both-stream pins.
   One key covers write_stdout and write_stderr (message stays
   stream-free for identical bytes; stream rides as a structured
   field), mirroring eval_with_io_tty_live_pipe. Two sites
   re-keyed; bail import dropped from io. TaskTee Write keeps its
   io::Error::other spelling (io::Result boundary, same
   create_os_pipe precedent). Core lib 410 plus integration 629
   green; clippy at baseline (3 pre-existing), fmt clean on both
   touched files.
   finish-run flow bulk 2026-11-27 (file-sized batch): zero new
   ctors. The three finish_run bare bail!s (BREAK/CONTINUE-outside,
   RETURN-outside) re-key to the existing check_flow_outside_loop
   (BREAK/CONTINUE as the word) and check_return_outside ctors,
   byte-identical via the shared KEYWORD constants, same as the
   handlers.rs top_level_flow precedent. Dropped the now-unused
   KEYWORD_ASYNC/KEYWORD_LET/KEYWORD_RETURN imports from mod
   (BREAK/CONTINUE stay). Core lib 410 plus integration 629
   green; clippy at baseline (3 pre-existing), fmt clean.
   text-pump bulk 2026-11-27 (file-sized batch): five new
   dsl.std.pump_* ctors with key plus wording pins. The four
   not-UTF-8 sites share parameterless std_pump_not_utf8; writer
   lock is parameterless std_pump_writer_lock_poisoned; write,
   spill-drain, and spill failures take &std::io::Error inline
   with no source (BrokenPipe keeps its typed
   PumpDownstreamClosed stop signal). Eight sites re-keyed
   across flush/push_case/push_trim/write/drain_into (4 UTF-8,
   1 lock poison, 1 write, 1 spill-drain, 1 spill-push); the only
   remaining anyhow! in args.rs is the lift_dispatch_error
   fallback test. Pump failures trip cancellation rather than
   surfacing, so bytes are unchanged by construction. Core lib
   410 plus integration 629 green; clippy at baseline
   (3 pre-existing), fmt clean on both touched files.
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
